#![allow(dead_code)]

use std::convert::TryFrom;

#[derive(Debug, Clone)]
pub enum JobResult {
    Pass = 0,
    Fail = 1,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobState {
    Pending = 0,
    Started = 1,
    Finished = 2,
    Error = 3,
    Invalid = 4,
}

impl TryFrom<u8> for JobState {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, String> {
        match value {
            0 => Ok(JobState::Pending),
            1 => Ok(JobState::Started),
            2 => Ok(JobState::Finished),
            3 => Ok(JobState::Error),
            4 => Ok(JobState::Invalid),
            other => Err(format!("invalid job state: {}", other)),
        }
    }
}

// remote_id is the remote from which we were notified. this is necessary so we know which remote
// to pull from to actually run the job.
pub const CREATE_JOBS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS jobs (id INTEGER PRIMARY KEY AUTOINCREMENT,
        artifacts_path TEXT,
        state INTEGER NOT NULL,
        run_host TEXT,
        build_token TEXT,
        remote_id INTEGER,
        commit_id INTEGER,
        created_time INTEGER,
        started_time INTEGER,
        complete_time INTEGER,
        job_timeout INTEGER,
        source TEXT,
        build_result INTEGER,
        final_status TEXT);";

pub const CREATE_METRICS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS metrics (id INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id INTEGER,
        name TEXT,
        value TEXT,
        UNIQUE(job_id, name)
    );";

pub const CREATE_COMMITS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS commits (id INTEGER PRIMARY KEY AUTOINCREMENT, sha TEXT UNIQUE);";

pub const CREATE_REPOS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS repos (id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo_name TEXT);";

// remote_api is `github` or NULL for now. hopefully a future cgit-style notifier one day.
// remote_path is some unique identifier for the relevant remote.
// * for `github` remotes, this will be `owner/repo`.
// * for others.. who knows.
// remote_url is a url for human interaction with the remote (think https://git.iximeow.net/zvm)
// remote_git_url is a url that can be `git clone`'d to fetch sources
pub const CREATE_REMOTES_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS remotes (id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo_id INTEGER,
        remote_path TEXT,
        remote_api TEXT,
        remote_url TEXT,
        remote_git_url TEXT,
        notifier_config_path TEXT);";

pub const CREATE_ARTIFACTS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS artifacts (id INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id INTEGER,
        name TEXT,
        desc TEXT);";

pub const CREATE_REMOTES_INDEX: &'static str = "\
    CREATE INDEX IF NOT EXISTS 'repo_to_remote' ON remotes(repo_id);";

pub const CREATE_REPO_NAME_INDEX: &'static str = "\
    CREATE UNIQUE INDEX IF NOT EXISTS 'repo_names' ON repos(repo_name);";

pub const PENDING_JOBS: &'static str = "\
    select id, artifacts_path, state, run_host, remote_id, commit_id, created_time, source from jobs where state=0;";

pub const LAST_ARTIFACTS_FOR_JOB: &'static str = "\
    select * from artifacts where job_id=?1 and (name like \"%(stderr)%\" or name like \"%(stdout)%\") order by id desc limit 2;";

pub const METRICS_FOR_JOB: &'static str = "\
    select * from metrics where job_id=?1 order by id asc;";

pub const COMMIT_TO_ID: &'static str = "\
    select id from commits where sha=?1;";

pub const REMOTES_FOR_REPO: &'static str = "\
    select * from remotes where repo_id=?1;";

pub const ALL_REPOS: &'static str = "\
    select * from repos;";

pub const LAST_JOBS_FROM_REMOTE: &'static str = "\
    select * from jobs where remote_id=?1 order by created_time desc limit ?2;";

