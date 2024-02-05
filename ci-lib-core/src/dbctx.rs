use std::sync::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};
use std::path::Path;
use std::path::PathBuf;

use crate::sql;

use crate::sql::ArtifactRecord;
use crate::sql::CommitName;
use crate::sql::Run;
use crate::sql::TokenValidity;
use crate::sql::MetricRecord;
use crate::sql::PendingRun;
use crate::sql::Job;
use crate::sql::Remote;
use crate::sql::Repo;

const TOKEN_EXPIRY_MS: u64 = 1000 * 60 * 30;

pub struct DbCtx {
    pub config_path: PathBuf,
    // don't love this but.. for now...
    pub conn: Mutex<Connection>,
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
        conn.execute(sql::CREATE_ARTIFACTS_TABLE, params![]).unwrap();
        conn.execute(sql::CREATE_JOBS_TABLE, params![]).unwrap();
        conn.execute(sql::CREATE_METRICS_TABLE, params![]).unwrap();
        conn.execute(sql::CREATE_COMMITS_TABLE, params![]).unwrap();
        conn.execute(sql::CREATE_COMMIT_NAMES_TABLE, params![]).unwrap();
        conn.execute(sql::CREATE_COMMIT_NAMES_INDEX, params![]).unwrap();
        conn.execute(sql::CREATE_REPOS_TABLE, params![]).unwrap();
        conn.execute(sql::CREATE_REPO_NAME_INDEX, params![]).unwrap();
        conn.execute(sql::CREATE_REMOTES_TABLE, params![]).unwrap();
        conn.execute(sql::CREATE_REMOTES_INDEX, params![]).unwrap();
        conn.execute(sql::CREATE_RUNS_TABLE, params![]).unwrap();
        conn.execute(sql::CREATE_HOSTS_TABLE, params![]).unwrap();

