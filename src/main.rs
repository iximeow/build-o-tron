#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(unused_imports)]

use chrono::{Utc, TimeZone};
use lazy_static::lazy_static;
use std::sync::RwLock;
use serde_derive::{Deserialize, Serialize};
use tokio::spawn;
use std::path::PathBuf;
use axum_server::tls_rustls::RustlsConfig;
use axum::routing::*;
use axum::Router;
use axum::response::{IntoResponse, Response, Html};
use std::net::SocketAddr;
use axum::extract::{Path, State};
use http_body::combinators::UnsyncBoxBody;
use axum::{Error, Json};
use axum::extract::rejection::JsonRejection;
use axum::body::Bytes;
use axum::http::{StatusCode, Uri};
use http::header::HeaderMap;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

mod io;
mod sql;
mod notifier;
mod dbctx;

use sql::JobState;

use dbctx::{DbCtx, Job};

use rusqlite::OptionalExtension;

#[derive(Serialize, Deserialize)]
struct WebserverConfig {
    psks: Vec<GithubPsk>,
    cert_path: PathBuf,
    key_path: PathBuf,
    config_path: PathBuf,
    db_path: PathBuf,
    debug_addr: Option<String>,
    server_addr: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct GithubPsk {
    key: String,
    gh_user: String,
}

lazy_static! {
    static ref PSKS: RwLock<Vec<GithubPsk>> = RwLock::new(Vec::new());
}

#[derive(Copy, Clone, Debug)]
enum GithubHookError {
    BodyNotObject,
    MissingElement { path: &'static str },
    BadType { path: &'static str, expected: &'static str },
}

#[derive(Debug)]
enum GithubEvent {
    Push { tip: String, repo_name: String, head_commit: serde_json::Map<String, serde_json::Value>, pusher: serde_json::Map<String, serde_json::Value> },
    Other {}
}

/// return a duration rendered as the largest two non-zero units.
///
/// 60000ms -> 1m
/// 60001ms -> 1m
/// 61000ms -> 1m1s
///  1030ms -> 1.03s
fn duration_as_human_string(duration_ms: u64) -> String {
    let duration_sec = duration_ms / 1000;
    let duration_min = duration_sec / 60;
    let duration_hours = duration_min / 60;

    let duration_ms = duration_ms % 1000;
    let duration_sec = duration_sec % 60;
    let duration_min = duration_hours % 60;
    // no need to clamp hours, we're gonna just hope that it's a reasonable number of hours

    if duration_hours != 0 {
        let mut res = format!("{}h", duration_hours);
        if duration_min != 0 {
            res.push_str(&format!("{}m", duration_min));
        }
        res
    } else if duration_min != 0 {
        let mut res = format!("{}m", duration_min);
        if duration_min != 0 {
            res.push_str(&format!("{}s", duration_sec));
        }
        res
    } else {
        let mut res = format!("{}", duration_sec);
        if duration_ms != 0 {
            res.push_str(&format!(".{:03}", duration_ms));
        }
        res.push('s');
        res
    }
}

/// try producing a url for whatever caused this job to be started, if possible
fn commit_url(job: &Job, commit_sha: &str, ctx: &Arc<DbCtx>) -> Option<String> {
    let remote = ctx.remote_by_id(job.remote_id).expect("query succeeds").expect("existing job references existing remote");

    match remote.remote_api.as_str() {
        "github" => {
            Some(format!("{}/commit/{}", remote.remote_url, commit_sha))
        },
        "email" => {
            None
        },
        _ => {
            None
        }
    }
}

/// produce a url to the ci.butactuallyin.space job details page
fn job_url(job: &Job, commit_sha: &str, ctx: &Arc<DbCtx>) -> String {
    let remote = ctx.remote_by_id(job.remote_id).expect("query succeeds").expect("existing job references existing remote");

    if remote.remote_api != "github" {
        eprintln!("job url for remote type {} can't be constructed, i think", &remote.remote_api);
    }

    format!("{}/{}", &remote.remote_path, commit_sha)
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

    let pusher = body.get("pusher")
        .ok_or(GithubHookError::MissingElement { path: "pusher" })?
        .as_object()
        .ok_or(GithubHookError::BadType { path: "pusher", expected: "obj" })?
        .to_owned();

    Ok(GithubEvent::Push { tip, repo_name, head_commit, pusher })
}

async fn process_push_event(ctx: Arc<DbCtx>, owner: String, repo: String, event: GithubEvent) -> impl IntoResponse {
    let (sha, repo, head_commit, pusher) = if let GithubEvent::Push { tip, repo_name, head_commit, pusher } = event {
        (tip, repo_name, head_commit, pusher)
    } else {
        panic!("process push event on non-push event");
    };

    println!("handling push event to {}/{}: sha {} in repo {}, {:?}\n  pusher: {:?}", owner, repo, sha, repo, head_commit, pusher);

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

    let remote_url = format!("https://www.github.com/{}.git", repo);
    eprintln!("looking for remote url: {}", remote_url);
    let (remote_id, repo_id): (u64, u64) = match ctx.conn.lock().unwrap()
        .query_row("select id, repo_id from remotes where remote_git_url=?1;", [&remote_url], |row| Ok((row.get(0).unwrap(), row.get(1).unwrap())))
        .optional()
        .unwrap() {
        Some(elems) => elems,
        None => {
            eprintln!("no remote registered for url {} (repo {})", remote_url, repo);
            return (StatusCode::NOT_FOUND, String::new());
        }
    };

    let pusher_email = pusher
        .get("email")
        .expect("has email")
        .as_str()
        .expect("is str");

    let job_id = ctx.new_job(remote_id, &sha, Some(pusher_email)).unwrap();

    let notifiers = ctx.notifiers_by_repo(repo_id).expect("can get notifiers");

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
        "status" => {
            eprintln!("[.] status update");
            "ok".into_response()
        }
        other => {
            eprintln!("unhandled event kind: {}, repo {}/{}. content: {:?}", other, owner, repo, body);
            "".into_response()
        }
    }
}

async fn handle_ci_index(State(ctx): State<Arc<DbCtx>>) -> impl IntoResponse {
    eprintln!("root index");
    let repos = match ctx.get_repos() {
        Ok(repos) => repos,
        Err(e) => {
            eprintln!("failed to get repos: {:?}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Html("gonna feel that one tomorrow".to_string()));
        }
    };

    let mut response = String::new();

    response.push_str("<html>\n");
    response.push_str("<style>\n");
    response.push_str(".build-table { font-family: monospace; border: 1px solid black; border-collapse: collapse; }\n");
    response.push_str(".row-item { padding-left: 4px; padding-right: 4px; border-right: 1px solid black; }\n");
    response.push_str(".odd-row { background: #eee; }\n");
    response.push_str(".even-row { background: #ddd; }\n");
    response.push_str("</style>\n");
    response.push_str("<h1>builds and build accessories</h1>\n");

    match repos.len() {
        0 => { response.push_str(&format!("<p>no repos configured, so there are no builds</p>\n")); },
        1 => { response.push_str("<p>1 repo configured</p>\n"); },
        other => { response.push_str(&format!("<p>{} repos configured</p>\n", other)); },
    }

    response.push_str("<table class='build-table'>");
    response.push_str("<tr>\n");
    let headings = ["repo", "last build", "job", "build commit", "duration", "status", "result"];
    for heading in headings {
        response.push_str(&format!("<th class='row-item'>{}</th>", heading));
    }
    response.push_str("</tr>\n");

    let mut row_num = 0;

    for repo in repos {
        let mut most_recent_job: Option<Job> = None;

        for remote in ctx.remotes_by_repo(repo.id).expect("remotes by repo works") {
            let last_job = ctx.last_job_from_remote(remote.id).expect("job by remote works");
            if let Some(last_job) = last_job {
                if most_recent_job.as_ref().map(|job| job.created_time < last_job.created_time).unwrap_or(true) {
                    most_recent_job = Some(last_job);
                }
            }
        }

        let row_html: String = match most_recent_job {
            Some(job) => {
                let job_commit = ctx.commit_sha(job.commit_id).expect("job has a commit");
                let commit_html = match commit_url(&job, &job_commit, &ctx) {
                    Some(url) => format!("<a href=\"{}\">{}</a>", url, &job_commit),
                    None => job_commit.clone()
                };

                let job_html = format!("<a href=\"{}\">{}</a>", job_url(&job, &job_commit, &ctx), job.id);

                let last_build_time = Utc.timestamp_millis_opt(job.created_time as i64).unwrap().to_rfc2822();
                let duration = if let Some(start_time) = job.start_time {
                    if let Some(complete_time) = job.complete_time {
                        if complete_time < start_time {
                            if job.state == JobState::Started {
                                // this job has been restarted. the completed time is stale.
                                // further, this is a currently active job.
                                let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).expect("now is after then").as_millis() as u64;
                                let mut duration = duration_as_human_string(now_ms - start_time);
                                duration.push_str(" (ongoing)");
                                duration
                            } else {
                                "invalid data".to_string()
                            }
                        } else {
                            let duration_ms = complete_time - start_time;
                            let duration = duration_as_human_string(duration_ms);
                            duration
                        }
                    } else {
                        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).expect("now is after then").as_millis() as u64;
                        let mut duration = duration_as_human_string(now_ms - start_time);
                        duration.push_str(" (ongoing)");
                        duration
                    }
                } else {
                    "not yet run".to_owned()
                };

                let status = format!("{:?}", job.state).to_lowercase();

                let result = match job.build_result {
                    Some(0) => "<span style='color:green;'>pass</span>",
                    Some(_) => "<span style='color:red;'>fail</span>",
                    None => match job.state {
                        JobState::Pending => { "unstarted" },
                        JobState::Started => { "<span style='color:darkgoldenrod;'>in progress</span>" },
                        _ => { "<span style='color:red;'>unreported</span>" }
                    }
                };

                let entries = [repo.name.as_str(), last_build_time.as_str(), job_html.as_str(), commit_html.as_str(), &duration, &status, &result];
                let entries = entries.iter().chain(std::iter::repeat(&"")).take(headings.len());

                let mut row_html = String::new();
                for entry in entries {
                    row_html.push_str(&format!("<td class='row-item'>{}</td>", entry));
                }
                row_html
            }
            None => {
                let entries = [repo.name.as_str()];
                let entries = entries.iter().chain(std::iter::repeat(&"")).take(headings.len());

                let mut row_html = String::new();
                for entry in entries {
                    row_html.push_str(&format!("<td class='row-item'>{}</td>", entry));
                }
                row_html
            }
        };

        let row_index = row_num % 2;
        response.push_str(&format!("<tr class=\"{}\">", ["even-row", "odd-row"][row_index]));
        response.push_str(&row_html);
        response.push_str("</tr>");
        response.push('\n');

        row_num += 1;
    }
    response.push_str("</table>");

