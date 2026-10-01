//! Files in and out, HTTP mode, Socket Mode.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use common::*;
use docsgpt::mock::{ev, sse};
use docsgpt_slack::slack::{http, socket, verify};
use serde_json::{Value, json};
use tower::ServiceExt;

fn dm_with_file(t: &TestBot, text: &str, name: &str, mime: &str, bytes: &[u8]) -> Value {
    let url = t.slack.add_file(name, mime, bytes);
    let mut e = dm_event(text, &next_ts(), None);
    e["subtype"] = "file_share".into();
    e["files"] =
        json!([{"id": "F9", "name": name, "mimetype": mime, "size": bytes.len(), "url_private_download": url}]);
    e
}

#[tokio::test]
async fn user_files_become_attachments() {
    let t = start(|c| c.agents = agents(&[("support", "k-support"), ("sales", "k-sales")])).await;
    let e = dm_with_file(
        &t,
        "#sales summarize this",
        "notes.md",
        "text/markdown",
        b"# Q3 pricing",
    );
    t.event(e).await;
    let dl = t.slack.rec.last("download").unwrap();
    assert_eq!(dl.body["authorization"], "Bearer xoxb-test");
    let up = t.docs.rec.last("/api/store_attachment").unwrap();
    assert_eq!(
        up.body["api_key"], "k-sales",
        "uploaded with the agent that will answer"
    );
    assert_eq!(&up.file("file").bytes[..], b"# Q3 pricing");
    let stream = t.docs.rec.last("/stream").unwrap().body;
    assert_eq!(
        (stream["attachments"].clone(), stream["api_key"].as_str()),
        (json!(["att-1"]), Some("k-sales"))
    );
}

#[tokio::test]
async fn file_stubs_are_looked_up_with_files_info() {
    // Slack often sends only {id, file_access: "check_file_info"} in the event.
    let t = start(|_| {}).await;
    t.slack.add_file("F42", "text/plain", b"the launch code is 4471");
    let mut e = dm_event("what's in it?", &next_ts(), None);
    e["subtype"] = "file_share".into();
    e["files"] = json!([{"id": "F42", "mode": "hidden_by_limit_not", "file_access": "check_file_info"}]);
    t.event(e).await;
    assert_eq!(t.slack.rec.last("files.info").unwrap().body["file"], "F42");
    let up = t.docs.rec.last("/api/store_attachment").expect("uploaded to DocsGPT");
    assert_eq!(up.file("file").filename, "F42.txt");
    assert_eq!(&up.file("file").bytes[..], b"the launch code is 4471");
    assert_eq!(t.slack.rejected(), Vec::<String>::new());
    assert!(
        t.slack
            .rec
            .calls("chat.postMessage")
            .iter()
            .all(|c| !c.body["text"].as_str().unwrap_or("").contains("can't open"))
    );
}

#[tokio::test]
async fn voice_clip_without_text_is_transcribed() {
    let t = start(|_| {}).await;
    t.docs.set_stt("how do I reset my password");
    let e = dm_with_file(&t, "", "clip.m4a", "audio/mp4", b"\x00\x01audio");
    t.event(e).await;
    assert_eq!(t.docs.rec.count("/api/stt"), 1);
    assert_eq!(t.docs.rec.count("/api/store_attachment"), 0);
    assert_eq!(
        t.docs.rec.last("/stream").unwrap().body["question"],
        "how do I reset my password"
    );

    // Speech-to-text switched off on the server: the clip goes as an attachment.
    t.docs
        .set_stt_reply(404, json!({"success": false, "message": "disabled"}));
    let e = dm_with_file(&t, "", "clip2.m4a", "audio/mp4", b"audio");
    t.event(e).await;
    assert_eq!(t.docs.rec.count("/api/store_attachment"), 1);
}

#[tokio::test]
async fn file_problems_are_reported() {
    let t = start(|c| c.max_file_mb = 1).await;
    let mut e = dm_event("look", &next_ts(), None);
    e["files"] = json!([
        {"name": "big.pdf", "size": 5 * 1024 * 1024, "url_private_download": "http://x"},
        {"name": "gone.txt", "size": 3, "url_private_download": format!("{}/files/missing", t.slack.url)},
    ]);
    t.event(e).await;
    let notices: Vec<String> = t
        .slack
        .rec
        .calls("chat.postMessage")
        .iter()
        .filter_map(|c| c.body["text"].as_str().map(String::from))
        .collect();
    assert!(
        notices.contains(&"big.pdf is larger than 1 MB.".to_string()),
        "{notices:?}"
    );
    assert!(
        notices.contains(&"I couldn't download gone.txt.".to_string()),
        "{notices:?}"
    );
    // The question is still asked.
    assert_eq!(t.docs.rec.count("/stream"), 1);
}

