//! Optional fatal-error reporting for the Rust runtime.
//!
//! Reporting is deliberately secondary: a missing/invalid webhook, a network failure, or a
//! poisoned deduplication lock never changes the process outcome. Secrets are removed before the
//! payload is built, and the in-memory dedup set is bounded like the Node implementation.

use std::{
    collections::HashSet,
    env,
    hash::{Hash, Hasher},
    sync::Mutex,
    time::Duration,
};

use reqwest::{Client, Url};
use serde_json::json;

const DEDUP_CAP: usize = 500;
const MAX_BODY_CHARS: usize = 1_500;
const MAX_CONTENT_CHARS: usize = 1_900;

pub struct ErrorReporter {
    url: Option<Url>,
    client: Client,
    seen: Mutex<HashSet<u64>>,
    secrets: Vec<String>,
}

impl ErrorReporter {
    pub fn from_environment() -> Self {
        let url = env::var("ERROR_WEBHOOK_URL")
            .ok()
            .and_then(|raw| Url::parse(raw.trim()).ok())
            .filter(|url| url.scheme() == "https");
        let secrets = [
            "DISCORD_TOKEN",
            "ERROR_WEBHOOK_URL",
            "TOPGG_WEBHOOK_SECRET",
            "TOPGG_HELPER_WEBHOOK_SECRET",
            "TOPGG_MUSIC_WEBHOOK_SECRET",
            "VOTE_REDEMPTION_SECRET",
            "ADMIN_SESSION_SECRET",
            "OAUTH_CLIENT_SECRET",
        ]
        .into_iter()
        .filter_map(|name| env::var(name).ok())
        .filter(|value| value.len() >= 8)
        .collect();
        Self {
            url,
            client: Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap_or_else(|_| Client::new()),
            seen: Mutex::new(HashSet::new()),
            secrets,
        }
    }

    /// Sends one fatal error at most once per process/error fingerprint. Never returns an error;
    /// the caller is already handling a fatal path and must not be made less reliable by this
    /// optional observability channel.
    pub async fn report(&self, error: &str, context: &str) {
        let Some(url) = &self.url else {
            return;
        };
        let content = format_error_message(error, context, &self.secrets);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        let fingerprint = hasher.finish();
        let should_send = {
            let Ok(mut seen) = self.seen.lock() else {
                return;
            };
            if seen.contains(&fingerprint) {
                false
            } else {
                if seen.len() >= DEDUP_CAP {
                    seen.clear();
                }
                seen.insert(fingerprint);
                true
            }
        };
        if !should_send {
            return;
        }

        let result = self
            .client
            .post(url.clone())
            .json(&json!({ "content": content }))
            .send()
            .await;
        let success = result
            .map(|response| response.status().is_success())
            .unwrap_or(false);
        if !success && let Ok(mut seen) = self.seen.lock() {
            seen.remove(&fingerprint);
        }
    }
}

fn format_error_message(error: &str, context: &str, secrets: &[String]) -> String {
    let body = scrub(error, secrets);
    let context = scrub(context, secrets);
    let body: String = body.chars().take(MAX_BODY_CHARS).collect();
    let mut content = format!("⚠️ **Vozen** — erro em `{context}`\n```\n{body}\n```");
    if content.chars().count() > MAX_CONTENT_CHARS {
        content = content.chars().take(MAX_CONTENT_CHARS - 4).collect();
        content.push_str("\n```");
    }
    content
}

fn scrub(text: &str, secrets: &[String]) -> String {
    secrets.iter().fold(text.to_owned(), |value, secret| {
        value.replace(secret, "[redigido]")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_redacts_configured_secrets_and_caps_body() {
        let secret = "secret-value-123".to_owned();
        let message = format_error_message(
            &format!("failure {secret} {}", "x".repeat(2_000)),
            "runtime",
            std::slice::from_ref(&secret),
        );
        assert!(!message.contains(&secret));
        assert!(message.chars().count() <= MAX_CONTENT_CHARS);
        assert!(message.contains("[redigido]"));
    }

    #[test]
    fn invalid_or_missing_webhook_is_a_noop_configuration() {
        let reporter = ErrorReporter::from_environment();
        assert!(
            reporter.url.is_none()
                || reporter
                    .url
                    .as_ref()
                    .is_some_and(|url| url.scheme() == "https")
        );
    }
}