        Ok(())
    }

    pub fn insert_metric(&self, run_id: u64, name: &str, value: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn
            .execute(
                "insert into metrics (run_id, name, value) values (?1, ?2, ?3) on conflict (run_id, name) do update set value=excluded.value",
                params![run_id, name, value]
            )
            .expect("can upsert");
        Ok(())
    }

    pub fn new_commit(&self, sha: &str) -> Result<u64, String> {
        let conn = self.conn.lock().unwrap();
        // TODO: ... if there's a unique constraint on the commits/sha column and the insert would
        // be a dupe, is that an error (eek expect()) or is that simply Ok(0)? either way this
        // should be made to not return the wrong id if there's a race on insert.
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

    pub async fn finalize_artifact(&self, artifact_id: u64) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn
            .execute(
                "update artifacts set completed_time=?1 where id=?2",
                params![crate::now_ms(), artifact_id]
            )
            .map(|_| ())
            .map_err(|e| {
                format!("{:?}", e)
            })
    }

    pub fn lookup_artifact(&self, run_id: u64, artifact_id: u64) -> Result<Option<ArtifactRecord>, String> {
        let conn = self.conn.lock().unwrap();
        conn
            .query_row(sql::ARTIFACT_BY_ID, [artifact_id, run_id], |row| {
                let (id, run_id, name, desc, created_time, completed_time) = row.try_into().unwrap();

                Ok(ArtifactRecord {
                    id, run_id, name, desc, created_time, completed_time
                })
            })
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn commit_sha(&self, commit_id: u64) -> Result<String, String> {
        self.conn.lock()
            .unwrap()
            .query_row(
                "select sha from commits where id=?1",
                [commit_id],
                |row| { row.get(0) }
            )
            .map_err(|e| e.to_string())
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

    pub fn run_for_token(&self, token: &str) -> Result<Option<(u64, Option<String>, TokenValidity)>, String> {
        self.conn.lock()
            .unwrap()
            .query_row(
                "select id, artifacts_path, started_time, run_timeout from runs where build_token=?1",
                [token],
                |row| {
                    let timeout: Option<u64> = row.get(3).unwrap();
                    let timeout = timeout.unwrap_or(TOKEN_EXPIRY_MS);

                    let now = crate::now_ms();

                    let time: Option<u64> = row.get(2).unwrap();
                    let validity = if let Some(time) = time {
                        if now > time + timeout {
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

    pub fn job_by_id(&self, id: u64) -> Result<Option<Job>, String> {
        self.conn.lock()
            .unwrap()
            .query_row(crate::sql::JOB_BY_ID, [id], |row| {
                let (id, source, created_time, remote_id, commit_id, run_preferences) = row.try_into().unwrap();

                Ok(Job {
                    id, source, created_time, remote_id, commit_id, run_preferences
                })
            })
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn remote_by_path_and_api(&self, api: &str, path: &str) -> Result<Option<Remote>, String> {
        self.conn.lock()
            .unwrap()
            .query_row("select id, repo_id, remote_path, remote_api, remote_url, remote_git_url, notifier_config_path from remotes where remote_api=?1 and remote_path=?2", [api, path], |row| {
                let (id, repo_id, remote_path, remote_api, remote_url, remote_git_url, notifier_config_path) = row.try_into().unwrap();

                Ok(Remote {
                    id, repo_id, remote_path, remote_api, remote_url, remote_git_url, notifier_config_path
                })
            })
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn remote_by_id(&self, id: u64) -> Result<Option<Remote>, String> {
        self.conn.lock()
            .unwrap()
            .query_row("select id, repo_id, remote_path, remote_api, remote_url, remote_git_url, notifier_config_path from remotes where id=?1", [id], |row| {
                let (id, repo_id, remote_path, remote_api, remote_url, remote_git_url, notifier_config_path) = row.try_into().unwrap();

                Ok(Remote {
                    id, repo_id, remote_path, remote_api, remote_url, remote_git_url, notifier_config_path
                })
            })
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
                params![repo_id, remote_path, remote_api, remote_url, remote_git_url, config_path]
            )
            .expect("can insert");

        Ok(conn.last_insert_rowid() as u64)
    }

    pub fn update_commit_name(&self, commit_id: u64, name: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();

        let rows_modified = conn.execute(
            "insert into commit_names (commit_id, name, name_state) values (?1, ?2, 0);",
            params![commit_id, name]
        ).unwrap();

        assert_eq!(1, rows_modified);

        Ok(())
    }

    pub fn nice_name_for_commit(&self, commit_id: u64) -> Result<Option<CommitName>, String> {
        let conn = self.conn.lock().unwrap();

        let mut names_query = conn.prepare(sql::NAMES_FOR_COMMIT).unwrap();
        let mut result = names_query.query([commit_id]).unwrap();
        let mut best_name: Option<CommitName> = None;

        while let Some(row) = result.next().unwrap() {
            let (_id, name, name_state): (u64, String, u8) = row.try_into().unwrap();
            if best_name.is_none() || best_name.as_ref().unwrap().stale() {
                best_name = Some(CommitName {
                    name,
                    state: name_state.try_into().unwrap()
                });
            }
        }

        Ok(best_name)
    }

    pub fn new_job(&self, remote_id: u64, sha: &str, pusher: Option<&str>, repo_default_run_pref: Option<String>) -> Result<(u64, u64), String> {
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
            "insert into jobs (remote_id, commit_id, created_time, source, run_preferences) values (?1, ?2, ?3, ?4, ?5);",
            params![remote_id, commit_id, created_time, pusher, repo_default_run_pref]
        ).unwrap();

        assert_eq!(1, rows_modified);

        let job_id = conn.last_insert_rowid() as u64;

        Ok((job_id, commit_id))
    }

    pub fn new_run(&self, job_id: u64, host_preference: Option<u32>) -> Result<PendingRun, String> {
        let created_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("now is before epoch")
            .as_millis() as u64;

        let conn = self.conn.lock().unwrap();

        let rows_modified = conn.execute(
            "insert into runs (job_id, state, created_time, host_preference) values (?1, ?2, ?3, ?4);",
            params![job_id, crate::sql::RunState::Pending as u64, created_time, host_preference]
        ).unwrap();

        assert_eq!(1, rows_modified);

        let run_id = conn.last_insert_rowid() as u64;

        Ok(PendingRun {
            id: run_id,
            job_id,
            create_time: created_time,
        })
    }

    pub fn reap_task(&self, task_id: u64) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();

        conn.execute(
            "update runs set final_status=\"lost signal\", state=4 where id=?1;",
            [task_id]
        ).unwrap();

        Ok(())
    }

    pub fn metrics_for_run(&self, run: u64) -> Result<Vec<MetricRecord>, String> {
        let conn = self.conn.lock().unwrap();

        let mut metrics_query = conn.prepare(sql::METRICS_FOR_RUN).unwrap();
        let mut result = metrics_query.query([run]).unwrap();
        let mut metrics = Vec::new();

        while let Some(row) = result.next().unwrap() {
            let (id, run_id, name, value): (u64, u64, String, String) = row.try_into().unwrap();
            metrics.push(MetricRecord { id, run_id, name, value });
        }

        Ok(metrics)
    }

    pub fn artifacts_for_run(&self, run: u64, limit: Option<u64>) -> Result<Vec<ArtifactRecord>, String> {
        let conn = self.conn.lock().unwrap();

        let mut artifacts_query = conn.prepare(sql::LAST_ARTIFACTS_FOR_RUN).unwrap();
        let mut result = artifacts_query.query([run, limit.unwrap_or(65535)]).unwrap();
        let mut artifacts = Vec::new();

        while let Some(row) = result.next().unwrap() {
            let (id, run_id, name, desc, created_time, completed_time): (u64, u64, String, String, u64, Option<u64>) = row.try_into().unwrap();
            artifacts.push(ArtifactRecord { id, run_id, name, desc, created_time, completed_time });
        }

        Ok(artifacts)
    }

    pub fn repo_by_id(&self, id: u64) -> Result<Option<Repo>, String> {
        self.conn.lock()
            .unwrap()
            .query_row("select id, repo_name, default_run_preference from repos where id=?1", [id], |row| {
                let (id, repo_name, default_run_preference) = row.try_into().unwrap();
                Ok(Repo {
                    id,
                    name: repo_name,
                    default_run_preference,
                })
            })
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn get_repos(&self) -> Result<Vec<Repo>, String> {
        let conn = self.conn.lock().unwrap();

        let mut repos_query = conn.prepare(sql::ALL_REPOS).unwrap();
        let mut repos = repos_query.query([]).unwrap();
        let mut result = Vec::new();

        while let Some(row) = repos.next().unwrap() {
            let (id, repo_name, default_run_preference) = row.try_into().unwrap();
            result.push(Repo {
                id,
                name: repo_name,
                default_run_preference,
            });
        }

        Ok(result)
    }

    pub fn last_job_from_remote(&self, id: u64) -> Result<Option<Job>, String> {
        self.recent_jobs_from_remote(id, 1)
            .map(|mut jobs| jobs.pop())
    }

    pub fn job_by_commit_id(&self, commit_id: u64) -> Result<Option<Job>, String> {
        let conn = self.conn.lock().unwrap();

        conn
            .query_row(sql::JOB_BY_COMMIT_ID, [commit_id], |row| {
                let (id, source, created_time, remote_id, commit_id, run_preferences) = row.try_into().unwrap();
                Ok(Job {
                    id,
                    remote_id,
                    commit_id,
                    created_time,
                    source,
                    run_preferences,
                })
            })
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn recent_jobs_from_remote(&self, id: u64, limit: u64) -> Result<Vec<Job>, String> {
        let conn = self.conn.lock().unwrap();

        let mut job_query = conn.prepare(sql::LAST_JOBS_FROM_REMOTE).unwrap();
        let mut result = job_query.query([id, limit]).unwrap();

        let mut jobs = Vec::new();

        while let Some(row) = result.next().unwrap() {
            let (id, source, created_time, remote_id, commit_id, run_preferences) = row.try_into().unwrap();
            jobs.push(Job {
                id,
                remote_id,
                commit_id,
                created_time,
                source,
                run_preferences,
            });
        }

        Ok(jobs)
    }

    pub fn get_active_runs(&self) -> Result<Vec<Run>, String> {
        let conn = self.conn.lock().unwrap();

        let mut started_query = conn.prepare(sql::ACTIVE_RUNS).unwrap();
        let mut runs = started_query.query([]).unwrap();
        let mut started = Vec::new();

        while let Some(row) = runs.next().unwrap() {
            started.push(Self::row2run(row));
        }

        Ok(started)
    }

    pub fn get_pending_runs(&self, host_id: Option<u32>) -> Result<Vec<PendingRun>, String> {
        let conn = self.conn.lock().unwrap();

        let mut pending_query = conn.prepare(sql::PENDING_RUNS).unwrap();
        let mut runs = pending_query.query([host_id]).unwrap();
        let mut pending = Vec::new();

        while let Some(row) = runs.next().unwrap() {
            let (id, job_id, create_time) = row.try_into().unwrap();
            let run = PendingRun {
                id,
                job_id,
                create_time,
            };
            pending.push(run);
        }

        Ok(pending)
    }

    pub fn jobs_needing_task_runs_for_host(&self, host_id: u64) -> Result<Vec<Job>, String> {
        // for jobs that this host has not run, we'll arbitrarily say that we won't generate new
        // runs for jobs more than a day old.
        //
        // we don't want to rebuild the entire history every time we see a new host by default; if
        // you really want to rebuild all of history on a new host, use `ci_ctl` to prepare the
        // runs.
        let cutoff = crate::now_ms() - 24 * 3600 * 1000;

        let conn = self.conn.lock().unwrap();

        let mut jobs_needing_task_runs = conn.prepare(sql::JOBS_NEEDING_HOST_RUN).unwrap();
        let mut job_rows = jobs_needing_task_runs.query([cutoff, host_id]).unwrap();
        let mut jobs = Vec::new();

        while let Some(row) = job_rows.next().unwrap() {
            let (id, source, created_time, remote_id, commit_id, run_preferences) = row.try_into().unwrap();

            jobs.push(Job {
                id, source, created_time, remote_id, commit_id, run_preferences,
            });
        }

        Ok(jobs)
    }


    pub fn remotes_by_repo(&self, repo_id: u64) -> Result<Vec<Remote>, String> {
        let mut remotes: Vec<Remote> = Vec::new();

        let conn = self.conn.lock().unwrap();
        let mut remotes_query = conn.prepare(crate::sql::REMOTES_FOR_REPO).unwrap();
        let mut remote_results = remotes_query.query([repo_id]).unwrap();

        while let Some(row) = remote_results.next().unwrap() {
            let (id, repo_id, remote_path, remote_api, remote_url, remote_git_url, notifier_config_path) = row.try_into().unwrap();
            remotes.push(Remote { id, repo_id, remote_path, remote_api, remote_url, remote_git_url, notifier_config_path });
        }

        Ok(remotes)
    }

    /// try to find a host close to `host_info`, but maybe not an exact match.
    ///
    /// specifically, we'll ignore microcode and family/os - enough that measurements ought to be
    /// comparable but maybe not perfectly so.
    pub fn find_id_like_host(&self, host_info: &crate::protocol::HostInfo) -> Result<Option<u32>, String> {
        self.conn.lock()
            .unwrap()
            .query_row(
                "select id from hosts where \
                    hostname=?1 and cpu_vendor_id=?2 and cpu_model_name=?3 and cpu_family=?4 and \
                    cpu_model=?5 and cpu_max_freq_khz=?6 and cpu_cores=?7 and mem_total=?8 and \
                    arch=?9);",
                params![
                    &host_info.hostname,
                    &host_info.cpu_info.vendor_id,
                    &host_info.cpu_info.model_name,
                    &host_info.cpu_info.family,
                    &host_info.cpu_info.model,
                    &host_info.cpu_info.max_freq,
                    &host_info.cpu_info.cores,
                    &host_info.memory_info.total,
                    &host_info.env_info.arch,
                ],
                |row| { row.get(0) }
            )
            .map_err(|e| e.to_string())
    }

    /// get an id for the host described by `host_info`. this may create a new record if no such
    /// host exists.
    pub fn id_for_host(&self, host_info: &crate::protocol::HostInfo) -> Result<u32, String> {
        let conn = self.conn.lock().unwrap();

        conn
            .execute(
                "insert or ignore into hosts \
                 (\
                     hostname, cpu_vendor_id, cpu_model_name, cpu_family, \
                     cpu_model, cpu_microcode, cpu_max_freq_khz, cpu_cores, \
                     mem_total, arch, family, os\
                 ) values (\
                     ?1, ?2, ?3, ?4, \
                     ?5, ?6, ?7, ?8, \
                     ?9, ?10, ?11, ?12 \
                 );",
                params![
                    &host_info.hostname,
                    &host_info.cpu_info.vendor_id,
                    &host_info.cpu_info.model_name,
                    &host_info.cpu_info.family,
                    &host_info.cpu_info.model,
                    &host_info.cpu_info.microcode,
                    &host_info.cpu_info.max_freq,
                    &host_info.cpu_info.cores,
                    &host_info.memory_info.total,
                    &host_info.env_info.arch,
                    &host_info.env_info.family,
                    &host_info.env_info.os,
                ]
            )
            .expect("can insert");

        conn
            .query_row(
                "select id from hosts where \
                    hostname=?1 and cpu_vendor_id=?2 and cpu_model_name=?3 and cpu_family=?4 and \
                    cpu_model=?5 and cpu_microcode=?6 and cpu_max_freq_khz=?7 and \
                    cpu_cores=?8 and mem_total=?9 and arch=?10 and family=?11 and os=?12;",
                params![
                    &host_info.hostname,
                    &host_info.cpu_info.vendor_id,
                    &host_info.cpu_info.model_name,
                    &host_info.cpu_info.family,
                    &host_info.cpu_info.model,
                    &host_info.cpu_info.microcode,
                    &host_info.cpu_info.max_freq,
                    &host_info.cpu_info.cores,
                    &host_info.memory_info.total,
                    &host_info.env_info.arch,
                    &host_info.env_info.family,
                    &host_info.env_info.os,
                ],
                |row| { row.get(0) }
            )
            .map_err(|e| e.to_string())
    }

    pub fn host_model_info(&self, host_id: u64) -> Result<(String, String, String, String, u64), String> {
        let conn = self.conn.lock().unwrap();
        conn
            .query_row("select hostname, cpu_vendor_id, cpu_family, cpu_model, cpu_max_freq_khz from hosts where id=?1;", [host_id], |row| {
                Ok((
                    row.get(0).unwrap(),
                    row.get(1).unwrap(),
                    row.get(2).unwrap(),
                    row.get(3).unwrap(),
                    row.get(4).unwrap(),
                ))
            })
            .map_err(|e| e.to_string())
    }

    pub fn runs_for_job_one_per_host(&self, job_id: u64) -> Result<Vec<Run>, String> {
        let conn = self.conn.lock().unwrap();
        let mut runs_query = conn.prepare(crate::sql::RUNS_FOR_JOB).unwrap();
        let mut runs_results = runs_query.query([job_id]).unwrap();

        let mut results = Vec::new();

        while let Some(row) = runs_results.next().unwrap() {
            results.push(Self::row2run(row));
        }

        Ok(results)
    }

    pub fn last_run_for_job(&self, job_id: u64) -> Result<Option<Run>, String> {
        let conn = self.conn.lock().unwrap();

        conn
            .query_row(sql::LAST_RUN_FOR_JOB, [job_id], |row| {
                Ok(Self::row2run(row))
            })
            .optional()
            .map_err(|e| e.to_string())
    }

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
}
