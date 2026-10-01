//! Socket Mode: an outbound WebSocket per app, reconnected as Slack asks.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::bot::SlackBot;
use crate::events::{self, Inbound};

/// Slack pings every ~30 s; treat a minute and a half of silence as a dead socket.
const IDLE: Duration = Duration::from_secs(90);

/// Keep a Socket Mode connection open until shutdown.
pub async fn run(bot: Arc<SlackBot>, app_token: String) {
    let api = bot.api.with_token(&app_token);
    let token = bot.shutdown.token().clone();
    let mut backoff = Duration::from_secs(1);
    while !token.is_cancelled() {
        match connect_once(&bot, &api).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                tracing::warn!(bot = %bot.cfg.name, error = %format!("{e:#}"), retry_in = ?backoff, "Socket Mode connection failed");
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
    tracing::info!(bot = %bot.cfg.name, "Socket Mode stopped");
}

async fn connect_once(bot: &Arc<SlackBot>, api: &crate::slack::api::SlackApi) -> Result<()> {
    let v = api
        .call("apps.connections.open", &json!({}))
        .await
        .context("apps.connections.open")?;
    let url = v["url"].as_str().context("no url from apps.connections.open")?;
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .context("connecting to Slack")?;
    let (mut tx, mut rx) = ws.split();
    let token = bot.shutdown.token().clone();
    loop {
        let msg = tokio::select! {
            _ = token.cancelled() => {
                let _ = tx.send(Message::Close(None)).await;
                return Ok(());
            }
            m = tokio::time::timeout(IDLE, rx.next()) => m,
        };
        let msg = match msg {
            Err(_) => bail!("no traffic for {IDLE:?}"),
            Ok(None) => return Ok(()),
            Ok(Some(Err(e))) => return Err(e).context("reading from Slack"),
            Ok(Some(Ok(m))) => m,
        };
        match msg {
            Message::Text(text) => {
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    tracing::warn!("unparseable Socket Mode frame");
                    continue;
                };
                match v["type"].as_str().unwrap_or("") {
                    "hello" => tracing::info!(bot = %bot.cfg.name, "Socket Mode connected"),
                    "disconnect" => {
                        tracing::info!(bot = %bot.cfg.name, reason = v["reason"].as_str().unwrap_or(""), "Slack asked to reconnect");
                        return Ok(());
                    }
                    kind => {
                        if let Some(ack) = envelope(bot, kind, &v).await {
                            tx.send(Message::Text(ack.to_string().into()))
                                .await
                                .context("sending ack")?;
                        }
                    }
                }
            }
            Message::Ping(p) => tx.send(Message::Pong(p)).await.context("pong")?,
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
}

/// Handle one envelope and return the ack. Events and interactions are acked
/// at once and answered in the background; commands reply in the ack.
pub async fn envelope(bot: &Arc<SlackBot>, kind: &str, v: &Value) -> Option<Value> {
    let id = v["envelope_id"].as_str()?;
    let payload = v["payload"].clone();
    let mut ack = json!({"envelope_id": id});
    match kind {
        "events_api" => spawn(bot, Inbound::Event(payload)),
        "interactive" => spawn(bot, Inbound::Interaction(payload)),
        "slash_commands" => ack["payload"] = events::on_command(bot, &payload).await,
        other => tracing::debug!(kind = other, "unhandled Socket Mode envelope"),
    }
    Some(ack)
}

/// Run a delivery in the background, tracked for graceful shutdown.
pub fn spawn(bot: &Arc<SlackBot>, inbound: Inbound) {
    if !events::is_new(bot, &inbound) {
        tracing::debug!("duplicate delivery ignored");
        return;
    }
    let b = bot.clone();
    bot.shutdown.spawn(events::dispatch(b, inbound));
}
