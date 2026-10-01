//! Answering in DMs and channels, end to end against mock Slack and mock DocsGPT.

mod common;

use std::time::{Duration, Instant};

use common::*;
use docsgpt::mock::{Step, StreamReply, answer_steps, ev, reply_text, sse};
use docsgpt_slack::config::Channels;
use serde_json::json;

#[tokio::test]
async fn dm_streams_an_answer_with_sources_and_feedback() {
    let t = start(|_| {}).await;
    t.docs.on_stream(|_| {
        sse(answer_steps(
            "DocsGPT answers from your docs.",
            "conv-1",
            &[("Guide", "https://g.example")],
        ))
    });
    let ts = t.dm("what is it?").await;

    let rec = &t.slack.rec;
    let status = rec.last("agents.sessions.setStatus").expect("session status");
    assert_eq!(
        status.body,
        json!({"channel_id": DM, "thread_ts": ts, "status": "processing", "title": "what is it?"})
    );
    let start = rec.last("chat.startStream").expect("stream started");
    assert_eq!(start.body["thread_ts"], ts.as_str());
    assert!(start.body.get("recipient_user_id").is_none(), "not needed in DMs");
    assert_eq!(start.body["task_display_mode"], "timeline");
    assert_eq!(t.slack.streamed_text(), "DocsGPT answers from your docs.");

    let stop = rec.last("chat.stopStream").expect("stream stopped");
    let blocks = stop.body["blocks"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "context");
    assert!(
        blocks[0]["elements"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<https://g.example|Guide>")
    );
    assert_eq!(blocks[1]["type"], "context_actions");
    assert_eq!(blocks[1]["elements"][0]["type"], "feedback_buttons");
    assert_eq!(rec.count("chat.postMessage"), 0);
    assert!(
        rec.calls("chat.startStream")
            .iter()
            .all(|c| c.query["authorization"] == "Bearer xoxb-test")
    );

    // 👍 on that message reaches DocsGPT as feedback on answer 0.
    t.click_feedback(DM, &t.answer_ts(), "positive").await;
    assert_eq!(
        t.docs.rec.last("/api/feedback").unwrap().body,
        json!({"feedback": "like", "conversation_id": "conv-1", "question_index": 0, "api_key": "key-default"})
    );
    // Clicking on some other message does nothing.
    t.click_feedback(DM, "1.2", "negative").await;
    assert_eq!(t.docs.rec.count("/api/feedback"), 1);
}

#[tokio::test]
async fn threads_continue_and_new_dm_messages_start_over() {
    let t = start(|_| {}).await;
    t.docs.on_stream(|body| {
        let conv = body["conversation_id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("conv-{}", body["question"].as_str().unwrap()));
        reply_text("ok", &conv)
    });
    let first = t.dm("one").await;
    t.event(dm_event("two", &next_ts(), Some(&first))).await;
    let calls = t.docs.rec.calls("/stream");
    assert!(calls[0].body.get("conversation_id").is_none());
    assert_eq!(calls[1].body["conversation_id"], "conv-one");

    // A new top-level DM message is a new session and a new conversation.
    t.dm("three").await;
    assert!(
        t.docs
            .rec
            .last("/stream")
            .unwrap()
            .body
            .get("conversation_id")
            .is_none()
    );

    // Feedback on the second answer in the thread is position 1.
    let stops = t.slack.rec.calls("chat.stopStream");
    t.click_feedback(DM, stops[1].body["ts"].as_str().unwrap(), "negative")
        .await;
    let fb = t.docs.rec.last("/api/feedback").unwrap().body;
    assert_eq!(
        (fb["question_index"].as_u64(), fb["feedback"].as_str()),
        (Some(1), Some("dislike"))
    );
}

#[tokio::test]
async fn agents_by_tag_command_and_unknown_tag() {
    let t = start(|c| c.agents = agents(&[("support", "k-support"), ("sales", "k-sales")])).await;
    t.dm("#sales what does it cost?").await;
    let body = t.docs.rec.last("/stream").unwrap().body;
    assert_eq!(
        (body["api_key"].as_str(), body["question"].as_str()),
        (Some("k-sales"), Some("what does it cost?"))
    );

    // /docsgpt agent switches the whole DM, across threads.
    let reply = t.command("agent sales", DM).await;
    assert_eq!(reply["response_type"], "ephemeral");
    assert!(reply["text"].as_str().unwrap().contains("#sales"));
    t.dm("and support?").await;
    assert_eq!(t.docs.rec.last("/stream").unwrap().body["api_key"], "k-sales");
    let list = t.command("agents", DM).await;
    assert!(list["text"].as_str().unwrap().contains("`#sales` (current)"), "{list}");

    t.dm("#nope hello").await;
    let notice = t.slack.rec.last("chat.postMessage").unwrap();
    assert_eq!(notice.body["text"], "Unknown agent #nope. Available: #support, #sales");
    assert_eq!(t.docs.rec.count("/stream"), 2);

    assert!(
        t.command("agent nobody", DM).await["text"]
            .as_str()
            .unwrap()
            .starts_with("Unknown agent")
    );
    assert!(
        t.command("", DM).await["text"]
            .as_str()
            .unwrap()
            .contains("/docsgpt agents")
    );
}

#[tokio::test]
async fn channel_mention_then_thread_follow_up() {
    let t = start(|_| {}).await;
    let ts = next_ts();
    t.event(mention_event("how do I deploy?", &ts, None)).await;
    let rec = &t.slack.rec;
    assert_eq!(
        rec.last("reactions.add").unwrap().body,
        json!({"channel": CHANNEL, "timestamp": ts, "name": "eyes"})
    );
    let start_call = rec.last("chat.startStream").unwrap();
    assert_eq!(
        (
            start_call.body["recipient_user_id"].as_str(),
            start_call.body["recipient_team_id"].as_str()
        ),
        (Some(USER), Some(TEAM))
    );
    assert_eq!(start_call.body["thread_ts"], ts.as_str());
    assert_eq!(rec.count("reactions.remove"), 1);
    assert_eq!(rec.count("agents.sessions.setStatus"), 0, "sessions are for DMs");
    assert_eq!(t.docs.rec.last("/stream").unwrap().body["question"], "how do I deploy?");

    // A reply in the bot's thread is answered without a mention.
    t.event(channel_reply("and on k8s?", &next_ts(), &ts)).await;
    assert_eq!(t.docs.rec.count("/stream"), 2);
    assert_eq!(t.docs.rec.last("/stream").unwrap().body["conversation_id"], "conv-1");
    // Not: replies starting with '!', mentioning someone else, or in other threads.
    t.event(channel_reply("!note to self", &next_ts(), &ts)).await;
    t.event(channel_reply("<@U2> what do you think?", &next_ts(), &ts))
        .await;
    t.event(channel_reply("unrelated", &next_ts(), "1600000000.000001"))
        .await;
    // A reply that mentions the bot is handled by app_mention, not twice.
    t.event(channel_reply(&format!("<@{BOT_USER}> again"), &next_ts(), &ts))
        .await;
    assert_eq!(t.docs.rec.count("/stream"), 2);
}

#[tokio::test]
async fn channel_modes_and_allow_list() {
    let t = start(set_channels(Channels::Mention)).await;
    let ts = next_ts();
    t.event(mention_event("q", &ts, None)).await;
    t.event(channel_reply("follow up", &next_ts(), &ts)).await;
    assert_eq!(t.docs.rec.count("/stream"), 1);

    let t = start(set_channels(Channels::Off)).await;
    t.event(mention_event("q", &next_ts(), None)).await;
    assert_eq!(t.docs.rec.count("/stream"), 0);
    t.dm("dm still works").await;
    assert_eq!(t.docs.rec.count("/stream"), 1);

    let t = start(|c| c.allowed_channels = vec!["C9".into()]).await;
    t.event(mention_event("q", &next_ts(), None)).await;
    assert_eq!(t.docs.rec.count("/stream"), 0);
}

#[tokio::test]
async fn ignores_bots_edits_and_duplicates() {
    let t = start(|_| {}).await;
    let mut e = dm_event("from a bot", &next_ts(), None);
    e["bot_id"] = "B2".into();
    t.event(e).await;
    let mut e = dm_event("edited", &next_ts(), None);
    e["subtype"] = "message_changed".into();
    t.event(e).await;
    let mut e = dm_event("mine", &next_ts(), None);
    e["user"] = BOT_USER.into();
    t.event(e).await;
    assert_eq!(t.docs.rec.count("/stream"), 0);

    // The same event delivered twice (a retry) is answered once.
    let env = envelope(dm_event("once", &next_ts(), None));
    for _ in 0..2 {
        let inbound = docsgpt_slack::events::Inbound::Event(env.clone());
        if docsgpt_slack::events::is_new(&t.bot, &inbound) {
            docsgpt_slack::events::dispatch(t.bot.clone(), inbound).await;
        }
    }
    assert_eq!(t.docs.rec.count("/stream"), 1);
}

#[tokio::test]
async fn native_stop_ends_the_turn() {
    let t = start(|_| {}).await;
    t.docs.on_stream(|_| {
        sse(vec![
            ev::step(ev::message_id("m", "conv-s")),
            ev::step(ev::answer("Partial answer ")),
            Step::Sleep(Duration::from_secs(8)),
            ev::step(ev::answer("never")),
            ev::step(ev::end()),
        ])
    });
    let ts = next_ts();
    let bot = t.bot.clone();
    let e = dm_event("long one", &ts, None);
    let started = Instant::now();
    let turn = tokio::spawn(async move {
        docsgpt_slack::events::dispatch(bot, docsgpt_slack::events::Inbound::Event(envelope(e))).await
    });
    t.slack.rec.wait_any("chat.startStream").await;
    t.event(json!({"type": "agent_session_stopped", "channel": DM, "thread_ts": ts, "streaming_message_ts": ["x"], "user": USER})).await;
    turn.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "took {:?}",
        started.elapsed()
    );
    let stop = t.slack.rec.last("chat.stopStream").unwrap();
    assert_eq!(stop.body["markdown_text"], "\n\n_Stopped._");
    assert!(!t.slack.streamed_text().contains("never"));
}

#[tokio::test]
async fn errors_are_shown() {
    // DocsGPT rejects the key: no stream, an apology, the session goes back to active.
    let t = start(|_| {}).await;
    t.docs
        .on_stream(|_| StreamReply::Http(401, "{\"error\": \"Unauthorized\"}".into()));
    t.dm("q").await;
    assert_eq!(t.slack.rec.count("chat.startStream"), 0);
    let msg = t.slack.rec.last("chat.postMessage").unwrap();
    assert_eq!(
        msg.body["blocks"][0]["text"],
        "Sorry, I couldn't get an answer right now.\n\nThe agent's API key was rejected."
    );
    assert_eq!(
        msg.body["blocks"].as_array().unwrap().len(),
        1,
        "no feedback buttons on an apology"
    );
    assert_eq!(
        t.slack.rec.last("agents.sessions.setStatus").unwrap().body["status"],
        "active"
    );

    // A partial answer, then an error: the note goes at the end of the stream.
    let t = start(|_| {}).await;
    t.docs.on_stream(|_| {
        sse(vec![
            ev::step(ev::message_id("m", "c")),
            ev::step(ev::answer("Half")),
            ev::step(ev::error("LLM overloaded")),
        ])
    });
    t.dm("q").await;
    assert_eq!(
        t.slack.streamed_text(),
        "Half\n\n_The answer was cut short: LLM overloaded_"
    );
}

#[tokio::test]
async fn stopped_by_user_from_slack_stops_the_turn() {
    let t = start(|_| {}).await;
    t.docs.on_stream(|_| {
        sse(vec![
            ev::step(ev::message_id("m", "c")),
            ev::step(ev::answer("one ")),
            Step::Sleep(Duration::from_millis(1500)),
            ev::step(ev::answer("two ")),
            Step::Sleep(Duration::from_secs(8)),
            ev::step(ev::end()),
        ])
    });
    t.slack.fail("chat.appendStream", "stopped_by_user", 5);
    t.slack.fail("chat.stopStream", "message_not_in_streaming_state", 5);
    let started = Instant::now();
    t.dm("q").await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "turn did not stop: {:?}",
        started.elapsed()
    );
    assert_eq!(t.slack.rec.count("chat.postMessage"), 0);
}

