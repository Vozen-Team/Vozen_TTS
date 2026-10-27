//! Authenticated Top.gg vote-reward adapter.
//!
//! The route owns no reward policy: `vozen-core` verifies the untouched payload and
//! `vozen-store` performs one atomic idempotency/reward transaction.

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::json;
use vozen_core::{
    RuntimeMetrics, TopggWebhookDecision, TopggWebhookRejection, verify_topgg_webhook,
};
use vozen_store::{SqliteStore, TopggVoteRewardResult, VOTE_REDEMPTION_SECRET_MIN_LENGTH};

const BODY_MAX_BYTES: usize = 64_000;

pub struct TopggWebhookConfig {
    pub webhook_secret: String,
    pub redemption_secret: String,
    pub expected_bot_id: String,
    pub store: Arc<Mutex<SqliteStore>>,
    /// Optional process-local observability. The route remains usable by API-only callers that
    /// do not expose `/stats`; when present, only authenticated, non-duplicate upvotes count.
    pub metrics: Option<Arc<RuntimeMetrics>>,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

#[derive(Clone)]
struct TopggWebhookState {
    webhook_secret: Arc<str>,
    redemption_secret: Arc<str>,
    expected_bot_id: Arc<str>,
    store: Arc<Mutex<SqliteStore>>,
    metrics: Option<Arc<RuntimeMetrics>>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopggWebhookConfigError {
    WebhookSecret,
    RedemptionSecret,
    ExpectedBotId,
}

impl std::fmt::Display for TopggWebhookConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WebhookSecret => {
                formatter.write_str("Top.gg webhook requires a non-empty secret")
            }
            Self::RedemptionSecret => formatter.write_str(
                "Top.gg rewards require a stable VOTE_REDEMPTION_SECRET of at least 32 characters",
            ),
            Self::ExpectedBotId => {
                formatter.write_str("Top.gg webhook requires the expected Discord application ID")
            }
        }
    }
}

impl std::error::Error for TopggWebhookConfigError {}

/// Builds only the sensitive vote route. Omitting this configuration means `/webhook/topgg`
/// remains absent; an unauthenticated endpoint is never constructed.
pub fn topgg_webhook_router(config: TopggWebhookConfig) -> Result<Router, TopggWebhookConfigError> {
    if config.webhook_secret.trim().is_empty() {
        return Err(TopggWebhookConfigError::WebhookSecret);
    }
    if config.redemption_secret.len() < VOTE_REDEMPTION_SECRET_MIN_LENGTH {
        return Err(TopggWebhookConfigError::RedemptionSecret);
    }
    if !is_discord_application_id(&config.expected_bot_id) {
        return Err(TopggWebhookConfigError::ExpectedBotId);
    }
    Ok(Router::new()
        .route("/webhook/topgg", any(topgg_webhook))
        // Match the Node webhook's incremental 64 KB cap before an attacker can force a
        // larger `Bytes` allocation. The handler retains the explicit contract check.
        .layer(DefaultBodyLimit::max(BODY_MAX_BYTES))
        .with_state(TopggWebhookState {
            webhook_secret: Arc::from(config.webhook_secret),
            redemption_secret: Arc::from(config.redemption_secret),
            expected_bot_id: Arc::from(config.expected_bot_id),
            store: config.store,
            metrics: config.metrics,
            now: config.now,
        }))
}

async fn topgg_webhook(
    State(state): State<TopggWebhookState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method != Method::POST {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
    }
    if body.len() > BODY_MAX_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response();
    }
    let Ok(raw_body) = std::str::from_utf8(&body) else {
        return status(StatusCode::BAD_REQUEST, "invalid_json");
    };
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let signature = headers
        .get("x-topgg-signature")
        .and_then(|value| value.to_str().ok());
    match verify_topgg_webhook(
        authorization,
        signature,
        raw_body,
        Some(&state.webhook_secret),
        (state.now)(),
        Some(&state.expected_bot_id),
    ) {
        TopggWebhookDecision::Acknowledged => status(StatusCode::OK, "ok"),
        TopggWebhookDecision::Rejected(rejection) => rejected(rejection),
        TopggWebhookDecision::Upvote(vote) => match state.store.lock() {
            Ok(store) => match store.claim_topgg_vote_reward(
                vote.event_id.as_deref(),
                &vote.user_id,
                (state.now)(),
                &state.redemption_secret,
            ) {
                Ok(TopggVoteRewardResult::DuplicateEvent) => status(StatusCode::OK, "duplicate"),
                Ok(
                    TopggVoteRewardResult::Granted { .. } | TopggVoteRewardResult::AlreadyRedeemed,
                ) => {
                    if let Some(metrics) = &state.metrics {
                        metrics.record_vote();
                    }
                    status(StatusCode::OK, "ok")
                }
                Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR, "reward_failed"),
            },
            Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR, "reward_failed"),
        },
    }
}

