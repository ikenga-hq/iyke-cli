//! Thin HTTP client over the Iyke server. All requests go to
//! `http://127.0.0.1:<port>` with a `Authorization: Bearer <token>`
//! header. Errors are normalized into a single `ApiError` so the main
//! command dispatcher can render them uniformly.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::control::ControlFile;

/// A non-2xx answer from the bridge. Its `Display` is the historical
/// `"<path> returned HTTP <code>: <body>"` string (so `cmd::missing_route`'s
/// text check still holds); callers that need the structured body — the
/// seat routes' `{code, message, details?}` (WP-70) — downcast to it.
#[derive(Debug)]
pub struct HttpStatusError {
    pub path: String,
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} returned HTTP {}: {}",
            self.path, self.status, self.body
        )
    }
}

impl std::error::Error for HttpStatusError {}

pub struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(cf: &ControlFile) -> Self {
        Self {
            base: format!("http://127.0.0.1:{}", cf.port),
            token: cf.token.clone(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(2))
                .timeout(std::time::Duration::from_secs(5))
                .build(),
        }
    }

    pub fn get_state(&self) -> Result<Value> {
        let resp = self
            .agent
            .get(&format!("{}/iyke/state", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .call();
        normalize_response(resp, "/iyke/state")
    }

    pub fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.post_with_timeout(path, body, std::time::Duration::from_secs(65))
    }

    pub fn post_with_timeout(
        &self,
        path: &str,
        body: Value,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        let resp = self
            .agent
            .post(&format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(timeout)
            .send_json(body);
        normalize_response(resp, path)
    }

    pub fn get_with_query(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        self.get_with_query_timeout(path, params, std::time::Duration::from_secs(5))
    }

    pub fn get_with_query_timeout(
        &self,
        path: &str,
        params: &[(&str, String)],
        timeout: std::time::Duration,
    ) -> Result<Value> {
        let mut req = self
            .agent
            .get(&format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(timeout);
        for (k, v) in params {
            req = req.query(k, v);
        }
        normalize_response(req.call(), path)
    }
}

fn normalize_response(resp: Result<ureq::Response, ureq::Error>, path: &str) -> Result<Value> {
    match resp {
        Ok(r) => {
            // Empty 200 from write endpoints — we still want valid JSON
            // back to callers so they can pretty-print or pipe into jq.
            let body = r.into_string().unwrap_or_default();
            if body.is_empty() {
                return Ok(serde_json::json!({"ok": true}));
            }
            serde_json::from_str(&body)
                .with_context(|| format!("parse response from {path}: {body}"))
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            Err(anyhow::Error::new(HttpStatusError {
                path: path.to_string(),
                status: code,
                body,
            }))
        }
        Err(ureq::Error::Transport(t)) => {
            // WP-62 review (C5): a request timeout (a live FE round trip like
            // `/iyke/keys/resolve` outrunning its deadline) means the app *is*
            // running, just slow to answer — a different diagnosis than
            // "not running at all", so don't collapse both into one message.
            if t.to_string().to_lowercase().contains("timed out") {
                Err(anyhow!(
                    "{path} timed out waiting for a response. The PA desktop app is running, but \
                     its webview hasn't answered yet (unfocused window, heavy load, or a wedged \
                     renderer) — retry, or check the app isn't frozen. Underlying error: {t}"
                ))
            } else {
                Err(anyhow!(
                    "could not reach iyke server at {path}: {t}. Is the PA desktop app running?"
                ))
            }
        }
    }
}
