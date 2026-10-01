//! How a turn looks in Slack: an agent session, a streamed message, and a
//! final message with sources and 👍/👎.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use docsgpt_bot::docsgpt::Download;
use docsgpt_bot::util::{safe_filename, truncate_chars};
use docsgpt_bot::{CancelGuard, Final, Progress, Surface, Turn};
use serde_json::{Value, json};

use crate::bot::SlackBot;
use crate::render;
use crate::slack::api::SlackError;

/// Where to answer one incoming message.
pub struct SlackSurface {
    pub bot: Arc<SlackBot>,
    pub team: String,
    pub channel: String,
    /// The thread the answer goes in (the question's own ts when it was top level).
    pub thread_ts: String,
    /// Who asked (for ephemeral notices and stream recipients).
    pub user: String,
    /// The question's ts (for the 👀 reaction).
    pub source_ts: String,
    pub is_dm: bool,
}

/// State of one answer in Slack.
pub struct SlackDraft {
    _cancel: CancelGuard,
    /// The streaming message, once started.
    stream_ts: Option<String>,
    /// Bytes of `Progress::raw` already delivered.
    sent: usize,
    /// Characters of text in the current streamed message.
    msg_chars: usize,
    /// Tool step shown as a task: (id, title).
    task: Option<(String, String)>,
    tasks: u32,
    /// False once streaming isn't possible for this turn: deliver at the end.
    streaming: bool,
    reacted: bool,
}

fn platform(e: SlackError) -> docsgpt_bot::Error {
    docsgpt_bot::Error::platform(e)
}

/// Codes after which the stream is gone (the user pressed Stop, or it ended).
fn stream_ended(e: &SlackError) -> bool {
    matches!(e.code(), Some("stopped_by_user" | "message_not_in_streaming_state"))
}

impl SlackSurface {
    pub fn cancel_key(channel: &str, thread_ts: &str) -> String {
        format!("{channel}:{thread_ts}")
    }

    fn session_enabled(&self) -> bool {
        self.is_dm && self.bot.cfg.agent_view
    }

    async fn set_session(&self, status: &str, title: Option<&str>) {
        let mut body = json!({"channel_id": self.channel, "thread_ts": self.thread_ts, "status": status});
        if let Some(t) = title.filter(|t| !t.is_empty()) {
            body["title"] = truncate_chars(t, 200).into();
        }
        if let Err(e) = self.bot.api.call("agents.sessions.setStatus", &body).await {
            tracing::debug!(error = %e, "agents.sessions.setStatus failed");
        }
    }

    async fn stream_call(&self, method: &str, mut body: Value) -> Result<Value, SlackError> {
        body["channel"] = self.channel.clone().into();
        self.bot.api.call(method, &body).await
    }

    /// Stop the stream; if Slack rejects the blocks, stop it without them.
    async fn stop_stream(&self, ts: &str, text: &str, blocks: &[Value]) -> Result<(), SlackError> {
        let mut body = json!({"ts": ts});
        if !text.is_empty() {
            // The stream started with `chunks`; Slack rejects switching to `markdown_text`.
            body["chunks"] = json!([{"type": "markdown_text", "text": text}]);
        }
        if !blocks.is_empty() {
            body["blocks"] = blocks.into();
        }
        match self.stream_call("chat.stopStream", body.clone()).await {
            Err(e) if !blocks.is_empty() && matches!(e.code(), Some("invalid_blocks" | "invalid_arguments")) => {
                tracing::warn!(error = %e, "stopStream rejected the blocks; stopping without them");
                body.as_object_mut().map(|o| o.remove("blocks"));
                self.stream_call("chat.stopStream", body).await.map(drop)
            }
            other => other.map(drop),
        }
    }

    /// Close a full streamed message; the rest of the answer is posted by `finish`.
    async fn close_full(&self, d: &mut SlackDraft) {
        if let Some(ts) = d.stream_ts.take()
            && let Err(e) = self.stop_stream(&ts, "", &[]).await
        {
            tracing::warn!(error = %e, "could not close a full stream");
        }
        d.streaming = false;
    }

