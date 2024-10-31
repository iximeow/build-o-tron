#![allow(dead_code)]

use std::convert::TryFrom;

#[derive(Debug, Clone)]
pub struct PendingRun {
    pub id: u64,
    pub job_id: u64,
    pub create_time: u64,
}

impl Run {
    fn into_pending_run(self) -> PendingRun {
        PendingRun {
            id: self.id,
            job_id: self.job_id,
            create_time: self.create_time,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenValidity {
    Expired,
    Invalid,
    Valid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricRecord {
    pub id: u64,
    pub run_id: u64,
    pub name: String,
    pub value: String
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub id: u64,
    pub run_id: u64,
    pub name: String,
    pub desc: String,
    pub created_time: u64,
    pub completed_time: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Repo {
    pub id: u64,
    pub name: String,
    pub default_run_preference: Option<String>,
}

#[derive(Debug)]
pub struct Remote {
    pub id: u64,
    pub repo_id: u64,
    pub remote_path: String,
    pub remote_api: String,
    pub remote_url: String,
    pub remote_git_url: String,
    pub notifier_config_path: String,
}

// a job tracks when we became aware of a commit from remote. typically a job will have a 1-1
// relationship with commits, and potentially many *runs* of that job.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub remote_id: u64,
    pub commit_id: u64,
    pub created_time: u64,
    pub source: Option<String>,
    pub run_preferences: Option<String>,
}

// a run tracks the intent or obligation to have some runner somewhere run a goodfile and report
// results. a job may have many runs from many different hosts rebuliding history, or reruns of the
// same job on the same hardware to collect more datapoints on the operation.
#[derive(Debug, Clone)]
pub struct Run {
    pub id: u64,
    pub job_id: u64,
    pub artifacts_path: Option<String>,
    pub state: RunState,
    pub host_id: Option<u64>,
    pub create_time: u64,
    pub start_time: Option<u64>,
    pub complete_time: Option<u64>,
    pub build_token: Option<String>,
    pub run_timeout: Option<u64>,
    pub build_result: Option<u8>,
    pub final_text: Option<String>,
}

#[derive(Debug, Clone)]
pub enum JobResult {
    Pass = 0,
    Fail = 1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameState {
    Fresh = 0,
    Stale = 1,
    // this reflects a name that is "nice" in that it's not a full commit hash, but it's not a ref
    // the commit is known by - it's just... a prefix of the full hash.
    ShortSha = 2,
}

impl TryFrom<u8> for NameState {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, String> {
        match value {
            0 => Ok(NameState::Fresh),
            1 => Ok(NameState::Stale),
            2 => Ok(NameState::ShortSha),
            other => Err(format!("invalid name state: {}", other)),
        }
    }
}

pub struct CommitName {
    pub name: String,
    pub state: NameState,
}

impl CommitName {
    pub fn stale(&self) -> bool {
        self.state != NameState::Fresh
    }

    pub fn stringy(&self) -> String {
        match self.state {
            NameState::Fresh => { self.name.clone() },
            NameState::Stale => { format!("{} (stale)", self.name) },
            NameState::ShortSha => { self.name.clone() },
        }
    }
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

/*
pub(crate) fn row2run(row: &rusqlite::Row) -> Run {
    let (id, job_id, artifacts_path, state, host_id, build_token, create_time, start_time, complete_time, run_timeout, build_result, final_text) = row.try_into().unwrap();
    let state: u8 = state;
    Run {
        id,
        job_id,
        artifacts_path,
        state: state.try_into().unwrap(),
        host_id,
        create_time,
        start_time,
        complete_time,
        build_token,
        run_timeout,
        build_result,
        final_text,
    }
}
*/

// remote_id is the remote from which we were notified. this is necessary so we know which remote
// to pull from to actually run the job.
pub const CREATE_JOBS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS jobs (id INTEGER PRIMARY KEY AUTOINCREMENT,
        source TEXT,
        created_time INTEGER,
        remote_id INTEGER,
        commit_id INTEGER,
        run_preferences TEXT);";

pub const CREATE_METRICS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS metrics (id INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id INTEGER,
        name TEXT,
        value TEXT,
        UNIQUE(run_id, name)
    );";

pub const CREATE_COMMITS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS commits (id INTEGER PRIMARY KEY AUTOINCREMENT, sha TEXT UNIQUE);";

pub const CREATE_COMMIT_NAMES_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS commit_names (id INTEGER PRIMARY KEY AUTOINCREMENT, commit_id INTEGER, name TEXT, name_state INTEGER);";

pub const CREATE_COMMIT_NAMES_INDEX: &'static str = "\
    CREATE INDEX IF NOT EXISTS 'names_by_commit' ON commit_names(commit_id);";

pub const CREATE_REPOS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS repos (id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo_name TEXT,
        default_run_preference TEXT);";

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

pub const CREATE_RUNS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS runs (id INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id INTEGER,
        artifacts_path TEXT,
        state INTEGER NOT NULL,
        host_preference INTEGER,
        host_id INTEGER,
        build_token TEXT,
        created_time INTEGER,
        started_time INTEGER,
        complete_time INTEGER,
        run_timeout INTEGER,
        build_result INTEGER,
        final_status TEXT);";

pub const CREATE_HOSTS_TABLE: &'static str = "\
    CREATE TABLE IF NOT EXISTS hosts (id INTEGER PRIMARY KEY AUTOINCREMENT,
        hostname TEXT,
        cpu_vendor_id TEXT,
        cpu_model_name TEXT,
        cpu_family TEXT,
        cpu_model TEXT,
        cpu_microcode TEXT,
        cpu_max_freq_khz INTEGER,
        cpu_cores INTEGER,
        mem_total TEXT,
        arch TEXT,
        family TEXT,
        os TEXT,
        UNIQUE(hostname, cpu_vendor_id, cpu_model_name, cpu_family, cpu_model, cpu_microcode, cpu_cores, mem_total, arch, family, os));";

pub const CREATE_REMOTES_INDEX: &'static str = "\
    CREATE INDEX IF NOT EXISTS 'repo_to_remote' ON remotes(repo_id);";

pub const CREATE_REPO_NAME_INDEX: &'static str = "\
    CREATE UNIQUE INDEX IF NOT EXISTS 'repo_names' ON repos(repo_name);";

pub const PENDING_RUNS: &'static str = "\
    select id, job_id, created_time, host_preference from runs where state=0 and (host_preference=?1 or host_preference is null) order by created_time desc;";

pub const JOBS_NEEDING_HOST_RUN: &'static str = "\
    select jobs.id, jobs.source, jobs.created_time, jobs.remote_id, jobs.commit_id, jobs.run_preferences from jobs \
    where jobs.run_preferences=\"all\" and jobs.created_time > ?1 \
    and not exists \
        (select 1 from runs r2 where r2.job_id = jobs.id and r2.host_id = ?2);";

pub const ACTIVE_RUNS: &'static str = "\
    select id,
        job_id,
        artifacts_path,
        state,
        host_id,
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
    select id, source, created_time, remote_id, commit_id, run_preferences from jobs where commit_id=?1;";

pub const ARTIFACT_BY_ID: &'static str = "\
    select * from artifacts where id=?1 and run_id=?2;";

pub const JOB_BY_ID: &'static str = "\
    select id, source, created_time, remote_id, commit_id, run_preferences from jobs where id=?1";

pub const NAMES_FOR_COMMIT: &'static str = "\
    select id, name, name_state from commit_names where commit_id=?1 order by id asc;";

pub const METRICS_FOR_RUN: &'static str = "\
    select * from metrics where run_id=?1 order by id asc;";

pub const METRICS_FOR_JOB: &'static str = "\
    select metrics.id, metrics.run_id, metrics.name, metrics.value from metrics \
    join runs on runs.id=metrics.run_id \
    where runs.job_id=?1 \
    order by metrics.run_id desc, metrics.id desc;";

pub const COMMIT_TO_ID: &'static str = "\
    select id from commits where sha=?1;";

pub const REMOTES_FOR_REPO: &'static str = "\
    select * from remotes where repo_id=?1;";

pub const ALL_REPOS: &'static str = "\
    select id, repo_name, default_run_preference from repos;";

pub const LAST_JOBS_FROM_REMOTE: &'static str = "\
    select id, source, created_time, remote_id, commit_id, run_preferences from jobs where remote_id=?1 order by created_time desc limit ?2;";

pub const LAST_RUN_FOR_JOB: &'static str = "\
    select id,
        job_id,
        artifacts_path,
        state,
        host_id,
        build_token,
        created_time,
        started_time,
        complete_time,
        run_timeout,
        build_result,
        final_status from runs where job_id=?1 order by started_time desc limit 1;";

// HELLO READER, I DO NOT UNDERSTAND SQL WELL ENOUGH, THIS MAY NOT WORK CORRECTLY!
// the intent of this query is to select one run per host that has run a job. which makes for an
// important question: which run should be representative of runs on a host? my somewhat arbitrary
// choice is "the most recent". this seems like a least-bad option: if a job has been re-run and is
// in progress, that in-progress one ought to be at least indicated. if the job has been re-run and
// that later run is complete, it is either substantially simpler to a previous run (since it would
// have the same commit, same goodfile, etc), or reflects the most recent intent of the CI runner
// environment (did a runner bug get fixed? flake of network, disk, something else being retried?).
// if it does *not* reflect the most recent intent, whatever is wrong can be fixed and retried
// again.
//
// so, do not consider the state of a job: the most recent one is the one to be shown. if data is
// present for a previous run and should be shown in a "imminently replaced" capacity, that's a
// problem for future me.
//
// ok, now with all that said, this once upon a time relied on a sqlite quirk allowing
// non-aggregate expressions that do not appear in the query's `group by`. sqlite even happened to do the thing i want in that circumstance - the query looked something like:
//  ```
//  select max(id), job_id, host_id, completed_time from runs where job_id=?1 group_by host_id;
//  ```
//  where `max(id)` collapses the group to one of the rows with max `id`. since `id` is distinct
//  this happens to collapse the group to one row, which as the desired fields for each subsequent
//  expression in the `select`.
//
//  this query is simply not allowed in many other database engines (exception MySQL). it was too
//  difficult to figure out why this works right (or if this is subtly wrong), so i've changed the
//  query this more portable form that is more obviously correct: only select aggregations or
//  non-aggregations that are part of `group by`. so only select `id` and `host_id`, subsequent
//  fields for each row have to be selected later on-demand.
pub const RUNS_FOR_JOB: &'static str = "\
    select max(id) from runs where job_id=?1 group by host_id;";

pub const RUN_TO_FIELDS: &'static str = "\
    select id,
        job_id,
        artifacts_path,
        state,
        host_id,
        build_token,
        created_time,
        started_time,
        complete_time,
        run_timeout,
        build_result,
        final_status from runs where id=?1;";

pub const SELECT_ALL_RUNS_WITH_JOB_INFO: &'static str = "\
    select jobs.id as job_id, runs.id as run_id, runs.state, runs.created_time, jobs.commit_id, jobs.run_preferences
    from jobs join runs on jobs.id=runs.job_id
    oder by runs.created_time asc;";