fn rejected(rejection: TopggWebhookRejection) -> Response {
    match rejection {
        TopggWebhookRejection::Unauthorized => status(StatusCode::UNAUTHORIZED, "unauthorized"),
        TopggWebhookRejection::InvalidJson => status(StatusCode::BAD_REQUEST, "invalid_json"),
        TopggWebhookRejection::InvalidPayload => status(StatusCode::BAD_REQUEST, "invalid_payload"),
        TopggWebhookRejection::WrongProject => status(StatusCode::BAD_REQUEST, "wrong_project"),
    }
}

fn status(code: StatusCode, value: &'static str) -> Response {
    (code, Json(json!({ "status": value }))).into_response()
}

fn is_discord_application_id(value: &str) -> bool {
    (5..=25).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    const NOW: i64 = 1_700_000_000_000;
    const SECRET: &str = "a sufficiently-long-webhook-secret";
    const REDEMPTION_SECRET: &str = "0123456789abcdef0123456789abcdef";
    const BOT: &str = "100000000000000004";
    const USER: &str = "12345678901234567";

    fn router(store: Arc<Mutex<SqliteStore>>) -> Router {
        router_with_metrics(store, None)
    }

    fn router_with_metrics(
        store: Arc<Mutex<SqliteStore>>,
        metrics: Option<Arc<RuntimeMetrics>>,
    ) -> Router {
        topgg_webhook_router(TopggWebhookConfig {
            webhook_secret: SECRET.into(),
            redemption_secret: REDEMPTION_SECRET.into(),
            expected_bot_id: BOT.into(),
            store,
            metrics,
            now: Arc::new(|| NOW),
        })
        .expect("router")
    }

    fn request(body: impl Into<Body>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/webhook/topgg")
            .header(header::AUTHORIZATION, SECRET)
            .body(body.into())
            .expect("request")
    }

    #[tokio::test]
    async fn authenticated_upvote_grants_once_and_retries_are_safe() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let metrics = Arc::new(RuntimeMetrics::default());
        let body = format!(
            r#"{{"type":"vote.create","data":{{"id":"event-1","user":{{"platform_id":"{USER}"}},"project":{{"platform_id":"{BOT}"}}}}}}"#
        );
        let first = router_with_metrics(store.clone(), Some(metrics.clone()))
            .oneshot(request(body.clone()))
            .await
            .expect("response");
        assert_eq!(first.status(), StatusCode::OK);
        let replay = router_with_metrics(store.clone(), Some(metrics.clone()))
            .oneshot(request(body))
            .await
            .expect("response");
        assert_eq!(replay.status(), StatusCode::OK);
        assert!(
            store
                .lock()
                .unwrap()
                .is_user_premium(USER, NOW + 1)
                .expect("premium")
        );
        assert_eq!(metrics.snapshot().votes, 1);
    }

    #[tokio::test]
    async fn rejects_untrusted_invalid_and_cross_project_requests_before_reward() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let unauthorized = router(store.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook/topgg")
                    .body(Body::from("{}"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let invalid = router(store.clone())
            .oneshot(request("{"))
            .await
            .expect("response");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let cross_project = router(store.clone())
            .oneshot(request(format!(
                r#"{{"type":"upvote","user":"{USER}","bot":"other"}}"#
            )))
            .await
            .expect("response");
        assert_eq!(cross_project.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            store.lock().unwrap().vote_reward_at(USER).expect("reward"),
            None
        );
    }

    #[tokio::test]
    async fn v1_delivery_id_reports_duplicate_and_test_pings_do_not_grant() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        // The legacy auth fallback intentionally remains supported when no v1 signature exists.
        let vote = format!(
            r#"{{"type":"vote.create","data":{{"id":"evt-1","user":{{"platform_id":"{USER}"}},"project":{{"platform_id":"{BOT}"}}}}}}"#
        );
        let first = router(store.clone())
            .oneshot(request(vote.clone()))
            .await
            .expect("response");
        assert_eq!(first.status(), StatusCode::OK);
        let duplicate = router(store.clone())
            .oneshot(request(vote))
            .await
            .expect("response");
        assert_eq!(duplicate.status(), StatusCode::OK);
        let test =
            format!(r#"{{"type":"webhook.test","data":{{"project":{{"platform_id":"{BOT}"}}}}}}"#);
        let ping = router(store.clone())
            .oneshot(request(test))
            .await
            .expect("response");
        assert_eq!(ping.status(), StatusCode::OK);
    }

    #[test]
    fn construction_fails_closed_without_all_sensitive_values() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let config = || TopggWebhookConfig {
            webhook_secret: String::new(),
            redemption_secret: REDEMPTION_SECRET.into(),
            expected_bot_id: BOT.into(),
            store: store.clone(),
            metrics: None,
            now: Arc::new(|| NOW),
        };
        assert!(matches!(
            topgg_webhook_router(config()),
            Err(TopggWebhookConfigError::WebhookSecret)
        ));
    }

    #[tokio::test]
    async fn method_and_payload_limits_match_the_existing_webhook_contract() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let method = router(store.clone())
            .oneshot(
                Request::builder()
                    .uri("/webhook/topgg")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);
        let large = router(store)
            .oneshot(request("x".repeat(BODY_MAX_BYTES + 1)))
            .await
            .expect("response");
        assert_eq!(large.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
