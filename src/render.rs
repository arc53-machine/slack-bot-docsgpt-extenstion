//! Slack text in, Block Kit out.

use std::sync::LazyLock;

use docsgpt_bot::docsgpt::Source;
use docsgpt_bot::markdown::{clean_title, split_markdown};
use regex::Regex;
use serde_json::{Value, json};

/// `markdown_text` and `markdown` blocks take at most 12,000 characters per call/message.
pub const MARKDOWN_LIMIT: usize = 11_500;
/// Action id of the 👍/👎 buttons.
pub const FEEDBACK_ACTION: &str = "docsgpt_feedback";

static SLACK_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<([^<>]+)>").expect("valid regex"));
static USER_MENTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<@([A-Z0-9]+)(?:\|[^>]*)?>").expect("valid regex"));

/// Turn a Slack message's text into a question: drop mentions of the bot,
/// unwrap links (`<url|label>` → `[label](url)`), channels and special
/// mentions, and unescape `&amp; &lt; &gt;`.
pub fn clean_incoming(text: &str, bot_user_id: &str) -> String {
    let out = SLACK_TOKEN.replace_all(text, |caps: &regex::Captures| {
        let inner = &caps[1];
        let (target, label) = match inner.split_once('|') {
            Some((t, l)) => (t, Some(l)),
            None => (inner, None),
        };
        if let Some(user) = target.strip_prefix('@') {
            return if user == bot_user_id {
                String::new()
            } else {
                format!("@{}", label.unwrap_or(user))
            };
        }
        if let Some(ch) = target.strip_prefix('#') {
            return format!("#{}", label.unwrap_or(ch));
        }
        if let Some(special) = target.strip_prefix('!') {
            let name = special.split('^').next().unwrap_or(special);
            return format!("@{}", label.unwrap_or(name));
        }
        match label {
            Some(l) if l != target => format!("[{l}]({target})"),
            _ => target.to_string(),
        }
    });
    let out = out.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
    out.split_whitespace().collect::<Vec<_>>().join(" ").trim().to_string()
}

/// True when the bot is mentioned.
pub fn mentions(text: &str, user_id: &str) -> bool {
    USER_MENTION.captures_iter(text).any(|c| &c[1] == user_id)
}

/// True when someone other than the bot is mentioned.
pub fn mentions_someone_else(text: &str, bot_user_id: &str) -> bool {
    USER_MENTION.captures_iter(text).any(|c| &c[1] != bot_user_id)
}

/// Escape text for mrkdwn.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A context block listing sources as links.
pub fn sources_block(sources: &[Source]) -> Option<Value> {
    if sources.is_empty() {
        return None;
    }
    let mut seen = Vec::new();
    let items: Vec<String> = sources
        .iter()
        .filter(|s| {
            let key = (s.title.clone(), s.url.clone());
            !seen.contains(&key) && {
                seen.push(key);
                true
            }
        })
        .take(10)
        .map(|s| {
            let title = escape(&clean_title(&s.title)).replace('|', "¦");
            match &s.url {
                Some(u) => format!("<{}|{}>", u.replace('|', "%7C").replace('>', "%3E"), title),
                None => title,
            }
        })
        .collect();
    let text: String = format!("*Sources:* {}", items.join("  ·  "))
        .chars()
        .take(2990)
        .collect();
    Some(json!({"type": "context", "elements": [{"type": "mrkdwn", "text": text}]}))
}

/// Image blocks for public image URLs (at most 5).
pub fn image_blocks(urls: &[String]) -> Vec<Value> {
    urls.iter()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .take(5)
        .map(|u| json!({"type": "image", "image_url": u, "alt_text": "Image from the answer"}))
        .collect()
}

/// 👍/👎 buttons. Slack shows the chosen one as selected.
pub fn feedback_block() -> Value {
    json!({
        "type": "context_actions",
        "elements": [{
            "type": "feedback_buttons",
            "action_id": FEEDBACK_ACTION,
            "positive_button": {
                "text": {"type": "plain_text", "text": "Good answer"},
                "value": "positive",
                "accessibility_label": "Mark this answer as helpful"
            },
            "negative_button": {
                "text": {"type": "plain_text", "text": "Bad answer"},
                "value": "negative",
                "accessibility_label": "Mark this answer as not helpful"
            }
        }]
    })
}

/// A `markdown` block.
pub fn markdown_block(text: &str) -> Value {
    json!({"type": "markdown", "text": text})
}

/// Split Markdown into message-sized pieces without breaking code blocks.
pub fn markdown_pieces(text: &str) -> Vec<String> {
    split_markdown(text, MARKDOWN_LIMIT)
}

/// Split text for `markdown_text` stream chunks (may cut anywhere; the
/// receiving message is one continuous text).
pub fn stream_pieces(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(MARKDOWN_LIMIT).map(|c| c.iter().collect()).collect()
}

/// Notification text (`text` field next to blocks): short and plain.
pub fn fallback_text(markdown: &str) -> String {
    let flat = markdown.replace("**", "").replace('`', "");
    docsgpt_bot::util::truncate_chars(flat.trim(), 300)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_incoming_text() {
        assert_eq!(
            clean_incoming("<@UBOT> what is <https://a.b/c|this>?", "UBOT"),
            "what is [this](https://a.b/c)?"
        );
        assert_eq!(
            clean_incoming("ask <@U2|bob> in <#C1|general> &lt;b&gt; &amp; <!here>", "UBOT"),
            "ask @bob in #general <b> & @here"
        );
        assert_eq!(clean_incoming("see <https://x.y>", "UBOT"), "see https://x.y");
        assert_eq!(clean_incoming("<mailto:a@b.c|a@b.c>", "U"), "[a@b.c](mailto:a@b.c)");
    }

    #[test]
    fn mentions_detection() {
        assert!(mentions("hi <@UBOT>", "UBOT"));
        assert!(!mentions("hi <@UOTHER>", "UBOT"));
        assert!(mentions_someone_else("hey <@U2> and <@UBOT>", "UBOT"));
        assert!(!mentions_someone_else("<@UBOT> hi", "UBOT"));
    }

    #[test]
    fn blocks() {
        let s = vec![
            Source {
                title: "Guide [v2]".into(),
                url: Some("https://d/x|y".into()),
            },
            Source {
                title: "Guide [v2]".into(),
                url: Some("https://d/x|y".into()),
            },
            Source {
                title: "<Notes>".into(),
                url: None,
            },
        ];
        let b = sources_block(&s).unwrap();
        assert_eq!(
            b["elements"][0]["text"],
            "*Sources:* <https://d/x%7Cy|Guide v2>  ·  &lt;Notes&gt;"
        );
        assert!(sources_block(&[]).is_none());
        assert_eq!(feedback_block()["elements"][0]["positive_button"]["value"], "positive");
        assert_eq!(image_blocks(&["https://i/a.png".into(), "data:x".into()]).len(), 1);
        assert_eq!(stream_pieces(&"x".repeat(MARKDOWN_LIMIT + 5)).len(), 2);
    }
}
