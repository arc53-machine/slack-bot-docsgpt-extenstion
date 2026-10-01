//! Test harness: an in-process mock Slack Web API (plus a Socket Mode
//! WebSocket), the `docsgpt` mock, and helpers that build real bots against
//! both and feed them Slack events.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use docsgpt::mock::{Call, MockDocsGpt, Recorder};
use docsgpt_bot::Shutdown;
use docsgpt_bot::storage::memory::MemoryStorage;
use docsgpt_slack::bot::SlackBot;
use docsgpt_slack::config::{BotConfig, Channels, Config, Mode};
use docsgpt_slack::events::{self, Inbound};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

pub const WAIT: Duration = Duration::from_secs(10);
pub const TEAM: &str = "T1";
pub const BOT_USER: &str = "UBOT";
pub const USER: &str = "U1";
pub const DM: &str = "D1";
pub const CHANNEL: &str = "C1";

#[derive(Clone)]
enum Fault {
    Error(String),
    RateLimited,
}

/// The mock Slack Web API.
pub struct MockSlack {
    pub url: String,
    pub rec: Recorder,
    ts: AtomicU64,
    faults: Mutex<HashMap<String, VecDeque<Fault>>>,
    files: Mutex<HashMap<String, (String, Bytes)>>,
    /// Frames to push to the Socket Mode client; acks arrive in `acks`.
    ws_out: Mutex<Option<mpsc::UnboundedSender<String>>>,
    ws_connected: Notify,
    pub acks: Recorder,
}

impl MockSlack {
    pub async fn start() -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let m = Arc::new(Self {
            url,
            rec: Recorder::default(),
            ts: AtomicU64::new(1000),
            faults: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
            ws_out: Mutex::new(None),
            ws_connected: Notify::new(),
            acks: Recorder::default(),
        });
        let app = Router::new()
            .route("/api/{method}", post(api))
            .route("/upload/{id}", post(upload))
            .route("/files/{name}", get(file))
            .route("/ws", any(ws))
            .with_state(m.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        m
    }

    pub fn api_url(&self) -> String {
        format!("{}/api", self.url)
    }

    /// The next `times` calls to `method` answer `{"ok": false, "error": code}`.
    pub fn fail(&self, method: &str, code: &str, times: usize) {
        let mut f = self.faults.lock().unwrap();
        f.entry(method.into())
            .or_default()
            .extend(std::iter::repeat_n(Fault::Error(code.into()), times));
    }

    /// The next `times` calls to `method` answer HTTP 429.
    pub fn rate_limit(&self, method: &str, times: usize) {
        let mut f = self.faults.lock().unwrap();
        f.entry(method.into())
            .or_default()
            .extend(std::iter::repeat_n(Fault::RateLimited, times));
    }

    /// Serve a private file at `url_private` = the returned URL.
    pub fn add_file(&self, name: &str, mime: &str, bytes: &[u8]) -> String {
        self.files
            .lock()
            .unwrap()
            .insert(name.into(), (mime.into(), Bytes::copy_from_slice(bytes)));
        format!("{}/files/{name}", self.url)
    }

    fn next_ts(&self) -> String {
        format!("{}.000100", self.ts.fetch_add(1, Ordering::SeqCst))
    }

    /// Text of a streamed message: startStream + appendStream chunks + stopStream text.
    pub fn streamed_text(&self) -> String {
        let mut out = String::new();
        for c in self.rec.all() {
            if matches!(
                c.method.as_str(),
                "chat.startStream" | "chat.appendStream" | "chat.stopStream"
            ) {
                if let Some(t) = c.body["markdown_text"].as_str() {
                    out.push_str(t);
                }
                for ch in c.body["chunks"].as_array().into_iter().flatten() {
                    if ch["type"] == "markdown_text" {
                        out.push_str(ch["text"].as_str().unwrap_or(""));
                    }
                }
            }
        }
        out
    }

    /// Wait for the Socket Mode client, then push a frame.
    pub async fn push_ws(&self, frame: Value) {
        loop {
            if let Some(tx) = self.ws_out.lock().unwrap().as_ref() {
                tx.send(frame.to_string()).unwrap();
                return;
            }
            tokio::time::timeout(WAIT, self.ws_connected.notified())
                .await
                .expect("socket client connected");
        }
    }
}

fn form_or_json(headers: &HeaderMap, body: &[u8]) -> Value {
    let ct = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("");
    if ct.starts_with("application/x-www-form-urlencoded") {
        Value::Object(
            url::form_urlencoded::parse(body)
                .into_owned()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        )
    } else {
        serde_json::from_slice(body).unwrap_or(Value::Null)
    }
}

