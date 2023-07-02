use std::time::Duration;
use std::os::unix::process::ExitStatusExt;
use rlua::prelude::LuaError;
use std::sync::{Arc, Mutex};
use reqwest::{StatusCode, Response};
use tokio::process::Command;
use std::process::Stdio;
use std::process::ExitStatus;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use serde_json::json;
use serde::{Deserialize, de::DeserializeOwned, Serialize};
use std::task::{Context, Poll};
use std::pin::Pin;
use std::marker::Unpin;

mod protocol;
mod lua;
mod io;

use crate::io::ArtifactStream;
use crate::protocol::{ClientProto, CommandInfo, TaskInfo, RequestedJob};

#[derive(Debug)]
enum WorkAcquireError {
    Reqwest(reqwest::Error),
    EarlyEof,
    Protocol(String),
}

struct RunnerClient {
    http: reqwest::Client,
    host: String,
    tx: hyper::body::Sender,
    rx: Response,
    current_job: Option<RequestedJob>,
}

impl RequestedJob {
    pub fn into_running(self, client: RunnerClient) -> RunningJob {
        RunningJob {
            job: self,
            client,
            current_step: StepTracker::new(),
        }
    }
}

struct JobEnv {
    lua: lua::BuildEnv,
    job: Arc<Mutex<RunningJob>>,
}

impl JobEnv {
    fn new(job: &Arc<Mutex<RunningJob>>) -> Self {
        let lua = lua::BuildEnv::new(job);
        JobEnv {
            lua,
            job: Arc::clone(job)
        }
    }

    async fn default_goodfile(self) -> Result<(), LuaError> {
        self.lua.run_build(crate::lua::DEFAULT_RUST_GOODFILE).await
    }

    async fn exec_goodfile(self) -> Result<(), LuaError> {
        let script = std::fs::read_to_string("./tmpdir/goodfile").unwrap();
        self.lua.run_build(script.as_bytes()).await
    }
}

pub struct RunningJob {
    job: RequestedJob,
    client: RunnerClient,
    current_step: StepTracker,
}

enum RepoError {
    CloneFailedIdk { exit_code: ExitStatus },
    CheckoutFailedIdk { exit_code: ExitStatus },
    CheckoutFailedMissingRef,
}

pub struct StepTracker {
    scopes: Vec<String>
}

impl StepTracker {
    pub fn new() -> Self {
        StepTracker {
            scopes: Vec::new()
        }
    }

    pub fn push(&mut self, name: String) {
        self.scopes.push(name);
    }

    pub fn pop(&mut self) {
        self.scopes.pop();
    }

    pub fn clear(&mut self) {
        self.scopes.clear();
    }

    pub fn full_step_path(&self) -> &[String] {
        self.scopes.as_slice()
    }
}

impl RunningJob {
    async fn send_metric(&mut self, name: &str, value: String) -> Result<(), String> {
        self.client.send_typed(&ClientProto::metric(name, value))
            .await
            .map_err(|e| format!("failed to send metric {}: {:?})", name, e))
    }

    // TODO: panics if hyper finds the channel is closed. hum
    async fn create_artifact(&self, name: &str, desc: &str) -> Result<ArtifactStream, String> {
        let (mut sender, body) = hyper::Body::channel();
        let resp = self.client.http.post("https://ci.butactuallyin.space:9876/api/artifact")
            .header("user-agent", "ci-butactuallyin-space-runner")
            .header("x-task-token", &self.job.build_token)
            .header("x-artifact-name", name)
            .header("x-artifact-desc", desc)
            .body(body)
            .send()
            .await
            .map_err(|e| format!("unable to send request: {:?}", e))?;

        if resp.status() == StatusCode::OK {
            eprintln!("[+] artifact '{}' started", name);
            Ok(ArtifactStream::new(sender))
        } else {
            Err(format!("[-] unable to create artifact: {:?}", resp))
        }
    }

