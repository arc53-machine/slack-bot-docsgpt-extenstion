//! One Slack app: its tokens, identity, DocsGPT core and runtime state.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use docsgpt_bot::storage::MESSAGE_REF_KIND;
use docsgpt_bot::{Agents, BotCore, CancelRegistry, MESSAGE_REF_TTL, Shutdown, Storage};

use crate::config::{BotConfig, Config};
use crate::ratelimit::{Limits, Seen};
use crate::slack::api::SlackApi;

/// A running Slack app.
pub struct SlackBot {
    pub cfg: BotConfig,
    pub core: BotCore,
    /// Web API with the bot token.
    pub api: SlackApi,
    /// The bot's user id (`U…`), for mentions.
    pub user_id: String,
    /// The bot's bot id (`B…`), to ignore its own messages.
    pub bot_id: String,
    pub team_id: String,
    pub cancels: CancelRegistry,
    pub limits: Limits,
    pub seen: Seen,
    pub shutdown: Shutdown,
}

impl SlackBot {
    /// Check the token with `auth.test` and build the bot.
    pub async fn init(
        cfg: BotConfig,
        global: &Config,
        storage: Arc<dyn Storage>,
        http: reqwest::Client,
        shutdown: Shutdown,
    ) -> Result<Arc<Self>> {
        let api = SlackApi::new(http.clone(), &cfg.slack_api_url, &cfg.bot_token);
        let me = api
            .call("auth.test", &serde_json::json!({}))
            .await
            .with_context(|| format!("bot {:?}: auth.test", cfg.name))?;
        let s = |k: &str| me[k].as_str().unwrap_or("").to_string();
        let client = docsgpt_bot::docsgpt::Client::builder(cfg.api_base(&global.api_base))
            .http_client(http)
            .build()
            .context("DocsGPT client")?;
        let agents = Agents::new(cfg.agents.clone())?;
        let core = BotCore::new(cfg.name.clone(), client, agents, storage);
        let bot = Arc::new(Self {
            user_id: s("user_id"),
            bot_id: s("bot_id"),
            team_id: s("team_id"),
            limits: Limits::new(cfg.rate_limits),
            seen: Seen::new(5000),
            cancels: CancelRegistry::default(),
            cfg,
            core,
            api,
            shutdown,
        });
        tracing::info!(bot = %bot.cfg.name, user = %bot.user_id, team = %bot.team_id, team_name = %s("team"), agents = bot.core.agents.len(), "connected to Slack");
        Ok(bot)
    }

    /// Drop message refs older than [`MESSAGE_REF_TTL`] once a day.
    pub fn spawn_housekeeping(self: &Arc<Self>) {
        let bot = self.clone();
        let token = self.shutdown.token().clone();
        tokio::spawn(async move {
            loop {
                match bot.core.storage.prune_json(MESSAGE_REF_KIND, MESSAGE_REF_TTL).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(bot = %bot.cfg.name, pruned = n, "pruned old message refs"),
                    Err(e) => tracing::warn!(error = %e, "pruning message refs failed"),
                }
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(24 * 3600)) => {}
                }
            }
        });
    }
}
