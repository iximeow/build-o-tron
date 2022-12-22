use std::sync::Mutex;
use rusqlite::{Connection, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::notifier::{RemoteNotifier, NotifierConfig};
use crate::sql;

pub struct DbCtx {
    pub config_path: String,
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
}

impl DbCtx {
    pub fn new(config_path: &str, db_path: &str) -> Self {
        DbCtx {
            config_path: config_path.to_owned(),
            conn: Mutex::new(Connection::open(db_path).unwrap())
        }
    }

    pub fn create_tables(&self) -> Result<(), ()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(sql::CREATE_JOBS_TABLE, ()).unwrap();
        conn.execute(sql::CREATE_COMMITS_TABLE, ()).unwrap();
        conn.execute(sql::CREATE_REPOS_TABLE, ()).unwrap();
        conn.execute(sql::CREATE_REPO_NAME_INDEX, ()).unwrap();
        conn.execute(sql::CREATE_REMOTES_TABLE, ()).unwrap();
        conn.execute(sql::CREATE_REMOTES_INDEX, ()).unwrap();

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

    pub fn new_job(&self, remote_id: u64, sha: &str) -> Result<u64, String> {
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
            (crate::sql::JobState::Pending as u64, remote_id, commit_id, created_time)
        ).unwrap();

        assert_eq!(1, rows_modified);

        Ok(conn.last_insert_rowid() as u64)
    }

    pub fn get_pending_jobs(&self) -> Result<Vec<PendingJob>, String> {
        let conn = self.conn.lock().unwrap();

        let mut pending_query = conn.prepare(sql::PENDING_JOBS).unwrap();
        let mut jobs = pending_query.query([]).unwrap();
        let mut pending = Vec::new();

        while let Some(row) = jobs.next().unwrap() {
            let (id, artifacts, state, run_host, remote_id, commit_id, created_time) = row.try_into().unwrap();
            let state: u8 = state;
            pending.push(PendingJob {
                id, artifacts,
                state: state.try_into().unwrap(),
                run_host, remote_id, commit_id, created_time
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
                    let notifier = RemoteNotifier {
                        remote_path: remote.remote_path,
                        notifier: NotifierConfig::github_from_file(&format!("{}/{}", self.config_path, remote.notifier_config_path))
                            .expect("can load notifier config")
                    };
                    notifiers.push(notifier);
                },
                "email" => {
                    let notifier = RemoteNotifier {
                        remote_path: remote.remote_path,
                        notifier: NotifierConfig::email_from_file(&format!("{}/{}", self.config_path, remote.notifier_config_path))
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