#[tokio::test]
async fn falls_back_to_one_message_when_streaming_is_unavailable() {
    let t = start(|_| {}).await;
    t.slack.fail("chat.startStream", "not_allowed", 1);
    t.docs
        .on_stream(|_| sse(answer_steps("A complete answer.", "conv-f", &[("Doc", "https://d")])));
    t.dm("q").await;
    let msg = t.slack.rec.last("chat.postMessage").unwrap();
    let blocks = msg.body["blocks"].as_array().unwrap();
    assert_eq!(blocks[0], json!({"type": "markdown", "text": "A complete answer."}));
    assert_eq!(blocks.last().unwrap()["type"], "context_actions");
    assert_eq!(msg.body["text"], "A complete answer.");
    // Feedback still maps to the posted message.
    t.click_feedback(
        DM,
        msg.body["thread_ts"].as_str().map(|_| "1000.000100").unwrap(),
        "positive",
    )
    .await;
    assert_eq!(t.docs.rec.count("/api/feedback"), 1);
}

#[tokio::test]
async fn stream_budget_falls_back_to_messages() {
    let t = start(|c| c.rate_limits = true).await;
    for _ in 0..10 {
        t.dm("q").await;
    }
    assert_eq!(
        t.slack.rec.count("chat.startStream"),
        9,
        "Tier 2 budget: 18/min, 2 per answer"
    );
    assert_eq!(t.slack.rec.count("chat.postMessage"), 1);
}

