//! A small Slack Web API client: JSON (or form) POSTs with a bearer token.

use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;

/// A failed Slack call.
#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    /// Slack answered `{"ok": false, "error": …}`.
    #[error("Slack {method}: {error}")]
    Api { method: String, error: String },
    /// HTTP 429; retry after this long.
    #[error("Slack {method}: rate limited (retry after {retry_after:?})")]
    RateLimited { method: String, retry_after: Duration },
    /// Non-2xx status or an unreadable body.
    #[error("Slack {method}: HTTP {status}")]
    Http { method: String, status: u16 },
    #[error("Slack {method}: {source}")]
    Transport {
        method: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("Slack {method}: {message}")]
    Other { method: String, message: String },
}

impl SlackError {
    /// The `error` code of an `ok: false` reply.
    pub fn code(&self) -> Option<&str> {
        match self {
            SlackError::Api { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Web API client for one token.
#[derive(Clone)]
pub struct SlackApi {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl SlackApi {
    pub fn new(http: reqwest::Client, base: &str, token: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// The same client with another token (e.g. the app-level token).
    pub fn with_token(&self, token: &str) -> Self {
        Self {
            token: token.to_string(),
            ..self.clone()
        }
    }

    /// POST a JSON body to `method`.
    pub async fn call(&self, method: &str, body: &Value) -> Result<Value, SlackError> {
        let req = self
            .http
            .post(format!("{}/{method}", self.base))
            .bearer_auth(&self.token)
            .json(body);
        self.send(method, req).await
    }

    /// POST form fields to `method` (for methods that don't take JSON).
    pub async fn call_form(&self, method: &str, fields: &[(&str, String)]) -> Result<Value, SlackError> {
        let req = self
            .http
            .post(format!("{}/{method}", self.base))
            .bearer_auth(&self.token)
            .form(fields);
        self.send(method, req).await
    }

    async fn send(&self, method: &str, req: reqwest::RequestBuilder) -> Result<Value, SlackError> {
        let resp = req
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|source| SlackError::Transport {
                method: method.into(),
                source,
            })?;
        let status = resp.status();
        if status.as_u16() == 429 {
            let secs = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            return Err(SlackError::RateLimited {
                method: method.into(),
                retry_after: Duration::from_secs(secs),
            });
        }
        if !status.is_success() {
            return Err(SlackError::Http {
                method: method.into(),
                status: status.as_u16(),
            });
        }
        let v: Value = resp.json().await.map_err(|source| SlackError::Transport {
            method: method.into(),
            source,
        })?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            if let Some(w) = v.get("warning").and_then(Value::as_str) {
                tracing::debug!(method, warning = w, "Slack warning");
            }
            Ok(v)
        } else {
            let error = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
                .to_string();
            Err(SlackError::Api {
                method: method.into(),
                error,
            })
        }
    }

    /// Download a private file (`url_private_download`) with the bot token.
    pub async fn download(&self, url: &str, max_bytes: usize) -> Result<(Bytes, Option<String>), SlackError> {
        let method = "file download";
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .map_err(|source| SlackError::Transport {
                method: method.into(),
                source,
            })?;
        if !resp.status().is_success() {
            return Err(SlackError::Http {
                method: method.into(),
                status: resp.status().as_u16(),
            });
        }
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());
        // Without files:read Slack serves its HTML sign-in page instead of the file.
        if mime.as_deref() == Some("text/html") {
            return Err(SlackError::Other {
                method: method.into(),
                message: "Slack returned a sign-in page; is the files:read scope granted?".into(),
            });
        }
        let mut body = resp.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|source| SlackError::Transport {
                method: method.into(),
                source,
            })?;
            if buf.len() + chunk.len() > max_bytes {
                return Err(SlackError::Other {
                    method: method.into(),
                    message: format!("file is larger than {max_bytes} bytes"),
                });
            }
            buf.extend_from_slice(&chunk);
        }
        Ok((Bytes::from(buf), mime))
    }

    /// Upload a file into a channel or thread (files.getUploadURLExternal →
    /// POST the bytes → files.completeUploadExternal). Returns the file id.
    pub async fn upload_file(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        filename: &str,
        bytes: Bytes,
    ) -> Result<String, SlackError> {
        let v = self
            .call_form(
                "files.getUploadURLExternal",
                &[("filename", filename.to_string()), ("length", bytes.len().to_string())],
            )
            .await?;
        let (Some(url), Some(id)) = (v["upload_url"].as_str(), v["file_id"].as_str()) else {
            return Err(SlackError::Other {
                method: "files.getUploadURLExternal".into(),
                message: "no upload_url".into(),
            });
        };
        let method = "file upload";
        let resp = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .map_err(|source| SlackError::Transport {
                method: method.into(),
                source,
            })?;
        if !resp.status().is_success() {
            return Err(SlackError::Http {
                method: method.into(),
                status: resp.status().as_u16(),
            });
        }
        let mut body = serde_json::json!({
            "files": [{"id": id, "title": filename}],
            "channel_id": channel,
        });
        if let Some(t) = thread_ts {
            body["thread_ts"] = t.into();
        }
        self.call("files.completeUploadExternal", &body).await?;
        Ok(id.to_string())
    }
}
