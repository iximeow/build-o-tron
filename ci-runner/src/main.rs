use std::time::Duration;
use std::os::unix::process::ExitStatusExt;
use rlua::prelude::LuaError;
use std::sync::{Arc, Mutex};
use reqwest::{StatusCode, Response};
use tokio::process::Command;
use std::process::Stdio;
use std::process::ExitStatus;
use tokio::io::AsyncWrite;
use tokio::fs::OpenOptions;
use serde::{Deserialize, de::DeserializeOwned, Serialize};
use std::marker::Unpin;
use std::path::PathBuf;

use ci_lib_native::io;
use ci_lib_native::io::{ArtifactStream, VecSink};
use ci_lib_core::protocol::{ClientProto, CommandInfo, TaskInfo, RequestedJob};

mod lua;

use crate::lua::CommandOutput;

#[allow(dead_code)]
#[derive(Debug)]
enum WorkAcquireError {
    Reqwest(reqwest::Error),
    EarlyEof,
    Protocol(String),
}

/// `Runner` describes the logic bridging a local task runner with whatever causes the task runner
/// to execute. most concretely, `Runner` is the implementation that varies between "run this
/// goodfile locally right here" and "run a remotely-requested goodfile and report results back to
/// a server"
#[async_trait::async_trait]
trait Runner: Send + Sync + 'static {
    async fn report_start(&mut self) -> Result<(), String>;
    async fn report_task_status(&mut self, status: TaskInfo) -> Result<(), String>;
    async fn report_command_info(&mut self, info: CommandInfo) -> Result<(), String>;
    async fn send_metric(&mut self, name: &str, value: String) -> Result<(), String>;
    async fn create_artifact(&self, name: &str, desc: &str, build_token: &str) -> Result<Box<dyn AsyncWrite + Unpin + Send>, String>;
}

#[allow(dead_code)]
struct LocalRunner {
    working_dir: PathBuf,
    current_job: Option<RequestedJob>,
}

