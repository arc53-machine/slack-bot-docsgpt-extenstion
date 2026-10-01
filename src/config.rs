//! Configuration: `docsgpt-slack.toml` with `${ENV}` references, or the v1
//! environment layout (SLACK_BOT_TOKEN / SLACK_APP_TOKEN / API_KEY / API_KEY_*).

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Result, bail};
use docsgpt_bot::config::{
    AgentConfig, Backend, DEFAULT_API_BASE, ServerConfig, StorageConfig, agents_from_env, find_config, load_toml,
    normalize_name,
};
use serde::Deserialize;

/// Default SQLite file (relative to the working directory).
pub const DEFAULT_SQLITE_PATH: &str = "data/docsgpt-slack.db";
/// Default config file name.
pub const CONFIG_FILE: &str = "docsgpt-slack.toml";
/// Environment variable naming the config file.
pub const CONFIG_ENV: &str = "DOCSGPT_SLACK_CONFIG";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// DocsGPT base URL.
    #[serde(default = "default_api_base")]
    pub api_base: String,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub bots: Vec<BotConfig>,
}

/// How a bot receives events.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Socket Mode: an outbound WebSocket; needs an app-level token. No public URL.
    #[default]
    Socket,
    /// HTTP Events API: Slack POSTs to `server.public_url`; needs the signing secret.
    Http,
}

/// Which channel messages get an answer.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Channels {
    /// A mention starts a thread; replies in the bot's threads are answered without one.
    #[default]
    Thread,
    /// Only messages that mention the bot.
    Mention,
    /// Ignore channels (DMs only).
    Off,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SuggestedPrompt {
    pub title: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotConfig {
    pub name: String,
    /// Bot User OAuth Token (`xoxb-…`).
    pub bot_token: String,
    /// App-level token with `connections:write` (`xapp-…`), for Socket Mode.
    #[serde(default)]
    pub app_token: Option<String>,
    /// Signing secret, for HTTP mode.
    #[serde(default)]
    pub signing_secret: Option<String>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub channels: Channels,
    /// Stream answers as they are generated (needs Slack's AI features).
    #[serde(default = "default_true")]
    pub streaming: bool,
    /// Use agent sessions (status, titles) in DMs; matches `agent_view` in the manifest.
    #[serde(default = "default_true")]
    pub agent_view: bool,
    /// 👀 on the question while answering in channels.
    #[serde(default = "default_true")]
    pub reactions: bool,
    /// Accept files from users and send files tools produce.
    #[serde(default = "default_true")]
    pub files: bool,
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u64,
    /// Channel ids the bot answers in (DMs are always allowed). Empty = all.
    #[serde(default)]
    pub allowed_channels: Vec<String>,
    /// Shown in the manifest (`agent_description`, ≤ 300 characters).
    #[serde(default)]
    pub description: Option<String>,
    /// Prompts offered when a user opens the bot's Messages tab.
    #[serde(default)]
    pub suggested_prompts: Vec<SuggestedPrompt>,
    /// Slash command name for the manifest (default `/docsgpt`).
    #[serde(default = "default_command")]
    pub command: String,
    /// Per-bot DocsGPT URL.
    #[serde(default)]
    pub api_base: Option<String>,
    /// Slack Web API base (tests point this at a mock).
    #[serde(default = "default_slack_api")]
    pub slack_api_url: String,
    /// Client-side budgets for Slack's per-workspace limits. Turn off only in tests.
    #[serde(default = "default_true")]
    pub rate_limits: bool,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
}

fn default_api_base() -> String {
    DEFAULT_API_BASE.into()
}
fn default_true() -> bool {
    true
}
fn default_max_file_mb() -> u64 {
    20
}
fn default_command() -> String {
    "/docsgpt".into()
}
pub fn default_slack_api() -> String {
    "https://slack.com/api".into()
}

impl BotConfig {
    pub fn api_base<'a>(&'a self, global: &'a str) -> &'a str {
        self.api_base.as_deref().unwrap_or(global)
    }

    pub fn channel_allowed(&self, channel: &str) -> bool {
        self.allowed_channels.is_empty() || self.allowed_channels.iter().any(|c| c == channel)
    }

    /// Suggested prompts: configured ones, else one per agent with a description.
    pub fn prompts(&self) -> Vec<SuggestedPrompt> {
        if !self.suggested_prompts.is_empty() {
            return self.suggested_prompts.clone();
        }
        let multi = self.agents.len() > 1;
        self.agents
            .iter()
            .filter_map(|a| {
                let d = a.description.as_deref()?;
                Some(SuggestedPrompt {
                    title: d.chars().take(75).collect(),
                    message: if multi { format!("#{} ", a.name) } else { d.to_string() },
                })
            })
            .take(4)
            .collect()
    }
}

/// Load configuration: explicit path → `DOCSGPT_SLACK_CONFIG` → `docsgpt-slack.toml` → environment.
pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let mut cfg = match find_config(explicit, CONFIG_ENV, CONFIG_FILE) {
        Some(path) => {
            tracing::info!(path = %path.display(), "loading config file");
            load_toml::<Config>(&path)?
        }
        None => from_env(std::env::vars())?,
    };
    normalize(&mut cfg)?;
    Ok(cfg)
}