async fn api(State(m): State<Arc<MockSlack>>, Path(method): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let body = form_or_json(&headers, &body);
    let mut call = Call::new(&method, body.clone());
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        call.query.insert("authorization".into(), auth.to_string());
    }
    m.rec.record(call);
    let fault = m.faults.lock().unwrap().get_mut(&method).and_then(VecDeque::pop_front);
    match fault {
        Some(Fault::Error(code)) => return axum::Json(json!({"ok": false, "error": code})).into_response(),
        Some(Fault::RateLimited) => return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")]).into_response(),
        None => {}
    }
    let reply = match method.as_str() {
        "auth.test" => {
            json!({"ok": true, "user_id": BOT_USER, "bot_id": "BBOT", "team_id": TEAM, "team": "Test", "user": "docsgpt"})
        }
        "chat.startStream" | "chat.postMessage" => {
            json!({"ok": true, "channel": body["channel"], "ts": m.next_ts()})
        }
        "files.getUploadURLExternal" => {
            json!({"ok": true, "upload_url": format!("{}/upload/F1", m.url), "file_id": "F1"})
        }
        "apps.connections.open" => {
            json!({"ok": true, "url": format!("{}/ws", m.url.replace("http://", "ws://"))})
        }
        _ => json!({"ok": true}),
    };
    axum::Json(reply).into_response()
}

async fn upload(State(m): State<Arc<MockSlack>>, Path(id): Path<String>, body: Bytes) -> StatusCode {
    let mut call = Call::new("upload", json!({"id": id, "len": body.len()}));
    call.files.insert(
        "file".into(),
        docsgpt::mock::UploadedFile {
            filename: id,
            content_type: None,
            bytes: body,
        },
    );
    m.rec.record(call);
    StatusCode::OK
}