/// `LocalRunner` is the implementation supporting "i want to run this goodfile locally and not
/// need to collaborate with a remote server for executing this task to completion"
#[async_trait::async_trait]
impl Runner for LocalRunner {
    async fn report_start(&mut self) -> Result<(), String> {
        println!("starting task");
        Ok(())
    }
    async fn report_task_status(&mut self, status: TaskInfo) -> Result<(), String> {
        println!("task status: {:?}", status);
        Ok(())
    }
    async fn report_command_info(&mut self, info: CommandInfo) -> Result<(), String> {
        println!("command info: {:?}", info);
        Ok(())
    }
    async fn send_metric(&mut self, name: &str, value: String) -> Result<(), String> {
        println!("metric reported: {} = {}", name, value);
        Ok(())
    }
    async fn create_artifact(&self, name: &str, _desc: &str, _build_token: &str) -> Result<Box<dyn AsyncWrite + Unpin + Send>, String> {
        let mut path = self.working_dir.clone();
        path.push(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .await
            .map_err(|e| format!("error opening file to store artifact {}: {:?}", name, e))?;
        Ok(Box::new(file))
    }
}

/// `RmoteServerRunner` is the implementation of `Runner` supporting "a remote server has given me
/// a task", including reporting metrics and statuses back to the CI server.
struct RemoteServerRunner {
    http: reqwest::Client,
    #[allow(dead_code)]
    host: String,
    tx: hyper::body::Sender,
    rx: Response,
    #[allow(dead_code)]
    current_job: Option<RequestedJob>,
}

#[async_trait::async_trait]
impl Runner for RemoteServerRunner {
    async fn report_start(&mut self) -> Result<(), String> {
        self.send_typed(&ClientProto::Started).await
    }
    async fn report_task_status(&mut self, status: TaskInfo) -> Result<(), String> {
        self.send_typed(&ClientProto::task_status(status))
            .await
    }
    async fn report_command_info(&mut self, info: CommandInfo) -> Result<(), String> {
        self.send_typed(&ClientProto::command(info))
            .await
            .map_err(|e| format!("failed to report command info: {:?})", e))
    }
    async fn send_metric(&mut self, name: &str, value: String) -> Result<(), String> {
        self.send_typed(&ClientProto::metric(name, value))
            .await
            .map_err(|e| format!("failed to send metric {}: {:?})", name, e))
    }
    // TODO: panics if hyper finds the channel is closed. hum
    async fn create_artifact(&self, name: &str, desc: &str, build_token: &str) -> Result<Box<dyn AsyncWrite + Unpin + Send>, String> {
        let (sender, body) = hyper::Body::channel();
        let resp = self.http.post("https://ci.butactuallyin.space:9876/api/artifact")
            .header("user-agent", "ci-butactuallyin-space-runner")
            .header("x-task-token", build_token)
            .header("x-artifact-name", name)
            .header("x-artifact-desc", desc)
            .body(body)
            .send()
            .await
            .map_err(|e| format!("unable to send request: {:?}", e))?;

        if resp.status() == StatusCode::OK {
            eprintln!("[+] artifact '{}' started", name);
            Ok(Box::new(ArtifactStream::new(sender)) as Box<dyn AsyncWrite + Unpin + Send>)
        } else {
            Err(format!("[-] unable to create artifact: {:?}", resp))
        }
    }
}

impl RunningJob {
    fn local_from_job(job: RequestedJob) -> Self {
        let mut working_dir = PathBuf::new();
        working_dir.push("ci_working_dir");
        if std::path::Path::new("ci_working_dir").exists() {
            eprintln!("[!] removing prior ci_working_dir to store artifacts");
            std::fs::remove_dir_all("ci_working_dir").unwrap();
        }
        std::fs::create_dir(&working_dir).expect("can create artifacts working dir");
        Self {
            job,
            runner_ctx: Box::new(LocalRunner {
                working_dir,
                current_job: None,
            }) as Box<dyn Runner>,
            current_step: StepTracker::new(),
        }
    }
    fn remote_from_job(job: RequestedJob, client: RemoteServerRunner) -> Self {
        Self {
            job,
            runner_ctx: Box::new(client) as Box<dyn Runner>,
            current_step: StepTracker::new(),
        }
    }
}

struct JobEnv {
    lua: lua::BuildEnv,
    #[allow(dead_code)]
    job: Arc<Mutex<Box<RunningJob>>>,
}

impl JobEnv {
    fn new(job: &Arc<Mutex<Box<RunningJob>>>) -> Self {
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
    runner_ctx: Box<dyn Runner>,
    current_step: StepTracker,
}

#[allow(dead_code)]
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
        self.runner_ctx.send_metric(name, value).await
    }

    async fn create_artifact(&self, name: &str, desc: &str) -> Result<Box<dyn AsyncWrite + Unpin + Send>, String> {
        self.runner_ctx.create_artifact(name, desc, &self.job.build_token).await
    }

