#![allow(dead_code)]
#![allow(unused_variables)]

use tokio::spawn;
use std::path::PathBuf;
use serde_derive::{Deserialize, Serialize};
use axum_server::tls_rustls::RustlsConfig;
use axum::routing::*;
use axum::Router;
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;
use axum::extract::{Path, State};
use http_body::combinators::UnsyncBoxBody;
use axum::{Error, Json};
use axum::extract::rejection::JsonRejection;
use axum::body::Bytes;
use axum::http::{StatusCode, Uri};
use http::header::HeaderMap;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

mod sql;

use rusqlite::{Connection, OptionalExtension};

#[derive(Copy, Clone, Debug)]
enum GithubHookError {
    BodyNotObject,
    MissingElement { path: &'static str },
    BadType { path: &'static str, expected: &'static str },
}

#[derive(Debug)]
enum GithubEvent {
    Push { tip: String, repo_name: String, head_commit: serde_json::Map<String, serde_json::Value> },
    Other {}
}

fn parse_push_event(body: serde_json::Value) -> Result<GithubEvent, GithubHookError> {
    let body = body.as_object()
        .ok_or(GithubHookError::BodyNotObject)?;

    let tip = body.get("after")
        .ok_or(GithubHookError::MissingElement { path: "after" })?
        .as_str()
        .ok_or(GithubHookError::BadType { path: "after", expected: "str" })?
        .to_owned();

    let repo_name = body.get("repository")
        .ok_or(GithubHookError::MissingElement { path: "repository" })?
        .as_object()
        .ok_or(GithubHookError::BadType { path: "repository", expected: "obj" })?
        .get("full_name")
        .ok_or(GithubHookError::MissingElement { path: "repository/full_name" })?
        .as_str()
        .ok_or(GithubHookError::BadType { path: "repository/full_name", expected: "str" })?
        .to_owned();

    let head_commit = body.get("head_commit")
        .ok_or(GithubHookError::MissingElement { path: "head_commit" })?
        .as_object()
        .ok_or(GithubHookError::BadType { path: "head_commit", expected: "obj" })?
        .to_owned();

    Ok(GithubEvent::Push { tip, repo_name, head_commit })
}

async fn process_push_event(ctx: Arc<DbCtx>, owner: String, repo: String, event: GithubEvent) -> impl IntoResponse {
    let (sha, repo, head_commit) = if let GithubEvent::Push { tip, repo_name, head_commit } = event {
        (tip, repo_name, head_commit)
    } else {
        panic!("process push event on non-push event");
    };

    println!("handling push event to {}/{}: sha {} in repo {}, {:?}", owner, repo, sha, repo, head_commit);

    // push event is in terms of a ref, but we don't know if it's a new commit (yet).
    // in terms of CI jobs, we care mainly about new commits.
    // so...
    // * look up the commit,
    // * if it known, bail out (new ref for existing commit we've already handled some way)
    // * create a new commit ref
    // * create a new job (state=pending) for the commit ref
    let commit_id: Option<u64> = ctx.conn.lock().unwrap()
        .query_row(sql::COMMIT_TO_ID, [sha.clone()], |row| row.get(0))
        .optional()
        .expect("can run query");

    if commit_id.is_some() {
        eprintln!("commit already exists");
        return (StatusCode::OK, String::new());
    }

    let remote_url = format!("https://github.com/{}.git", repo);
    let (remote_id, repo_id): (u64, u64) = ctx.conn.lock().unwrap()
        .query_row("select id, repo_id from remotes where remote_git_url=?1;", [&remote_url], |row| Ok((row.get(0).unwrap(), row.get(1).unwrap())))
        .unwrap();

    let job_id = ctx.new_job(remote_id, &sha).unwrap();

    let notifiers = ctx.notifiers_by_name(&repo).expect("can get notifiers");

    for notifier in notifiers {
        notifier.tell_pending_job(&ctx, repo_id, &sha, job_id).await.expect("can notify");
    }

    (StatusCode::OK, String::new())
}

async fn handle_github_event(ctx: Arc<DbCtx>, owner: String, repo: String, event_kind: String, body: serde_json::Value) -> Response<UnsyncBoxBody<Bytes, Error>> {
    println!("got github event: {}, {}, {}", owner, repo, event_kind);
    match event_kind.as_str() {
        "push" => {
            let push_event = parse_push_event(body)
                .map_err(|e| {
                    eprintln!("TODO: handle push event error: {:?}", e);
                    panic!()
                })
                .expect("parse works");
            let res = process_push_event(ctx, owner, repo, push_event).await;
            "ok".into_response()
        },
        other => {
            eprintln!("unhandled event kind: {}, repo {}/{}. content: {:?}", other, owner, repo, body);
            "".into_response()
        }
    }
}

async fn handle_commit_status(Path(path): Path<(String, String, String)>, State(ctx): State<Arc<DbCtx>>) -> impl IntoResponse {
    eprintln!("path: {}/{}, sha {}", path.0, path.1, path.2);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("now is before epoch");

    format!("requested: {:?}", path)
}

async fn handle_repo_event(Path(path): Path<(String, String)>, headers: HeaderMap, State(ctx): State<Arc<DbCtx>>, body: Bytes) -> impl IntoResponse {
    let json: Result<serde_json::Value, _> = serde_json::from_slice(&body);
    eprintln!("repo event: {:?} {:?} {:?}", path.0, path.1, headers);

    let payload = match json {
        Ok(payload) => { payload },
        Err(e) => {
            eprintln!("bad request: path={}/{}\nheaders: {:?}\nbody err: {:?}", path.0, path.1, headers, e); 
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    let sent_hmac = match headers.get("x-hub-signature-256") {
        Some(sent_hmac) => { sent_hmac.to_str().expect("valid ascii string").to_owned() },
        None => {
            eprintln!("bad request: path={}/{}\nheaders: {:?}\nno x-hub-signature-256", path.0, path.1, headers); 
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    let mut mac = Hmac::<Sha256>::new_from_slice(GITHUB_PSK)
        .expect("hmac can be constructed");
    mac.update(&body);
    let result = mac.finalize().into_bytes().to_vec();

    // hack: skip sha256=
    let decoded = hex::decode(&sent_hmac[7..]).expect("provided hmac is valid hex");
    if decoded != result {
        eprintln!("bad hmac:\n\
           got:      {:?}\n\
           expected: {:?}", decoded, result);
        return (StatusCode::BAD_REQUEST, "").into_response();
    }

    let kind = match headers.get("x-github-event") {
        Some(kind) => { kind.to_str().expect("valid ascii string").to_owned() },
        None => {
            eprintln!("bad request: path={}/{}\nheaders: {:?}\nno x-github-event", path.0, path.1, headers); 
            return (StatusCode::BAD_REQUEST, "").into_response();
        }
    };

    handle_github_event(ctx, path.0, path.1, kind, payload).await
}

struct DbCtx {
    conn: Mutex<Connection>,
}

struct RemoteNotifier {
    remote_path: String,
    notifier: NotifierConfig,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum NotifierConfig {
    GitHub {
        token: String,
    },
    Email {
        username: String,
        password: String,
        mailserver: String,
        from: String,
        to: String,
    }
}

impl NotifierConfig {
    fn github_from_file(path: &str) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("can't read notifier config at {}: {:?}", path, e))?;
        let config = serde_json::from_slice(&bytes)
            .map_err(|e| format!("can't deserialize notifier config at {}: {:?}", path, e))?;

        if matches!(config, NotifierConfig::GitHub { .. }) {
            Ok(config)
        } else {
            Err(format!("config at {} doesn't look like a github config (but was otherwise valid?)", path))
        }
    }

    fn email_from_file(path: &str) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("can't read notifier config at {}: {:?}", path, e))?;
        let config = serde_json::from_slice(&bytes)
            .map_err(|e| format!("can't deserialize notifier config at {}: {:?}", path, e))?;

        if matches!(config, NotifierConfig::Email { .. }) {
            Ok(config)
        } else {
            Err(format!("config at {} doesn't look like an email config (but was otherwise valid?)", path))
        }
    }
}

impl RemoteNotifier {
    async fn tell_pending_job(&self, ctx: &Arc<DbCtx>, repo_id: u64, sha: &str, job_id: u64) -> Result<(), String> {
        match &self.notifier {
            NotifierConfig::GitHub { token } => {
                let status_info = serde_json::json!({
                    "state": "pending",
                    "target_url": format!(
                        "https://{}/{}/{}",
                        "ci.butactuallyin.space",
                        &self.remote_path,
                        sha,
                    ),
                    "description": "build is queued",
                    "context": "actuallyinspace runner",
                });

                // TODO: should pool (probably in ctx?) to have an upper bound in concurrent
                // connections.
                let client = reqwest::Client::new();
                let res = client.post(&format!("https://api.github.com/repos/{}/statuses/{}", &self.remote_path, sha))
                    .body(serde_json::to_string(&status_info).expect("can stringify json"))
                    .header("authorization", format!("Bearer {}", token))
                    .header("accept", "application/vnd.github+json")
                    .send()
                    .await;

                match res {
                    Ok(res) => {
                        if res.status() == StatusCode::OK {
                            Ok(())
                        } else {
                            Err(format!("bad response: {}, response data: {:?}", res.status().as_u16(), res))
                        }
                    }
                    Err(e) => {
                        Err(format!("failure sending request: {:?}", e))
                    }
                }
            }
            NotifierConfig::Email { username, password, mailserver, from, to } => {
                panic!("should send an email saying that a job is now pending for `sha`")
            }
        }
    }
}

impl DbCtx {
    fn new(db_path: &'static str) -> Self {
        DbCtx {
            conn: Mutex::new(Connection::open(db_path).unwrap())
        }
    }

    fn new_commit(&self, sha: &str) -> Result<u64, String> {
        let conn = self.conn.lock().unwrap();
        conn
            .execute(
                "insert into commits (sha) values (?1)",
                [sha.clone()]
            )
            .expect("can insert");

        Ok(conn.last_insert_rowid() as u64)
    }

    fn new_job(&self, remote_id: u64, sha: &str) -> Result<u64, String> {
        // TODO: potential race: if two remotes learn about a commit at the same time and we decide
        // to create two jobs at the same time, this might return an incorrect id if the insert
        // didn't actually insert a new row.
        let commit_id = self.new_commit(sha).expect("can create commit record");

        let created_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("now is before epoch")
            .as_millis() as u64;

        let conn = self.conn.lock().unwrap();

        let rows_modified = conn.execute(
            "insert into jobs (state, remote_id, commit_id, created_time) values (?1, ?2, ?3, ?4);",
            (sql::JobState::Pending as u64, remote_id, commit_id, created_time)
        ).unwrap();

        assert_eq!(1, rows_modified);

        Ok(conn.last_insert_rowid() as u64)
    }

    fn notifiers_by_name(&self, repo: &str) -> Result<Vec<RemoteNotifier>, String> {
        let maybe_repo_id: Option<u64> = self.conn.lock()
            .unwrap()
            .query_row("select * from repos where repo_name=?1", [repo], |row| row.get(0))
            .optional()
            .expect("query succeeds");
        match maybe_repo_id {
            Some(repo_id) => {
                // get remotes
                
                #[derive(Debug)]
                #[allow(dead_code)]
                struct Remote {
                    id: u64,
                    repo_id: u64,
                    remote_path: String,
                    remote_api: String,
                    notifier_config_path: String,
                }

                let mut remotes: Vec<Remote> = Vec::new();

                let conn = self.conn.lock().unwrap();
                let mut remotes_query = conn.prepare(sql::REMOTES_FOR_REPO).unwrap();
                let mut remote_results = remotes_query.query([repo_id]).unwrap();

                while let Some(row) = remote_results.next().unwrap() {
                    let (id, repo_id, remote_path, remote_api, notifier_config_path) = row.try_into().unwrap();
                    remotes.push(Remote { id, repo_id, remote_path, remote_api, notifier_config_path });
                }

                let mut notifiers: Vec<RemoteNotifier> = Vec::new();

                for remote in remotes.into_iter() {
                    match remote.remote_api.as_str() {
                        "github" => {
                            let notifier = RemoteNotifier {
                                remote_path: remote.remote_path,
                                notifier: NotifierConfig::github_from_file(&remote.notifier_config_path)
                                    .expect("can load notifier config")
                            };
                            notifiers.push(notifier);
                        },
                        "email" => {
                            let notifier = RemoteNotifier {
                                remote_path: remote.remote_path,
                                notifier: NotifierConfig::email_from_file(&remote.notifier_config_path)
                                    .expect("can load notifier config")
                            };
                            notifiers.push(notifier);
                        }
                        other => {
                            eprintln!("unknown remote api kind: {:?}, remote is {:?}", other, &remote)
                        }
                    }
                }

                Ok(notifiers)
            }
            None => {
                return Err(format!("repo '{}' is not known", repo));
            }
        }
    }
}

async fn make_app_server(db_path: &'static str) -> Router {
    /*

    // GET /hello/warp => 200 OK with body "Hello, warp!"
    let hello = warp::path!("hello" / String)
        .map(|name| format!("Hello, {}!\n", name));

    let github_event = warp::post()
        .and(warp::path!(String / String))
        .and_then(|owner, repo| {
            warp::header::<String>("x-github-event")
                .and(warp::body::content_length_limit(1024 * 1024))
                .and(warp::body::json())
                .and_then(|event, json| handle_github_event(owner, repo, event, json))
                .recover(|e| {
                    async fn handle_rejection(err: Rejection) -> Result<impl Reply, Rejection> {
                       Ok(warp::reply::with_status("65308", StatusCode::BAD_REQUEST))
                    }
                    handle_rejection(e)
                })
        });

    let repo_status = warp::get()
        .and(warp::path!(String / String / String))
        .map(|owner, repo, sha| format!("CI status for {}/{} commit {}\n", owner, repo, sha));

    let other =
            warp::post()
                .and(warp::path::full())
                .and(warp::addr::remote())
                .and(warp::body::content_length_limit(1024 * 1024))
                .and(warp::body::bytes())
                .map(move |path, addr: Option<std::net::SocketAddr>, body| {
                    println!("{}: lets see what i got {:?}, {:?}", addr.unwrap(), path, body);
                    "hello :)\n"
                })
            .or(
                warp::get()
                    .and(warp::path::full())
                    .and(warp::addr::remote())
                    .map(move |path, addr: Option<std::net::SocketAddr>| {
                        println!("{}: GET to {:?}", addr.unwrap(), path);
                        "hello!\n"
                    })
            )
        .recover(|e| {
            async fn handle_rejection(err: Rejection) -> Result<impl Reply, std::convert::Infallible> {
               Ok(warp::reply::with_status("50834", StatusCode::BAD_REQUEST))
            }
            handle_rejection(e)
        });
    */

    async fn fallback_get(uri: Uri) -> impl IntoResponse {
        (StatusCode::OK, "get resp")
    }

    async fn fallback_post(Path(path): Path<String>) -> impl IntoResponse {
        "post resp"
    }

    Router::new()
        .route("/:owner/:repo/:sha", get(handle_commit_status))
        .route("/:owner/:repo", post(handle_repo_event))
        .fallback(fallback_get)
        .with_state(Arc::new(DbCtx::new(db_path)))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config = RustlsConfig::from_pem_file(
        PathBuf::from("/etc/letsencrypt/live/ci.butactuallyin.space/fullchain.pem"),
        PathBuf::from("/etc/letsencrypt/live/ci.butactuallyin.space/privkey.pem"),
    ).await.unwrap();
    spawn(axum_server::bind_rustls("127.0.0.1:8080".parse().unwrap(), config.clone())
        .serve(make_app_server("/root/ixi_ci_server/state.db").await.into_make_service()));
    axum_server::bind_rustls("0.0.0.0:443".parse().unwrap(), config)
        .serve(make_app_server("/root/ixi_ci_server/state.db").await.into_make_service())
        .await
        .unwrap();
}
