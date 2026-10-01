//! Files users send: download from Slack, then transcribe (audio) or upload to DocsGPT.

use std::time::Duration;

use docsgpt_bot::docsgpt::{Error as DocsError, Upload};
use docsgpt_bot::util::safe_filename;
use docsgpt_bot::{AgentConfig, Routed, Scope};
use serde_json::Value;

use crate::bot::SlackBot;

/// At most this many files per message.
const MAX_FILES: usize = 5;

/// What the files of one message became.
#[derive(Debug, Default)]
pub struct Prepared {
    /// DocsGPT attachment ids.
    pub attachments: Vec<String>,
    /// A transcript to use as the question when the message had no text.
    pub transcript: Option<String>,
    /// Problems to tell the user about.
    pub problems: Vec<String>,
}

/// The agent a message will go to, so files are uploaded with its key.
pub async fn agent_for(bot: &SlackBot, text: &str, state_scope: &Scope) -> AgentConfig {
    let active = bot
        .core
        .storage
        .chat_state(state_scope)
        .await
        .ok()
        .and_then(|s| s.active_agent);
    match bot.core.agents.route(text, active.as_deref()) {
        Routed::Agent { agent, .. } => agent.clone(),
        Routed::UnknownTag { .. } => bot.core.agents.default_agent().clone(),
    }
}

fn is_audio(mime: &str, name: &str) -> bool {
    mime.starts_with("audio/")
        || mime.starts_with("video/")
        || [".m4a", ".mp3", ".ogg", ".wav", ".webm", ".mp4"]
            .iter()
            .any(|e| name.to_ascii_lowercase().ends_with(e))
}

/// Download and process the `files` of a Slack message.
pub async fn prepare(bot: &SlackBot, agent: &AgentConfig, files: &[Value], question_empty: bool) -> Prepared {
    let mut out = Prepared::default();
    let max = (bot.cfg.max_file_mb as usize) * 1024 * 1024;
    for f in files.iter().take(MAX_FILES) {
        let name = safe_filename(f["name"].as_str().or(f["title"].as_str()).unwrap_or("file"), "file");
        if f["mode"].as_str() == Some("hidden_by_limit") || f["file_access"].as_str() == Some("check_file_info") {
            out.problems.push(format!("I can't open {name}."));
            continue;
        }
        if f["size"].as_u64().is_some_and(|s| s as usize > max) {
            out.problems
                .push(format!("{name} is larger than {} MB.", bot.cfg.max_file_mb));
            continue;
        }
        let Some(url) = f["url_private_download"].as_str().or(f["url_private"].as_str()) else {
            continue;
        };
        let (bytes, mime) = match bot.api.download(url, max).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, file = %name, "download from Slack failed");
                out.problems.push(format!("I couldn't download {name}."));
                continue;
            }
        };
        let mime = f["mimetype"].as_str().map(str::to_string).or(mime);
        let upload = Upload {
            filename: name.clone(),
            bytes,
            mime: mime.clone(),
        };
        if question_empty && out.transcript.is_none() && is_audio(mime.as_deref().unwrap_or(""), &name) {
            match bot.core.client.stt(&agent.api_key, &upload).await {
                Ok(text) => {
                    out.transcript = Some(text);
                    continue;
                }
                Err(DocsError::FeatureDisabled { .. }) => {
                    tracing::debug!("speech-to-text disabled; sending the clip as an attachment")
                }
                Err(e) => tracing::warn!(error = %e, "speech-to-text failed; sending the clip as an attachment"),
            }
        }
        match bot.core.client.upload_attachment(&agent.api_key, &upload).await {
            Ok(att) => {
                if let Some(task) = &att.task_id
                    && let Err(e) = bot.core.client.wait_for_task(task, Duration::from_secs(90)).await
                {
                    tracing::warn!(error = %e, file = %name, "attachment processing failed");
                    out.problems.push(format!("DocsGPT couldn't read {name}."));
                    continue;
                }
                out.attachments.push(att.id);
            }
            Err(e) => {
                tracing::warn!(error = %e, file = %name, "attachment upload failed");
                out.problems.push(format!("I couldn't attach {name}."));
            }
        }
    }
    out
}
