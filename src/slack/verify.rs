//! Slack request signatures (HTTP mode).

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// Requests older (or newer) than this are rejected as replays.
pub const MAX_SKEW_SECS: i64 = 300;

/// Check `X-Slack-Signature` (`v0=<hex hmac>`) over `v0:{timestamp}:{body}`.
pub fn verify(secret: &str, timestamp: &str, signature: &str, body: &[u8], now: i64) -> bool {
    let Ok(ts) = timestamp.parse::<i64>() else {
        return false;
    };
    if (now - ts).abs() > MAX_SKEW_SECS {
        return false;
    }
    let Some(hex_sig) = signature.strip_prefix("v0=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(format!("v0:{timestamp}:").as_bytes());
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Sign a body the way Slack does (used by tests and local tooling).
pub fn sign(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(format!("v0:{timestamp}:").as_bytes());
    mac.update(body);
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_slack_signatures() {
        // Example from Slack's "Verifying requests" guide.
        let secret = "8f742231b10e8888abcd99yyyzzz85a5";
        let body = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
        let sig = "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503";
        assert!(verify(secret, "1531420618", sig, body, 1531420618 + 10));
        assert!(!verify(secret, "1531420618", sig, body, 1531420618 + 400), "replayed");
        assert!(!verify("wrong", "1531420618", sig, body, 1531420618));
        assert!(!verify(secret, "1531420618", "v0=zz", body, 1531420618));
        let ours = sign(secret, "1531420618", body);
        assert_eq!(ours, sig);
    }
}