#[tokio::test]
async fn tool_progress_shows_as_tasks() {
    let t = start(|_| {}).await;
    t.docs.on_stream(|_| {
        sse(vec![
            ev::step(ev::message_id("m", "c")),
            ev::step(ev::tool_call(
                json!({"tool_name": "code_executor", "call_id": "1", "status": "pending"}),
            )),
            Step::Sleep(Duration::from_millis(1400)),
            ev::step(ev::tool_call(
                json!({"tool_name": "code_executor", "call_id": "1", "status": "completed"}),
            )),
            Step::Sleep(Duration::from_millis(1400)),
            ev::step(ev::answer("Done.")),
            ev::step(ev::end()),
        ])
    });
    t.dm("compute").await;
    let tasks: Vec<(String, String)> = t
        .slack
        .rec
        .all()
        .into_iter()
        .filter(|c| c.method.ends_with("Stream"))
        .flat_map(|c| c.body["chunks"].as_array().cloned().unwrap_or_default())
        .filter(|ch| ch["type"] == "task_update")
        .map(|ch| {
            (
                ch["title"].as_str().unwrap().to_string(),
                ch["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        tasks,
        vec![
            ("Running code".into(), "in_progress".into()),
            ("Running code".into(), "complete".into())
        ]
    );
    assert_eq!(t.slack.streamed_text(), "Done.");
}

#[tokio::test]
async fn long_answers_are_split_across_calls() {
    let t = start(|_| {}).await;
    let long = "word ".repeat(5000); // 25k characters
    let l2 = long.clone();
    t.docs.on_stream(move |_| {
        sse(vec![
            ev::step(ev::message_id("m", "c")),
            ev::step(ev::answer(&l2)),
            ev::step(ev::end()),
        ])
    });
    t.dm("q").await;
    assert_eq!(t.slack.streamed_text(), long);
    for c in t.slack.rec.all() {
        let len = c.body["markdown_text"].as_str().map(|s| s.chars().count()).unwrap_or(0)
            + c.body["chunks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|ch| ch["text"].as_str().map(|s| s.chars().count()).unwrap_or(0))
                .sum::<usize>();
        assert!(len <= 12_000, "{} sent {len} characters", c.method);
    }
}

#[tokio::test]
async fn suggested_prompts_on_messages_tab() {
    let t = start(|c| {
        c.agents = agents(&[("support", "k1"), ("sales", "k2")]);
        c.agents[1].description = Some("Pricing and plans".into());
    })
    .await;
    t.event(json!({"type": "app_home_opened", "user": USER, "channel": DM, "tab": "messages"}))
        .await;
    let call = t.slack.rec.last("assistant.threads.setSuggestedPrompts").unwrap();
    assert_eq!(
        call.body["prompts"],
        json!([{"title": "Pricing and plans", "message": "#sales "}])
    );
    t.event(json!({"type": "app_home_opened", "user": USER, "channel": DM, "tab": "home"}))
        .await;
    assert_eq!(t.slack.rec.count("assistant.threads.setSuggestedPrompts"), 1);
}
