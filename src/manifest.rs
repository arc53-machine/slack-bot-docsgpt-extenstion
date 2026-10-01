//! The Slack app manifest for a bot (paste it into "Create New App → From a manifest").

use serde_json::{Value, json};

use crate::config::{BotConfig, Channels, Mode};

pub const SCOPES: &[&str] = &[
    "app_mentions:read",
    "assistant:write",
    "channels:history",
    "chat:write",
    "chat:write.customize",
    "commands",
    "files:read",
    "files:write",
    "groups:history",
    "im:history",
    "im:read",
    "im:write",
    "mpim:history",
    "reactions:write",
    "users:read",
];

/// Manifest JSON for `bot`. `public_url` fills the request URLs in HTTP mode.
pub fn manifest(bot: &BotConfig, display_name: &str, public_url: Option<&str>) -> Value {
    let description = bot
        .description
        .clone()
        .unwrap_or_else(|| "Answers questions with DocsGPT agents.".into())
        .chars()
        .take(300)
        .collect::<String>();
    let mut events = vec![
        "app_home_opened",
        "message.im",
        "app_mention",
        "agent_session_stopped",
        "agent_session_title_changed",
        "app_context_changed",
    ];
    if bot.channels == Channels::Thread {
        events.extend(["message.channels", "message.groups", "message.mpim"]);
    }
    let mut features = json!({
        "app_home": {"home_tab_enabled": false, "messages_tab_enabled": true, "messages_tab_read_only_enabled": false},
        "bot_user": {"display_name": display_name, "always_online": true},
        "slash_commands": [{
            "command": bot.command,
            "description": "List or switch DocsGPT agents",
            "usage_hint": "[agents | agent <name> | help]",
            "should_escape": false,
        }],
    });
    if bot.agent_view {
        let prompts: Vec<Value> = bot
            .prompts()
            .iter()
            .map(|p| json!({"title": p.title, "message": p.message}))
            .collect();
        features["agent_view"] = json!({"agent_description": description});
        if !prompts.is_empty() {
            features["agent_view"]["suggested_prompts"] = prompts.into();
        }
    }
    let mut settings = json!({
        "event_subscriptions": {"bot_events": events},
        "interactivity": {"is_enabled": true},
        "org_deploy_enabled": false,
        "socket_mode_enabled": bot.mode == Mode::Socket,
        "token_rotation_enabled": false,
    });
    if bot.mode == Mode::Http {
        let base = public_url.unwrap_or("https://YOUR-PUBLIC-URL").trim_end_matches('/');
        let url = format!("{base}/slack/{}/events", bot.name);
        settings["event_subscriptions"]["request_url"] = url.clone().into();
        settings["interactivity"]["request_url"] = url.clone().into();
        features["slash_commands"][0]["url"] = url.into();
    }
    json!({
        "_metadata": {"major_version": 1, "minor_version": 1},
        "display_information": {
            "name": display_name,
            "description": description.chars().take(140).collect::<String>(),
            "background_color": "#0f172a"
        },
        "features": features,
        "oauth_config": {"scopes": {"bot": SCOPES}},
        "settings": settings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, from_env};

    fn bot(vars: &[(&str, &str)]) -> BotConfig {
        let cfg: Config = from_env(vars.iter().map(|(k, v)| (k.to_string(), v.to_string()))).unwrap();
        cfg.bots.into_iter().next().unwrap()
    }

    #[test]
    fn socket_and_http_manifests() {
        let m = manifest(
            &bot(&[
                ("SLACK_BOT_TOKEN", "xoxb"),
                ("SLACK_APP_TOKEN", "xapp"),
                ("API_KEY", "k"),
            ]),
            "DocsGPT",
            None,
        );
        assert_eq!(m["settings"]["socket_mode_enabled"], true);
        assert!(m["settings"]["event_subscriptions"].get("request_url").is_none());
        let events = m["settings"]["event_subscriptions"]["bot_events"].as_array().unwrap();
        assert!(events.iter().any(|e| e == "agent_session_stopped") && events.iter().any(|e| e == "message.channels"));
        assert!(m["features"]["agent_view"]["agent_description"].as_str().unwrap().len() <= 300);

        let m = manifest(
            &bot(&[
                ("SLACK_BOT_TOKEN", "xoxb"),
                ("SLACK_SIGNING_SECRET", "s"),
                ("API_KEY", "k"),
                ("CHANNELS", "mention"),
                ("AGENT_VIEW", "false"),
            ]),
            "Docs",
            Some("https://bot.example.com/"),
        );
        assert_eq!(m["settings"]["socket_mode_enabled"], false);
        assert_eq!(
            m["settings"]["event_subscriptions"]["request_url"],
            "https://bot.example.com/slack/docsgpt/events"
        );
        assert_eq!(
            m["features"]["slash_commands"][0]["url"],
            "https://bot.example.com/slack/docsgpt/events"
        );
        assert!(m["features"].get("agent_view").is_none());
        let events = m["settings"]["event_subscriptions"]["bot_events"].as_array().unwrap();
        assert!(!events.iter().any(|e| e == "message.channels"));
    }
}