#[tokio::test]
async fn tool_files_are_uploaded_to_the_thread() {
    let t = start(|_| {}).await;
    t.docs.add_artifact("a1", "report.csv", "text/csv", &b"a,b\n1,2\n"[..]);
    t.docs.on_stream(|_| {
        sse(vec![
            ev::step(ev::message_id("m", "conv-t")),
            ev::step(ev::tool_call(
                json!({"tool_name": "code_executor", "call_id": "1", "status": "completed", "artifact_id": "a1"}),
            )),
            ev::step(ev::answer("Here is the report.")),
            ev::step(ev::end()),
        ])
    });
    let ts = t.dm("make a report").await;
    let get = t.slack.rec.last("files.getUploadURLExternal").unwrap();
    assert_eq!(
        (get.body["filename"].as_str(), get.body["length"].as_str()),
        (Some("report.csv"), Some("8"))
    );
    assert_eq!(
        &t.slack.rec.last("upload").unwrap().file("file").bytes[..],
        b"a,b\n1,2\n"
    );
    let done = t.slack.rec.last("files.completeUploadExternal").unwrap();
    assert_eq!(
        done.body,
        json!({"files": [{"id": "F1", "title": "report.csv"}], "channel_id": DM, "thread_ts": ts})
    );
}

// ---------------------------------------------------------------------------
// HTTP mode
// ---------------------------------------------------------------------------

fn now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string()
}

fn signed(path: &str, content_type: &str, body: String, secret: &str) -> Request<Body> {
    let ts = now();
    let sig = verify::sign(secret, &ts, body.as_bytes());
    Request::post(path)
        .header("content-type", content_type)
        .header("x-slack-request-timestamp", ts)
        .header("x-slack-signature", sig)
        .body(Body::from(body))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn http_mode_end_to_end() {
    let t = start(http_mode).await;
    let app = http::router(Arc::new(HashMap::from([("test".to_string(), t.bot.clone())])));

    // url_verification
    let resp = app
        .clone()
        .oneshot(signed(
            "/slack/test/events",
            "application/json",
            json!({"type": "url_verification", "challenge": "abc"}).to_string(),
            "secret",
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await, json!({"challenge": "abc"}));

    // Bad signature and unknown bot.
    let bad = signed("/slack/test/events", "application/json", "{}".into(), "not-the-secret");
    assert_eq!(app.clone().oneshot(bad).await.unwrap().status(), 401);
    let unknown = signed("/slack/other/events", "application/json", "{}".into(), "secret");
    assert_eq!(app.clone().oneshot(unknown).await.unwrap().status(), 404);

    // An event, then Slack's retry of the same event: answered once.
    let env = envelope(dm_event("over http", &next_ts(), None)).to_string();
    for _ in 0..2 {
        let mut req = signed("/slack/test/events", "application/json", env.clone(), "secret");
        req.headers_mut().insert("x-slack-retry-num", "1".parse().unwrap());
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 200);
    }
    t.slack.rec.wait_any("chat.stopStream").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(t.docs.rec.count("/stream"), 1);

    // A slash command (form) answers in the response body.
    let form = "command=%2Fdocsgpt&text=agents&channel_id=D1&user_id=U1&team_id=T1".to_string();
    let resp = app
        .clone()
        .oneshot(signed(
            "/slack/test/commands",
            "application/x-www-form-urlencoded",
            form,
            "secret",
        ))
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert_eq!(v["response_type"], "ephemeral");
    assert!(v["text"].as_str().unwrap().contains("#default"));

    // An interaction (form with payload=…) records feedback.
    let ts = t.answer_ts();
    let payload = json!({"type": "block_actions", "container": {"message_ts": ts, "channel_id": DM},
        "actions": [{"action_id": "docsgpt_feedback", "value": "positive"}]});
    let form = format!(
        "payload={}",
        url::form_urlencoded::byte_serialize(payload.to_string().as_bytes()).collect::<String>()
    );
    let resp = app
        .clone()
        .oneshot(signed(
            "/slack/test/interactions",
            "application/x-www-form-urlencoded",
            form,
            "secret",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    t.docs.rec.wait_any("/api/feedback").await;

    let health = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        axum::body::to_bytes(health.into_body(), 100).await.unwrap(),
        "ok bots=1"
    );
}

// ---------------------------------------------------------------------------
// Socket Mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn socket_mode_acks_and_answers() {
    let t = start(|_| {}).await;
    let runner = tokio::spawn(socket::run(t.bot.clone(), "xapp-test".into()));

    let event = ws_envelope("events_api", envelope(dm_event("over the socket", &next_ts(), None)));
    t.slack.push_ws(event.clone()).await;
    let ack = t.slack.acks.wait_any("ack").await;
    assert_eq!(ack.body, json!({"envelope_id": event["envelope_id"]}));
    t.slack.rec.wait_any("chat.stopStream").await;
    assert_eq!(t.docs.rec.last("/stream").unwrap().body["question"], "over the socket");
    let open = t.slack.rec.last("apps.connections.open").unwrap();
    assert_eq!(open.query["authorization"], "Bearer xapp-test");

    // Slash commands reply inside the ack.
    let cmd = ws_envelope(
        "slash_commands",
        json!({"command": "/docsgpt", "text": "help", "channel_id": DM, "user_id": USER, "team_id": TEAM}),
    );
    t.slack.push_ws(cmd.clone()).await;
    let ack = t
        .slack
        .acks
        .wait_for("ack", |c| c.body["envelope_id"] == cmd["envelope_id"], WAIT)
        .await;
    assert_eq!(ack.body["payload"]["response_type"], "ephemeral");

    // Slack asks for a reconnect: the client opens a new connection.
    t.slack
        .push_ws(json!({"type": "disconnect", "reason": "refresh_requested"}))
        .await;
    t.slack.rec.wait_count("apps.connections.open", 2, WAIT).await;

    // Shutdown ends the loop.
    t.bot.shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("socket loop stops")
        .unwrap();
}
