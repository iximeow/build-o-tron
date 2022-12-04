use rusqlite::Connection;
use std::process::Command;
use std::path::{Path, PathBuf};

mod sql;

use std::time::{SystemTime, UNIX_EPOCH};

fn reserve_artifacts_dir(job: u64) -> std::io::Result<PathBuf> {
    let mut path: PathBuf = "/root/ixi_ci_server/jobs/".into();
    path.push(job.to_string());
    std::fs::create_dir(&path)?;
    Ok(path)
}

fn activate_job(connection: &mut Connection, job: u64, artifacts: Option<String>, state: u8, run_host: Option<String>, commit_id: u64, repo_url: String, repo_name: String) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("now is before epoch")
        .as_millis();

    let commit_sha: String = connection
        .query_row("select sha from commits where id=?1", [commit_id], |row| row.get(0))
        .expect("can run query");

    let artifacts: PathBuf = match artifacts {
        Some(artifacts) => PathBuf::from(artifacts),
        None => reserve_artifacts_dir(job).expect("can reserve a directory for artifacts")
    };

    if run_host == None {
        eprintln!("need to find a host to run the job");
    }

    eprintln!("cloning {}", repo_url);
    let mut repo_dir = artifacts.clone();
    repo_dir.push("repo");
    eprintln!(" ... into {}", repo_dir.display());

    Command::new("git")
        .arg("clone")
        .arg(repo_url)
        .arg(&format!("{}", repo_dir.display()))
        .status()
        .expect("can clone the repo");

    eprintln!("checking out {}", commit_sha);
    Command::new("git")
        .current_dir(&repo_dir)
        .arg("checkout")
        .arg(commit_sha)
        .status()
        .expect("can checkout hash");

    eprintln!("running {}", repo_name);
    /*
     * find the CI script, figure out how to run it
     */

    connection.execute(
        "update jobs set started_time=?1, run_host=?2, state=1, artifacts_path=?3 where id=?4",
        (now as u64, "test host".to_string(), format!("{}", artifacts.display()), job)
    )
        .expect("can update");
}

fn main() {
    let mut connection = Connection::open("/root/ixi_ci_server/state.db").unwrap();
    connection.execute(sql::CREATE_JOBS_TABLE, ()).unwrap();
    connection.execute(sql::CREATE_COMMITS_TABLE, ()).unwrap();
    connection.execute(sql::CREATE_REPOS_TABLE, ()).unwrap();
    connection.execute(sql::CREATE_REMOTES_TABLE, ()).unwrap();

    loop {
        let mut pending_query = connection.prepare(sql::PENDING_JOBS).unwrap();
        let mut jobs = pending_query.query([]).unwrap();
        let mut to_start = Vec::new();
        while let Some(row) = jobs.next().unwrap() {
            let (id, artifacts, state, run_host, commit_id, repo_url, repo_name): (u64, Option<String>, u8, Option<String>, u64, String, String)= TryInto::try_into(row).unwrap();
            to_start.push((id, artifacts, state, run_host, commit_id, repo_url, repo_name));
        }
        std::mem::drop(jobs);
        std::mem::drop(pending_query);
        if to_start.len() > 0 {
            println!("{} new jobs", to_start.len());

            for job in to_start.into_iter() {
                activate_job(&mut connection, job.0, job.1, job.2, job.3, job.4, job.5, job.6);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