    /// Post `text` as one or more messages; the last one carries `extra` blocks. Returns its ts.
    async fn post_answer(&self, text: &str, extra: Vec<Value>) -> Result<String, SlackError> {
        let pieces = render::markdown_pieces(text);
        let count = pieces.len().max(1);
        let mut last_ts = String::new();
        for (i, piece) in pieces
            .iter()
            .map(String::as_str)
            .chain(pieces.is_empty().then_some(""))
            .enumerate()
        {
            let mut blocks = Vec::new();
            if !piece.is_empty() {
                blocks.push(render::markdown_block(piece));
            }
            if i + 1 == count {
                blocks.extend(extra.iter().cloned());
            }
            if blocks.is_empty() {
                continue;
            }
            let v = self
                .bot
                .api
                .call(
                    "chat.postMessage",
                    &json!({
                        "channel": self.channel,
                        "thread_ts": self.thread_ts,
                        "text": render::fallback_text(if piece.is_empty() { text } else { piece }),
                        "blocks": blocks,
                        "unfurl_links": false,
                    }),
                )
                .await?;
            last_ts = v["ts"].as_str().unwrap_or("").to_string();
        }
        Ok(last_ts)
    }
}

#[async_trait]
impl Surface for SlackSurface {
    type Draft = SlackDraft;

    async fn begin(&self, turn: &Turn) -> docsgpt_bot::Result<SlackDraft> {
        let guard = self
            .bot
            .cancels
            .insert(Self::cancel_key(&self.channel, &self.thread_ts), turn.cancel.clone());
        if self.session_enabled() {
            self.set_session("processing", Some(&turn.question)).await;
        }
        let mut reacted = false;
        if !self.is_dm && self.bot.cfg.reactions {
            let r = self
                .bot
                .api
                .call(
                    "reactions.add",
                    &json!({"channel": self.channel, "timestamp": self.source_ts, "name": "eyes"}),
                )
                .await;
            reacted = r.is_ok();
            if let Err(e) = r {
                tracing::debug!(error = %e, "reaction failed");
            }
        }
        Ok(SlackDraft {
            _cancel: guard,
            stream_ts: None,
            sent: 0,
            msg_chars: 0,
            task: None,
            tasks: 0,
            streaming: self.bot.cfg.streaming,
            reacted,
        })
    }

    async fn update(&self, turn: &Turn, d: &mut SlackDraft, p: Progress<'_>) -> docsgpt_bot::Result<()> {
        if !d.streaming || p.raw.len() < d.sent {
            return Ok(());
        }
        let mut chunks = Vec::new();
        let mut next_task = d.task.clone();
        let mut tasks = d.tasks;
        if next_task.as_ref().map(|(_, t)| t.as_str()) != p.status {
            if let Some((id, title)) = next_task.take() {
                chunks.push(json!({"type": "task_update", "id": id, "title": title, "status": "complete"}));
            }
            if let Some(title) = p.status {
                tasks += 1;
                let title = truncate_chars(title, 256);
                let id = format!("task-{tasks}");
                chunks.push(json!({"type": "task_update", "id": id, "title": title, "status": "in_progress"}));
                next_task = Some((id, title));
            }
        }
        if d.stream_ts.is_some() && d.msg_chars >= render::STREAM_MESSAGE_LIMIT {
            // This message is full: close it; the rest is posted at the end.
            self.close_full(d).await;
            return Ok(());
        }
        let room = render::STREAM_MESSAGE_LIMIT - d.msg_chars;
        let delta: String = p.raw[d.sent..].chars().take(room).collect();
        if !delta.is_empty() {
            chunks.push(json!({"type": "markdown_text", "text": delta}));
        }
        if chunks.is_empty() {
            return Ok(());
        }

        let result = match &d.stream_ts {
            None => {
                if !self.bot.limits.start_stream() {
                    tracing::info!(bot = %self.bot.cfg.name, "stream budget used up; answering in one message");
                    d.streaming = false;
                    return Ok(());
                }
                let mut body = json!({"thread_ts": self.thread_ts, "chunks": chunks, "task_display_mode": "timeline"});
                if !self.is_dm {
                    body["recipient_user_id"] = self.user.clone().into();
                    body["recipient_team_id"] = self.team.clone().into();
                }
                self.stream_call("chat.startStream", body).await.map(|v| {
                    d.stream_ts = v["ts"].as_str().map(str::to_string);
                })
            }
            Some(ts) => {
                if !self.bot.limits.append() {
                    return Ok(()); // the text goes out with the next update
                }
                self.stream_call("chat.appendStream", json!({"ts": ts, "chunks": chunks}))
                    .await
                    .map(drop)
            }
        };
        match result {
            Ok(()) => {
                d.sent += delta.len();
                d.msg_chars += delta.chars().count();
                d.task = next_task;
                d.tasks = tasks;
                Ok(())
            }
            Err(SlackError::RateLimited { .. }) => Ok(()),
            Err(e) if stream_ended(&e) => {
                tracing::info!(error = %e, "stream ended by Slack; stopping the turn");
                turn.cancel.cancel();
                Ok(())
            }
            Err(e) if e.code() == Some("msg_too_long") => {
                self.close_full(d).await;
                Ok(())
            }
            Err(e) if d.stream_ts.is_none() => {
                tracing::warn!(error = %e, "could not start a stream; answering in one message");
                d.streaming = false;
                Ok(())
            }
            Err(e) => Err(platform(e)),
        }
    }

