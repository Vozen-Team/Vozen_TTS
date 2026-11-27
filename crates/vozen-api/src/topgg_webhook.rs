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
    pub giveaway_webhook_secrets: [Option<String>; 2],
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
    giveaway_only: bool,
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
    let state = TopggWebhookState {
        webhook_secret: Arc::from(config.webhook_secret),
        redemption_secret: Arc::from(config.redemption_secret),
        expected_bot_id: Arc::from(config.expected_bot_id),
        giveaway_only: false,
        store: config.store,
        metrics: config.metrics,
        now: config.now,
    };
    let mut router = Router::new()
        .route("/webhook/topgg", any(topgg_webhook))
        .with_state(state.clone());
    for ((path, bot_id), secret) in [
        ("/webhook/topgg/helper", vozen_core::GIVEAWAY_VOTE_BOTS[1].1),
        ("/webhook/topgg/music", vozen_core::GIVEAWAY_VOTE_BOTS[2].1),
    ]
    .into_iter()
    .zip(config.giveaway_webhook_secrets)
    {
        if let Some(secret) = secret {
            if secret.trim().is_empty() {
                return Err(TopggWebhookConfigError::WebhookSecret);
            }
            router = router.merge(Router::new().route(path, any(topgg_webhook)).with_state(
                TopggWebhookState {
                    webhook_secret: Arc::from(secret),
                    expected_bot_id: Arc::from(bot_id),
                    giveaway_only: true,
                    ..state.clone()
                },
            ));
        }
    }
    Ok(router.layer(DefaultBodyLimit::max(BODY_MAX_BYTES)))
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
        TopggWebhookDecision::Upvote(vote) if state.giveaway_only => {
            let Some((event_id, voted_at)) = vote
                .event_id
                .as_deref()
                .zip(parse_vote_time(vote.voted_at.as_deref()))
            else {
                return status(StatusCode::OK, "ok");
            };
            match state.store.lock() {
                Ok(store) => match store.record_giveaway_vote(
                    &state.expected_bot_id,
                    event_id,
                    &vote.user_id,
                    voted_at,
                    (state.now)(),
                ) {
                    Ok(_) => status(StatusCode::OK, "ok"),
                    Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR, "entry_failed"),
                },
                Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR, "entry_failed"),
            }
        }
        TopggWebhookDecision::Upvote(vote) => match state.store.lock() {
            Ok(store) => match store.claim_topgg_vote_reward_at(
                vote.event_id.as_deref(),
                &vote.user_id,
                (state.now)(),
                &state.redemption_secret,
                parse_vote_time(vote.voted_at.as_deref()),
            ) {
                Ok(TopggVoteRewardResult::DuplicateEvent) => status(StatusCode::OK, "duplicate"),
                Ok(TopggVoteRewardResult::Granted { .. } | TopggVoteRewardResult::RateLimited) => {
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

fn parse_vote_time(value: Option<&str>) -> Option<i64> {
    value
        .and_then(|value| {
            time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
        })
        .and_then(|value| i64::try_from(value.unix_timestamp_nanos() / 1_000_000).ok())
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
            giveaway_webhook_secrets: [None, None],
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
            giveaway_webhook_secrets: [None, None],
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
    #[tokio::test]
    async fn giveaways_count_verified_votes_once_even_after_the_plus_reward_cap() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap()));
        let id = {
            let guard = store.lock().unwrap();
            let action = serde_json::from_value(json!({
                "action":"save","request_id":"ea01298f-a728-489a-9639-856befd711cc",
                "channel_id":"200","title":"","description":"","plan":"plus","days":30,"seats":0,
                "winner_count":1,"starts_at":NOW-1000,"ends_at":NOW+7*86400000,"claim_hours":48
            }))
            .unwrap();
            let id = guard
                .giveaway_admin("100", action, NOW - 1000)
                .unwrap()
                .unwrap();
            guard
                .giveaway_admin(
                    "100",
                    vozen_store::GiveawayAction::Schedule { id },
                    NOW - 1000,
                )
                .unwrap()
                .unwrap();
            guard.giveaway_published(id, "300").unwrap();
            id
        };
        for (index, event) in [
            "vote-1", "vote-1", "vote-2", "vote-3", "vote-4", "vote-5", "vote-6",
        ]
        .into_iter()
        .enumerate()
        {
            let now = NOW + index as i64 * 12 * 3600000;
            let voted_at = time::OffsetDateTime::from_unix_timestamp(now / 1000)
                .unwrap()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
            let app = topgg_webhook_router(TopggWebhookConfig {
                webhook_secret: SECRET.into(),
                redemption_secret: REDEMPTION_SECRET.into(),
                expected_bot_id: BOT.into(),
                giveaway_webhook_secrets: [None, None],
                store: store.clone(),
                metrics: None,
                now: Arc::new(move || now),
            })
            .unwrap();
            let body = json!({"type":"vote.create","data":{"id":event,"created_at":voted_at,
                "user":{"platform_id":USER},"project":{"platform_id":BOT}}})
            .to_string();
            let timestamp = (now / 1000).to_string();
            let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
            mac.update(timestamp.as_bytes());
            mac.update(b".");
            mac.update(body.as_bytes());
            let signature = mac
                .finalize()
                .into_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let req = Request::builder()
                .method("POST")
                .uri("/webhook/topgg")
                .header("x-topgg-signature", format!("t={timestamp},v1={signature}"))
                .body(Body::from(body))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(req).await.unwrap().status(),
                StatusCode::OK
            );
        }
        let guard = store.lock().unwrap();
        assert_eq!(guard.giveaways("100").unwrap()[0].entries, 1);
        assert_eq!(guard.giveaway_candidates(id, true).unwrap().len(), 1);
    }
    #[tokio::test]
    async fn three_project_votes_are_authenticated_capped_and_do_not_share_plus_rewards() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap()));
        {
            let guard = store.lock().unwrap();
            let save = serde_json::from_value(json!({
                "action":"save","request_id":"ea01298f-a728-489a-9639-856befd711cd","channel_id":"200",
                "title":"Three votes","description":"","plan":"plus","days":30,"seats":0,
                "winner_count":1,"starts_at":NOW-1000,"ends_at":NOW+3600000,"claim_hours":48
            }))
            .unwrap();
            let id = guard
                .giveaway_admin("100", save, NOW - 1000)
                .unwrap()
                .unwrap();
            guard
                .giveaway_admin(
                    "100",
                    vozen_store::GiveawayAction::Schedule { id },
                    NOW - 1000,
                )
                .unwrap();
            guard.giveaway_published(id, "300").unwrap();
        }
        let helper_secret = "whs_helper_project_secret";
        let music_secret = "whs_music_project_secret";
        let app = topgg_webhook_router(TopggWebhookConfig {
            expected_bot_id: vozen_core::GIVEAWAY_VOTE_BOTS[0].1.into(),
            webhook_secret: SECRET.into(),
            redemption_secret: REDEMPTION_SECRET.into(),
            giveaway_webhook_secrets: [Some(helper_secret.into()), Some(music_secret.into())],
            store: store.clone(),
            metrics: None,
            now: Arc::new(|| NOW),
        })
        .unwrap();
        let signed = |path: &str, bot: &str, event: &str, secret: &str| {
            let at = time::OffsetDateTime::from_unix_timestamp(NOW / 1000)
                .unwrap()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
            let body = json!({"type":"vote.create","data":{"id":event,"created_at":at,
                "user":{"platform_id":USER},"project":{"platform_id":bot}}})
            .to_string();
            let timestamp = (NOW / 1000).to_string();
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
            mac.update(format!("{timestamp}.{body}").as_bytes());
            let signature = mac
                .finalize()
                .into_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            Request::builder()
                .method("POST")
                .uri(path)
                .header("x-topgg-signature", format!("t={timestamp},v1={signature}"))
                .body(Body::from(body))
                .unwrap()
        };
        for (index, path, secret) in [
            (1, "/webhook/topgg/helper", helper_secret),
            (2, "/webhook/topgg/music", music_secret),
        ] {
            let bot = vozen_core::GIVEAWAY_VOTE_BOTS[index].1;
            for event in ["first", "first", "later"] {
                let req = signed(path, bot, &format!("{index}-{event}"), secret);
                assert_eq!(
                    app.clone().oneshot(req).await.unwrap().status(),
                    StatusCode::OK
                );
            }
            assert!(
                store
                    .lock()
                    .unwrap()
                    .vote_reward_at(USER)
                    .unwrap()
                    .is_none()
            );
        }
        let tts = vozen_core::GIVEAWAY_VOTE_BOTS[0].1;
        for event in ["tts-first", "tts-first"] {
            assert_eq!(
                app.clone()
                    .oneshot(signed("/webhook/topgg", tts, event, SECRET))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
        }
        let helper = vozen_core::GIVEAWAY_VOTE_BOTS[1].1;
        assert_eq!(
            app.clone()
                .oneshot(signed(
                    "/webhook/topgg/helper",
                    helper,
                    "bad-secret",
                    music_secret
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.oneshot(signed(
                "/webhook/topgg/helper",
                tts,
                "wrong-project",
                helper_secret
            ))
            .await
            .unwrap()
            .status(),
            StatusCode::BAD_REQUEST
        );
        let guard = store.lock().unwrap();
        assert_eq!(guard.giveaways("100").unwrap()[0].entries, 3);
        assert!(guard.vote_reward_at(USER).unwrap().is_some());
    }
}