async fn file(State(m): State<Arc<MockSlack>>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    m.rec
        .record(Call::new("download", json!({"name": name, "authorization": auth})));
    if !auth.starts_with("Bearer xoxb-") {
        // What Slack does without a token: the sign-in page.
        return ([("content-type", "text/html")], "<html>sign in</html>").into_response();
    }
    match m.files.lock().unwrap().get(&name).cloned() {
        Some((mime, bytes)) => ([("content-type", mime)], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn ws(State(m): State<Arc<MockSlack>>, up: WebSocketUpgrade) -> Response {
    up.on_upgrade(move |socket| serve_ws(m, socket))
}

async fn serve_ws(m: Arc<MockSlack>, mut socket: WebSocket) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    socket
        .send(WsMessage::Text(json!({"type": "hello"}).to_string().into()))
        .await
        .ok();
    *m.ws_out.lock().unwrap() = Some(tx);
    m.ws_connected.notify_waiters();
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(text) => { if socket.send(WsMessage::Text(text.into())).await.is_err() { break; } }
                None => break,
            },
            inc = socket.recv() => match inc {
                Some(Ok(WsMessage::Text(t))) => {
                    m.acks.record(Call::new("ack", serde_json::from_str(&t).unwrap_or(Value::Null)));
                }
                Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    }
    *m.ws_out.lock().unwrap() = None;
}

// ---------------------------------------------------------------------------
// Bots
// ---------------------------------------------------------------------------

pub struct TestBot {
    pub bot: Arc<SlackBot>,
    pub slack: Arc<MockSlack>,
    pub docs: Arc<MockDocsGpt>,
}

static EVENT_SEQ: AtomicU64 = AtomicU64::new(1);
static MSG_TS: AtomicU64 = AtomicU64::new(1);

pub fn agents(list: &[(&str, &str)]) -> Vec<docsgpt_bot::AgentConfig> {
    list.iter()
        .map(|(n, k)| docsgpt_bot::AgentConfig::new(*n, *k))
        .collect()
}

pub fn bot_config(slack: &MockSlack) -> BotConfig {
    let toml = format!(
        r#"
        name = "test"
        bot_token = "xoxb-test"
        app_token = "xapp-test"
        signing_secret = "secret"
        slack_api_url = "{}"
        rate_limits = false
        "#,
        slack.api_url()
    );
    let mut cfg: BotConfig = toml::from_str(&toml).unwrap();
    cfg.agents = agents(&[("default", "key-default")]);
    cfg
}

/// Build a bot against fresh mocks; `tweak` adjusts its config.
pub async fn start(tweak: impl FnOnce(&mut BotConfig)) -> TestBot {
    let slack = MockSlack::start().await;
    let docs = MockDocsGpt::start().await;
    let mut cfg = bot_config(&slack);
    tweak(&mut cfg);
    let global = Config {
        api_base: docs.url.clone(),
        storage: Default::default(),
        server: Default::default(),
        bots: vec![],
    };
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let bot = SlackBot::init(cfg, &global, Arc::new(MemoryStorage::default()), http, Shutdown::new())
        .await
        .unwrap();
    TestBot { bot, slack, docs }
}

pub fn next_ts() -> String {
    format!("1700000000.{:06}", MSG_TS.fetch_add(1, Ordering::SeqCst))
}

/// An `event_callback` envelope around `event`.
pub fn envelope(event: Value) -> Value {
    json!({
        "type": "event_callback",
        "team_id": TEAM,
        "api_app_id": "A1",
        "event_id": format!("Ev{}", EVENT_SEQ.fetch_add(1, Ordering::SeqCst)),
        "event": event,
    })
}

pub fn dm_event(text: &str, ts: &str, thread_ts: Option<&str>) -> Value {
    let mut e = json!({"type": "message", "channel_type": "im", "channel": DM, "user": USER, "text": text, "ts": ts, "team": TEAM});
    if let Some(t) = thread_ts {
        e["thread_ts"] = t.into();
    }
    e
}

pub fn mention_event(text: &str, ts: &str, thread_ts: Option<&str>) -> Value {
    let mut e = json!({"type": "app_mention", "channel": CHANNEL, "user": USER, "text": format!("<@{BOT_USER}> {text}"), "ts": ts, "team": TEAM});
    if let Some(t) = thread_ts {
        e["thread_ts"] = t.into();
    }
    e
}

pub fn channel_reply(text: &str, ts: &str, thread_ts: &str) -> Value {
    json!({"type": "message", "channel_type": "channel", "channel": CHANNEL, "user": USER, "text": text, "ts": ts, "thread_ts": thread_ts, "team": TEAM})
}

impl TestBot {
    /// Deliver an event and wait for the bot to finish handling it.
    pub async fn event(&self, event: Value) {
        let inbound = Inbound::Event(envelope(event));
        if events::is_new(&self.bot, &inbound) {
            events::dispatch(self.bot.clone(), inbound).await;
        }
    }

    /// A DM; returns its ts.
    pub async fn dm(&self, text: &str) -> String {
        let ts = next_ts();
        self.event(dm_event(text, &ts, None)).await;
        ts
    }

    pub async fn interaction(&self, payload: Value) {
        events::dispatch(self.bot.clone(), Inbound::Interaction(payload)).await;
    }

    /// Click 👍 or 👎 on message `ts` in `channel`.
    pub async fn click_feedback(&self, channel: &str, ts: &str, value: &str) {
        self.interaction(json!({
            "type": "block_actions",
            "user": {"id": USER},
            "container": {"type": "message", "message_ts": ts, "channel_id": channel},
            "channel": {"id": channel},
            "message": {"ts": ts},
            "actions": [{"type": "feedback_buttons", "action_id": "docsgpt_feedback", "value": value}],
        }))
        .await;
    }

    pub async fn command(&self, text: &str, channel: &str) -> Value {
        events::on_command(
            &self.bot,
            &json!({"command": "/docsgpt", "text": text, "channel_id": channel, "user_id": USER, "team_id": TEAM}),
        )
        .await
    }

    /// The `ts` of the answer message (from startStream or the last postMessage).
    pub fn answer_ts(&self) -> String {
        let all = self.slack.rec.all();
        let streamed = all
            .iter()
            .rev()
            .find(|c| c.method == "chat.stopStream")
            .map(|c| c.body["ts"].as_str().unwrap_or("").to_string());
        streamed.unwrap_or_else(|| "missing".into())
    }
}

/// Socket Mode envelope.
pub fn ws_envelope(kind: &str, payload: Value) -> Value {
    json!({"envelope_id": format!("env-{}", EVENT_SEQ.fetch_add(1, Ordering::SeqCst)), "type": kind, "payload": payload, "accepts_response_payload": kind == "slash_commands"})
}

pub fn set_channels(c: Channels) -> impl FnOnce(&mut BotConfig) {
    move |cfg| cfg.channels = c
}

pub fn http_mode(cfg: &mut BotConfig) {
    cfg.mode = Mode::Http;
}
