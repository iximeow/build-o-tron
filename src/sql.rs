#![allow(dead_code)]

use std::convert::TryFrom;

#[derive(Debug, Clone)]
pub enum JobState {
    Pending = 0,
    Started = 1,
    Complete = 2,
    Error = 3,
    Invalid = 4,
}

impl TryFrom<u8> for JobState {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, String> {
        match value {
            0 => Ok(JobState::Pending),
            1 => Ok(JobState::Started),
            2 => Ok(JobState::Complete),
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
        complete_time INTEGER);";

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

pub const CREATE_REMOTES_INDEX: &'static str = "\
    CREATE INDEX IF NOT EXISTS 'repo_to_remote' ON remotes(repo_id);";

pub const CREATE_REPO_NAME_INDEX: &'static str = "\
    CREATE UNIQUE INDEX IF NOT EXISTS 'repo_names' ON repos(repo_name);";

pub const PENDING_JOBS: &'static str = "\
    select * from jobs where state=0;";

pub const COMMIT_TO_ID: &'static str = "\
    select id from commits where sha=?1;";

pub const REMOTES_FOR_REPO: &'static str = "\
    select * from remotes where repo_id=?1;";
