//! Real Slack, scripted DocsGPT: sends real turns into a DM and reads the
//! thread back. Ignored by default; posts messages, so point it at a test DM.
//!
//! ```text
//! SLACK_BOT_TOKEN=xoxb-… SLACK_LIVE_USER=U… \
//!   cargo test --test live_slack -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::sync::Arc;
use std::time::Duration;

use docsgpt::mock::{MockDocsGpt, Step, StreamReply, answer_steps, ev, sse};
use docsgpt_bot::storage::memory::MemoryStorage;
use docsgpt_bot::{Ask, Outcome, Scope, Shutdown, TurnReport, run_turn};
use docsgpt_slack::bot::SlackBot;
use docsgpt_slack::config::{BotConfig, Config};
use docsgpt_slack::slack::api::SlackApi;
use docsgpt_slack::surface::SlackSurface;
use serde_json::{Value, json};

const IMAGE: &str = "https://raw.githubusercontent.com/arc53/DocsGPT/main/frontend/public/apple-touch-icon.png";

struct Live {
    bot: Arc<SlackBot>,
    docs: Arc<MockDocsGpt>,
    api: SlackApi,
    channel: String,
    user: String,
}

async fn live(tweak: impl FnOnce(&mut BotConfig)) -> Option<Live> {
    let _ = dotenvy::dotenv();
    let token = std::env::var("SLACK_BOT_TOKEN").ok()?;
    let user = std::env::var("SLACK_LIVE_USER").ok()?;
    let docs = MockDocsGpt::start().await;
    let mut cfg: BotConfig =
        toml::from_str(&format!("name = \"live\"\nbot_token = \"{token}\"\napp_token = \"x\"")).unwrap();
    cfg.agents = common::agents(&[("default", "key")]);
    tweak(&mut cfg);
    let global = Config {
        api_base: docs.url.clone(),
        storage: Default::default(),
        server: Default::default(),
        bots: vec![],
    };
    let http = reqwest::Client::new();
    let bot = SlackBot::init(
        cfg,
        &global,
        Arc::new(MemoryStorage::default()),
        http.clone(),
        Shutdown::new(),
    )
    .await
    .unwrap();
    let api = SlackApi::new(http, "https://slack.com/api", &token);
    let open = api
        .call("conversations.open", &json!({"users": user}))
        .await
        .expect("conversations.open");
    let channel = open["channel"]["id"].as_str().unwrap().to_string();
    Some(Live {
        bot,
        docs,
        api,
        channel,
        user,
    })
}

impl Live {
    /// Post a thread root (stands in for the user's question) and run a turn in its thread.
    async fn turn(&self, title: &str) -> (TurnReport, String) {
        let root = self
            .api
            .call(
                "chat.postMessage",
                &json!({"channel": self.channel, "text": format!("🧪 Live test: {title}")}),
            )
            .await
            .unwrap();
        let ts = root["ts"].as_str().unwrap().to_string();
        let surface = SlackSurface {
            bot: self.bot.clone(),
            team: self.bot.team_id.clone(),
            channel: self.channel.clone(),
            thread_ts: ts.clone(),
            user: self.user.clone(),
            source_ts: ts.clone(),
            is_dm: true,
        };
        let mut ask = Ask::new(
            Scope::new("live", format!("{}:{}", self.bot.team_id, self.channel), ts.clone()),
            title,
        );
        ask.state_scope = Some(Scope::new("live", "state", "0"));
        let report = run_turn(&self.bot.core, &surface, ask).await.expect("turn");
        (report, ts)
    }