    async fn clone_remote(&self) -> Result<(), RepoError> {
        let mut git_clone = Command::new("git");
        git_clone
            .arg("clone")
            .arg(&self.job.remote_url)
            .arg("tmpdir");

        let clone_res = self.execute_command_and_report(git_clone, "git clone log", &format!("git clone {} tmpdir", &self.job.remote_url)).await
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

        let checkout_res = self.execute_command_and_report(git_checkout, "git checkout log", &format!("git checkout {}", &self.job.commit)).await
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

    async fn execute_command_and_report(&self, command: Command, name: &str, desc: &str) -> Result<ExitStatus, String> {
        let stdout_artifact = self.create_artifact(
            &format!("{} (stdout)", name),
            &format!("{} (stdout)", desc)
        ).await.expect("works");
        let stderr_artifact = self.create_artifact(
            &format!("{} (stderr)", name),
            &format!("{} (stderr)", desc)
        ).await.expect("works");

        let exit_status = self.execute_command(command, name, desc, stdout_artifact, stderr_artifact).await?;

        Ok(exit_status)
    }

    async fn execute_command_capture_output(&self, command: Command, name: &str, desc: &str) -> Result<crate::lua::CommandOutput, String> {
        let stdout_collector = VecSink::new();
        let stderr_collector = VecSink::new();

        let exit_status = self.execute_command(command, name, desc, stdout_collector.clone(), stderr_collector.clone()).await?;

        Ok(CommandOutput {
            exit_status,
            stdout: stdout_collector.take_buf(),
            stderr: stderr_collector.take_buf(),
        })
    }

    async fn execute_command(&self, mut command: Command, name: &str, _desc: &str, mut stdout_reporter: impl AsyncWrite + Unpin + Send + 'static, mut stderr_reporter: impl AsyncWrite + Unpin + Send + 'static) -> Result<ExitStatus, String> {
        eprintln!("[.] running {}: {:?}", name, command);

        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn '{}', {:?}", name, e))?;

        let mut child_stdout = child.stdout.take().unwrap();
        let mut child_stderr = child.stderr.take().unwrap();

        eprintln!("[.] '{}': forwarding stdout", name);
        tokio::spawn(async move { io::forward_data(&mut child_stdout, &mut stdout_reporter).await });
        eprintln!("[.] '{}': forwarding stderr", name);
        tokio::spawn(async move { io::forward_data(&mut child_stderr, &mut stderr_reporter).await });

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
        self.runner_ctx.report_start().await.unwrap();

        if std::path::Path::new("tmpdir").exists() {
            eprintln!("[!] removing prior tmpdir to rebuild into");
            std::fs::remove_dir_all("tmpdir").unwrap();
        }
        std::fs::create_dir("tmpdir").unwrap();

        let ctx = Arc::new(Mutex::new(Box::new(self) as Box<RunningJob>));

        let checkout_res = ctx.lock().unwrap().clone_remote().await;

        if let Err(_e) = checkout_res {
            let status = "bad_ref";
            let status = TaskInfo::finished(status);
            eprintln!("checkout failed, reporting status: {:?}", status);

            let res = ctx.lock().unwrap().runner_ctx.report_task_status(status).await;
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
                let status = TaskInfo::finished(status);
                eprintln!("reporting status: {:?}", status);

                let res = ctx.lock().unwrap().runner_ctx.report_task_status(status).await;
                if let Err(e) = res {
                    eprintln!("[!] FAILED TO REPORT JOB STATUS ({}): {:?}", "success", e);
                }
            }
            Err((status, lua_err)) => {
                eprintln!("[-] job error: {}", status);
                let status = TaskInfo::interrupted(status, lua_err.to_string());

                let res = ctx
                    .lock()
                    .unwrap()
                    .runner_ctx
                    .report_task_status(status.clone()).await;
                if let Err(e) = res {
                    eprintln!("[!] FAILED TO REPORT JOB STATUS ({:?}): {:?}", status, e);
                }
            }
        }
    }

    fn prep_command(command: &[String], working_dir: Option<&str>) -> (Command, String) {
        let mut cmd = Command::new(&command[0]);
        let cwd = match working_dir {
            Some(dir) => {
                format!("tmpdir/{}", dir)
            },
            None => {
                "tmpdir".to_string()
            }
        };
        eprintln!("prepared {:?} to run in {}", &command, &cwd);
        let human_name = command.join(" ");
        cmd
            .current_dir(cwd)
            .args(&command[1..]);
        (cmd, human_name)
    }

    async fn run_with_output(&mut self, command: &[String], working_dir: Option<&str>) -> Result<CommandOutput, String> {
        let (cmd, human_name) = Self::prep_command(command, working_dir);

        let cmd_res = self.execute_command_capture_output(cmd, &format!("{} log", human_name), &human_name).await?;

        if !cmd_res.exit_status.success() {
            return Err(format!("{} failed: {:?}", &human_name, cmd_res.exit_status));
        }
        Ok(cmd_res)
    }

    async fn run_command(&mut self, command: &[String], working_dir: Option<&str>) -> Result<(), String> {
        self.runner_ctx.report_command_info(CommandInfo::started(command, working_dir, 1)).await.unwrap();

        let (cmd, human_name) = Self::prep_command(command, working_dir);

        let cmd_res = self.execute_command_and_report(cmd, &format!("{} log", human_name), &human_name).await?;

        self.runner_ctx.report_command_info(CommandInfo::finished(cmd_res.code(), 1)).await.unwrap();

        if !cmd_res.success() {
            return Err(format!("{} failed: {:?}", &human_name, cmd_res));
        }

        Ok(())
    }
}