    async fn clone_remote(&self) -> Result<(), RepoError> {
        let mut git_clone = Command::new("git");
        git_clone
            .arg("clone")
            .arg(&self.job.remote_url)
            .arg("tmpdir");

        let clone_res = self.execute_command(git_clone, "git clone log", &format!("git clone {} tmpdir", &self.job.remote_url)).await
            .map_err(|e| {
                eprintln!("stringy error (exec failed?) for clone: {}", e);
                RepoError::CloneFailedIdk { exit_code: ExitStatus::from_raw(0) }
            })?;

        if !clone_res.success() {
            return Err(RepoError::CloneFailedIdk { exit_code: clone_res });
        }

        let mut git_checkout = Command::new("git");
        git_checkout
            .current_dir("tmpdir")
            .arg("checkout")
            .arg(&self.job.commit);

        let checkout_res = self.execute_command(git_checkout, "git checkout log", &format!("git checkout {}", &self.job.commit)).await
            .map_err(|e| {
                eprintln!("stringy error (exec failed?) for checkout: {}", e);
                RepoError::CheckoutFailedIdk { exit_code: ExitStatus::from_raw(0) }
            })?;

        if !checkout_res.success() {
            if checkout_res.code() == Some(128) {
                return Err(RepoError::CheckoutFailedIdk { exit_code: checkout_res });
            } else {
                return Err(RepoError::CheckoutFailedMissingRef);
            }
        }

        Ok(())
    }

    async fn execute_command(&self, mut command: Command, name: &str, desc: &str) -> Result<ExitStatus, String> {
        eprintln!("[.] running {}", name);
        let mut stdout_artifact = self.create_artifact(
            &format!("{} (stdout)", name),
            &format!("{} (stdout)", desc)
        ).await.expect("works");
        let mut stderr_artifact = self.create_artifact(
            &format!("{} (stderr)", name),
            &format!("{} (stderr)", desc)
        ).await.expect("works");

        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn '{}', {:?}", name, e))?;

        let mut child_stdout = child.stdout.take().unwrap();
        let mut child_stderr = child.stderr.take().unwrap();

        eprintln!("[.] '{}': forwarding stdout", name);
        tokio::spawn(async move { crate::io::forward_data(&mut child_stdout, &mut stdout_artifact).await });
        eprintln!("[.] '{}': forwarding stderr", name);
        tokio::spawn(async move { crate::io::forward_data(&mut child_stderr, &mut stderr_artifact).await });

        let res = child.wait().await
            .map_err(|e| format!("failed to wait? {:?}", e))?;

        if res.success() {
            eprintln!("[+] '{}' success", name);
        } else {
            eprintln!("[-] '{}' fail: {:?}", name, res);
        }

        Ok(res)
    }

    async fn run(mut self) {
        self.client.send_typed(&ClientProto::Started).await.unwrap();

        std::fs::remove_dir_all("tmpdir").unwrap();
        std::fs::create_dir("tmpdir").unwrap();

        let ctx = Arc::new(Mutex::new(self));

        let checkout_res = ctx.lock().unwrap().clone_remote().await;

        if let Err(e) = checkout_res {
            let status = "bad_ref";
            let status = ClientProto::task_status(TaskInfo::finished(status));
            eprintln!("checkout failed, reporting status: {:?}", status);

            let res = ctx.lock().unwrap().client.send_typed(&status).await;
            if let Err(e) = res {
                eprintln!("[!] FAILED TO REPORT JOB STATUS ({}): {:?}", "success", e);
            }

            return;
        }

        let lua_env = JobEnv::new(&ctx);

        let metadata = std::fs::metadata("./tmpdir/goodfile");
        let res: Result<String, (String, String)> = match metadata {
            Ok(_) => {
                match lua_env.exec_goodfile().await {
                    Ok(()) => {
                        Ok("pass".to_string())
                    },
                    Err(lua_err) => {
                        Err(("failed".to_string(), lua_err.to_string()))
                    }
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match lua_env.default_goodfile().await {
                    Ok(()) => {
                        Ok("pass".to_string())
                    },
                    Err(lua_err) => {
                        Err(("failed".to_string(), lua_err.to_string()))
                    }
                }
            },
            Err(e) => {
                eprintln!("[-] error finding goodfile: {:?}", e);
                Err(("failed".to_string(), "inaccessible goodfile".to_string()))
            }
        };

        match res {
            Ok(status) => {
                eprintln!("[+] job success!");
                let status = ClientProto::task_status(TaskInfo::finished(status));
                eprintln!("reporting status: {:?}", status);

                let res = ctx.lock().unwrap().client.send_typed(&status).await;
                if let Err(e) = res {
                    eprintln!("[!] FAILED TO REPORT JOB STATUS ({}): {:?}", "success", e);
                }
            }
            Err((status, lua_err)) => {
                eprintln!("[-] job error: {}", status);

                let status = ClientProto::task_status(TaskInfo::interrupted(status, lua_err.to_string()));
                let res = ctx.lock().unwrap().client.send_typed(&status).await;
                if let Err(e) = res {
                    eprintln!("[!] FAILED TO REPORT JOB STATUS ({:?}): {:?}", status, e);
                }
            }
        }
    }

    async fn run_command(&mut self, command: &[String], working_dir: Option<&str>) -> Result<(), String> {
        self.client.send_typed(&ClientProto::command(CommandInfo::started(command, working_dir, 1)))
            .await.unwrap();

        let mut cmd = Command::new(&command[0]);
        let cwd = match working_dir {
            Some(dir) => {
                format!("tmpdir/{}", dir)
            },
            None => {
                "tmpdir".to_string()
            }
        };
        eprintln!("running {:?} in {}", &command, &cwd);
        let human_name = command.join(" ");
        cmd
            .current_dir(cwd)
            .args(&command[1..]);

        let cmd_res = self.execute_command(cmd, &format!("{} log", human_name), &human_name).await?;

        self.client.send_typed(&ClientProto::command(CommandInfo::finished(cmd_res.code(), 1)))
            .await.unwrap();


        if !cmd_res.success() {
            return Err(format!("{} failed: {:?}", &human_name, cmd_res));
        }

        Ok(())
    }
}

impl RunnerClient {
    async fn new(host: &str, mut sender: hyper::body::Sender, mut res: Response) -> Result<Self, String> {
        if res.status() != StatusCode::OK {
            return Err(format!("server returned a bad response: {:?}, response itself: {:?}", res.status(), res));
        }

        let hello = res.chunk().await.expect("chunk");
        if hello.as_ref().map(|x| &x[..]) != Some(b"hello") {
            return Err(format!("bad hello: {:?}", hello));
        }

        Ok(Self {
            http: reqwest::ClientBuilder::new()
                .connect_timeout(Duration::from_millis(1000))
                .timeout(Duration::from_millis(600000))
                .build()
                .expect("can build client"),
            host: host.to_string(),
            tx: sender,
            rx: res,
            current_job: None,
        })
    }

