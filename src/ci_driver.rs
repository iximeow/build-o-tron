use std::process::Command;
use std::io::Read;
use serde_derive::{Deserialize, Serialize};
use futures_util::StreamExt;
use std::fmt;
use std::path::{Path, PathBuf};
use tokio::spawn;
use tokio_stream::wrappers::ReceiverStream;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use axum_server::tls_rustls::RustlsConfig;
use axum::body::StreamBody;
use axum::http::{StatusCode};
use hyper::HeaderMap;
use axum::Router;
use axum::routing::*;
use axum::extract::State;
use axum::extract::BodyStream;
use axum::response::IntoResponse;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use serde_json::json;

mod dbctx;
mod sql;
mod notifier;

use crate::dbctx::{DbCtx, PendingJob};
use crate::sql::JobState;

fn reserve_artifacts_dir(job: u64) -> std::io::Result<PathBuf> {
    let mut path: PathBuf = "/root/ixi_ci_server/jobs/".into();
    path.push(job.to_string());
    match std::fs::create_dir(&path) {
        Ok(()) => {
            Ok(path)
        },
        Err(e) => {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(path)
            } else {
                Err(e)
            }
        }
    }
}

async fn activate_job(dbctx: Arc<DbCtx>, job: &PendingJob, clients: &mut mpsc::Receiver<RunnerClient>) -> Result<(), String> {
    let connection = dbctx.conn.lock().unwrap();
    let (repo_id, remote_git_url): (u64, String) = connection
        .query_row("select repo_id, remote_git_url from remotes where id=?1", [job.remote_id], |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
        .expect("query succeeds");
    let repo_name: String = connection
        .query_row("select repo_name from repos where id=?1", [repo_id], |row| row.get(0))
        .expect("query succeeds");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("now is before epoch")
        .as_millis();

    let commit_sha: String = connection
        .query_row("select sha from commits where id=?1", [job.commit_id], |row| row.get(0))
        .expect("can run query");

    let artifacts: PathBuf = match &job.artifacts {
        Some(artifacts) => PathBuf::from(artifacts),
        None => reserve_artifacts_dir(job.id).expect("can reserve a directory for artifacts")
    };
    /*

    if job.run_host.as_ref() == None {
        eprintln!("need to find a host to run the job");
    }

    eprintln!("cloning {}", remote_git_url);
    let mut repo_dir = artifacts.clone();
    repo_dir.push("repo");
    eprintln!(" ... into {}", repo_dir.display());

    Command::new("git")
        .arg("clone")
        .arg(&remote_git_url)
        .arg(&format!("{}", repo_dir.display()))
        .status()
        .expect("can clone the repo");

    eprintln!("checking out {}", commit_sha);
    Command::new("git")
        .current_dir(&repo_dir)
        .arg("checkout")
        .arg(&commit_sha)
        .status()
        .expect("can checkout hash");
    */

    eprintln!("running {}", repo_name);
    /*
     * find the CI script, figure out how to run it
     */

    let mut client_job = loop {
        let mut candidate = clients.recv().await
            .ok_or_else(|| "client channel disconnected".to_string())?;

        if let Ok(Some(mut client_job)) = candidate.submit(&dbctx, &job, &remote_git_url, &commit_sha).await {
            break client_job;
        } else {
            // failed to submit job, move on for now
        }
    };

    let run_host = client_job.client.name.clone();

    connection.execute(
        "update jobs set started_time=?1, run_host=?2, state=1, artifacts_path=?3, build_token=?4 where id=?5",
        (now as u64, run_host, format!("{}", artifacts.display()), &client_job.client.build_token, job.id)
    )
        .expect("can update");

    spawn(async move {
        client_job.run().await
    });

    Ok(())
}

struct RunnerClient {
    tx: mpsc::Sender<Result<String, String>>,
    rx: BodyStream,
    name: String,
    build_token: String,
}

fn random_name() -> String {
    "random name".to_string()
}

fn token_for_job() -> String {
    let mut data = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut data)
        .unwrap();

    base64::encode(data)
}

struct ClientJob {
    dbctx: Arc<DbCtx>,
    remote_git_url: String,
    sha: String,
    job: PendingJob,
    client: RunnerClient    
}

