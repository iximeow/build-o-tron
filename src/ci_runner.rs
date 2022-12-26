use std::time::Duration;
use reqwest::{StatusCode, Response};
use tokio::process::Command;
use std::process::Stdio;
use std::process::ExitStatus;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use serde_derive::{Deserialize, Serialize};
use serde_json::json;
use serde::{Deserialize, de::DeserializeOwned, Serialize};
use std::task::{Context, Poll};
use std::pin::Pin;
use std::marker::Unpin;

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
    // TODO: panics if hyper finds the channel is closed. hum
    async fn create_artifact(&self, client: &mut RunnerClient, name: &str, desc: &str) -> Result<ArtifactStream, String> {
        eprintln!("[?] creating artifact...");
        let (mut sender, body) = hyper::Body::channel();
        let resp = client.http.post("https://ci.butactuallyin.space:9876/api/artifact")
            .header("user-agent", "ci-butactuallyin-space-runner")
            .header("x-job-token", &self.build_token)
            .header("x-artifact-name", name)
            .header("x-artifact-desc", desc)
            .body(body)
            .send()
            .await
            .map_err(|e| format!("unable to send request: {:?}", e))?;

        if resp.status() == StatusCode::OK {
            eprintln!("[+] artifact '{}' started", name);
            Ok(ArtifactStream {
                sender,
            })
        } else {
            Err(format!("[-] unable to create artifact: {:?}", resp))
        }
    }

    async fn execute_command(&self, client: &mut RunnerClient, mut command: Command, name: &str, desc: &str) -> Result<ExitStatus, String> {
        eprintln!("[.] running {}", name);
        async fn forward_data(mut source: impl AsyncRead + Unpin, mut dest: impl AsyncWrite + Unpin) -> Result<(), String> {
            let mut buf = vec![0; 1024 * 1024];
            loop {
                let n_read = source.read(&mut buf).await
                    .map_err(|e| format!("failed to read: {:?}", e))?;

                if n_read == 0 {
                    return Ok(());
                }

                dest.write_all(&buf[..n_read]).await
                    .map_err(|e| format!("failed to write: {:?}", e))?;
            }
        }

        let stdout_artifact = self.create_artifact(
            client,
            &format!("{} (stdout)", name),
            &format!("{} (stdout)", desc)
        ).await.expect("works");
        let stderr_artifact = self.create_artifact(
            client,
            &format!("{} (stderr)", name),
            &format!("{} (stderr)", desc)
        ).await.expect("works");

        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn '{}', {:?}", name, e))?;

        let child_stdout = child.stdout.take().unwrap();
        let child_stderr = child.stderr.take().unwrap();

        eprintln!("[.] '{}': forwarding stdout", name);
        tokio::spawn(forward_data(child_stdout, stdout_artifact));
        eprintln!("[.] '{}': forwarding stderr", name);
        tokio::spawn(forward_data(child_stderr, stderr_artifact));

        let res = child.wait().await
            .map_err(|e| format!("failed to wait? {:?}", e))?;

        if res.success() {
            eprintln!("[+] '{}' success", name);
        } else {
            eprintln!("[-] '{}' fail: {:?}", name, res);
        }

        Ok(res)
    }

    async fn execute_goodfile(&self, client: &mut RunnerClient) -> Result<String, String> {
        let mut git_clone = Command::new("git");
        git_clone
            .arg("clone")
            .arg(&self.remote_url)
            .arg("tmpdir");

        let clone_res = self.execute_command(client, git_clone, "git clone log", &format!("git clone {} tmpdir", &self.remote_url)).await?;

        if !clone_res.success() {
            return Err(format!("git clone failed: {:?}", clone_res));
        }

        let mut git_checkout = Command::new("git");
        git_checkout
            .current_dir("tmpdir")
            .arg("checkout")
            .arg(&self.commit);

        let checkout_res = self.execute_command(client, git_checkout, "git checkout log", &format!("git checkout {}", &self.commit)).await?;

        if !checkout_res.success() {
            return Err(format!("git checkout failed: {:?}", checkout_res));
        }

        let mut build = Command::new("cargo");
        build
            .current_dir("tmpdir")
            .arg("build");

        let build_res = self.execute_command(client, build, "cargo build log", "cargo build").await?;

        if !build_res.success() {
            return Err(format!("cargo build failed: {:?}", build_res));
        }

        let mut test = Command::new("cargo");
        test
            .current_dir("tmpdir")
            .arg("test");

        let test_res = self.execute_command(client, test, "cargo test log", "cargo test").await?;

        match test_res.code() {
            Some(0) => Ok("pass".to_string()),
            Some(n) => Ok(format!("error: {}", n)),
            None => Ok(format!("abnormal exit")),
        }
    }
}

struct ArtifactStream {
    sender: hyper::body::Sender,
}

impl tokio::io::AsyncWrite for ArtifactStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8]
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut().sender.try_send_data(buf.to_vec().into()) {
            Ok(()) => {
                Poll::Ready(Ok(buf.len()))
            },
            _ => {
                Poll::Pending
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
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

    async fn run_job(&mut self, job: RequestedJob) {
        self.send(serde_json::json!({
            "status": "started"
        })).await.unwrap();

        std::fs::remove_dir_all("tmpdir").unwrap();
        std::fs::create_dir("tmpdir").unwrap();

        let res = job.execute_goodfile(self).await;

        match res {
            Ok(status) => {
                eprintln!("[+] job success!");

                self.send(serde_json::json!({
                    "kind": "job_status",
                    "state": "finished",
                    "result": status
                })).await.unwrap();
            }
            Err(status) => {
                eprintln!("[-] job error: {}", status);

                self.send(serde_json::json!({
                    "kind": "job_status",
                    "state": "interrupted",
                    "result": status
                })).await.unwrap();
            }
        }
    }
}

#[tokio::main]
async fn main() {
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
                client.run_job(job).await;
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
