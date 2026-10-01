//! HTTP mode: Slack POSTs events, interactions and commands to
//! `/slack/<bot>/events` (one Request URL for all three works too).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use crate::bot::SlackBot;
use crate::events::{self, Inbound};
use crate::slack::socket::spawn;
use crate::slack::verify;

/// The bots served over HTTP, by name.
pub type Bots = Arc<HashMap<String, Arc<SlackBot>>>;

pub fn router(bots: Bots) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/slack/{bot}/events", post(inbound))
        .route("/slack/{bot}/interactions", post(inbound))
        .route("/slack/{bot}/commands", post(inbound))
        .with_state(bots)
}

async fn healthz(State(bots): State<Bots>) -> String {
    format!("ok bots={}", bots.len())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn inbound(State(bots): State<Bots>, Path(name): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(bot) = bots.get(&name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let header = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    let secret = bot.cfg.signing_secret.as_deref().unwrap_or("");
    if !verify::verify(
        secret,
        header("x-slack-request-timestamp"),
        header("x-slack-signature"),
        &body,
        now(),
    ) {
        tracing::warn!(bot = %name, "request with a bad or missing Slack signature");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let is_form = header("content-type").starts_with("application/x-www-form-urlencoded");
    if !is_form {
        // Events API: JSON.
        let Ok(v) = serde_json::from_slice::<Value>(&body) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        return match v["type"].as_str() {
            Some("url_verification") => axum::Json(json!({"challenge": v["challenge"]})).into_response(),
            Some("event_callback") => {
                if !header("x-slack-retry-num").is_empty() {
                    tracing::debug!(
                        retry = header("x-slack-retry-num"),
                        reason = header("x-slack-retry-reason"),
                        "Slack retry"
                    );
                }
                spawn(bot, Inbound::Event(v));
                StatusCode::OK.into_response()
            }
            _ => StatusCode::OK.into_response(),
        };
    }
    let fields: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    if let Some(payload) = fields.get("payload") {
        // Interactivity: payload=<json>.
        match serde_json::from_str::<Value>(payload) {
            Ok(p) => spawn(bot, Inbound::Interaction(p)),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        }
        return StatusCode::OK.into_response();
    }
    if fields.contains_key("command") {
        let v = Value::Object(fields.into_iter().map(|(k, v)| (k, Value::String(v))).collect());
        return axum::Json(events::on_command(bot, &v).await).into_response();
    }
    StatusCode::BAD_REQUEST.into_response()
}
