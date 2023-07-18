#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(unused_imports)]

use chrono::{Utc, TimeZone};
use lazy_static::lazy_static;
use std::sync::RwLock;
use std::collections::HashMap;
use serde::{Deserialize, Serialize};
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
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use axum::body::StreamBody;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

// mod protocol;

use ci_lib_core::sql::RunState;

use ci_lib_core::dbctx::DbCtx;
use ci_lib_core::sql::{ArtifactRecord, Job, Run};

use rusqlite::OptionalExtension;

#[derive(Serialize, Deserialize)]
struct WebserverConfig {
    psks: Vec<GithubPsk>,
    jobs_path: PathBuf,
    config_path: PathBuf,
    db_path: PathBuf,
    debug_addr: Option<serde_json::Value>,
    server_addr: Option<serde_json::Value>,
}

#[derive(Clone)]
struct WebserverState {
    jobs_path: PathBuf,
    dbctx: Arc<DbCtx>,
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
        .query_row(ci_lib_core::sql::COMMIT_TO_ID, [sha.clone()], |row| row.get(0))
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

    let repo_default_run_pref: Option<String> = ctx.conn.lock().unwrap()
        .query_row("select default_run_preference from repos where id=?1;", [repo_id], |row| {
            Ok((row.get(0)).unwrap())
        })
        .expect("can query");

    let pusher_email = pusher
        .get("email")
        .expect("has email")
        .as_str()
        .expect("is str");

    let job_id = ctx.new_job(remote_id, &sha, Some(pusher_email), repo_default_run_pref).unwrap();
    let _ = ctx.new_run(job_id, None).unwrap();

    let notifiers = ci_lib_native::dbctx_ext::notifiers_by_repo(&ctx, repo_id).expect("can get notifiers");

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

async fn handle_ci_index(State(ctx): State<WebserverState>) -> impl IntoResponse {
    eprintln!("root index");
    match ci_lib_web::build_repo_index(&ctx.dbctx) {
        Ok(html) => {
            (StatusCode::OK, Html(html))
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Html(e))
        }
    }
}