impl ClientJob {
    pub async fn run(&mut self) {
        loop {
            eprintln!("waiting on response..");
            let msg = match self.client.recv().await.expect("recv works") {
                Some(msg) => msg,
                None => {
                    eprintln!("client hung up. job's done, i hope?");
                    return;
                }
            };
            eprintln!("got {:?}", msg);
            let msg_kind = msg.as_object().unwrap().get("kind").unwrap().as_str().unwrap();
            match msg_kind {
                "job_status" => {
                    let state = msg.as_object().unwrap().get("state").unwrap().as_str().unwrap();
                    let (result, state): (Result<String, String>, JobState) = if state == "finished" {
                        let result = msg.as_object().unwrap().get("result").unwrap().as_str().unwrap();
                        eprintln!("job update: state is {} and result is {}", state, result);
                        match result {
                            "pass" => {
                                (Ok("success".to_string()), JobState::Complete)
                            },
                            other => {
                                (Err(other.to_string()), JobState::Error)
                            }
                        }
                    } else if state == "interrupted" {
                        let result = msg.as_object().unwrap().get("result").unwrap().as_str().unwrap();
                        eprintln!("job update: state is {} and result is {}", state, result);
                        (Err(result.to_string()), JobState::Error)
                    } else {
                        eprintln!("job update: state is {}", state);
                        (Err(format!("atypical completion status: {}", state)), JobState::Invalid)
                    };

                    let repo_id = self.dbctx.repo_id_by_remote(self.job.remote_id).unwrap().expect("remote exists");

                    for notifier in self.dbctx.notifiers_by_repo(repo_id).expect("can get notifiers") {
                        notifier.tell_complete_job(&self.dbctx, repo_id, &self.sha, self.job.id, result.clone()).await.expect("can notify");
                    }

                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("now is before epoch")
                        .as_millis();

                    self.dbctx.conn.lock().unwrap().execute(
                        "update jobs set complete_time=?1, state=?2 where id=?3",
                        (now as u64, state as u64, self.job.id)
                    )
                        .expect("can update");
                }
                "artifact_create" => {
                    eprintln!("creating artifact {:?}", msg);
                    self.client.send(serde_json::json!({
                        "status": "ok",
                        "object_id": "10",
                    })).await.unwrap();
                },
                other => {
                    eprintln!("unhandled message kind {:?} ({:?})", msg_kind, msg);
                    return;
                }
            }
        }
    }
}

impl RunnerClient {
    async fn new(sender: mpsc::Sender<Result<String, String>>, resp: BodyStream) -> Result<Self, String> {
        let name = random_name();
        let token = token_for_job();
        let client = RunnerClient {
            tx: sender,
            rx: resp,
            name,
            build_token: token,
        };
        Ok(client)
    }

    async fn send(&mut self, msg: serde_json::Value) -> Result<(), String> {
        self.tx.send(Ok(serde_json::to_string(&msg).unwrap()))
            .await
            .map_err(|e| e.to_string())
    }

    async fn recv(&mut self) -> Result<Option<serde_json::Value>, String> {
        match self.rx.next().await {
            Some(Ok(bytes)) => {
                serde_json::from_slice(&bytes)
                    .map(Option::Some)
                    .map_err(|e| e.to_string())
            },
            Some(Err(e)) => {
                eprintln!("e: {:?}", e);
                Err(format!("no client job: {:?}", e))
            },
            None => {
                Ok(None)
            }
        }
    }

    async fn submit(mut self, dbctx: &Arc<DbCtx>, job: &PendingJob, remote_git_url: &str, sha: &str) -> Result<Option<ClientJob>, String> {
        self.send(serde_json::json!({
            "commit": sha,
            "remote_url": remote_git_url,
            "build_token": &self.build_token,
        })).await?;
        match self.recv().await {
            Ok(Some(resp)) => {
                if resp == serde_json::json!({
                    "status": "started"
                }) {
                    eprintln!("resp: {:?}", resp);
                    Ok(Some(ClientJob {
                        job: job.clone(),
                        dbctx: Arc::clone(dbctx),
                        sha: sha.to_string(),
                        remote_git_url: remote_git_url.to_string(),
                        client: self
                    }))
                } else {
                    Err("client rejected job".to_string())
                }
            }
            Ok(None) => {
                Ok(None)
            }
            Err(e) => {
                eprintln!("e: {:?}", e);
                Err(e)
            }
        }
    }
}

impl fmt::Debug for RunnerClient {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("RunnerClient { .. }")
    }
}