    async fn wait_for_work(&mut self, accepted_pushers: Option<&[String]>) -> Result<Option<RequestedJob>, WorkAcquireError> {
        match self.rx.chunk().await {
            Ok(Some(chunk)) => {
                eprintln!("got chunk: {:?}", &chunk);
                let proto_message: ClientProto = serde_json::from_slice(&chunk)
                    .map_err(|e| {
                        WorkAcquireError::Protocol(format!("not json: {:?}", e))
                    })?;
                if let ClientProto::NewTask(new_task) = proto_message {
                    Ok(Some(new_task))
                } else {
                    Err(WorkAcquireError::Protocol(format!("unexpected message: {:?}", proto_message)))
                }
            }
            Ok(None) => {
                Ok(None)
            },
            Err(e) => {
                Err(WorkAcquireError::Reqwest(e))
            }
        }
    }

    async fn recv(&mut self) -> Result<Option<serde_json::Value>, String> {
        self.recv_typed().await
    }

    async fn recv_typed<T: DeserializeOwned>(&mut self) -> Result<Option<T>, String> {
        match self.rx.chunk().await {
            Ok(Some(chunk)) => {
                serde_json::from_slice(&chunk)
                    .map(Option::Some)
                    .map_err(|e| {
                        format!("not json: {:?}", e)
                    })
            },
            Ok(None) => Ok(None),
            Err(e) => {
                Err(format!("error in recv: {:?}", e))
            }
        }
    }

    async fn send(&mut self, value: serde_json::Value) -> Result<(), String> {
        self.send_typed(&value).await
    }

    async fn send_typed<T: Serialize>(&mut self, t: &T) -> Result<(), String> {
        self.tx.send_data(
            serde_json::to_vec(t)
                .map_err(|e| format!("json error: {:?}", e))?
                .into()
        ).await
            .map_err(|e| format!("send error: {:?}", e))
    }
}

#[derive(Deserialize, Serialize)]
struct RunnerConfig {
    server_address: String,
    auth_secret: String,
    allowed_pushers: Option<Vec<String>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let mut args = std::env::args();
    args.next().expect("first arg exists");
    let config_path = args.next().unwrap_or("./runner_config.json".to_string());
    let runner_config: RunnerConfig = serde_json::from_reader(std::fs::File::open(config_path).expect("file exists and is accessible")).expect("valid json for RunnerConfig");
    let client = reqwest::ClientBuilder::new()
        .connect_timeout(Duration::from_millis(1000))
        .timeout(Duration::from_millis(600000))
        .build()
        .expect("can build client");

    let host_info = host_info::collect_host_info();
    eprintln!("host info: {:?}", host_info);

