use std::sync::Arc;

use chrono::{Utc, TimeZone};

use ci_lib_core::dbctx::DbCtx;
use ci_lib_core::sql::{Job, Run, RunState};

/// return a duration rendered as the largest two non-zero units.
///
/// 60000ms -> 1m
/// 60001ms -> 1m
/// 61000ms -> 1m1s
///  1030ms -> 1.03s
pub fn duration_as_human_string(duration_ms: u64) -> String {
    let duration_sec = duration_ms / 1000;
    let duration_min = duration_sec / 60;
    let duration_hours = duration_min / 60;

    let duration_ms = duration_ms % 1000;
    let duration_sec = duration_sec % 60;
    let duration_min = duration_min % 60;
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
pub fn commit_url(job: &Job, commit_sha: &str, ctx: &Arc<DbCtx>) -> (String, Option<String>) {
    let remote = ctx.remote_by_id(job.remote_id).expect("query succeeds").expect("existing job references existing remote");

    match remote.remote_api.as_str() {
        "github" => {
            ("github".to_string(), Some(format!("{}/commit/{}", remote.remote_url, commit_sha)))
        },
        "email" => {
            ("email".to_string(), None)
        },
        other => {
            (other.to_string(), None)
        }
    }
}

/// produce a url to the ci.butactuallyin.space job details page
pub fn job_url(job: &Job, commit_sha: &str, ctx: &Arc<DbCtx>) -> String {
    let remote = ctx.remote_by_id(job.remote_id).expect("query succeeds").expect("existing job references existing remote");

    if remote.remote_api != "github" {
        eprintln!("job url for remote type {} can't be constructed, i think", &remote.remote_api);
    }

    format!("{}/{}", &remote.remote_path, commit_sha)
}

/// render how long a run took, or is taking, in a human-friendly way
pub fn display_run_time(run: &Run) -> String {
    if let Some(start_time) = run.start_time {
        if let Some(complete_time) = run.complete_time {
            if complete_time < start_time {
                if run.state == RunState::Started {
                    // this run has been restarted. the completed time is stale.
                    // further, this is a currently active run.
                    let now_ms = ci_lib_core::now_ms();
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
            if run.state != RunState::Invalid {
                let now_ms = ci_lib_core::now_ms();
                let mut duration = duration_as_human_string(now_ms - start_time);
                duration.push_str(" (ongoing)");
                duration
            } else {
                "n/a".to_string()
            }
        }
    } else {
        "not yet run".to_owned()
    }
}

pub fn build_repo_index(ctx: &Arc<DbCtx>) -> Result<String, String> {
    let repos = match ctx.get_repos() {
        Ok(repos) => repos,
        Err(e) => {
            eprintln!("failed to get repos: {:?}", e);
            return Err("gonna feel that one tomorrow".to_string());
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
    let headings = ["repo", "last build", "commit/job", "remote", "duration", "status", "result"];
    for heading in headings {
        response.push_str(&format!("<th class='row-item'>{}</th>", heading));
    }
    response.push_str("</tr>\n");

    let mut row_num = 0;

    for repo in repos {
        let mut most_recent_run: Option<(Job, Run)> = None;

        for remote in ctx.remotes_by_repo(repo.id).expect("remotes by repo works") {
            let last_job = ctx.last_job_from_remote(remote.id).expect("job by remote works");
            if let Some(last_job) = last_job {
                if let Some(last_run) = ctx.last_run_for_job(last_job.id).expect("can query") {
                    if most_recent_run.as_ref().map(|run| run.1.create_time < last_run.create_time).unwrap_or(true) {
                        most_recent_run = Some((last_job, last_run));
                    }
                }
            }
        }

        let repo_html = format!("<a href=\"/{}\">{}</a>", &repo.name, &repo.name);

        let row_html: String = match most_recent_run {
            Some((job, run)) => {
                let job_commit = ctx.commit_sha(job.commit_id).expect("job has a commit");
                let nice_name = ctx.nice_name_for_commit(job.commit_id).expect("can try to get a nice name");
                let commit_html = match (job_url(&job, &job_commit, &ctx), nice_name) {
                    (url, Some(name)) => format!("<a href=\"{}\">{}</a> (job {}) {}", url, &job_commit[..9], job.id, name.stringy()),
                    (url, None) => format!("<a href=\"{}\">{}</a> (job {})", url, &job_commit[..9], job.id),
                };

                let remote_html = match commit_url(&job, &job_commit, &ctx) {
                    (source, Some(url)) => format!("<a href=\"{}\">{}</a>", url, source),
                    (source, None) => format!("{}", source),
                };

                let last_build_time = Utc.timestamp_millis_opt(run.create_time as i64).unwrap().to_rfc2822();
                let duration = display_run_time(&run);

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

                let entries = [repo_html.as_str(), last_build_time.as_str(), commit_html.as_str(), remote_html.as_str(), &duration, &status, &result];
                let entries = entries.iter().chain(std::iter::repeat(&"")).take(headings.len());

                let mut row_html = String::new();
                for entry in entries {
                    row_html.push_str(&format!("<td class='row-item'>{}</td>", entry));
                }
                row_html
            }
            None => {
                let entries = [repo_html.as_str()];
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

    response.push_str("<h4>active tasks</h4>\n");

    let runs = ctx.get_active_runs().expect("can query");
    if runs.len() == 0 {
        response.push_str("<p>(none)</p>\n");
    } else {
        response.push_str("<table class='build-table'>");
        response.push_str("<tr>\n");
        let headings = ["repo", "last build", "commit/job", "remote", "duration", "status", "result"];
        for heading in headings {
            response.push_str(&format!("<th class='row-item'>{}</th>", heading));
        }
        response.push_str("</tr>\n");

        let mut row_num = 0;

        for run in runs.iter() {
            let row_index = row_num % 2;

            let job = ctx.job_by_id(run.job_id).expect("query succeeds").expect("job id is valid");
            let remote = ctx.remote_by_id(job.remote_id).expect("query succeeds").expect("remote id is valid");
            let repo = ctx.repo_by_id(remote.repo_id).expect("query succeeds").expect("repo id is valid");

            let repo_html = format!("<a href=\"/{}\">{}</a>", &repo.name, &repo.name);

            let job_commit = ctx.commit_sha(job.commit_id).expect("job has a commit");
            let nice_name = ctx.nice_name_for_commit(job.commit_id).expect("can try to get a nice name");
            let commit_html = match (job_url(&job, &job_commit, &ctx), nice_name) {
                (url, Some(name)) => format!("<a href=\"{}\">{}</a> (job {}) {}", url, &job_commit[..9], job.id, name.stringy()),
                (url, None) => format!("<a href=\"{}\">{}</a> (job {})", url, &job_commit[..9], job.id),
            };

            let remote_html = match commit_url(&job, &job_commit, &ctx) {
                (source, Some(url)) => format!("<a href=\"{}\">{}</a>", url, source),
                (source, None) => format!("{}", source),
            };

            let last_build_time = Utc.timestamp_millis_opt(run.create_time as i64).unwrap().to_rfc2822();
            let duration = display_run_time(&run);

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

            let entries = [repo_html.as_str(), last_build_time.as_str(), commit_html.as_str(), remote_html.as_str(), &duration, &status, &result];
            let entries = entries.iter().chain(std::iter::repeat(&"")).take(headings.len());

            let mut row_html = String::new();
            for entry in entries {
                row_html.push_str(&format!("<td class='row-item'>{}</td>", entry));
            }


            response.push_str(&format!("<tr class=\"{}\">", ["even-row", "odd-row"][row_index]));
            response.push_str(&row_html);
            response.push_str("</tr>");
            response.push('\n');

            row_num += 1;
        }

        response.push_str("</table>\n");
    }

    response.push_str("</html>");
    Ok(response)
}