    async fn finish(&self, _turn: &Turn, d: SlackDraft, f: &Final) -> docsgpt_bot::Result<Option<String>> {
        if d.reacted
            && let Err(e) = self
                .bot
                .api
                .call(
                    "reactions.remove",
                    &json!({"channel": self.channel, "timestamp": self.source_ts, "name": "eyes"}),
                )
                .await
        {
            tracing::debug!(error = %e, "removing reaction failed");
        }
        let mut blocks = render::image_blocks(&f.images);
        blocks.extend(render::sources_block(&f.sources));
        if f.can_rate() {
            blocks.push(render::feedback_block());
        }

        // What hasn't been shown yet: the rest of the answer and the note, or
        // the whole display text when nothing was streamed.
        let text = if d.sent == 0 {
            f.display_text()
        } else {
            let rest = f.raw.get(d.sent..).unwrap_or("");
            match f.note() {
                Some(n) => format!("{rest}\n\n{n}"),
                None => rest.to_string(),
            }
        };
        let ts = match &d.stream_ts {
            Some(ts) if text.chars().count() + d.msg_chars <= render::STREAM_MESSAGE_LIMIT => {
                match self.stop_stream(ts, &text, &blocks).await {
                    Ok(()) => ts.clone(),
                    Err(e) if stream_ended(&e) => {
                        // Stopped from Slack; the answer so far stays as it is.
                        tracing::info!(error = %e, "stream already stopped");
                        return Ok(None);
                    }
                    Err(e) if e.code() == Some("msg_too_long") => {
                        let _ = self.stop_stream(ts, "", &[]).await;
                        self.post_answer(text.trim(), blocks).await.map_err(platform)?
                    }
                    Err(e) => return Err(platform(e)),
                }
            }
            Some(ts) => {
                // Too long for this message: close it and post the rest.
                let _ = self.stop_stream(ts, "", &[]).await;
                self.post_answer(text.trim(), blocks).await.map_err(platform)?
            }
            None => {
                let ts = self.post_answer(text.trim(), blocks).await.map_err(platform)?;
                if self.session_enabled() {
                    self.set_session("active", None).await;
                }
                ts
            }
        };
        Ok((f.can_rate() && !ts.is_empty()).then(|| format!("{}:{ts}", self.channel)))
    }

    async fn send_file(&self, _turn: &Turn, file: Download) -> docsgpt_bot::Result<()> {
        let name = safe_filename(&file.filename, "file");
        self.bot
            .api
            .upload_file(&self.channel, Some(&self.thread_ts), &name, file.bytes)
            .await
            .map_err(platform)?;
        Ok(())
    }

    async fn notice(&self, text: &str) -> docsgpt_bot::Result<()> {
        let r = if self.is_dm {
            self.bot
                .api
                .call(
                    "chat.postMessage",
                    &json!({"channel": self.channel, "thread_ts": self.thread_ts, "text": text}),
                )
                .await
        } else {
            self.bot
                .api
                .call(
                    "chat.postEphemeral",
                    &json!({"channel": self.channel, "user": self.user, "thread_ts": self.thread_ts, "text": text}),
                )
                .await
        };
        r.map(drop).map_err(platform)
    }

    fn update_interval(&self) -> Duration {
        Duration::from_millis(1200)
    }
}