/// The single-bot layout of v1: SLACK_BOT_TOKEN + SLACK_APP_TOKEN (Socket Mode)
/// or SLACK_SIGNING_SECRET (HTTP), API_KEY and API_KEY_<NAME>.
pub fn from_env(vars: impl IntoIterator<Item = (String, String)>) -> Result<Config> {
    let vars: Vec<(String, String)> = vars.into_iter().collect();
    let get = |k: &str| {
        vars.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    let storage_type = get("STORAGE_TYPE").unwrap_or_default().to_ascii_lowercase();
    let backend = match storage_type.as_str() {
        "" | "sqlite" => Backend::Sqlite,
        "memory" => Backend::Memory,
        "mongodb" => bail!(
            "STORAGE_TYPE=mongodb is no longer supported. Version 2 keeps only conversation ids, in SQLite by \
             default; remove STORAGE_TYPE (or set it to sqlite) and mount a volume for data/. See \
             \"Upgrading from version 1\" in the README."
        ),
        other => bail!("unknown STORAGE_TYPE {other:?}; use sqlite or memory"),
    };
    let Some(bot_token) = get("SLACK_BOT_TOKEN") else {
        bail!("no {CONFIG_FILE} found and SLACK_BOT_TOKEN is not set");
    };
    let app_token = get("SLACK_APP_TOKEN");
    let signing_secret = get("SLACK_SIGNING_SECRET");
    let mode = match (&app_token, &signing_secret) {
        (Some(_), _) => Mode::Socket,
        (None, Some(_)) => Mode::Http,
        (None, None) => bail!("set SLACK_APP_TOKEN (Socket Mode) or SLACK_SIGNING_SECRET (HTTP mode)"),
    };
    let channels = match get("CHANNELS").unwrap_or_default().to_ascii_lowercase().as_str() {
        "" | "thread" => Channels::Thread,
        "mention" => Channels::Mention,
        "off" => Channels::Off,
        other => bail!("unknown CHANNELS {other:?}; use thread, mention or off"),
    };
    let flag = |k: &str, default: bool| match get(k).as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("false" | "0" | "no" | "off") => false,
        Some(_) => true,
        None => default,
    };
    let mut server = ServerConfig::default();
    if let Some(b) = get("HTTP_BIND") {
        server.bind = b;
    }
    server.public_url = get("PUBLIC_URL");
    Ok(Config {
        api_base: get("API_BASE").unwrap_or_else(default_api_base),
        storage: StorageConfig {
            backend,
            path: get("SQLITE_PATH"),
        },
        server,
        bots: vec![BotConfig {
            name: get("BOT_NAME").unwrap_or_else(|| "docsgpt".into()),
            bot_token,
            app_token,
            signing_secret,
            mode,
            channels,
            streaming: flag("STREAMING", true),
            agent_view: flag("AGENT_VIEW", true),
            reactions: flag("REACTIONS", true),
            files: flag("FILES", true),
            max_file_mb: get("MAX_FILE_MB")
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(default_max_file_mb),
            allowed_channels: get("ALLOWED_CHANNELS")
                .map(|v| {
                    v.split(',')
                        .map(|c| c.trim().to_string())
                        .filter(|c| !c.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            description: get("AGENT_DESCRIPTION"),
            suggested_prompts: Vec::new(),
            command: default_command(),
            api_base: None,
            slack_api_url: get("SLACK_API_URL").unwrap_or_else(default_slack_api),
            rate_limits: true,
            agents: agents_from_env(vars.iter().cloned()),
        }],
    })
}

fn normalize(cfg: &mut Config) -> Result<()> {
    if cfg.bots.is_empty() {
        bail!("no bots configured");
    }
    let mut names = HashSet::new();
    for bot in &mut cfg.bots {
        bot.name = normalize_name("bot", &bot.name)?;
        if !names.insert(bot.name.clone()) {
            bail!("duplicate bot name {:?}", bot.name);
        }
        if !bot.bot_token.starts_with("xoxb-") && !bot.slack_api_url.starts_with("http://") {
            bail!("bot {:?}: bot_token should be a bot token (xoxb-…)", bot.name);
        }
        match bot.mode {
            Mode::Socket if bot.app_token.as_deref().is_none_or(|t| t.trim().is_empty()) => {
                bail!(
                    "bot {:?}: Socket Mode needs app_token (xapp-…, scope connections:write)",
                    bot.name
                )
            }
            Mode::Http if bot.signing_secret.as_deref().is_none_or(|t| t.trim().is_empty()) => {
                bail!("bot {:?}: HTTP mode needs signing_secret", bot.name)
            }
            Mode::Http if cfg.server.public_url.is_none() => {
                tracing::warn!(bot = %bot.name, "server.public_url is not set; --print-manifest can't fill in request URLs");
            }
            _ => {}
        }
        if !bot.command.starts_with('/') {
            bot.command = format!("/{}", bot.command);
        }
        // Validates names, keys and the default agent.
        bot.agents = docsgpt_bot::Agents::new(std::mem::take(&mut bot.agents))
            .map_err(|e| anyhow::anyhow!("bot {:?}: {e}", bot.name))?
            .iter()
            .cloned()
            .collect();
    }
    cfg.api_base = cfg.api_base.trim_end_matches('/').to_string();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn v1_env_layout_still_works() {
        let mut cfg = from_env(env(&[
            ("SLACK_BOT_TOKEN", "xoxb-1"),
            ("SLACK_APP_TOKEN", "xapp-1"),
            ("API_KEY", "k"),
            ("API_KEY_SALES", "k2"),
        ]))
        .unwrap();
        normalize(&mut cfg).unwrap();
        let bot = &cfg.bots[0];
        assert_eq!(bot.mode, Mode::Socket);
        assert_eq!(bot.channels, Channels::Thread);
        assert_eq!(cfg.storage.backend, Backend::Sqlite);
        let names: Vec<_> = bot.agents.iter().map(|a| (a.name.as_str(), a.default)).collect();
        assert_eq!(names, vec![("default", true), ("sales", false)]);
        assert_eq!(cfg.api_base, "https://gptcloud.arc53.com");
    }

    #[test]
    fn http_mode_from_signing_secret_and_mongodb_is_refused() {
        let cfg = from_env(env(&[
            ("SLACK_BOT_TOKEN", "xoxb-1"),
            ("SLACK_SIGNING_SECRET", "s"),
            ("API_KEY", "k"),
        ]))
        .unwrap();
        assert_eq!(cfg.bots[0].mode, Mode::Http);
        let err = from_env(env(&[
            ("SLACK_BOT_TOKEN", "xoxb-1"),
            ("SLACK_APP_TOKEN", "xapp"),
            ("STORAGE_TYPE", "mongodb"),
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("no longer supported"), "{err}");
        assert!(from_env(env(&[("SLACK_BOT_TOKEN", "xoxb-1"), ("API_KEY", "k")])).is_err());
    }

    #[test]
    fn multi_bot_toml() {
        let mut cfg: Config = docsgpt_bot::config::parse_toml(
            r#"
            api_base = "https://docs.example.com/"
            [server]
            public_url = "https://bot.example.com"
            [[bots]]
            name = "Support"
            bot_token = "xoxb-a"
            app_token = "xapp-a"
            channels = "mention"
            [[bots.agents]]
            name = "support"
            api_key = "k1"
            description = "Product questions"
            [[bots.agents]]
            name = "sales"
            api_key = "k2"
            description = "Pricing"
            [[bots]]
            name = "internal"
            bot_token = "xoxb-b"
            signing_secret = "sec"
            mode = "http"
            command = "ask"
            [[bots.agents]]
            name = "docs"
            api_key = "k3"
            "#,
        )
        .unwrap();
        normalize(&mut cfg).unwrap();
        assert_eq!(cfg.api_base, "https://docs.example.com");
        assert_eq!(cfg.bots[0].name, "support");
        assert_eq!(cfg.bots[0].channels, Channels::Mention);
        assert_eq!(cfg.bots[1].command, "/ask");
        let prompts = cfg.bots[0].prompts();
        assert_eq!(
            prompts[0],
            SuggestedPrompt {
                title: "Product questions".into(),
                message: "#support ".into()
            }
        );
        assert!(cfg.bots[1].prompts().is_empty());
    }

    #[test]
    fn validation() {
        let base = r#"
            [[bots]]
            name = "a"
            bot_token = "xoxb-a"
            mode = "http"
            [[bots.agents]]
            name = "x"
            api_key = "k"
        "#;
        let mut cfg: Config = docsgpt_bot::config::parse_toml(base).unwrap();
        assert!(normalize(&mut cfg).unwrap_err().to_string().contains("signing_secret"));
        let mut cfg: Config = docsgpt_bot::config::parse_toml(&base.replace("mode = \"http\"", "")).unwrap();
        assert!(normalize(&mut cfg).unwrap_err().to_string().contains("app_token"));
        let mut cfg: Config = docsgpt_bot::config::parse_toml(
            &base
                .replace("xoxb-a", "xoxp-user")
                .replace("mode = \"http\"", "app_token = \"x\""),
        )
        .unwrap();
        assert!(normalize(&mut cfg).unwrap_err().to_string().contains("xoxb"));
    }
}

#[cfg(test)]
mod example_tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docsgpt-slack.example.toml")).unwrap();
        let raw = regex::Regex::new(r"\$\{[A-Z_]+\}")
            .unwrap()
            .replace_all(&raw, "xoxb-placeholder");
        let mut cfg: Config = toml::from_str(&raw).unwrap();
        normalize(&mut cfg).unwrap();
        assert_eq!(cfg.bots.len(), 2);
        assert_eq!(cfg.bots[1].mode, Mode::Http);
        assert_eq!(cfg.bots[0].suggested_prompts.len(), 1);
    }
}