impl RemoteServerRunner {
    async fn new(host: &str, sender: hyper::body::Sender, mut res: Response) -> Result<Self, String> {
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

    async fn wait_for_work(&mut self, _accepted_pushers: Option<&[String]>) -> Result<Option<RequestedJob>, WorkAcquireError> {
        loop {
            let message = self.recv_typed::<ClientProto>().await;
            match message {
                Ok(Some(ClientProto::NewTask(new_task))) => {
                    // TODO: verify that `new_task` is for a commit authored by someone we're
                    // willing to run work for.
                    //
                    // we're also trusting the server to only tell us about work we would be
                    // interested in running, so if this rejects a task it's a server bug that we
                    // got the task in the first place or a client bug that the list of accepted
                    // pushers varied.
                    return Ok(Some(new_task));
                },
                Ok(Some(ClientProto::Ping)) => {
                    self.send_typed(&ClientProto::Pong).await
                        .map_err(|e| WorkAcquireError::Protocol(format!("failed to pong: {}", e)))?;
                },
                Ok(Some(other)) => {
                    return Err(WorkAcquireError::Protocol(format!("unexpected message: {:?}", other)));
                },
                Ok(None) => {
                    return Ok(None);
                },
                Err(e) => {
                    return Err(WorkAcquireError::Protocol(e));
                }
            }
        }
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

    if config_path.ends_with("goodfile") {
        run_local(config_path).await
    } else {
        run_remote(config_path).await
    }
}

async fn run_local(config_path: String) {
    let path = PathBuf::from(config_path);
    let repo = path.parent().expect("goodfile has a parent directory");
    if !std::path::Path::new(&format!("{}/.git", repo.display())).exists() {
        panic!("found goodfile parent directory '{}', but it is not a git repo", repo.display());
    }
    // TODO: check for uncommitted changes, warn that these will not be cloned or built...
    let log_output = Command::new("git")
        .current_dir(repo)
        .args(&["log", "--pretty=format:%H", "-n", "1"])
        .output()
        .await
        .expect("git log completes");
    let out_string = std::str::from_utf8(&log_output.stdout).expect("git reports utf8 text");
    let current_commit = out_string.trim().to_string();
    let job = RequestedJob {
        commit: current_commit,
        remote_url: repo.display().to_string(),
        build_token: "n/a".to_string(),
    };
    let job = RunningJob::local_from_job(job);
    job.run().await;
}

async fn run_remote(config_path: String) {
    let runner_file = std::fs::File::open(&config_path).unwrap_or_else(|e| {
        panic!("could not open runner config at '{}': {}", config_path, e);
    });
    let runner_config: RunnerConfig = serde_json::from_reader(runner_file).expect("valid json for RunnerConfig");
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
            Ok(res) => {
                let mut client = match RemoteServerRunner::new("ci.butactuallyin.space:9876", sender, res).await {
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

                let job = RunningJob::remote_from_job(job, client);
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
    use ci_lib_core::protocol::{CpuInfo, EnvInfo, HostInfo, MemoryInfo};

    // get host model name, microcode, and how many cores
    fn collect_cpu_info() -> CpuInfo {
        fn find_line(lines: &[String], prefix: &str) -> String {
            try_find_line(lines, prefix).expect(&format!("{} line is present", prefix))
        }

        fn try_find_line(lines: &[String], prefix: &str) -> Option<String> {
            lines.iter()
                .find(|line| line.starts_with(prefix))
                .map(|line| {
                    line
                        .split(":")
                        .last()
                        .unwrap()
                        .trim()
                        .to_string()
                })
        }

        /// try finding core `cpu`'s max frequency in khz. we'll assume this is the actual speed a
        /// build would run at.. fingers crossed.
        fn try_finding_cpu_freq(cpu: u32) -> Result<u64, String> {
            if let Ok(freq_str) = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq") {
                Ok(freq_str.trim().parse().unwrap())
            } else {
                // so cpufreq probably isn't around, maybe /proc/cpuinfo's mhz figure is present?
                let cpu_lines: Vec<String> = std::fs::read_to_string("/proc/cpuinfo").unwrap().split("\n").map(|line| line.to_string()).collect();
                let cpu_mhzes: Vec<&String> = cpu_lines.iter().filter(|line| line.starts_with("cpu MHz")).collect();
                match cpu_mhzes.get(cpu as usize) {
                    Some(mhz) => {
                        let mut line_parts = mhz.split(":");
                        let _ = line_parts.next();
                        let mhz = line_parts.next().unwrap().trim();
                        let mhz: f64 = mhz.parse().unwrap();
                        Ok((mhz * 1000.0) as u64)
                    },
                    None => {
                        panic!("could not get cpu freq either from cpufreq or /proc/cpuinfo?");
                    }
                }
            }
        }

        // we'll have to deploy one of a few techniques, because x86/x86_64 is internally
        // consistent, but aarch64 is different. who knows what other CPUs think.
        match std::env::consts::ARCH {
            "x86" | "x86_64" => {
                let cpu_lines: Vec<String> = std::fs::read_to_string("/proc/cpuinfo").unwrap().split("\n").map(|line| line.to_string()).collect();
                let model_names: Vec<&String> = cpu_lines.iter().filter(|line| line.starts_with("model name")).collect();
                let cores = model_names.len() as u32;
                let model_name = find_line(&cpu_lines, "model name");
                let vendor_id = find_line(&cpu_lines, "vendor_id");
                let family = find_line(&cpu_lines, "cpu family");
                let model = find_line(&cpu_lines, "model\t");
                let microcode = find_line(&cpu_lines, "microcode");
                let max_freq = try_finding_cpu_freq(0).unwrap();

                CpuInfo { model_name, microcode, cores, vendor_id, family, model, max_freq }
            }
            "aarch64" => {
                let cpu_lines: Vec<String> = std::fs::read_to_string("/proc/cpuinfo").unwrap().split("\n").map(|line| line.to_string()).collect();
                let processors: Vec<&String> = cpu_lines.iter().filter(|line| line.starts_with("processor")).collect();
                let cores = processors.len() as u32;

                let model_name = if let Ok(value) = std::env::var("MODEL_NAME_OVERRIDE") {
                    value
                } else {
                    // alternate possible path: /sys/firmware/devicetree/base/compatible
                    // except the one mt. jade server which has just.. nothing under devicetree. no
                    // idea why! start there, and if there's nothing then... try reading it from the
                    // runner config???
                    let model_name_path = std::path::Path::new("/proc/device-tree/compatible");
                    if model_name_path.exists() {
                        let model_name = std::fs::read_to_string(model_name_path).unwrap();
                        let model_name = model_name.replace("\x00", ";");
                        model_name
                    } else {
                        eprintln!("[!] {} does not exist, model name is unknown!", model_name_path.display());
                        "unknown".to_string()
                    }
                };
                let vendor_id = find_line(&cpu_lines, "CPU implementer");
                let vendor_name = match vendor_id.as_str() {
                    "0x41" => "Arm Limited".to_string(),
                    "0x42" => "Broadcom Corporation".to_string(),
                    "0x43" => "Cavium Inc".to_string(),
                    "0x44" => "Digital Equipment Corporation".to_string(),
                    "0x46" => "Fujitsu Ltd".to_string(),
                    "0x49" => "Infineon Technologies AG".to_string(),
                    "0x4d" => "Motorola".to_string(),
                    "0x4e" => "NVIDIA Corporation".to_string(),
                    "0x50" => "Applied Micro Circuits Corporation".to_string(),
                    "0x51" => "Qualcomm Inc".to_string(),
                    "0x56" => "Marvell International Ltd".to_string(),
                    "0x69" => "Intel Corporation".to_string(),
                    "0xc0" => "Ampere Computing".to_string(),
                    other => format!("unknown aarch64 vendor {}", other),
                };
                let family = find_line(&cpu_lines, "CPU architecture");
                let model = find_line(&cpu_lines, "CPU part");
                let microcode = String::new();
                let max_freq = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").unwrap().trim().parse().unwrap();

                CpuInfo { model_name, microcode, cores, vendor_id: vendor_name, family, model, max_freq }
            }
            other => {
                panic!("dunno how to find cpu info for {}, panik", other);
            }
        }
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
            libc::gethostname(bytes.as_mut_ptr() as *mut std::ffi::c_char, bytes.len())
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

