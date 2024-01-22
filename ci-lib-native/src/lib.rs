pub mod io;
pub mod dbctx_ext;
pub mod notifier;

use axum::http::StatusCode;

pub struct GithubApi<'a> {
    pub token: &'a str,
    pub webhook_token: &'a str,
}

impl GithubApi<'_> {
    pub async fn post_status(&self, remote_path: &str, sha: &str, state: &str, description: &str, target_url: &str) -> Result<reqwest::Response, reqwest::Error> {
        let status_info = serde_json::json!({
            "state": state,
            "description": description,
            "target_url": target_url,
            "context": "actuallyinspace runner",
        });

        let client = reqwest::Client::new();
        let req = client.post(&format!("https://api.github.com/repos/{}/statuses/{}", remote_path, sha))
            .body(serde_json::to_string(&status_info).expect("can stringify json"))
            .header("content-type", "application/json")
            .header("user-agent", "iximeow")
            .header("authorization", format!("Bearer {}", &self.token))
            .header("accept", "application/vnd.github+json");
        eprintln!("sending {:?}", req);
        eprintln!("  body: {}", serde_json::to_string(&status_info).expect("can stringify json"));
        req.send().await
    }

    pub async fn has_ci_webhook(&self, remote_path: &str) -> Result<bool, String> {
        let url = format!("https://api.github.com/repos/{}/hooks", remote_path);

        let client = reqwest::Client::new();
        let req = client.get(&url)
            .header("user-agent", "iximeow")
            .header("authorization", format!("Bearer {}", &self.token))
            .header("accept", "application/vnd.github+json");
        let resp = req.send().await
            .map_err(|e| format!("failed to list webhooks on {}: {:?}", remote_path, e))?;

        if resp.status() != StatusCode::OK {
            let msg = format!("got non-OK response trying to look up webhooks on {}: {:?}", url, resp);
            return Err(msg);
        }

        let bytes = resp.bytes().await
            .map_err(|e| format!("failed to fetch response body for webhooks on {}: {:?}", remote_path, e))?;

        let hooks: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("could not parse response body: {:?}", e))?;

        for v in hooks.as_array().ok_or_else(|| format!("response json was not an array"))? {
            let o = v.as_object().ok_or_else(|| format!("response array did not contain objects"))?;
            let conf = o.get("config")
                .ok_or_else(|| format!("hook objects do not have config"))?
                .as_object().ok_or_else(|| format!("hook objects have config but it is not an object"))?;
            let url = conf.get("url")
                .ok_or_else(|| format!("config object does not have url"))?
                .as_str().ok_or_else(|| format!("config url has url but it not a string"))?;
            if url.starts_with("https://ci.butactuallyin.space") {
                return Ok(true);
            }
        }

        return Ok(false);
    }

    pub async fn create_ci_webhook(&self, remote_path: &str) -> Result<(), String> {
        let ci_server = "ci.butactuallyin.space";
        let webhook_url = format!("https://ci.butactuallyin.space/{remote_path}");

        let webhook_config = serde_json::json!({
            "name": "web",
            "active": true,
            "events": ["push"],
            "config": {
                "url": webhook_url,
                "content_type": "json",
                "secret": self.webhook_token,
                "insecure_ssl": "0"
            }
        });

        let client = reqwest::Client::new();
        let req = client.post(&format!("https://api.github.com/repos/{remote_path}/hooks"))
            .body(serde_json::to_string(&webhook_config).expect("can stringify json"))
            .header("content-type", "application/json")
            .header("user-agent", "iximeow")
            .header("authorization", format!("Bearer {}", &self.token))
            .header("accept", "application/vnd.github+json");

        eprintln!("[.] creating webhook on github.com/{remote_path} to send push events to {ci_server}");
        let resp = req.send().await;

        match resp {
            Ok(resp) if resp.status() == StatusCode::OK || resp.status() == StatusCode::CREATED => {
                eprintln!("[+] created webhook successfully");
                Ok(())
            }
            Ok(resp) => {
                Err(format!("got non-OK response trying to look up webhooks on github.com/{remote_path}: {resp:?}"))
            }
            Err(e) => {
                Err(format!("failed to send request to create push webhook: {e:?}"))
            }
        }

    }
}
