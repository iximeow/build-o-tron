#![allow(dead_code)]

use std::convert::TryFrom;

use crate::dbctx::Run;
use crate::dbctx::Job;

#[derive(Debug, Clone)]
pub enum JobResult {
    Pass = 0,
    Fail = 1,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum RunState {
    Pending = 0,
    Started = 1,
    Finished = 2,
    Error = 3,
    Invalid = 4,
}

impl TryFrom<u8> for RunState {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, String> {
        match value {
            0 => Ok(RunState::Pending),
            1 => Ok(RunState::Started),
            2 => Ok(RunState::Finished),
            3 => Ok(RunState::Error),
            4 => Ok(RunState::Invalid),
            other => Err(format!("invalid job state: {}", other)),
        }
    }
}

pub(crate) fn row2run(row: &rusqlite::Row) -> Run {
    let (id, job_id, artifacts_path, state, run_host, build_token, create_time, start_time, complete_time, run_timeout, build_result, final_text) = row.try_into().unwrap();
    let state: u8 = state;
    Run {
        id,
        job_id,
        artifacts_path,
        state: state.try_into().unwrap(),
        run_host,
        create_time,
        start_time,
        complete_time,
        build_token,
        run_timeout,
        build_result,
        final_text,
    }
}

// remote_id is the remote from which we were notified. this is necessary so we know which remote
// to pull from to actually run the job.
pub const CREATE_JOBS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS jobs (id INTEGER PRIMARY KEY AUTOINCREMENT,
        source TEXT,
        created_time INTEGER,
        remote_id INTEGER,
        commit_id INTEGER);";

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
        run_id INTEGER,
        name TEXT,
        desc TEXT,
        created_time INTEGER,
        completed_time INTEGER);";

pub const CREATE_RUN_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS runs (id INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id INTEGER,
        artifacts_path TEXT,
        state INTEGER NOT NULL,
        run_host TEXT,
        build_token TEXT,
        created_time INTEGER,
        started_time INTEGER,
        complete_time INTEGER,
        run_timeout INTEGER,
        build_result INTEGER,
        final_status TEXT);";

pub const CREATE_REMOTES_INDEX: &'static str = "\
    CREATE INDEX IF NOT EXISTS 'repo_to_remote' ON remotes(repo_id);";

pub const CREATE_REPO_NAME_INDEX: &'static str = "\
    CREATE UNIQUE INDEX IF NOT EXISTS 'repo_names' ON repos(repo_name);";

pub const PENDING_RUNS: &'static str = "\
    select id, job_id, created_time from runs where state=0;";

pub const ACTIVE_RUNS: &'static str = "\
    select id,
        job_id,
        artifacts_path,
        state,
        run_host,
        build_token,
        created_time,
        started_time,
        complete_time,
        run_timeout,
        build_result,
        final_status from runs where state=1 or state=0;";

pub const LAST_ARTIFACTS_FOR_RUN: &'static str = "\
    select * from artifacts where run_id=?1 and (name like \"%(stderr)%\" or name like \"%(stdout)%\") order by id desc limit ?2;";

pub const JOB_BY_COMMIT_ID: &'static str = "\
    select id, source, created_time, remote_id, commit_id from jobs where commit_id=?1;";

pub const ARTIFACT_BY_ID: &'static str = "\
    select * from artifacts where id=?1 and run_id=?2;";

pub const JOB_BY_ID: &'static str = "\
    select id, source, created_time, remote_id, commit_id from jobs where id=?1";

pub const METRICS_FOR_RUN: &'static str = "\
    select * from metrics where run_id=?1 order by id asc;";

pub const COMMIT_TO_ID: &'static str = "\
    select id from commits where sha=?1;";

pub const REMOTES_FOR_REPO: &'static str = "\
    select * from remotes where repo_id=?1;";

pub const ALL_REPOS: &'static str = "\
    select * from repos;";

pub const LAST_JOBS_FROM_REMOTE: &'static str = "\
    select id, source, created_time, remote_id, commit_id from jobs where remote_id=?1 order by created_time desc limit ?2;";

pub const LAST_RUN_FOR_JOB: &'static str = "\
    select id,
        job_id,
        artifacts_path,
        state,
        run_host,
        build_token,
        created_time,
        started_time,
        complete_time,
        run_timeout,
        build_result,
        final_status from runs where job_id=?1;";

pub const SELECT_ALL_RUNS_WITH_JOB_INFO: &'static str = "\
    select jobs.id as job_id, runs.id as run_id, runs.state, runs.created_time, jobs.commit_id
    from jobs join runs on jobs.id=runs.job_id
    oder by runs.created_time asc;";
