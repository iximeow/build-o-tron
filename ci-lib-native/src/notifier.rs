use serde::{Deserialize, Serialize};
use std::sync::Arc;
use axum::http::StatusCode;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::Message;
use lettre::transport::smtp::extension::ClientId;
use lettre::transport::smtp::client::{SmtpConnection, TlsParametersBuilder};
use std::time::Duration;
use std::path::Path;

use ci_lib_core::dbctx::DbCtx;

pub struct RemoteNotifier {
    pub remote_path: String,
    pub notifier: NotifierConfig,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum NotifierConfig {
    GitHub {
        token: String,
        webhook_token: String,
    },
    Email {
        username: String,
        password: String,
        mailserver: String,
        from: String,
        to: String,
    }
}

impl NotifierConfig {
    pub fn github_from_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)
            .map_err(|e| format!("can't read notifier config at {}: {:?}", path.display(), e))?;
        let config = serde_json::from_slice(&bytes)
            .map_err(|e| format!("can't deserialize notifier config at {}: {:?}", path.display(), e))?;

        if matches!(config, NotifierConfig::GitHub { .. }) {
            Ok(config)
        } else {
            Err(format!("config at {} doesn't look like a github config (but was otherwise valid?)", path.display()))
        }
    }

    pub fn email_from_file<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)
            .map_err(|e| format!("can't read notifier config at {}: {:?}", path.display(), e))?;
        let config = serde_json::from_slice(&bytes)
            .map_err(|e| format!("can't deserialize notifier config at {}: {:?}", path.display(), e))?;

        if matches!(config, NotifierConfig::Email { .. }) {
            Ok(config)
        } else {
            Err(format!("config at {} doesn't look like an email config (but was otherwise valid?)", path.display()))
        }
    }
}

impl RemoteNotifier {
    pub async fn tell_pending_job(&self, ctx: &Arc<DbCtx>, repo_id: u64, sha: &str, job_id: u64) -> Result<(), String> {
        self.tell_job_status(
            ctx,
            repo_id, sha, job_id,
            "pending", "build is queued", &format!("https://{}/{}/{}", "ci.butactuallyin.space", &self.remote_path, sha)
        ).await
    }

    pub async fn tell_complete_job(&self, ctx: &Arc<DbCtx>, repo_id: u64, sha: &str, job_id: u64, desc: Result<String, String>) -> Result<(), String> {
        match desc {
            Ok(status) => {
                self.tell_job_status(
                    ctx,
                    repo_id, sha, job_id,
                    "success", &status, &format!("https://{}/{}/{}", "ci.butactuallyin.space", &self.remote_path, sha)
                ).await
            },
            Err(status) => {
                self.tell_job_status(
                    ctx,
                    repo_id, sha, job_id,
                    "failure", &status, &format!("https://{}/{}/{}", "ci.butactuallyin.space", &self.remote_path, sha)
                ).await
            }
        }
    }

    pub async fn tell_job_status(&self, _ctx: &Arc<DbCtx>, _repo_id: u64, sha: &str, _job_id: u64, state: &str, desc: &str, target_url: &str) -> Result<(), String> {
        match &self.notifier {
            NotifierConfig::GitHub { token, webhook_token } => {
                // TODO: should pool (probably in ctx?) to have an upper bound in concurrent
                // connections.
                let res = (crate::GithubApi { token, webhook_token }).post_status(&self.remote_path, sha, state, desc, target_url).await;

                match res {
                    Ok(res) => {
                        if res.status() == StatusCode::OK || res.status() == StatusCode::CREATED {
                            Ok(())
                        } else {
                            Err(format!("bad response: {}, response data: {:?}", res.status().as_u16(), res))
                        }
                    }
                    Err(e) => {
                        Err(format!("failure sending request: {:?}", e))
                    }
                }
            }
            NotifierConfig::Email { username, password, mailserver, from, to } => {
                eprintln!("[.] emailing {} for job {} via {}", state, &self.remote_path, mailserver);

                let subject = format!("{}: job for {}", state, &self.remote_path);

                let body = format!("{}", subject);

                // TODO: when ci.butactuallyin.space has valid certs again, ... fix this.
                let tls = TlsParametersBuilder::new(mailserver.to_string())
                    .dangerous_accept_invalid_certs(true)
                    .build()
                    .unwrap();

                let mut mailer = SmtpConnection::connect(
                    mailserver,
                    Some(Duration::from_millis(5000)),
                    &ClientId::Domain("ci.butactuallyin.space".to_string()),
                    None,
                    None,
                ).unwrap();

                mailer.starttls(
                    &tls,
                    &ClientId::Domain("ci.butactuallyin.space".to_string()),
                ).unwrap();

                let resp = mailer.auth(
                    &[Mechanism::Plain, Mechanism::Login],
                    &Credentials::new(username.to_owned(), password.to_owned())
                ).unwrap();
                assert!(resp.is_positive());

                let email = Message::builder()
                    .from(from.parse().unwrap())
                    .to(to.parse().unwrap())
                    .subject(&subject)
                    .body(body)
                    .unwrap();

                match mailer.send(email.envelope(), &email.formatted()) {
                    Ok(_) => {
                        eprintln!("[+] notified {}@{}", username, mailserver);
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("[-] could not send email: {:?}", e);
                        Err(e.to_string())
                    }
                }
            }
        }
    }
}
