use std::time::Duration;
use reqwest::{StatusCode, Response};
use std::process::Command;
use serde_derive::{Deserialize, Serialize};
use serde::{Deserialize, de::DeserializeOwned, Serialize};

#[derive(Debug)]
enum WorkAcquireError {
    Reqwest(reqwest::Error),
    EarlyEof,
    Protocol(String),
}

struct RunnerClient {
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
        let create_artifact_message = serde_json::json!({
            "kind": "artifact_create",
            "name": name,
            "description": desc,
            "job_token": &self.build_token,
        });
        client.send(create_artifact_message).await
            .map_err(|e| format!("create artifact send error: {:?}", e))?;
        let resp = client.recv().await
            .map_err(|e| format!("create artifact recv error: {:?}", e))?;
        eprintln!("resp: {:?}", resp);
        let object_id = resp.unwrap()
            .as_object().expect("is an object")
            .get("object_id").unwrap().as_str().expect("is str")
            .to_owned();
        // POST to this object id...
        Ok(ArtifactStream {
            object_id,
        })
    }

    async fn execute_goodfile(&self, client: &mut RunnerClient) -> Result<String, String> {
        let clone_log = self.create_artifact(client, "git clone log", &format!("git clone {} tmpdir", &self.remote_url)).await.expect("works");

        let clone_res = Command::new("git")
            .arg("clone")
            .arg(&self.remote_url)
            .arg("tmpdir")
            .status()
            .map_err(|e| format!("failed to run git clone? {:?}", e))?;

        if !clone_res.success() {
            return Err(format!("git clone failed: {:?}", clone_res));
        }

        let checkout_log = self.create_artifact(client, "git checkout log", &format!("git checkout {}", &self.commit)).await.expect("works");

        let checkout_res = Command::new("git")
            .current_dir("tmpdir")
            .arg("checkout")
            .arg(&self.commit)
            .status()
            .map_err(|e| format!("failed to run git checkout? {:?}", e))?;

        if !checkout_res.success() {
            return Err(format!("git checkout failed: {:?}", checkout_res));
        }

        let build_log = self.create_artifact(client, "cargo build log", "cargo build").await.expect("works");

        let build_res = Command::new("cargo")
            .current_dir("tmpdir")
            .arg("build")
            .status()
            .map_err(|e| format!("failed to run cargo build? {:?}", e))?;

        if !build_res.success() {
            return Err(format!("cargo build failed: {:?}", build_res));
        }

        let test_log = self.create_artifact(client, "cargo test log", "cargo test").await.expect("works");

        let test_result = Command::new("cargo")
            .current_dir("tmpdir")
            .arg("test")
            .status()
            .map_err(|e| format!("failed to run cargo test? {:?}", e))?;

        match test_result.code() {
            Some(0) => Ok("pass".to_string()),
            Some(n) => Ok(format!("error: {}", n)),
            None => Ok(format!("abnormal exit")),
        }
    }
}

struct ArtifactStream {
    object_id: String,
}

impl RunnerClient {
    async fn new(host: &str, sender: hyper::body::Sender, mut res: Response) -> Result<Self, String> {
        if res.status() != StatusCode::OK {
            return Err(format!("server returned a bad response: {:?}, response itself: {:?}", res.status(), res));
        }

        let hello = res.chunk().await.expect("chunk");
        if hello.as_ref().map(|x| &x[..]) != Some(b"hello") {
            return Err(format!("bad hello: {:?}", hello));
        }

        Ok(Self {
            host: host.to_string(),
            tx: sender,
            rx: res,
            current_job: None,
        })
    }

    async fn wait_for_work(&mut self) -> Result<Option<RequestedJob>, WorkAcquireError> {
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
                self.send(serde_json::json!({
                    "kind": "job_status",
                    "state": "finished",
                    "result": status
                })).await.unwrap();
            }
            Err(status) => {
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

    loop {
        let (mut sender, body) = hyper::Body::channel();
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
                let job = match client.wait_for_work().await {
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
