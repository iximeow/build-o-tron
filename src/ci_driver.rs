use std::process::Command;
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use lazy_static::lazy_static;
use std::io::Read;
use serde_derive::{Deserialize, Serialize};
use futures_util::StreamExt;
use std::fmt;
use std::path::{Path, PathBuf};
use tokio::spawn;
use tokio_stream::wrappers::ReceiverStream;
use std::sync::{Arc, Weak};
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
mod io;

use crate::dbctx::{DbCtx, PendingRun, Job, Run};
use crate::sql::JobResult;
use crate::sql::RunState;

lazy_static! {
    static ref AUTH_SECRET: RwLock<Option<String>> = RwLock::new(None);
    static ref ACTIVE_TASKS: Mutex<HashMap<u64, Weak<()>>> = Mutex::new(HashMap::new());
}

fn reserve_artifacts_dir(run: u64) -> std::io::Result<PathBuf> {
    let mut path: PathBuf = "/root/ixi_ci_server/artifacts/".into();
    path.push(run.to_string());
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

async fn activate_run(dbctx: Arc<DbCtx>, run: &PendingRun, clients: &mut mpsc::Receiver<RunnerClient>) -> Result<(), String> {
    eprintln!("activating task {:?}", run);
    let job = dbctx.job_by_id(run.job_id).expect("can query").expect("job exists");

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
    std::mem::drop(connection);

    let artifacts: PathBuf = reserve_artifacts_dir(run.id).expect("can reserve a directory for artifacts");

    eprintln!("running {}", repo_name);

    let mut client_job = loop {
        let mut candidate = clients.recv().await
            .ok_or_else(|| "client channel disconnected".to_string())?;

        if !candidate.will_accept(&job) {
            eprintln!("client {:?} would not accept job {:?}", candidate, job);
            continue;
        }

        if let Ok(Some(mut client_job)) = candidate.submit(&dbctx, &run, &remote_git_url, &commit_sha).await {
            break client_job;
        } else {
            // failed to submit job, move on for now
        }
    };

    let run_host = client_job.client.name.clone();

    let connection = dbctx.conn.lock().unwrap();
    connection.execute(
        "update runs set started_time=?1, run_host=?2, state=1, artifacts_path=?3, build_token=?4 where id=?5",
        (now as u64, run_host, format!("{}", artifacts.display()), &client_job.client.build_token, run.id)
    )
        .expect("can update");
    std::mem::drop(connection);

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
    accepted_sources: Option<Vec<String>>,
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
    task: PendingRun,
    client: RunnerClient,
    // exists only as confirmation this `ClientJob` is somewhere, still alive and being processed.
    task_witness: Arc<()>,
}

impl ClientJob {
    pub async fn run(&mut self) {
        loop {
            eprintln!("waiting on response..");
            let msg = match self.client.recv().await.expect("recv works") {
                Some(msg) => msg,
                None => {
                    eprintln!("client hung up. task's done, i hope?");
                    return;
                }
            };
            eprintln!("got {:?}", msg);
            let msg_kind = msg.as_object().unwrap().get("kind").unwrap().as_str().unwrap();
            match msg_kind {
                "new_task_please" => {
                    eprintln!("misdirected task request (after handshake?)");
                    return;
                },
                "task_status" => {
                    let state = msg.as_object().unwrap().get("state").unwrap().as_str().unwrap();
                    let (result, state): (Result<String, String>, RunState) = if state == "finished" {
                        let result = msg.as_object().unwrap().get("result").unwrap().as_str().unwrap();
                        eprintln!("task update: state is {} and result is {}", state, result);
                        match result {
                            "pass" => {
                                (Ok("success".to_string()), RunState::Finished)
                            },
                            other => {
                                let desc = msg.as_object().unwrap().get("desc")
                                    .map(|x| x.as_str().unwrap().to_string())
                                    .unwrap_or_else(|| other.to_string());
                                (Err(desc), RunState::Error)
                            }
                        }
                    } else if state == "interrupted" {
                        let result = msg.as_object().unwrap().get("result").unwrap().as_str().unwrap();
                        eprintln!("task update: state is {} and result is {}", state, result);
                        let desc = msg.as_object().unwrap().get("desc")
                            .map(|x| x.as_str().unwrap().to_string())
                            .unwrap_or_else(|| result.to_string());
                        (Err(desc), RunState::Error)
                    } else {
                        eprintln!("task update: state is {}", state);
                        (Err(format!("atypical completion status: {}", state)), RunState::Invalid)
                    };

                    let job = self.dbctx.job_by_id(self.task.job_id).expect("can query").expect("job exists");
                    let repo_id = self.dbctx.repo_id_by_remote(job.remote_id).unwrap().expect("remote exists");

                    for notifier in self.dbctx.notifiers_by_repo(repo_id).expect("can get notifiers") {
                        if let Err(e) = notifier.tell_complete_job(&self.dbctx, repo_id, &self.sha, self.task.id, result.clone()).await {
                            eprintln!("could not notify {:?}: {:?}", notifier.remote_path, e);
                        }
                    }

                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("now is before epoch")
                        .as_millis();

                    let build_result = if result.is_ok() {
                        JobResult::Pass
                    } else {
                        JobResult::Fail
                    };
                    let result_desc = match result {
                        Ok(msg) => msg,
                        Err(msg) => msg,
                    };

                    self.dbctx.conn.lock().unwrap().execute(
                        "update runs set complete_time=?1, state=?2, build_result=?3, final_status=?4 where id=?5",
                        (now as u64, state as u64, build_result as u8, result_desc, self.task.id)
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
                "metric" => {
                    let name = msg.as_object().unwrap().get("name").unwrap().as_str().unwrap();
                    let value = msg.as_object().unwrap().get("value").unwrap().as_str().unwrap();

                    self.dbctx.insert_metric(self.task.id, name, value)
                        .expect("TODO handle metric insert error?");
                }
                "command" => {
                    // record information about commands, start/stop, etc. probably also allow
                    // artifacts to be attached to commands and default to attaching stdout/stderr?
                }
                other => {
                    eprintln!("unhandled message kind {:?} ({:?})", msg_kind, msg);
                }
            }
        }
    }
}

impl RunnerClient {
    async fn new(sender: mpsc::Sender<Result<String, String>>, resp: BodyStream, accepted_sources: Option<Vec<String>>) -> Result<Self, String> {
        let name = random_name();
        let token = token_for_job();
        let client = RunnerClient {
            tx: sender,
            rx: resp,
            name,
            build_token: token,
            accepted_sources,
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

    // is this client willing to run the job based on what it has told us so far?
    fn will_accept(&self, job: &Job) -> bool {
        match (job.source.as_ref(), self.accepted_sources.as_ref()) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(source), Some(accepted_sources)) => {
                accepted_sources.contains(source)
            }
        }
    }

    async fn submit(mut self, dbctx: &Arc<DbCtx>, job: &PendingRun, remote_git_url: &str, sha: &str) -> Result<Option<ClientJob>, String> {
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
                    let task_witness = Arc::new(());
                    ACTIVE_TASKS.lock().unwrap().insert(job.id, Arc::downgrade(&task_witness));
                    eprintln!("resp: {:?}", resp);
                    Ok(Some(ClientJob {
                        task: job.clone(),
                        dbctx: Arc::clone(dbctx),
                        sha: sha.to_string(),
                        remote_git_url: remote_git_url.to_string(),
                        client: self,
                        task_witness,
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
    eprintln!("artifact request");
    let run_token = match headers.get("x-task-token") {
        Some(run_token) => run_token.to_str().expect("valid string"),
        None => {
            eprintln!("bad artifact post: headers: {:?}\nno x-tasak-token", headers);
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    let (run, artifact_path, token_validity) = match ctx.0.run_for_token(&run_token).unwrap() {
        Some(result) => result,
        None => {
            eprintln!("bad artifact post: headers: {:?}\nrun token is not known", headers);
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

    let mut artifact = match ctx.0.reserve_artifact(run, artifact_name, artifact_desc).await {
        Ok(artifact) => artifact,
        Err(err) => {
            eprintln!("failure to reserve artifact: {:?}", err);
            return (StatusCode::INTERNAL_SERVER_ERROR, "").into_response();
        }
    };

    eprintln!("spawning task...");
    let dbctx_ref = Arc::clone(&ctx.0);
    spawn(async move {
        artifact.store_all(artifact_content).await.unwrap();
        dbctx_ref.finalize_artifact(artifact.artifact_id).await.unwrap();
    });
    eprintln!("done?");

    (StatusCode::OK, "").into_response()
}

#[derive(Serialize, Deserialize)]
struct WorkRequest {
    kind: String,
    accepted_pushers: Option<Vec<String>>
}

async fn handle_next_job(State(ctx): State<(Arc<DbCtx>, mpsc::Sender<RunnerClient>)>, headers: HeaderMap, mut job_resp: BodyStream) -> impl IntoResponse {
    let auth_token = match headers.get("authorization") {
        Some(token) => {
            if Some(token.to_str().unwrap_or("")) != AUTH_SECRET.read().unwrap().as_ref().map(|x| &**x) {
                eprintln!("BAD AUTH SECRET SUBMITTED: {:?}", token);
                return (StatusCode::BAD_REQUEST, "").into_response();
            }
        }
        None => {
            eprintln!("bad artifact post: headers: {:?}\nno authorization", headers);
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    let (tx_sender, tx_receiver) = mpsc::channel(8);
    let resp_body = StreamBody::new(ReceiverStream::new(tx_receiver));
    tx_sender.send(Ok("hello".to_string())).await.expect("works");

    let request = job_resp.next().await.expect("request chunk").expect("chunk exists");
    let request = std::str::from_utf8(&request).unwrap();
    let request: WorkRequest = match serde_json::from_str(&request) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("couldn't parse work request: {:?}", e);
            return (StatusCode::MISDIRECTED_REQUEST, resp_body).into_response();
        }
    };
    if &request.kind != "new_job_please" {
        eprintln!("bad request kind: {:?}", &request.kind);
        return (StatusCode::MISDIRECTED_REQUEST, resp_body).into_response();
    }

    let client = match RunnerClient::new(tx_sender, job_resp, request.accepted_pushers).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("unable to register client");
            return (StatusCode::MISDIRECTED_REQUEST, resp_body).into_response();
        }
    };

    match ctx.1.try_send(client) {
        Ok(()) => {
            eprintln!("client requested work...");
            return (StatusCode::OK, resp_body).into_response();
        }
        Err(TrySendError::Full(client)) => {
            return (StatusCode::IM_A_TEAPOT, resp_body).into_response();
        }
        Err(TrySendError::Closed(client)) => {
            panic!("client holder is gone?");
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

#[derive(Deserialize, Serialize)]
struct DriverConfig {
    cert_path: PathBuf,
    key_path: PathBuf,
    config_path: PathBuf,
    db_path: PathBuf,
    server_addr: String,
    auth_secret: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let mut args = std::env::args();
    args.next().expect("first arg exists");
    let config_path = args.next().unwrap_or("./driver_config.json".to_string());
    let driver_config: DriverConfig = serde_json::from_reader(std::fs::File::open(config_path).expect("file exists and is accessible")).expect("valid json for DriverConfig");
    let mut auth_secret = AUTH_SECRET.write().unwrap();
    *auth_secret = Some(driver_config.auth_secret.clone());
    std::mem::drop(auth_secret);

    let config = RustlsConfig::from_pem_file(
        driver_config.cert_path.clone(),
        driver_config.key_path.clone(),
    ).await.unwrap();

    let dbctx = Arc::new(DbCtx::new(&driver_config.config_path, &driver_config.db_path));

    dbctx.create_tables().unwrap();

    let (api_server, mut channel) = make_api_server(Arc::clone(&dbctx)).await;
    spawn(axum_server::bind_rustls(driver_config.server_addr.parse().unwrap(), config)
          .serve(api_server.into_make_service()));

    spawn(old_task_reaper(Arc::clone(&dbctx)));

    loop {
        let runs = dbctx.get_pending_runs().unwrap();

        if runs.len() > 0 {
            println!("{} new runs", runs.len());

            for run in runs.into_iter() {
                activate_run(Arc::clone(&dbctx), &run, &mut channel).await;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn old_task_reaper(dbctx: Arc<DbCtx>) {
    let mut potentially_stale_tasks = dbctx.get_active_runs().unwrap();

    let active_tasks = ACTIVE_TASKS.lock().unwrap();

    for (id, witness) in active_tasks.iter() {
        if let Some(idx) = potentially_stale_tasks.iter().position(|task| task.id == *id) {
            potentially_stale_tasks.swap_remove(idx);
        }
    }

    std::mem::drop(active_tasks);

    // ok, so we have tasks that are not active, now if the task is started we know someone should
    // be running it and they are not. retain only those tasks, as they are ones we may want to
    // mark dead.
    //
    // further, filter out any tasks created in the last 60 seconds. this is a VERY generous grace
    // period for clients that have accepted a job but for some reason we have not recorded them as
    // active (perhaps they are slow to ack somehow).

    let stale_threshold = crate::io::now_ms() - 60_000;

    let stale_tasks: Vec<Run> = potentially_stale_tasks.into_iter().filter(|task| {
        match (task.state, task.start_time) {
            // `run` is atomically set to `Started` and adorned with a `start_time`. disagreement
            // between the two means this run is corrupt and should be reaped.
            (RunState::Started, None) => {
                true
            },
            (RunState::Started, Some(start_time)) => {
                start_time < stale_threshold
            }
            // and if it's not `started`, it's either pending (not assigned yet, so not stale), or
            // one of the complete statuses.
            _ => {
                false
            }
        }
    }).collect();

    for task in stale_tasks.iter() {
        eprintln!("looks like task {} is stale, reaping", task.id);
        dbctx.reap_task(task.id).expect("works");
    }
}