    response.push_str("</html>");

    (StatusCode::OK, Html(response))
}

async fn handle_commit_status(Path(path): Path<(String, String, String)>, State(ctx): State<Arc<DbCtx>>) -> impl IntoResponse {
    eprintln!("path: {}/{}, sha {}", path.0, path.1, path.2);
    let remote_path = format!("{}/{}", path.0, path.1);
    let sha = path.2;

    let (commit_id, sha): (u64, String) = if sha.len() >= 7 {
        match ctx.conn.lock().unwrap()
            .query_row("select id, sha from commits where sha like ?1;", [&format!("{}%", sha)], |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
            .optional()
            .expect("can query") {
            Some((commit_id, sha)) => (commit_id, sha),
            None => {
                return (StatusCode::NOT_FOUND, Html("<html><body>no such commit</body></html>".to_string()));
            }
        }
    } else {
        return (StatusCode::NOT_FOUND, Html("<html><body>no such commit</body></html>".to_string()));
    };

    let (remote_id, repo_id): (u64, u64) = ctx.conn.lock().unwrap()
        .query_row("select id, repo_id from remotes where remote_path=?1;", [&remote_path], |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
        .expect("can query");

    let (job_id, state, build_result, result_desc): (u64, u8, Option<u8>, Option<String>) = ctx.conn.lock().unwrap()
        .query_row("select id, state, build_result, final_status from jobs where commit_id=?1;", [commit_id], |row| Ok((row.get_unwrap(0), row.get_unwrap(1), row.get_unwrap(2), row.get_unwrap(3))))
        .expect("can query");

    let state: sql::JobState = unsafe { std::mem::transmute(state) };

    let repo_name: String = ctx.conn.lock().unwrap()
        .query_row("select repo_name from repos where id=?1;", [repo_id], |row| row.get(0))
        .expect("can query");

    let deployed = false;

    let head = format!("<head><title>ci.butactuallyin.space - {}</title></head>", repo_name);
    let remote_commit_elem = format!("<a href=\"https://www.github.com/{}/commit/{}\">{}</a>", &remote_path, &sha, &sha);
    let status_elem = match state {
        JobState::Pending | JobState::Started => {
            "<span style='color:#660;'>pending</span>"
        },
        JobState::Finished => {
            if let Some(build_result) = build_result {
                if build_result == 0 {
                    "<span style='color:green;'>pass</span>"
                } else {
                    "<span style='color:red;'>failed</span>"
                }
            } else {
                eprintln!("job {} for commit {} is missing a build result but is reportedly finished (old data)?", job_id, commit_id);
                "<span style='color:red;'>unreported</span>"
            }
        },
        JobState::Error => {
            "<span style='color:red;'>error</span>"
        }
        JobState::Invalid => {
            "<span style='color:red;'>(server error)</span>"
        }
    };

    let output = if state == JobState::Finished && build_result == Some(1) || state == JobState::Error {
        // collect stderr/stdout from the last artifacts, then the last 10kb of each, insert it in
        // the page...
        let artifacts = ctx.artifacts_for_job(job_id).unwrap();
        if artifacts.len() > 0 {
            let mut streams = String::new();
            for artifact in artifacts.iter() {
                eprintln!("found artifact {:?} for job {:?}", artifact, job_id);
                streams.push_str(&format!("<div>step: <pre style='display:inline;'>{}</pre></div>\n", &artifact.name));
                streams.push_str("<pre>");
                streams.push_str(&std::fs::read_to_string(format!("./jobs/{}/{}", artifact.job_id, artifact.id)).unwrap());
                streams.push_str("</pre>");
            }
            Some(streams)
        } else {
            None
        }
    } else {
        None
    };

    let metrics = ctx.metrics_for_job(job_id).unwrap();
    let metrics_section = if metrics.len() > 0 {
        let mut section = String::new();
        section.push_str("<div>");
        section.push_str("<h3>metrics</h3>");
        section.push_str("<table style='font-family: monospace;'>");
        section.push_str("<tr><th>name</th><th>value</th></tr>");
        for metric in metrics {
            section.push_str(&format!("<tr><td>{}</td><td>{}</td></tr>", &metric.name, &metric.value));
        }
        section.push_str("</table>");
        section.push_str("</div>");
        Some(section)
    } else {
        None
    };

    let mut html = String::new();
    html.push_str("<html>\n");
    html.push_str(&format!("  {}\n", head));
    html.push_str("  <body>\n");
    html.push_str("    <pre>\n");
    html.push_str(&format!("repo: {}\n", repo_name));
    html.push_str(&format!("commit: {}\n", remote_commit_elem));
    html.push_str(&format!("status: {}\n", status_elem));
    if let Some(desc) = result_desc {
        html.push_str(&format!("  description: {}\n  ", desc));
    }
    html.push_str(&format!("deployed: {}\n", deployed));
    html.push_str("    </pre>\n");
    if let Some(output) = output {
        html.push_str("    <div>last build output</div>\n");
        html.push_str(&format!("    {}\n", output));
    }
    if let Some(metrics) = metrics_section {
        html.push_str(&metrics);
    }
    html.push_str("  </body>\n");
    html.push_str("</html>");

    (StatusCode::OK, Html(html))
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

    let mut hmac_ok = false;

    for psk in PSKS.read().unwrap().iter() {
        let mut mac = Hmac::<Sha256>::new_from_slice(psk.key.as_bytes())
            .expect("hmac can be constructed");
        mac.update(&body);
        let result = mac.finalize().into_bytes().to_vec();

        // hack: skip sha256=
        let decoded = hex::decode(&sent_hmac[7..]).expect("provided hmac is valid hex");
        if decoded == result {
            hmac_ok = true;
            break;
        }
    }

    if !hmac_ok {
        eprintln!("bad hmac by all psks");
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


async fn make_app_server(cfg_path: &PathBuf, db_path: &PathBuf) -> Router {
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
        .route("/", get(handle_ci_index))
        .fallback(fallback_get)
        .with_state(Arc::new(DbCtx::new(cfg_path, db_path)))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let mut args = std::env::args();
    args.next().expect("first arg exists");
    let config_path = args.next().unwrap_or("./webserver_config.json".to_string());
    let web_config: WebserverConfig = serde_json::from_reader(std::fs::File::open(config_path).expect("file exists and is accessible")).expect("valid json for WebserverConfig");
    let mut psks = PSKS.write().expect("can write lock");
    *psks = web_config.psks.clone();

    let config = RustlsConfig::from_pem_file(
        web_config.cert_path.clone(),
        web_config.key_path.clone(),
    ).await.unwrap();

    let config_path = web_config.config_path.clone();
    let db_path = web_config.db_path.clone();
    if let Some(addr) = web_config.debug_addr.as_ref() {
        spawn(axum_server::bind_rustls("127.0.0.1:8080".parse().unwrap(), config.clone())
            .serve(make_app_server(&config_path, &db_path).await.into_make_service()));
    }
    if let Some(addr) = web_config.server_addr.as_ref() {
        spawn(axum_server::bind_rustls("0.0.0.0:443".parse().unwrap(), config)
            .serve(make_app_server(&config_path, &db_path).await.into_make_service()));
    }
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }
}