#[axum_macros::debug_handler]
async fn handle_artifact(State(ctx): State<(Arc<DbCtx>, mpsc::Sender<RunnerClient>)>, headers: HeaderMap, artifact_content: BodyStream) -> impl IntoResponse {
    let job_token = match headers.get("x-job-token") {
        Some(job_token) => job_token.to_str().expect("valid string"),
        None => {
            eprintln!("bad artifact post: headers: {:?}\nno x-job-token", headers);
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    let (job, artifact_path, token_validity) = match ctx.0.job_for_token(&job_token).unwrap() {
        Some(result) => result,
        None => {
            eprintln!("bad artifact post: headers: {:?}\njob token is not known", headers);
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    if token_validity != dbctx::TokenValidity::Valid {
        eprintln!("bad artifact post: headers: {:?}. token is not valid: {:?}", headers, token_validity);
        return (StatusCode::BAD_REQUEST, "").into_response();
    }

    let artifact_path = if let Some(artifact_path) = artifact_path {
        artifact_path
    } else {
        eprintln!("bad artifact post: headers: {:?}. no artifact path?", headers);
        return (StatusCode::BAD_REQUEST, "").into_response();
    };

    let artifact_name = match headers.get("x-artifact-name") {
        Some(artifact_name) => artifact_name.to_str().expect("valid string"),
        None => {
            eprintln!("bad artifact post: headers: {:?}\nno x-artifact-name", headers);
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    let artifact_desc = match headers.get("x-artifact-desc") {
        Some(artifact_desc) => artifact_desc.to_str().expect("valid string"),
        None => {
            eprintln!("bad artifact post: headers: {:?}\nno x-artifact-desc", headers);
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    let mut artifact = match ctx.0.reserve_artifact(job, artifact_name, artifact_desc).await {
        Ok(artifact) => artifact,
        Err(err) => {
            eprintln!("failure to reserve artifact: {:?}", err);
            return (StatusCode::INTERNAL_SERVER_ERROR, "").into_response();
        }
    };

    spawn(async move { artifact.store_all(artifact_content).await });

    (StatusCode::OK, "").into_response()
}

async fn handle_next_job(State(ctx): State<(Arc<DbCtx>, mpsc::Sender<RunnerClient>)>, job_resp: BodyStream) -> impl IntoResponse {
    let (tx_sender, tx_receiver) = mpsc::channel(8);
    let resp_body = StreamBody::new(ReceiverStream::new(tx_receiver));
    tx_sender.send(Ok("hello".to_string())).await.expect("works");
    let client = RunnerClient::new(tx_sender, job_resp).await;
    match client {
        Ok(client) => {
            eprintln!("registering client");
            match ctx.1.try_send(client) {
                Ok(()) => {
                    eprintln!("response established...");
                    return (StatusCode::OK, resp_body);
                }
                Err(TrySendError::Full(client)) => {
                    return (StatusCode::IM_A_TEAPOT, resp_body);
                }
                Err(TrySendError::Closed(client)) => {
                    panic!("client holder is gone?");
                }
            }
        }
        Err(e) => {
            eprintln!("unable to register client");
            return (StatusCode::MISDIRECTED_REQUEST, resp_body);
        }
    }
}

async fn make_api_server(dbctx: Arc<DbCtx>) -> (Router, mpsc::Receiver<RunnerClient>) {
    let (pending_client_sender, pending_client_receiver) = mpsc::channel(8);

    let router = Router::new()
        .route("/api/next_job", post(handle_next_job))
        .route("/api/artifact", post(handle_artifact))
        .with_state((dbctx, pending_client_sender));
    (router, pending_client_receiver)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config = RustlsConfig::from_pem_file(
        PathBuf::from("/etc/letsencrypt/live/ci.butactuallyin.space/fullchain.pem"),
        PathBuf::from("/etc/letsencrypt/live/ci.butactuallyin.space/privkey.pem"),
    ).await.unwrap();

    let dbctx = Arc::new(DbCtx::new("/root/ixi_ci_server/config", "/root/ixi_ci_server/state.db"));

    let (api_server, mut channel) = make_api_server(Arc::clone(&dbctx)).await;
    spawn(axum_server::bind_rustls("0.0.0.0:9876".parse().unwrap(), config)
          .serve(api_server.into_make_service()));

    dbctx.create_tables().unwrap();

    loop {
        let jobs = dbctx.get_pending_jobs().unwrap();

        if jobs.len() > 0 {
            println!("{} new jobs", jobs.len());

            for job in jobs.into_iter() {
                activate_job(Arc::clone(&dbctx), &job, &mut channel).await;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
