//! Pure Top.gg webhook verification.
//!
//! This boundary performs no database write and never grants an entitlement. Its only job is to
//! authenticate the untouched raw body, parse the two currently supported payload shapes and
//! identify a real upvote for the configured Discord application. A later durable ledger owns
//! idempotency and the rolling Plus reward.

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

pub const GIVEAWAY_VOTE_BOTS: [(&str, &str); 3] = [
    ("TTS", "1523826014935842997"),
    ("Helper", "1526211106081734666"),
    ("Music", "1564573935234973817"),
];

pub const TOPGG_SIGNATURE_TOLERANCE_MS: i64 = 5 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopggVote {
    pub user_id: String,
    pub event_id: Option<String>,
    pub voted_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopggWebhookRejection {
    Unauthorized,
    InvalidJson,
    InvalidPayload,
    WrongProject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopggWebhookDecision {
    /// A valid provider request which is not a countable upvote (including Top.gg test pings).
    Acknowledged,
    /// A valid upvote, still requiring a durable idempotency/reward transaction.
    Upvote(TopggVote),
    Rejected(TopggWebhookRejection),
}

/// The webhook adapter must retain the raw body exactly as received for v1 verification. An
/// empty secret is accepted here only for Node compatibility of the pure function; production
/// route configuration must reject an empty secret before exposing an HTTP listener.
pub fn verify_topgg_webhook(
    authorization: Option<&str>,
    signature: Option<&str>,
    body: &str,
    secret: Option<&str>,
    now_ms: i64,
    expected_bot_id: Option<&str>,
) -> TopggWebhookDecision {
    if let Some(secret) = secret.filter(|value| !value.is_empty()) {
        let authenticated = match signature {
            Some(signature) => v1_signature_matches(signature, body, secret, now_ms),
            None => legacy_authorization_matches(authorization, secret),
        };
        if !authenticated {
            return TopggWebhookDecision::Rejected(TopggWebhookRejection::Unauthorized);
        }
    }

    let payload = match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(object)) => object,
        Ok(_) => return TopggWebhookDecision::Rejected(TopggWebhookRejection::InvalidPayload),
        Err(_) => return TopggWebhookDecision::Rejected(TopggWebhookRejection::InvalidJson),
    };
    let event_type = string_field(&payload, "type").unwrap_or_default();
    let (user_id, bot_id, event_id) =
        if matches!(event_type.as_str(), "vote.create" | "webhook.test") {
            let data = object_field(&payload, "data");
            let user = data.and_then(|data| object_field(data, "user"));
            let project = data.and_then(|data| object_field(data, "project"));
            (
                user.and_then(|user| string_field(user, "platform_id"))
                    .unwrap_or_default(),
                project.and_then(|project| string_field(project, "platform_id")),
                data.and_then(|data| string_field(data, "id")),
            )
        } else {
            (
                string_field(&payload, "user").unwrap_or_default(),
                string_field(&payload, "bot"),
                None,
            )
        };
    let is_upvote = matches!(event_type.as_str(), "upvote" | "vote.create");
    if is_upvote && expected_bot_id.is_some_and(|expected| bot_id.as_deref() != Some(expected)) {
        return TopggWebhookDecision::Rejected(TopggWebhookRejection::WrongProject);
    }
    if is_upvote && !user_id.is_empty() {
        let voted_at =
            object_field(&payload, "data").and_then(|data| string_field(data, "created_at"));
        TopggWebhookDecision::Upvote(TopggVote {
            user_id,
            event_id,
            voted_at,
        })
    } else {
        TopggWebhookDecision::Acknowledged
    }
}

fn legacy_authorization_matches(authorization: Option<&str>, secret: &str) -> bool {
    let received = Sha256::digest(authorization.unwrap_or_default().as_bytes());
    let expected = Sha256::digest(secret.as_bytes());
    received.as_slice().ct_eq(expected.as_slice()).into()
}

