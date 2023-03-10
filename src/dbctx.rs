use std::sync::Mutex;
use futures_util::StreamExt;
use rusqlite::{Connection, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::path::Path;
use std::path::PathBuf;

use crate::io::ArtifactDescriptor;
use crate::notifier::{RemoteNotifier, NotifierConfig};
use crate::sql;

const TOKEN_EXPIRY_MS: u64 = 1000 * 60 * 30;

pub struct DbCtx {
    pub config_path: PathBuf,
    // don't love this but.. for now...
    pub conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct PendingJob {
    pub id: u64,
    pub artifacts: Option<String>,
    pub state: sql::JobState,
    pub run_host: Option<String>,
    pub remote_id: u64,
    pub commit_id: u64,
    pub created_time: u64,
    pub source: Option<String>,
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
    pub job_id: u64,
    pub name: String,
    pub value: String
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub id: u64,
    pub job_id: u64,
    pub name: String,
    pub desc: String
}

impl DbCtx {
    pub fn new<P: AsRef<Path>>(config_path: P, db_path: P) -> Self {
        DbCtx {
            config_path: config_path.as_ref().to_owned(),
            conn: Mutex::new(Connection::open(db_path).unwrap())
        }
    }

    pub fn create_tables(&self) -> Result<(), ()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(sql::CREATE_ARTIFACTS_TABLE, ()).unwrap();
        conn.execute(sql::CREATE_JOBS_TABLE, ()).unwrap();
        conn.execute(sql::CREATE_METRICS_TABLE, ()).unwrap();
        conn.execute(sql::CREATE_COMMITS_TABLE, ()).unwrap();
        conn.execute(sql::CREATE_REPOS_TABLE, ()).unwrap();
        conn.execute(sql::CREATE_REPO_NAME_INDEX, ()).unwrap();
        conn.execute(sql::CREATE_REMOTES_TABLE, ()).unwrap();
        conn.execute(sql::CREATE_REMOTES_INDEX, ()).unwrap();

        Ok(())
    }

    pub fn insert_metric(&self, job_id: u64, name: &str, value: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn
            .execute(
                "insert into metrics (job_id, name, value) values (?1, ?2, ?3) on conflict (job_id, name) do update set value=excluded.value",
                (job_id, name, value)
            )
            .expect("can upsert");
        Ok(())
    }

    pub fn new_commit(&self, sha: &str) -> Result<u64, String> {
        let conn = self.conn.lock().unwrap();
        conn
            .execute(
                "insert into commits (sha) values (?1)",
                [sha.clone()]
            )
            .expect("can insert");

        Ok(conn.last_insert_rowid() as u64)
    }

    pub fn new_repo(&self, name: &str) -> Result<u64, String> {
        let conn = self.conn.lock().unwrap();
        conn
            .execute(
                "insert into repos (repo_name) values (?1)",
                [name.clone()]
            )
            .map_err(|e| {
                format!("{:?}", e)
            })?;

        Ok(conn.last_insert_rowid() as u64)
    }

    pub async fn reserve_artifact(&self, job_id: u64, name: &str, desc: &str) -> Result<ArtifactDescriptor, String> {
        let artifact_id = {
            let conn = self.conn.lock().unwrap();
            conn
                .execute(
                    "insert into artifacts (job_id, name, desc) values (?1, ?2, ?3)",
                    (job_id, name, desc)
                )
                .map_err(|e| {
                    format!("{:?}", e)
                })?;

            conn.last_insert_rowid() as u64
        };

        ArtifactDescriptor::new(job_id, artifact_id).await
    }

    pub fn job_for_commit(&self, sha: &str) -> Result<Option<u64>, String> {
        self.conn.lock()
            .unwrap()
            .query_row(
                "select id from commits where sha=?1",
                [sha],
                |row| { row.get(0) }
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn job_for_token(&self, token: &str) -> Result<Option<(u64, Option<String>, TokenValidity)>, String> {
        self.conn.lock()
            .unwrap()
            .query_row(
                "select id, artifacts_path, started_time, job_timeout from jobs where build_token=?1",
                [token],
                |row| {
                    let timeout: Option<u64> = row.get(3).unwrap();
                    let timeout = timeout.unwrap_or(TOKEN_EXPIRY_MS);

                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("now is before epoch")
                        .as_millis();

                    let time: Option<u64> = row.get(2).unwrap();
                    let validity = if let Some(time) = time {
                        if now > time as u128 + timeout as u128 {
                            TokenValidity::Expired
                        } else {
                            TokenValidity::Valid
                        }
                    } else {
                        TokenValidity::Invalid
                    };
                    Ok((row.get(0).unwrap(), row.get(1).unwrap(), validity))
                }
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn repo_id_by_remote(&self, remote_id: u64) -> Result<Option<u64>, String> {
        self.conn.lock()
            .unwrap()
            .query_row("select repo_id from remotes where id=?1", [remote_id], |row| row.get(0))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn repo_id_by_name(&self, repo_name: &str) -> Result<Option<u64>, String> {
        self.conn.lock()
            .unwrap()
            .query_row("select id from repos where repo_name=?1", [repo_name], |row| row.get(0))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn new_remote(&self, repo_id: u64, remote: &str, remote_kind: &str, config_path: &str) -> Result<u64, String> {
        let (remote_path, remote_api, remote_url, remote_git_url) = match remote_kind {
            "github" => {
                (remote.to_owned(), remote_kind.to_owned(), format!("https://www.github.com/{}", remote), format!("https://www.github.com/{}.git", remote))
            },
            "github-email" => {
                (remote.to_owned(), "email".to_owned(), format!("https://www.github.com/{}", remote), format!("http://www.github.com/{}.git", remote))
            },
            other => {
                panic!("unsupported remote kind: {}", other);
            }
        };

        let conn = self.conn.lock().unwrap();
        conn
            .execute(
                "insert into remotes (repo_id, remote_path, remote_api, remote_url, remote_git_url, notifier_config_path) values (?1, ?2, ?3, ?4, ?5, ?6);",
                (repo_id, remote_path, remote_api, remote_url, remote_git_url, config_path)
            )
            .expect("can insert");

        Ok(conn.last_insert_rowid() as u64)
    }

    pub fn new_job(&self, remote_id: u64, sha: &str, pusher: Option<&str>) -> Result<u64, String> {
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
            "insert into jobs (state, remote_id, commit_id, created_time, source) values (?1, ?2, ?3, ?4, ?5);",
            (crate::sql::JobState::Pending as u64, remote_id, commit_id, created_time, pusher)
        ).unwrap();

        assert_eq!(1, rows_modified);

        Ok(conn.last_insert_rowid() as u64)
    }

    pub fn metrics_for_job(&self, job: u64) -> Result<Vec<MetricRecord>, String> {
        let conn = self.conn.lock().unwrap();

        let mut metrics_query = conn.prepare(sql::METRICS_FOR_JOB).unwrap();
        let mut result = metrics_query.query([job]).unwrap();
        let mut metrics = Vec::new();

        while let Some(row) = result.next().unwrap() {
            let (id, job_id, name, value): (u64, u64, String, String) = row.try_into().unwrap();
            metrics.push(MetricRecord { id, job_id, name, value });
        }

        Ok(metrics)
    }

    pub fn artifacts_for_job(&self, job: u64) -> Result<Vec<ArtifactRecord>, String> {
        let conn = self.conn.lock().unwrap();

        let mut artifacts_query = conn.prepare(sql::LAST_ARTIFACTS_FOR_JOB).unwrap();
        let mut result = artifacts_query.query([job]).unwrap();
        let mut artifacts = Vec::new();

        while let Some(row) = result.next().unwrap() {
            let (id, job_id, name, desc): (u64, u64, String, String) = row.try_into().unwrap();
            artifacts.push(ArtifactRecord { id, job_id, name, desc });
        }

        Ok(artifacts)
    }

    pub fn get_pending_jobs(&self) -> Result<Vec<PendingJob>, String> {
        let conn = self.conn.lock().unwrap();

        let mut pending_query = conn.prepare(sql::PENDING_JOBS).unwrap();
        let mut jobs = pending_query.query([]).unwrap();
        let mut pending = Vec::new();

        while let Some(row) = jobs.next().unwrap() {
            let (id, artifacts, state, run_host, remote_id, commit_id, created_time, source) = row.try_into().unwrap();
            let state: u8 = state;
            pending.push(PendingJob {
                id, artifacts,
                state: state.try_into().unwrap(),
                run_host, remote_id, commit_id, created_time,
                source,
            });
        }

        Ok(pending)
    }

    pub fn notifiers_by_repo(&self, repo_id: u64) -> Result<Vec<RemoteNotifier>, String> {
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
        let mut remotes_query = conn.prepare(crate::sql::REMOTES_FOR_REPO).unwrap();
        let mut remote_results = remotes_query.query([repo_id]).unwrap();

        while let Some(row) = remote_results.next().unwrap() {
            let (id, repo_id, remote_path, remote_api, remote_url, remote_git_url, notifier_config_path) = row.try_into().unwrap();
            let _: String = remote_url;
            let _: String = remote_git_url;
            remotes.push(Remote { id, repo_id, remote_path, remote_api, notifier_config_path });
        }

        let mut notifiers: Vec<RemoteNotifier> = Vec::new();

        for remote in remotes.into_iter() {
            match remote.remote_api.as_str() {
                "github" => {
                    let mut notifier_path = self.config_path.clone();
                    notifier_path.push(&remote.notifier_config_path);

                    let notifier = RemoteNotifier {
                        remote_path: remote.remote_path,
                        notifier: NotifierConfig::github_from_file(&notifier_path)
                            .expect("can load notifier config")
                    };
                    notifiers.push(notifier);
                },
                "email" => {
                    let mut notifier_path = self.config_path.clone();
                    notifier_path.push(&remote.notifier_config_path);

                    let notifier = RemoteNotifier {
                        remote_path: remote.remote_path,
                        notifier: NotifierConfig::email_from_file(&notifier_path)
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
}

