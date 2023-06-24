use std::time::Duration;
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

mod lua;
mod io;

use crate::io::ArtifactStream;

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

#[derive(Debug, Serialize, Deserialize)]
struct RequestedJob {
    commit: String,
    remote_url: String,
    build_token: String,
}

impl RequestedJob {
    pub fn into_running(self, client: RunnerClient) -> RunningJob {
        RunningJob {
            job: self,
            client,
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
}

impl RunningJob {
    async fn send_metric(&mut self, name: &str, value: String) -> Result<(), String> {
        self.client.send(serde_json::json!({
            "kind": "metric",
            "name": name,
            "value": value,
        })).await
            .map_err(|e| format!("failed to send metric {}: {:?})", name, e))
    }

    // TODO: panics if hyper finds the channel is closed. hum
    async fn create_artifact(&self, name: &str, desc: &str) -> Result<ArtifactStream, String> {
        let (mut sender, body) = hyper::Body::channel();
        let resp = self.client.http.post("https://ci.butactuallyin.space:9876/api/artifact")
            .header("user-agent", "ci-butactuallyin-space-runner")
            .header("x-job-token", &self.job.build_token)
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

    async fn clone_remote(&self) -> Result<(), String> {
        let mut git_clone = Command::new("git");
        git_clone
            .arg("clone")
            .arg(&self.job.remote_url)
            .arg("tmpdir");

        let clone_res = self.execute_command(git_clone, "git clone log", &format!("git clone {} tmpdir", &self.job.remote_url)).await?;

        if !clone_res.success() {
            return Err(format!("git clone failed: {:?}", clone_res));
        }

        let mut git_checkout = Command::new("git");
        git_checkout
            .current_dir("tmpdir")
            .arg("checkout")
            .arg(&self.job.commit);

        let checkout_res = self.execute_command(git_checkout, "git checkout log", &format!("git checkout {}", &self.job.commit)).await?;

        if !checkout_res.success() {
            return Err(format!("git checkout failed: {:?}", checkout_res));
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
        self.client.send(serde_json::json!({
            "status": "started"
        })).await.unwrap();

        std::fs::remove_dir_all("tmpdir").unwrap();
        std::fs::create_dir("tmpdir").unwrap();

        self.clone_remote().await.expect("clone succeeds");
        
        let ctx = Arc::new(Mutex::new(self));

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
                let status = serde_json::json!({
                    "kind": "job_status",
                    "state": "finished",
                    "result": status
                });
                eprintln!("reporting status: {}", status);

                let res = ctx.lock().unwrap().client.send(status).await;
                if let Err(e) = res {
                    eprintln!("[!] FAILED TO REPORT JOB STATUS ({}): {:?}", "success", e);
                }
            }
            Err((status, lua_err)) => {
                eprintln!("[-] job error: {}", status);

                let res = ctx.lock().unwrap().client.send(serde_json::json!({
                    "kind": "job_status",
                    "state": "interrupted",
                    "result": status,
                    "desc": lua_err.to_string(),
                })).await;
                if let Err(e) = res {
                    eprintln!("[!] FAILED TO REPORT JOB STATUS ({}): {:?}", status, e);
                }
            }
        }
    }

    async fn run_command(&mut self, command: &[String], working_dir: Option<&str>) -> Result<(), String> {
        self.client.send(serde_json::json!({
            "kind": "command",
            "state": "started",
            "command": command,
            "cwd": working_dir,
            "id": 1,
        })).await.unwrap();

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

        self.client.send(serde_json::json!({
            "kind": "command",
            "state": "finished",
            "exit_code": cmd_res.code(),
            "id": 1,
        })).await.unwrap();


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
                serde_json::from_slice(&chunk)
                    .map(Option::Some)
                    .map_err(|e| {
                        WorkAcquireError::Protocol(format!("not json: {:?}", e))
                    })
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
        self.tx.send_data(
            serde_json::to_vec(&value)
                .map_err(|e| format!("json error: {:?}", e))?
                .into()
        ).await
            .map_err(|e| format!("send error: {:?}", e))
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let secret = std::fs::read_to_string("./auth_secret").unwrap();
    let client = reqwest::ClientBuilder::new()
        .connect_timeout(Duration::from_millis(1000))
        .timeout(Duration::from_millis(600000))
        .build()
        .expect("can build client");

    let allowed_pushers: Option<Vec<String>> = None;

    loop {
        let (mut sender, body) = hyper::Body::channel();

        sender.send_data(serde_json::to_string(&json!({
            "kind": "new_job_please",
            "accepted_pushers": &["git@iximeow.net", "me@iximeow.net"],
        })).unwrap().into()).await.expect("req");

        let poll = client.post("https://ci.butactuallyin.space:9876/api/next_job")
            .header("user-agent", "ci-butactuallyin-space-runner")
            .header("authorization", &secret)
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
                let job = match client.wait_for_work(allowed_pushers.as_ref().map(|x| x.as_ref())).await {
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