    loop {
        let (mut sender, body) = hyper::Body::channel();

        sender.send_data(serde_json::to_string(&ClientProto::new_task_please(
            runner_config.allowed_pushers.clone(),
            host_info.clone(),
        )).unwrap().into()).await.expect("req");

        let poll = client.post("https://ci.butactuallyin.space:9876/api/next_job")
            .header("user-agent", "ci-butactuallyin-space-runner")
            .header("authorization", runner_config.auth_secret.trim())
            .body(body)
            .send()
            .await;

        match poll {
            Ok(mut res) => {
                let mut client = match RunnerClient::new("ci.butactuallyin.space:9876", sender, res).await {
                    Ok(client) => client,
                    Err(e) => {
                        eprintln!("failed to initialize client: {:?}", e);
                        std::thread::sleep(Duration::from_millis(10000));
                        continue;
                    }
                };
                let job = match client.wait_for_work(runner_config.allowed_pushers.as_ref().map(|x| x.as_ref())).await {
                    Ok(Some(request)) => request,
                    Ok(None) => {
                        eprintln!("no work to do (yet)");
                        std::thread::sleep(Duration::from_millis(2000));
                        continue;
                    }
                    Err(e) => {
                        eprintln!("failed to get work: {:?}", e);
                        std::thread::sleep(Duration::from_millis(10000));
                        continue;
                    }
                };
                eprintln!("requested work: {:?}", job);

                eprintln!("doing {:?}", job);

                let mut job = job.into_running(client);
                job.run().await;
                std::thread::sleep(Duration::from_millis(10000));
            },
            Err(e) => {
                let message = format!("{}", e);

                if message.contains("tcp connect error") {
                    eprintln!("could not reach server. sleeping a bit and retrying.");
                    std::thread::sleep(Duration::from_millis(5000));
                    continue;
                }

                eprintln!("unhandled error: {}", message);

                std::thread::sleep(Duration::from_millis(1000));
            }
        }
    }
}

mod host_info {
    use crate::protocol::{CpuInfo, EnvInfo, HostInfo, MemoryInfo};

    // get host model name, microcode, and how many cores
    fn collect_cpu_info() -> CpuInfo {
        let cpu_lines: Vec<String> = std::fs::read_to_string("/proc/cpuinfo").unwrap().split("\n").map(|line| line.to_string()).collect();
        let model_names: Vec<&String> = cpu_lines.iter().filter(|line| line.starts_with("model name")).collect();
        let model_name = model_names.first().expect("can get model name").to_string().split(":").last().unwrap().trim().to_string();
        let cores = model_names.len() as u32;
        let vendor_id = cpu_lines.iter().find(|line| line.starts_with("vendor_id")).expect("vendor_id line is present").split(":").last().unwrap().trim().to_string();
        let family = cpu_lines.iter().find(|line| line.starts_with("vendor_id")).expect("vendor_id line is present").split(":").last().unwrap().trim().to_string();
        let model = cpu_lines.iter().find(|line| line.starts_with("model\t")).expect("vendor_id line is present").split(":").last().unwrap().trim().to_string();
        let microcode = cpu_lines.iter().find(|line| line.starts_with("microcode")).expect("microcode line is present").split(":").last().unwrap().trim().to_string();

        CpuInfo { model_name, microcode, cores, vendor_id, family, model }
    }

    fn collect_mem_info() -> MemoryInfo {
        let mem_lines: Vec<String> = std::fs::read_to_string("/proc/meminfo").unwrap().split("\n").map(|line| line.to_string()).collect();
        let total = mem_lines[0].split(":").last().unwrap().trim().to_string();
        let available = mem_lines[2].split(":").last().unwrap().trim().to_string();

        MemoryInfo { total, available }
    }

    fn hostname() -> String {
        let mut bytes = [0u8; 4096];
        let res = unsafe {
            libc::gethostname(bytes.as_mut_ptr() as *mut i8, bytes.len())
        };
        if res != 0 {
            panic!("gethostname failed {:?}", res);
        }
        let end = bytes.iter().position(|b| *b == 0).expect("hostname is null-terminated");
        std::ffi::CStr::from_bytes_with_nul(&bytes[..end+1]).expect("null-terminated string").to_str().expect("is utf-8").to_string()
    }

    pub fn collect_env_info() -> EnvInfo {
        EnvInfo {
            arch: std::env::consts::ARCH.to_string(),
            family: std::env::consts::FAMILY.to_string(),
            os: std::env::consts::OS.to_string(),
        }
    }

    pub fn collect_host_info() -> HostInfo {
        let cpu_info = collect_cpu_info();
        let memory_info = collect_mem_info();
        let hostname = hostname();
        let env_info = collect_env_info();

        HostInfo {
            hostname,
            cpu_info,
            memory_info,
            env_info,
        }
    }
}

