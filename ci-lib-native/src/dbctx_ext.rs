use crate::io::ArtifactDescriptor;
use crate::notifier::{RemoteNotifier, NotifierConfig};
use tokio::fs::{File, OpenOptions};

use ci_lib_core::dbctx::DbCtx;

pub fn notifiers_by_repo(ctx: &DbCtx, repo_id: u64) -> Result<Vec<RemoteNotifier>, String> {
    let remotes = ctx.remotes_by_repo(repo_id)?;

    let mut notifiers: Vec<RemoteNotifier> = Vec::new();

    for remote in remotes.into_iter() {
        match remote.remote_api.as_str() {
            "github" => {
                let mut notifier_path = ctx.config_path.clone();
                notifier_path.push(&remote.notifier_config_path);

                let notifier = RemoteNotifier {
                    remote_path: remote.remote_path,
                    notifier: NotifierConfig::github_from_file(&notifier_path)
                        .expect("can load notifier config")
                };
                notifiers.push(notifier);
            },
            "email" => {
                let mut notifier_path = ctx.config_path.clone();
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

pub async fn reserve_artifact(ctx: &DbCtx, run_id: u64, name: &str, desc: &str) -> Result<ArtifactDescriptor, String> {
    let artifact_id = {
        let created_time = ci_lib_core::now_ms();
        let conn = ctx.conn.lock().unwrap();
        conn
            .execute(
                "insert into artifacts (run_id, name, desc, created_time) values (?1, ?2, ?3, ?4)",
                (run_id, name, desc, created_time)
            )
            .map_err(|e| {
                format!("{:?}", e)
            })?;

        conn.last_insert_rowid() as u64
    };

    ArtifactDescriptor::new(run_id, artifact_id).await
}
