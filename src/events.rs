//! What the bot does with each Slack event, interaction and command. Both
//! transports (Socket Mode and HTTP) feed the same functions with raw JSON.

use std::sync::Arc;

use docsgpt_bot::docsgpt::Feedback;
use docsgpt_bot::{Ask, Scope, StatePatch, run_turn, submit_feedback};
use serde_json::{Value, json};

use crate::bot::SlackBot;
use crate::config::Channels;
use crate::files;
use crate::render::{FEEDBACK_ACTION, clean_incoming, mentions, mentions_someone_else};
use crate::surface::SlackSurface;

/// Something Slack delivered.
#[derive(Debug, Clone)]
pub enum Inbound {
    /// An `event_callback` envelope.
    Event(Value),
    /// An interaction payload (`block_actions`, …).
    Interaction(Value),
}

/// True when this delivery hasn't been handled yet (dedupes retries).
pub fn is_new(bot: &SlackBot, inbound: &Inbound) -> bool {
    match inbound {
        Inbound::Event(env) => env["event_id"].as_str().is_none_or(|id| bot.seen.first(id)),
        Inbound::Interaction(_) => true,
    }
}

/// Handle one delivery. Runs the whole turn; transports spawn it.
pub async fn dispatch(bot: Arc<SlackBot>, inbound: Inbound) {
    match inbound {
        Inbound::Event(env) => on_event(bot, env).await,
        Inbound::Interaction(p) => on_interaction(bot, p).await,
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}

async fn on_event(bot: Arc<SlackBot>, env: Value) {
    let ev = &env["event"];
    let team = [str_of(&env, "team_id"), str_of(ev, "team"), bot.team_id.as_str()]
        .into_iter()
        .find(|t| !t.is_empty())
        .unwrap_or("")
        .to_string();
    match str_of(ev, "type") {
        "message" => on_message(bot, team, ev).await,
        "app_mention" => on_mention(bot, team, ev).await,
        "app_home_opened" if str_of(ev, "tab") == "messages" => suggest_prompts(&bot, str_of(ev, "channel")).await,
        "agent_session_stopped" => {
            let key = SlackSurface::cancel_key(str_of(ev, "channel"), str_of(ev, "thread_ts"));
            let stopped = bot.cancels.cancel(&key);
            tracing::info!(bot = %bot.cfg.name, %key, stopped, "stop requested");
        }
        other => tracing::debug!(event = other, "ignored event"),
    }
}

/// A user message to answer.
struct Question {
    team: String,
    channel: String,
    thread_ts: String,
    ts: String,
    user: String,
    text: String,
    files: Vec<Value>,
    is_dm: bool,
}

fn from_bot(bot: &SlackBot, ev: &Value) -> bool {
    ev.get("bot_id").is_some_and(|b| !b.is_null()) || str_of(ev, "user") == bot.user_id || str_of(ev, "user").is_empty()
}

fn question(bot: &SlackBot, team: String, ev: &Value, is_dm: bool) -> Question {
    let ts = str_of(ev, "ts").to_string();
    let thread_ts = ev["thread_ts"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| ts.clone());
    Question {
        team,
        channel: str_of(ev, "channel").to_string(),
        thread_ts,
        ts,
        user: str_of(ev, "user").to_string(),
        text: clean_incoming(str_of(ev, "text"), &bot.user_id),
        files: ev["files"].as_array().cloned().unwrap_or_default(),
        is_dm,
    }
}

async fn on_message(bot: Arc<SlackBot>, team: String, ev: &Value) {
    if !matches!(ev["subtype"].as_str(), None | Some("file_share" | "thread_broadcast")) || from_bot(&bot, ev) {
        return;
    }
    let text = str_of(ev, "text");
    match str_of(ev, "channel_type") {
        "im" => {
            let q = question(&bot, team, ev, true);
            answer(bot, q).await
        }
        "channel" | "group" | "mpim" => {
            // A reply in one of the bot's threads, without a mention (mentions
            // arrive as app_mention).
            let in_thread = ev["thread_ts"].as_str().is_some_and(|t| t != str_of(ev, "ts"));
            if bot.cfg.channels != Channels::Thread
                || !in_thread
                || mentions(text, &bot.user_id)
                || mentions_someone_else(text, &bot.user_id)
                || text.trim_start().starts_with('!')
                || !bot.cfg.channel_allowed(str_of(ev, "channel"))
            {
                return;
            }
            let q = question(&bot, team, ev, false);
            let scope = conversation_scope(&bot, &q);
            let mut ours = false;
            for agent in bot.core.agents.iter() {
                if matches!(bot.core.storage.conversation(&scope, &agent.name).await, Ok(Some(_))) {
                    ours = true;
                    break;
                }
            }
            if ours {
                answer(bot, q).await
            }
        }
        _ => {}
    }
}

async fn on_mention(bot: Arc<SlackBot>, team: String, ev: &Value) {
    if bot.cfg.channels == Channels::Off || from_bot(&bot, ev) || !bot.cfg.channel_allowed(str_of(ev, "channel")) {
        return;
    }
    let q = question(&bot, team, ev, false);
    answer(bot, q).await
}

fn space(bot: &SlackBot, team: &str, channel: &str) -> (String, String) {
    (bot.cfg.name.clone(), format!("{team}:{channel}"))
}

fn conversation_scope(bot: &SlackBot, q: &Question) -> Scope {
    let (b, s) = space(bot, &q.team, &q.channel);
    Scope::new(b, s, q.thread_ts.clone())
}

fn state_scope(bot: &SlackBot, team: &str, channel: &str) -> Scope {
    let (b, s) = space(bot, team, channel);
    Scope::new(b, s, "0")
}

async fn answer(bot: Arc<SlackBot>, q: Question) {
    let surface = SlackSurface {
        bot: bot.clone(),
        team: q.team.clone(),
        channel: q.channel.clone(),
        thread_ts: q.thread_ts.clone(),
        user: q.user.clone(),
        source_ts: q.ts.clone(),
        is_dm: q.is_dm,
    };
    let state = state_scope(&bot, &q.team, &q.channel);
    let mut text = q.text.clone();
    let mut attachments = Vec::new();
    if bot.cfg.files && !q.files.is_empty() {
        let agent = files::agent_for(&bot, &text, &state).await;
        let prepared = files::prepare(&bot, &agent, &q.files, text.is_empty()).await;
        for p in &prepared.problems {
            let _ = docsgpt_bot::Surface::notice(&surface, p).await;
        }
        if text.is_empty()
            && let Some(t) = prepared.transcript
        {
            text = t;
        }
        attachments = prepared.attachments;
    }
    if text.is_empty() && attachments.is_empty() {
        return;
    }
    let mut ask = Ask::new(conversation_scope(&bot, &q), text).attachments(attachments);
    ask.state_scope = Some(state);
    match run_turn(&bot.core, &surface, ask).await {
        Ok(report) => tracing::debug!(?report, "turn done"),
        Err(e) => tracing::warn!(bot = %bot.cfg.name, error = %e, "turn failed"),
    }
}

async fn suggest_prompts(bot: &SlackBot, channel: &str) {
    let prompts = bot.cfg.prompts();
    if prompts.is_empty() || !bot.cfg.agent_view || channel.is_empty() {
        return;
    }
    let body = json!({
        "channel_id": channel,
        "title": "Try asking",
        "prompts": prompts.iter().map(|p| json!({"title": p.title, "message": p.message})).collect::<Vec<_>>(),
    });
    if let Err(e) = bot.api.call("assistant.threads.setSuggestedPrompts", &body).await {
        tracing::debug!(error = %e, "setSuggestedPrompts failed");
    }
}

async fn on_interaction(bot: Arc<SlackBot>, p: Value) {
    if str_of(&p, "type") != "block_actions" {
        return;
    }
    for action in p["actions"].as_array().into_iter().flatten() {
        if str_of(action, "action_id") != FEEDBACK_ACTION {
            continue;
        }
        let feedback = match str_of(action, "value") {
            "positive" => Feedback::Like,
            "negative" => Feedback::Dislike,
            _ => continue,
        };
        let container = &p["container"];
        let channel = [str_of(container, "channel_id"), str_of(&p["channel"], "id")]
            .into_iter()
            .find(|c| !c.is_empty())
            .unwrap_or("");
        let ts = [str_of(container, "message_ts"), str_of(&p["message"], "ts")]
            .into_iter()
            .find(|c| !c.is_empty())
            .unwrap_or("");
        let id = format!("{channel}:{ts}");
        match submit_feedback(&bot.core, &id, feedback).await {
            Ok(true) => tracing::info!(bot = %bot.cfg.name, message = %id, ?feedback, "feedback sent"),
            Ok(false) => tracing::info!(message = %id, "feedback on an unknown answer"),
            Err(e) => tracing::warn!(error = %e, "feedback failed"),
        }
    }
}

/// Answer a slash command; the reply goes back in the ack (ephemeral).
pub async fn on_command(bot: &SlackBot, fields: &Value) -> Value {
    let text = str_of(fields, "text").trim();
    let (sub, arg) = text
        .split_once(char::is_whitespace)
        .map(|(a, b)| (a, b.trim()))
        .unwrap_or((text, ""));
    let team = [str_of(fields, "team_id"), bot.team_id.as_str()]
        .into_iter()
        .find(|t| !t.is_empty())
        .unwrap_or("");
    let scope = state_scope(bot, team, str_of(fields, "channel_id"));
    let command = &bot.cfg.command;
    let reply = match sub.to_ascii_lowercase().as_str() {
        "agents" => {
            let active = bot
                .core
                .storage
                .chat_state(&scope)
                .await
                .ok()
                .and_then(|s| s.active_agent);
            let current = active
                .as_deref()
                .and_then(|a| bot.core.agents.get(a))
                .unwrap_or_else(|| bot.core.agents.default_agent());
            let lines: Vec<String> = bot
                .core
                .agents
                .iter()
                .map(|a| {
                    let mark = if a.name == current.name { " (current)" } else { "" };
                    let about = a.description.as_deref().map(|d| format!(" — {d}")).unwrap_or_default();
                    format!("• `#{}`{mark}{about}", a.name)
                })
                .collect();
            format!(
                "Agents:\n{}\n\nSwitch with `{command} agent <name>`, or start a message with `#name` to ask one once.",
                lines.join("\n")
            )
        }
        "agent" => match bot.core.agents.get(arg) {
            Some(a) => match bot
                .core
                .storage
                .update_chat_state(&scope, StatePatch::active_agent(Some(&a.name)))
                .await
            {
                Ok(_) => format!("Now answering with `#{}` here.", a.name),
                Err(e) => format!("Couldn't switch agents: {e}"),
            },
            None => {
                let names: Vec<String> = bot.core.agents.iter().map(|a| format!("`{}`", a.name)).collect();
                format!("Unknown agent `{arg}`. Available: {}", names.join(", "))
            }
        },
        _ => {
            let multi = bot.core.agents.len() > 1;
            let mut help = String::from(
                "Ask me anything in a DM, or mention me in a channel and keep talking in the thread.\n\
                 Files you attach are sent along with the question.",
            );
            if multi {
                help.push_str(&format!(
                    "\n\n`{command} agents` lists the agents, `{command} agent <name>` switches, `#name question` asks one once."
                ));
            }
            help
        }
    };
    json!({"response_type": "ephemeral", "text": reply})
}