async fn handle_commit_status(Path(path): Path<(String, String, String)>, State(ctx): State<WebserverState>) -> impl IntoResponse {
    eprintln!("path: {}/{}, sha {}", path.0, path.1, path.2);
    let remote_path = format!("{}/{}", path.0, path.1);
    let sha = path.2;

    let (commit_id, sha): (u64, String) = if sha.len() >= 7 {
        match ctx.dbctx.conn.lock().unwrap()
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

    let short_sha = &sha[0..9];

    let (remote_id, repo_id): (u64, u64) = ctx.dbctx.conn.lock().unwrap()
        .query_row("select id, repo_id from remotes where remote_path=?1;", [&remote_path], |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
        .expect("can query");

    let job = ctx.dbctx.job_by_commit_id(commit_id).expect("can query").expect("job exists");

    let run = ctx.dbctx.last_run_for_job(job.id).expect("can query").expect("run exists");

    let complete_time = run.complete_time.unwrap_or_else(ci_lib_core::now_ms);

    let (status_elem, status_desc) = match run.state {
        RunState::Pending | RunState::Started => {
            ("<span style='color:#660;'>pending</span>", "⌛in progress")
        },
        RunState::Finished => {
            if let Some(build_result) = run.build_result {
                if build_result == 0 {
                    ("<span style='color:green;'>pass</span>", "✅ passed")
                } else {
                    ("<span style='color:red;'>failed</span>", "❌ failed")
                }
            } else {
                eprintln!("run {} for commit {} is missing a build result but is reportedly finished (old data)?", run.id, commit_id);
                ("<span style='color:red;'>unreported</span>", "❔ missing status")
            }
        },
        RunState::Error => {
            ("<span style='color:red;'>error</span>", "🧯 error, uncompleted")
        }
        RunState::Invalid => {
            ("<span style='color:red;'>(server error)</span>", "dude even i don't know")
        }
    };
    let debug_info = run.state == RunState::Finished && run.build_result == Some(1) || run.state == RunState::Error;

    let repo_name: String = ctx.dbctx.conn.lock().unwrap()
        .query_row("select repo_name from repos where id=?1;", [repo_id], |row| row.get(0))
        .expect("can query");

    let deployed = false;

    let mut head = String::new();
    head.push_str("<head>");
    head.push_str(&format!("<title>ci.butactuallyin.space - {}</title>", repo_name));
    let include_og_tags = true;
    if include_og_tags {
        head.push_str("\n");
        head.push_str(&format!("<meta property=\"og:type\" content=\"website\">\n"));
        head.push_str(&format!("<meta property=\"og:site_name\" content=\"ci.butactuallyin.space\">\n"));
        head.push_str(&format!("<meta property=\"og:url\" content=\"/{}/{}/{}\">\n", &path.0, &path.1, &sha));
        head.push_str(&format!("<meta property=\"og:title\" contents=\"{}/{} commit {}\">", &path.0, &path.1, &short_sha));
        let build_og_description = format!("commit {} of {}/{}, {} after {}",
            short_sha,
            path.0, path.1,
            status_desc,
            ci_lib_web::display_run_time(&run)
        );
        head.push_str(&format!("<meta name=\"description\" content=\"{}\"\n>", build_og_description));
        head.push_str(&format!("<meta property=\"og:description\" content=\"{}\"\n>", build_og_description));
    }
    head.push_str("</head>\n");
    let repo_html = format!("<a href=\"/{}\">{}</a>", &repo_name, &repo_name);
    let remote_commit_elem = format!("<a href=\"https://www.github.com/{}/commit/{}\">{}</a>", &remote_path, &sha, &sha);

    let mut artifacts_fragment = String::new();
    let mut artifacts: Vec<ArtifactRecord> = ctx.dbctx.artifacts_for_run(run.id, None).unwrap()
        .into_iter() // HACK: filter out artifacts for previous runs of a run. artifacts should be attached to a run, runs should be distinct from run. but i'm sleepy.
        .filter(|artifact| artifact.created_time >= run.start_time.unwrap_or_else(ci_lib_core::now_ms))
        .collect();

    artifacts.sort_by_key(|artifact| artifact.created_time);

    fn diff_times(run_completed: u64, artifact_completed: Option<u64>) -> u64 {
        let artifact_completed = artifact_completed.unwrap_or_else(ci_lib_core::now_ms);
        let run_completed = std::cmp::max(run_completed, artifact_completed);
        run_completed - artifact_completed
    }

    let recent_artifacts: Vec<ArtifactRecord> = artifacts.iter().filter(|artifact| diff_times(complete_time, artifact.completed_time) <= 60_000).cloned().collect();
    let old_artifacts: Vec<ArtifactRecord> = artifacts.iter().filter(|artifact| diff_times(complete_time, artifact.completed_time) > 60_000).cloned().collect();

    for artifact in old_artifacts.iter() {
        let created_time_str = Utc.timestamp_millis_opt(artifact.created_time as i64).unwrap().to_rfc2822();
        artifacts_fragment.push_str(&format!("<div><pre style='display:inline;'>{}</pre> step: <pre style='display:inline;'>{}</pre></div>\n", created_time_str, &artifact.name));
        let duration_str = ci_lib_web::duration_as_human_string(artifact.completed_time.unwrap_or_else(ci_lib_core::now_ms) - artifact.created_time);
        let size_str = (std::fs::metadata(&format!("./artifacts/{}/{}", artifact.run_id, artifact.id)).expect("metadata exists").len() / 1024).to_string();
        artifacts_fragment.push_str(&format!("<pre>  {}kb in {} </pre>\n", size_str, duration_str));
    }

    for artifact in recent_artifacts.iter() {
        let created_time_str = Utc.timestamp_millis_opt(artifact.created_time as i64).unwrap().to_rfc2822();
        artifacts_fragment.push_str(&format!("<div><pre style='display:inline;'>{}</pre> step: <pre style='display:inline;'>{}</pre></div>\n", created_time_str, &artifact.name));
        if debug_info {
            artifacts_fragment.push_str("<pre>");
            artifacts_fragment.push_str(&std::fs::read_to_string(format!("./artifacts/{}/{}", artifact.run_id, artifact.id)).unwrap());
            artifacts_fragment.push_str("</pre>\n");
        } else {
            let duration_str = ci_lib_web::duration_as_human_string(artifact.completed_time.unwrap_or_else(ci_lib_core::now_ms) - artifact.created_time);
            let size_str = std::fs::metadata(&format!("./artifacts/{}/{}", artifact.run_id, artifact.id)).map(|md| {
                (md.len() / 1024).to_string()
            }).unwrap_or_else(|e| format!("[{}]", e));
            artifacts_fragment.push_str(&format!("<pre>  {}kb in {} </pre>\n", size_str, duration_str));
        }
    }

    let metrics = summarize_job_metrics(&ctx.dbctx, run.id, run.job_id).unwrap();

    let mut html = String::new();
    html.push_str("<html>\n");
    html.push_str(&format!("  {}\n", head));
    html.push_str("  <body>\n");
    html.push_str("    <pre>\n");
    html.push_str(&format!("repo: {}\n", repo_html));
    html.push_str(&format!("commit: {}, run: {}\n", remote_commit_elem, run.id));
    html.push_str(&format!("status: {} in {}\n", status_elem, ci_lib_web::display_run_time(&run)));
    if let Some(desc) = run.final_text.as_ref() {
        html.push_str(&format!("  description: {}\n  ", desc));
    }
    html.push_str(&format!("deployed: {}\n", deployed));
    html.push_str("    </pre>\n");
    if artifacts_fragment.len() > 0 {
        html.push_str("    <div>artifacts</div>\n");
        html.push_str(&artifacts_fragment);
    }
    if let Some(metrics) = metrics {
        html.push_str(&metrics);
    }
    html.push_str("  </body>\n");
    html.push_str("</html>");

    (StatusCode::OK, Html(html))
}

fn summarize_job_metrics(dbctx: &Arc<DbCtx>, run_id: u64, job_id: u64) -> Result<Option<String>, String> {
    let runs = dbctx.runs_for_job_one_per_host(job_id)?;

    let mut section = String::new();
    section.push_str("<div>\n");
    section.push_str("<h3>metrics</h3>\n");
    section.push_str("<table style='font-family: monospace;'>\n");

    if runs.len() == 1 {
        let metrics = dbctx.metrics_for_run(run_id).unwrap();
        if metrics.is_empty() {
            return Ok(None);
        }

        section.push_str("<tr><th>name</th><th>value</th></tr>");
        for metric in metrics {
            section.push_str(&format!("<tr><td>{}</td><td>{}</td></tr>", &metric.name, &metric.value));
        }
    } else {
        // very silly ordering issue: need an authoritative ordering of metrics to display metrics
        // in a consistent order across runs (though they SHOULD all be ordered the same).
        //
        // the first run might not have all metrics (first run could be on the slowest build host
        // f.ex), so for now just assume that metrics *will* be consistently ordered and build a
        // list of metrics from the longest list of metrics we've seen. builders do not support
        // concurrency so at least the per-run metrics-order-consistency assumption should hold..
        let mut all_names: Vec<String> = Vec::new();

        let all_metrics: Vec<(HashMap<String, String>, HostDesc)> = runs.iter().map(|run| {
            let metrics = dbctx.metrics_for_run(run.id).unwrap();

            let mut metrics_map = HashMap::new();
            for metric in metrics.into_iter() {
                if !all_names.contains(&metric.name) {
                    all_names.push(metric.name.clone());
                }
                metrics_map.insert(metric.name, metric.value);
            }

            let (hostname, cpu_vendor_id, cpu_family, cpu_model, cpu_max_freq_khz) = match run.host_id {
                Some(host_id) => {
                    dbctx.host_model_info(host_id).unwrap()
                }
                None => {
                    ("unknown".to_string(), "unknown".to_string(), "0".to_string(), "0".to_string(), 0)
                }
            };

            (metrics_map, HostDesc::from_parts(hostname, cpu_vendor_id, cpu_family, cpu_model, cpu_max_freq_khz))
        }).collect();

        if all_metrics.is_empty() {
            return Ok(None);
        }

        let mut header = "<tr><th>name</th>".to_string();
        for (_, host) in all_metrics.iter() {
            header.push_str(&format!("<th>{}</br>{} @ {:.3}GHz</th>", &host.hostname, &host.cpu_desc, (host.cpu_max_freq_khz as f64) / 1000_000.0));
        }
        header.push_str("</tr>\n");
        section.push_str(&header);

        for name in all_names.iter() {
            let mut row = format!("<tr><td>{}</td>", &name);
            for (metrics, _) in all_metrics.iter() {
                let value = metrics.get(name)
                    .map(|x| x.clone())
                    .unwrap_or_else(String::new);
                row.push_str(&format!("<td>{}</td>", value));
            }
            row.push_str("</tr>\n");
            section.push_str(&row);
        }
    };
    section.push_str("</table>\n");
    section.push_str("</div>\n");

    Ok(Some(section))
}

async fn handle_get_artifact(Path(path): Path<(String, String)>, State(ctx): State<WebserverState>) -> impl IntoResponse {
    eprintln!("get artifact, run={}, artifact={}", path.0, path.1);
    let run: u64 = path.0.parse().unwrap();
    let artifact_id: u64 = path.1.parse().unwrap();

    let artifact_descriptor = match ctx.dbctx.lookup_artifact(run, artifact_id).unwrap() {
        Some(artifact) => artifact,
        None => {
            return (StatusCode::NOT_FOUND, Html("no such artifact")).into_response();
        }
    };

    let mut live_artifact = false;

    if let Some(completed_time) = artifact_descriptor.completed_time {
        if completed_time < artifact_descriptor.created_time {
            live_artifact = true;
        }
    } else {
        live_artifact = true;
    }

    if live_artifact {
        let (mut tx_sender, tx_receiver) = tokio::io::duplex(65536);
        let resp_body = axum_extra::body::AsyncReadBody::new(tx_receiver);
        let mut artifact_path = ctx.jobs_path.clone();
        artifact_path.push(artifact_descriptor.run_id.to_string());
        artifact_path.push(artifact_descriptor.id.to_string());
        spawn(async move {
            let mut artifact = artifact_descriptor;

            let mut artifact_file = tokio::fs::File::open(&artifact_path)
                .await
                .expect("artifact file exists?");
            while artifact.completed_time.is_none() {
                match ci_lib_native::io::forward_data(&mut artifact_file, &mut tx_sender).await {
                    Ok(()) => {
                        // reached the current EOF, wait and then commit an unspeakable sin
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        // this would be much implemented as yielding on a condvar woken when an
                        // inotify event on the file indicates a write has occurred. but i am
                        // dreadfully lazy, so we'll just uhh. busy-poll on the file? lmao.
                        artifact = ctx.dbctx.lookup_artifact(artifact.run_id, artifact.id)
                            .expect("can query db")
                            .expect("artifact still exists");
                    }
                    Err(e) => {
                        eprintln!("artifact file streaming failed: {}", e);
                    }
                }
            }

            eprintln!("[+] artifact {} is done being written, and we've sent the whole thing. bye!", artifact.id);
        });
        (StatusCode::OK, resp_body).into_response()
    } else {
        (StatusCode::OK, Html("all done")).into_response()
    }
}

async fn handle_repo_summary(Path(path): Path<String>, State(ctx): State<WebserverState>) -> impl IntoResponse {
    eprintln!("get repo summary: {:?}", path);

    let mut last_builds = Vec::new();

    let (repo_id, repo_name, default_run_preference): (u64, String, Option<String>) = match ctx.dbctx.conn.lock().unwrap()
        .query_row("select id, repo_name, default_run_preference from repos where repo_name=?1;", [&path], |row| Ok((row.get(0).unwrap(), row.get(1).unwrap(), row.get(2).unwrap())))
        .optional()
        .unwrap() {
        Some(elem) => elem,
        None => {
            eprintln!("no repo named {}", path);
            return (StatusCode::NOT_FOUND, Html(String::new()));
        }
    };

    // TODO: display default_run_preference somehow on the web summary?

    for remote in ctx.dbctx.remotes_by_repo(repo_id).expect("can get repo from a path") {
        let mut last_ten_jobs = ctx.dbctx.recent_jobs_from_remote(remote.id, 10).expect("can look up jobs for a repo");
        last_builds.extend(last_ten_jobs.drain(..));
    }
    last_builds.sort_by_key(|job| -(job.created_time as i64));

    let mut response = String::new();
    response.push_str("<html>\n");
    response.push_str(&format!("<title> ci.butactuallyin.space - {} </title>\n", repo_name));
    response.push_str("<style>\n");
    response.push_str(".build-table { font-family: monospace; border: 1px solid black; border-collapse: collapse; }\n");
    response.push_str(".row-item { padding-left: 4px; padding-right: 4px; border-right: 1px solid black; }\n");
    response.push_str(".odd-row { background: #eee; }\n");
    response.push_str(".even-row { background: #ddd; }\n");
    response.push_str("</style>\n");
    response.push_str(&format!("<h1>{} build history</h1>\n", repo_name));
    response.push_str("<a href=/>full repos index</a><p> </p>\n");

    response.push_str("<table class='build-table'>");
    response.push_str("<tr>\n");
    let headings = ["last build", "job", "build commit", "duration", "status", "result"];
    for heading in headings {
        response.push_str(&format!("<th class='row-item'>{}</th>", heading));
    }
    response.push_str("</tr>\n");

    let mut row_num = 0;

    for job in last_builds.iter().take(10) {
        let run = ctx.dbctx.last_run_for_job(job.id).expect("query succeeds").expect("TODO: run exists if job exists (small race if querying while creating job ....");
        let job_commit = ctx.dbctx.commit_sha(job.commit_id).expect("job has a commit");
        let commit_html = match ci_lib_web::commit_url(&job, &job_commit, &ctx.dbctx) {
            Some(url) => format!("<a href=\"{}\">{}</a>", url, &job_commit),
            None => job_commit.clone()
        };

        let job_html = format!("<a href=\"{}\">{}</a>", ci_lib_web::job_url(&job, &job_commit, &ctx.dbctx), job.id);

        let last_build_time = Utc.timestamp_millis_opt(run.create_time as i64).unwrap().to_rfc2822();
        let duration = ci_lib_web::display_run_time(&run);

        let status = format!("{:?}", run.state).to_lowercase();

        let result = match run.build_result {
            Some(0) => "<span style='color:green;'>pass</span>",
            Some(_) => "<span style='color:red;'>fail</span>",
            None => match run.state {
                RunState::Pending => { "unstarted" },
                RunState::Started => { "<span style='color:darkgoldenrod;'>in progress</span>" },
                _ => { "<span style='color:red;'>unreported</span>" }
            }
        };

        let entries = [last_build_time.as_str(), job_html.as_str(), commit_html.as_str(), &duration, &status, &result];
        let entries = entries.iter().chain(std::iter::repeat(&"")).take(headings.len());

        let mut row_html = String::new();
        for entry in entries {
            row_html.push_str(&format!("<td class='row-item'>{}</td>", entry));
        }

        let row_index = row_num % 2;
        response.push_str(&format!("<tr class=\"{}\">", ["even-row", "odd-row"][row_index]));
        response.push_str(&row_html);
        response.push_str("</tr>\n");

        row_num += 1;
    }
    response.push_str("</html>");

    (StatusCode::OK, Html(response))
}

async fn handle_repo_event(Path(path): Path<(String, String)>, headers: HeaderMap, State(ctx): State<WebserverState>, body: Bytes) -> impl IntoResponse {
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

    handle_github_event(Arc::clone(&ctx.dbctx), path.0, path.1, kind, payload).await
}


async fn make_app_server(jobs_path: PathBuf, cfg_path: &PathBuf, db_path: &PathBuf) -> Router {
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
        .route("/:owner", get(handle_repo_summary))
        .route("/:owner/:repo", post(handle_repo_event))
        .route("/artifact/:b/:artifact_id", get(handle_get_artifact))
        .route("/", get(handle_ci_index))
        .fallback(fallback_get)
        .with_state(WebserverState {
            jobs_path,
            dbctx: Arc::new(DbCtx::new(cfg_path, db_path))
        })
}

async fn bind_server(conf: serde_json::Value, jobs_path: PathBuf, config_path: PathBuf, db_path: PathBuf) -> std::io::Result<()> {
    let server = make_app_server(jobs_path.clone(), &config_path, &db_path).await.into_make_service();
    use serde_json::Value;
    match conf {
        Value::String(address) => {
            axum_server::bind(address.parse().unwrap())
                .serve(server).await
        },
        Value::Object(map) => {
            let address = match map.get("address") {
                Some(Value::String(address)) => address.clone(),
                None => {
                    panic!("no local address");
                },
                other => {
                    panic!("invalid local address: {:?}", other);
                }
            };

            match (map.get("cert_path"), map.get("key_path")) {
                (Some(Value::String(cert_path)), Some(Value::String(key_path))) => {
                    let config = RustlsConfig::from_pem_file(
                        cert_path.clone(),
                        key_path.clone(),
                    ).await.unwrap();
                    axum_server::bind_rustls(address.parse().unwrap(), config)
                        .serve(server).await
                },
                (Some(_), _) | (_, Some(_)) => {
                    panic!("invalid local tls config: only one of `cert_path` or `key_path` has been provided");
                },
                (None, None) => {
                    axum_server::bind(address.parse().unwrap())
                        .serve(server).await
                }
            }
        },
        other => {
            panic!("invalid server bind config: {:?}", other);
        }
    }
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
    // drop write lock so we can read PSKS elsewhere WITHOUT deadlocking.
    std::mem::drop(psks);

    let jobs_path = web_config.jobs_path.clone();
    let config_path = web_config.config_path.clone();
    let db_path = web_config.db_path.clone();
    if let Some(addr_conf) = web_config.debug_addr.as_ref() {
        spawn(bind_server(addr_conf.clone(), jobs_path.clone(), config_path.clone(), db_path.clone()));
    }
    if let Some(addr_conf) = web_config.server_addr.as_ref() {
        spawn(bind_server(addr_conf.clone(), jobs_path.clone(), config_path.clone(), db_path.clone()));
    }
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }
}

struct HostDesc {
    hostname: String,
    cpu_desc: String,
    cpu_max_freq_khz: u64,
}
impl HostDesc {
    fn from_parts(hostname: String, vendor_id: String, cpu_family: String, model: String, cpu_max_freq_khz: u64) -> Self {
        let cpu_desc = match (vendor_id.as_str(), cpu_family.as_str(), model.as_str()) {
            ("Arm Limited", "8", "0xd03") => "aarch64 A53".to_string(),
            ("GenuineIntel", "6", "85") => "x86_64 Skylake".to_string(),
            ("AuthenticAMD", "23", "113") => "x86_64 Matisse".to_string(),
            (vendor, family, model) => format!("unknown {}:{}:{}", vendor, family, model)
        };

        HostDesc {
            hostname,
            cpu_desc,
            cpu_max_freq_khz,
        }
    }
}
