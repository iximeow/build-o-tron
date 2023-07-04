use clap::{Parser, Subcommand};

mod sql;
mod dbctx;
mod notifier;
mod io;
mod protocol;

use dbctx::DbCtx;
use notifier::NotifierConfig;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// path to a database to manage (defaults to "./state.db")
    db_path: Option<String>,

    /// path to where configs should be found (defaults to "./config")
    config_path: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// add _something_ to the db
    Add {
        #[command(subcommand)]
        what: AddItem,
    },

    /// make sure that the state looks reasonable.
    ///
    /// currently, ensure that all notifiers have a config, that config references an existing
    /// file, and that the referenced file is valid.
    Validate,

    /// do something with jobs
    Job {
        #[command(subcommand)]
        what: JobAction,
    },
}

#[derive(Subcommand)]
enum JobAction {
    List,
    Rerun {
        which: u32
    },
    RerunCommit {
        commit: String
    }
}

#[derive(Subcommand)]
enum AddItem {
    Repo {
        name: String,
        remote: Option<String>,
        remote_kind: Option<String>,
        config: Option<String>,
    },
    Remote {
        repo_name: String,
        remote: String,
        remote_kind: String,
        config: String,
    },
}

fn main() {
    let args = Args::parse();

    let db_path = args.db_path.unwrap_or_else(|| "./state.db".to_owned());
    let config_path = args.config_path.unwrap_or_else(|| "./config".to_owned());

    match args.command {
        Command::Job { what } => {
            match what {
                JobAction::List => {
                    let db = DbCtx::new(&config_path, &db_path);
                    let mut conn = db.conn.lock().unwrap();
                    let mut query = conn.prepare(crate::sql::SELECT_ALL_RUNS_WITH_JOB_INFO).unwrap();
                    let mut jobs = query.query([]).unwrap();
                    while let Some(row) = jobs.next().unwrap() {
                        let (job_id, run_id, state, created_time, commit_id, run_preferences): (u64, u64, u64, u64, u64, Option<String>) = row.try_into().unwrap();

                        eprint!("[+] {:04} ({:04}) | {: >8?} | {} | {}", run_id, job_id, state, created_time, commit_id);
                        if let Some(run_preferences) = run_preferences {
                            eprintln!(" | run preference: {}", run_preferences);
                        } else {
                            eprintln!("");
                        }
                    }
                    eprintln!("jobs");
                },
                JobAction::Rerun { which } => {
                    let db = DbCtx::new(&config_path, &db_path);
                    let task_id = db.new_run(which as u64, None).expect("db can be queried").id;
                    eprintln!("[+] rerunning job {} as task {}", which, task_id);
                }
                JobAction::RerunCommit { commit } => {
                    let db = DbCtx::new(&config_path, &db_path);
                    let job_id = db.job_for_commit(&commit).unwrap();
                    if let Some(job_id) = job_id {
                        let task_id = db.new_run(job_id, None).expect("db can be queried").id;
                        eprintln!("[+] rerunning job {} (commit {}) as task {}", job_id, commit, task_id);
                    } else {
                        eprintln!("[-] no job for commit {}", commit);
                    }
                }
            }
        },
        Command::Add { what } => {
            match what {
                AddItem::Repo { name, remote, remote_kind, config } => {
                    let remote_config = match (remote, remote_kind, config) {
                        (Some(remote), Some(remote_kind), Some(config_path)) => {
                            // do something
                            if remote_kind != "github" {
                                eprintln!("unknown remote kind: {}", remote);
                                return;
                            }
                            Some((remote, remote_kind, config_path))
                        },
                        (None, None, None) => {
                            None
                        },
                        _ => {
                            eprintln!("when specifying a remote, `remote`, `remote_kind`, and `config_path` must either all be provided together or not at all");
                            return;
                        }
                    };

                    let db = DbCtx::new(&config_path, &db_path);
                    let repo_id = match db.new_repo(&name) {
                        Ok(repo_id) => repo_id,
                        Err(e) => {
                            if e.contains("UNIQUE constraint failed") {
                                eprintln!("[!] repo '{}' already exists", name);
                                return;
                            } else {
                                eprintln!("[!] failed to create repo entry: {}", e);
                                return;
                            }
                        }
                    };
                    println!("[+] new repo created: '{}' id {}", &name, repo_id);
                    if let Some((remote, remote_kind, config_path)) = remote_config {
                        let full_config_file_path = format!("{}/{}", &db.config_path.display(), config_path);
                        let config = match remote_kind.as_ref() {
                            "github" => {
                                assert!(NotifierConfig::github_from_file(&full_config_file_path).is_ok());
                            }
                            "github-email" => {
                                assert!(NotifierConfig::email_from_file(&full_config_file_path).is_ok());
                            }
                            other => {
                                panic!("[-] notifiers for '{}' remotes are not supported", other);
                            }
                        };
                        db.new_remote(repo_id, remote.as_str(), remote_kind.as_str(), config_path.as_str()).unwrap();
                        println!("[+] new remote created: repo '{}', {} remote at {}", &name, remote_kind, remote);
                        match remote_kind.as_str() {
                            "github" => {
                                println!("[!] now go make sure your github repo has a webhook set for `https://ci.butactuallyin.space/{}` to receive at least the `push` event.", remote.as_str());
                                println!("      the secret sent with calls to this webhook should be the same preshared secret as the CI server is configured to know.");
                            }
                            _ => { }
                        }
                    }
                },
                AddItem::Remote { repo_name, remote, remote_kind, config } => {
                    let db = DbCtx::new(&config_path, &db_path);
                    let repo_id = match db.repo_id_by_name(&repo_name) {
                        Ok(Some(id)) => id,
                        Ok(None) => {
                            eprintln!("[-] repo '{}' does not exist", repo_name);
                            return;
                        },
                        Err(e) => {
                            eprintln!("[!] couldn't look up repo '{}': {:?}", repo_name, e);
                            return;
                        }
                    };
                    let config_file = format!("{}/{}", config_path, config);
                    match remote_kind.as_ref() {
                        "github" => {
                            NotifierConfig::github_from_file(&config_file).unwrap();
                        }
                        "github-email" => {
                            NotifierConfig::email_from_file(&config_file).unwrap();
                        }
                        other => {
                            panic!("notifiers for '{}' remotes are not supported", other);
                        }
                    };
                    db.new_remote(repo_id, remote.as_str(), remote_kind.as_str(), config.as_str()).unwrap();
                    println!("[+] new remote created: repo '{}', {} remote at {}", &repo_name, remote_kind, remote);
                },
            }
        },
        Command::Validate => {
            println!("ok");
        }
    }
}
