use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use docsgpt_bot::Shutdown;
use docsgpt_slack::bot::SlackBot;
use docsgpt_slack::config::{self, DEFAULT_SQLITE_PATH, Mode};
use docsgpt_slack::manifest::manifest;
use docsgpt_slack::slack::{http, socket};

/// Slack bots for DocsGPT agents.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Config file (default: $DOCSGPT_SLACK_CONFIG, then ./docsgpt-slack.toml, then environment variables).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Check the config and the Slack tokens, then exit.
    #[arg(long)]
    check: bool,
    /// Print the Slack app manifest (JSON) for a bot, then exit.
    #[arg(long)]
    print_manifest: bool,
    /// Which bot --print-manifest is for (default: the first).
    #[arg(long)]
    bot: Option<String>,
    /// App name to put in the manifest.
    #[arg(long, default_value = "DocsGPT")]
    app_name: String,
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if std::env::var("LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json")) {
        builder.json().init();
    } else {
        builder.init();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let args = Args::parse();
    init_logging();
    let cfg = config::load(args.config.as_deref())?;

    if args.print_manifest {
        let bot = match &args.bot {
            Some(name) => cfg
                .bots
                .iter()
                .find(|b| &b.name == name)
                .with_context(|| format!("no bot named {name:?}"))?,
            None => &cfg.bots[0],
        };
        let m = manifest(bot, &args.app_name, cfg.server.public_url.as_deref());
        println!("{}", serde_json::to_string_pretty(&m)?);
        return Ok(());
    }

    let storage = docsgpt_bot::storage::open(&cfg.storage, DEFAULT_SQLITE_PATH).await?;
    let http_client = reqwest::Client::builder()
        .user_agent(concat!("docsgpt-slack/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()?;
    let shutdown = Shutdown::new();

    let mut bots = Vec::new();
    for b in cfg.bots.clone() {
        bots.push(SlackBot::init(b, &cfg, storage.clone(), http_client.clone(), shutdown.clone()).await?);
    }
    if args.check {
        for b in &bots {
            println!(
                "ok: {} → team {} as <@{}> ({} agents, {:?} mode)",
                b.cfg.name,
                b.team_id,
                b.user_id,
                b.core.agents.len(),
                b.cfg.mode
            );
        }
        return Ok(());
    }

    let mut tasks = Vec::new();
    let mut http_bots = HashMap::new();
    for b in &bots {
        b.spawn_housekeeping();
        match b.cfg.mode {
            Mode::Socket => {
                let token = b.cfg.app_token.clone().unwrap_or_default();
                tasks.push(tokio::spawn(socket::run(b.clone(), token)));
            }
            Mode::Http => {
                http_bots.insert(b.cfg.name.clone(), b.clone());
            }
        }
    }
    if !http_bots.is_empty() {
        let names: Vec<_> = http_bots.keys().cloned().collect();
        let app = http::router(Arc::new(http_bots));
        let listener = tokio::net::TcpListener::bind(&cfg.server.bind)
            .await
            .with_context(|| format!("binding {}", cfg.server.bind))?;
        tracing::info!(bind = %cfg.server.bind, bots = ?names, "HTTP mode listening on /slack/<bot>/events");
        let token = shutdown.token().clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(async move { token.cancelled().await })
                .await
            {
                tracing::error!(error = %e, "HTTP server failed");
            }
        }));
    }
    if tasks.is_empty() {
        bail!("nothing to run");
    }

    shutdown.wait_for_signal().await;
    if !shutdown.drain(Duration::from_secs(30)).await {
        tracing::warn!("some answers were cut off by shutdown");
    }
    for t in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
    }
    Ok(())
}