fn v1_signature_matches(signature: &str, body: &str, secret: &str, now_ms: i64) -> bool {
    let mut timestamp = None;
    let mut received = None;
    for part in signature.split(',') {
        let (name, value) = part.split_once('=').unwrap_or((part, ""));
        match name.trim() {
            "t" => timestamp = Some(value.trim()),
            "v1" => received = Some(value.trim()),
            _ => {}
        }
    }
    let (Some(timestamp), Some(received)) = (timestamp, received) else {
        return false;
    };
    if !timestamp.bytes().all(|byte| byte.is_ascii_digit())
        || received.len() != 64
        || !received.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    let Ok(seconds) = timestamp.parse::<i64>() else {
        return false;
    };
    let Some(timestamp_ms) = seconds.checked_mul(1_000) else {
        return false;
    };
    if now_ms.saturating_sub(timestamp_ms).unsigned_abs() > TOPGG_SIGNATURE_TOLERANCE_MS as u64 {
        return false;
    }
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    let expected = hex_lower(&mac.finalize().into_bytes());
    expected.as_bytes().ct_eq(received.as_bytes()).into()
}

fn object_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Option<&'a serde_json::Map<String, Value>> {
    object.get(key)?.as_object()
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object.get(key)?.as_str().map(str::to_owned)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "a sufficiently-long-local-test-secret";
    const NOW: i64 = 1_700_000_000_000;

    fn v1_header(body: &str) -> String {
        let timestamp = (NOW / 1_000).to_string();
        let mut mac = HmacSha256::new_from_slice(SECRET.as_bytes()).expect("hmac key");
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body.as_bytes());
        format!(
            "t={timestamp},v1={}",
            hex_lower(&mac.finalize().into_bytes())
        )
    }

    #[test]
    fn v1_upvote_requires_raw_body_signature_freshness_and_our_project() {
        let body = r#"{"type":"vote.create","data":{"id":"event","user":{"platform_id":"user"},"project":{"platform_id":"bot"}}}"#;
        assert_eq!(
            verify_topgg_webhook(
                None,
                Some(&v1_header(body)),
                body,
                Some(SECRET),
                NOW,
                Some("bot"),
            ),
            TopggWebhookDecision::Upvote(TopggVote {
                voted_at: None,
                user_id: "user".into(),
                event_id: Some("event".into()),
            })
        );
        assert_eq!(
            verify_topgg_webhook(
                None,
                Some(&v1_header(body)),
                &format!("{body} "),
                Some(SECRET),
                NOW,
                Some("bot"),
            ),
            TopggWebhookDecision::Rejected(TopggWebhookRejection::Unauthorized)
        );
        assert_eq!(
            verify_topgg_webhook(
                None,
                Some(&v1_header(body)),
                body,
                Some(SECRET),
                NOW + TOPGG_SIGNATURE_TOLERANCE_MS + 1,
                Some("bot"),
            ),
            TopggWebhookDecision::Rejected(TopggWebhookRejection::Unauthorized)
        );
    }

    #[test]
    fn legacy_auth_and_acknowledged_events_preserve_node_compatibility() {
        assert_eq!(
            verify_topgg_webhook(
                Some(SECRET),
                None,
                r#"{"type":"upvote","user":"user","bot":"bot"}"#,
                Some(SECRET),
                NOW,
                Some("bot"),
            ),
            TopggWebhookDecision::Upvote(TopggVote {
                voted_at: None,
                user_id: "user".into(),
                event_id: None,
            })
        );
        assert_eq!(
            verify_topgg_webhook(
                Some(SECRET),
                None,
                r#"{"type":"test"}"#,
                Some(SECRET),
                NOW,
                Some("bot"),
            ),
            TopggWebhookDecision::Acknowledged
        );
    }

    #[test]
    fn malformed_or_cross_project_payloads_never_become_votes() {
        assert_eq!(
            verify_topgg_webhook(Some(SECRET), None, "{", Some(SECRET), NOW, Some("bot")),
            TopggWebhookDecision::Rejected(TopggWebhookRejection::InvalidJson)
        );
        assert_eq!(
            verify_topgg_webhook(Some(SECRET), None, "[]", Some(SECRET), NOW, Some("bot")),
            TopggWebhookDecision::Rejected(TopggWebhookRejection::InvalidPayload)
        );
        assert_eq!(
            verify_topgg_webhook(
                Some(SECRET),
                None,
                r#"{"type":"upvote","user":"user","bot":"other"}"#,
                Some(SECRET),
                NOW,
                Some("bot"),
            ),
            TopggWebhookDecision::Rejected(TopggWebhookRejection::WrongProject)
        );
    }
}