    async fn replies(&self, ts: &str) -> Vec<Value> {
        tokio::time::sleep(Duration::from_millis(800)).await;
        let v = self
            .api
            .call_form(
                "conversations.replies",
                &[("channel", self.channel.clone()), ("ts", ts.to_string())],
            )
            .await
            .unwrap();
        v["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .skip(1)
            .collect()
    }
}

fn block_types(m: &Value) -> Vec<String> {
    m["blocks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| b["type"].as_str().unwrap_or("").to_string())
        .collect()
}

#[tokio::test]
#[ignore]
async fn streamed_answer_with_everything() {
    let Some(l) = live(|_| {}).await else { return };
    l.docs
        .add_artifact("a1", "numbers.csv", "text/csv", &b"n\n1\n2\n3\n"[..]);
    l.docs.on_stream(|_| {
        let mut steps = vec![
            ev::step(ev::message_id("m", "conv-live")),
            ev::step(ev::tool_call(json!({"tool_name": "code_executor", "call_id": "1", "status": "pending"}))),
            Step::Sleep(Duration::from_millis(1500)),
            ev::step(ev::tool_call(json!({"tool_name": "code_executor", "call_id": "1", "status": "completed", "artifact_id": "a1",
                "artifacts": [{"id": "a1", "filename": "numbers.csv"}]}))),
        ];
        let text = "## Result\n\nHere is a **streamed** answer with a table:\n\n| n | square |\n|---|---|\n| 1 | 1 |\n| 2 | 4 |\n\n```python\nprint(sum([1, 2, 3]))\n```\n\nAnd an image:\n\n![icon](IMAGE)\n\nDone.";
        for w in text.replace("IMAGE", IMAGE).split_inclusive(' ') {
            steps.push(ev::step(ev::answer(w)));
            steps.push(Step::Sleep(Duration::from_millis(60)));
        }
        steps.push(ev::step(ev::source(&[("DocsGPT on GitHub", "https://github.com/arc53/DocsGPT"), ("Docs", "https://docs.docsgpt.cloud")])));
        steps.push(ev::step(ev::id("conv-live")));
        steps.push(ev::step(ev::end()));
        sse(steps)
    });
    let (report, ts) = l.turn("streaming, tools, sources, image, file").await;
    println!("{report:?}");
    assert!(matches!(
        report,
        TurnReport::Answered {
            outcome: Outcome::Complete,
            message_id: Some(_),
            ..
        }
    ));
    let msgs = l.replies(&ts).await;
    for m in &msgs {
        println!(
            "reply: blocks={:?} files={}",
            block_types(m),
            m["files"].as_array().map(|f| f.len()).unwrap_or(0)
        );
    }
    let answer = msgs
        .iter()
        .find(|m| block_types(m).contains(&"context_actions".to_string()))
        .expect("answer with feedback buttons");
    let types = block_types(answer);
    assert!(
        types.contains(&"image".to_string()) && types.contains(&"context".to_string()),
        "{types:?}"
    );
    assert!(
        msgs.iter().any(|m| m["files"]
            .as_array()
            .is_some_and(|f| f.iter().any(|f| f["name"] == "numbers.csv"))),
        "csv uploaded"
    );
}

#[tokio::test]
#[ignore]
async fn errors_and_partial_answers() {
    let Some(l) = live(|_| {}).await else { return };
    l.docs
        .on_stream(|_| StreamReply::Http(401, "{\"error\": \"Unauthorized\"}".into()));
    let (report, ts) = l.turn("DocsGPT rejects the key").await;
    assert!(matches!(
        report,
        TurnReport::Answered {
            outcome: Outcome::Failed { .. },
            ..
        }
    ));
    let msgs = l.replies(&ts).await;
    assert!(
        msgs.iter()
            .any(|m| m["text"].as_str().is_some_and(|t| t.contains("API key was rejected"))),
        "{msgs:?}"
    );

    l.docs.on_stream(|_| {
        sse(vec![
            ev::step(ev::message_id("m", "c2")),
            ev::step(ev::answer("This answer starts fine ")),
            Step::Sleep(Duration::from_millis(1500)),
            ev::step(ev::answer("and then")),
            ev::step(ev::error("LLM overloaded")),
        ])
    });
    let (report, _) = l.turn("partial answer then an error").await;
    assert!(matches!(
        report,
        TurnReport::Answered {
            outcome: Outcome::Failed { .. },
            ..
        }
    ));
}

#[tokio::test]
#[ignore]
async fn long_answer_and_non_streaming_fallback() {
    let Some(l) = live(|_| {}).await else { return };
    let long = (1..=600)
        .map(|i| format!("Line {i}: lorem ipsum dolor sit amet."))
        .collect::<Vec<_>>()
        .join("\n");
    let l2 = long.clone();
    l.docs.on_stream(move |_| {
        sse(vec![
            ev::step(ev::message_id("m", "c3")),
            ev::step(ev::answer(&l2)),
            ev::step(ev::end()),
        ])
    });
    let (report, ts) = l.turn("a 20k-character answer").await;
    assert!(
        matches!(
            report,
            TurnReport::Answered {
                outcome: Outcome::Complete,
                ..
            }
        ),
        "{report:?}"
    );
    println!("long answer: {} replies", l.replies(&ts).await.len());

    let Some(l) = live(|c| c.streaming = false).await else {
        return;
    };
    l.docs.on_stream(|_| {
        sse(answer_steps(
            "Posted in one go, with **markdown** and a [link](https://docsgpt.cloud).",
            "c4",
            &[("Docs", "https://docs.docsgpt.cloud")],
        ))
    });
    let (report, ts) = l.turn("streaming off: one message").await;
    assert!(matches!(
        report,
        TurnReport::Answered {
            outcome: Outcome::Complete,
            message_id: Some(_),
            ..
        }
    ));
    let msgs = l.replies(&ts).await;
    assert_eq!(
        block_types(&msgs[0]),
        vec!["rich_text", "context", "context_actions"],
        "{msgs:?}"
    );
}
