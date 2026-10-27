//! Opt-in Top.gg server-count publishing.
//!
//! Listing availability is never part of Discord's critical path. Requests are bounded and all
//! failures become observable private health data; this integration only uses the current v1
//! API and never silently falls back to a legacy token or endpoint.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Notify;
use vozen_store::TopggSyncDetail;

const V1_PROJECT_URL: &str = "https://top.gg/api/v1/projects/@me";
const V1_METRICS_URL: &str = "https://top.gg/api/v1/projects/@me/metrics";
const V1_COMMANDS_URL: &str = "https://top.gg/api/v1/projects/@me/commands";
pub const TOPGG_POST_INTERVAL: Duration = Duration::from_secs(30 * 60);
const TOPGG_TIMEOUT: Duration = Duration::from_secs(10);

/// Coalesces guild lifecycle changes into an immediate Top.gg publish. `Notify` intentionally
/// keeps at most one pending wake-up so a burst of Guild Create/Delete events cannot cause a
/// request storm.
#[derive(Clone, Default)]
pub struct TopggMetricsTrigger(Arc<Notify>);

impl TopggMetricsTrigger {
    pub fn request_sync(&self) {
        self.0.notify_one();
    }

    pub async fn notified(&self) {
        self.0.notified().await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopggMetricsRequest {
    pub url: String,
    pub method: TopggMetricsMethod,
    pub authorization: String,
    pub body: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopggMetricsMethod {
    Get,
    Patch,
    Put,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopggMetricsResponse {
    pub status: u16,
}

/// The privacy-safe result of a metrics publish attempt. It deliberately keeps no response
/// body: Top.gg can return problem details that are useful for an operator but must never be
/// copied into a public status endpoint or logs unbounded remote data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopggMetricsOutcome {
    Success { status: u16 },
    HttpFailure { status: u16 },
    TransportFailure,
    InvalidConfiguration,
}

impl TopggMetricsOutcome {
    pub const fn succeeded(self) -> bool {
        matches!(self, Self::Success { .. })
    }

    pub const fn status(self) -> Option<u16> {
        match self {
            Self::Success { status } | Self::HttpFailure { status } => Some(status),
            Self::TransportFailure | Self::InvalidConfiguration => None,
        }
    }

    pub const fn detail(self) -> TopggSyncDetail {
        match self {
            Self::Success { .. } => TopggSyncDetail::Delivered,
            Self::HttpFailure { status: 401 | 403 } => TopggSyncDetail::V1AuthenticationFailed,
            Self::HttpFailure { status: 404 } => TopggSyncDetail::ProjectNotFound,
            Self::HttpFailure { status: 400 | 422 } => TopggSyncDetail::InvalidMetricsPayload,
            Self::HttpFailure { status: 429 } => TopggSyncDetail::RateLimited,
            Self::HttpFailure { .. } => TopggSyncDetail::HttpFailure,
            Self::TransportFailure => TopggSyncDetail::TransportFailure,
            Self::InvalidConfiguration => TopggSyncDetail::InvalidConfiguration,
        }
    }
}

#[async_trait]
pub trait TopggMetricsHttp: Send + Sync {
    async fn send(&self, request: TopggMetricsRequest) -> Result<TopggMetricsResponse, ()>;
}

pub struct ReqwestTopggMetricsHttp {
    client: reqwest::Client,
}

impl ReqwestTopggMetricsHttp {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder().timeout(TOPGG_TIMEOUT).build()?,
        })
    }
}

#[async_trait]
impl TopggMetricsHttp for ReqwestTopggMetricsHttp {
    async fn send(&self, request: TopggMetricsRequest) -> Result<TopggMetricsResponse, ()> {
        let TopggMetricsRequest {
            url,
            method,
            authorization,
            body,
        } = request;
        let method = match method {
            TopggMetricsMethod::Get => reqwest::Method::GET,
            TopggMetricsMethod::Patch => reqwest::Method::PATCH,
            TopggMetricsMethod::Put => reqwest::Method::PUT,
        };
        let request = self
            .client
            .request(method, url)
            .header(reqwest::header::AUTHORIZATION, authorization);
        let request = if let Some(body) = body {
            request.json(&body)
        } else {
            request
        };
        let response = request.send().await.map_err(|_| ())?;
        Ok(TopggMetricsResponse {
            status: response.status().as_u16(),
        })
    }
}

/// Verifies the current Top.gg v1 token before any mutable request is made.
///
/// v0 credentials cannot authenticate to this Bearer-only endpoint. Top.gg
/// intentionally does not expose enough detail to distinguish a revoked token
/// from a legacy one, so both are reported as a single actionable, sanitized
/// v1 authentication failure.
pub async fn validate_topgg_v1_token(
    http: &impl TopggMetricsHttp,
    token: &str,
) -> TopggMetricsOutcome {
    if token.trim().is_empty() {
        return TopggMetricsOutcome::InvalidConfiguration;
    }
    match http
        .send(TopggMetricsRequest {
            url: V1_PROJECT_URL.into(),
            method: TopggMetricsMethod::Get,
            authorization: format!("Bearer {token}"),
            body: None,
        })
        .await
    {
        Ok(response) if (200..300).contains(&response.status) => TopggMetricsOutcome::Success {
            status: response.status,
        },
        Ok(response) => TopggMetricsOutcome::HttpFailure {
            status: response.status,
        },
        Err(()) => TopggMetricsOutcome::TransportFailure,
    }
}

/// Sends only a non-negative exact count. Returns false for every remote error so an outage
/// cannot interfere with Discord startup or command handling.
#[cfg(test)]
pub async fn post_topgg_stats(
    http: &impl TopggMetricsHttp,
    bot_id: &str,
    token: &str,
    server_count: usize,
) -> bool {
    post_topgg_stats_with_shards(http, bot_id, token, server_count, 1)
        .await
        .succeeded()
}

/// Publishes an exact guild count and the number of active gateway shards. The detailed outcome
/// is consumed by the runtime's private health surface so configuration/authentication failures
/// do not disappear into a boolean while Discord itself keeps serving normally.
pub async fn post_topgg_stats_with_shards(
    http: &impl TopggMetricsHttp,
    bot_id: &str,
    token: &str,
    server_count: usize,
    shard_count: usize,
) -> TopggMetricsOutcome {
    if token.trim().is_empty() || !is_discord_application_id(bot_id) {
        return TopggMetricsOutcome::InvalidConfiguration;
    }
    let v1 = http
        .send(TopggMetricsRequest {
            url: V1_METRICS_URL.into(),
            method: TopggMetricsMethod::Patch,
            authorization: format!("Bearer {token}"),
            body: Some(serde_json::json!({
                "server_count": server_count,
                "shard_count": shard_count.max(1),
            })),
        })
        .await;
    match v1 {
        Ok(response) if (200..300).contains(&response.status) => TopggMetricsOutcome::Success {
            status: response.status,
        },
        Ok(response) => TopggMetricsOutcome::HttpFailure {
            status: response.status,
        },
        Err(()) => TopggMetricsOutcome::TransportFailure,
    }
}

/// Syncs exactly the already-validated public Discord registration payload. Owner-only commands
/// are intentionally excluded because they do not belong in a public discovery listing.
pub async fn sync_topgg_commands(
    http: &impl TopggMetricsHttp,
    token: &str,
    commands: Vec<Value>,
) -> bool {
    if token.trim().is_empty() {
        return false;
    }
    http.send(TopggMetricsRequest {
        url: V1_COMMANDS_URL.into(),
        method: TopggMetricsMethod::Put,
        authorization: format!("Bearer {token}"),
        body: Some(Value::Array(commands)),
    })
    .await
    .is_ok_and(|response| (200..300).contains(&response.status))
}

fn is_discord_application_id(value: &str) -> bool {
    (5..=25).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use super::*;

    struct FakeHttp {
        responses: Mutex<VecDeque<Result<TopggMetricsResponse, ()>>>,
        requests: Mutex<Vec<TopggMetricsRequest>>,
    }

    #[async_trait]
    impl TopggMetricsHttp for FakeHttp {
        async fn send(&self, request: TopggMetricsRequest) -> Result<TopggMetricsResponse, ()> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(()))
        }
    }

    fn fake(responses: impl IntoIterator<Item = Result<TopggMetricsResponse, ()>>) -> FakeHttp {
        FakeHttp {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    const BOT: &str = "100000000000000004";

    #[tokio::test]
    async fn v1_success_uses_bearer_auth_without_a_fallback() {
        let http = fake([Ok(TopggMetricsResponse { status: 204 })]);
        assert!(post_topgg_stats(&http, BOT, "token", 42).await);
        assert_eq!(
            http.requests.lock().unwrap().as_slice(),
            &[TopggMetricsRequest {
                url: V1_METRICS_URL.into(),
                method: TopggMetricsMethod::Patch,
                authorization: "Bearer token".into(),
                body: Some(serde_json::json!({ "server_count": 42, "shard_count": 1 })),
            }]
        );
    }

    #[tokio::test]
    async fn missing_v1_endpoint_is_an_observable_failure_without_legacy_fallback() {
        let http = fake([Ok(TopggMetricsResponse { status: 404 })]);
        assert_eq!(
            post_topgg_stats_with_shards(&http, BOT, "token", 3, 1).await,
            TopggMetricsOutcome::HttpFailure { status: 404 }
        );
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn v1_token_validation_uses_the_read_only_project_endpoint() {
        let http = fake([Ok(TopggMetricsResponse { status: 200 })]);
        assert_eq!(
            validate_topgg_v1_token(&http, "token").await,
            TopggMetricsOutcome::Success { status: 200 }
        );
        assert_eq!(
            http.requests.lock().unwrap().as_slice(),
            &[TopggMetricsRequest {
                url: V1_PROJECT_URL.into(),
                method: TopggMetricsMethod::Get,
                authorization: "Bearer token".into(),
                body: None,
            }]
        );
    }

    #[tokio::test]
    async fn v1_token_authentication_failure_has_a_sanitized_actionable_detail() {
        let http = fake([Ok(TopggMetricsResponse { status: 401 })]);
        let outcome = validate_topgg_v1_token(&http, "old-token").await;
        assert_eq!(outcome.status(), Some(401));
        assert_eq!(outcome.detail(), TopggSyncDetail::V1AuthenticationFailed);
    }

    #[tokio::test]
    async fn auth_remote_and_transport_errors_do_not_try_a_legacy_endpoint() {
        for response in [Ok(TopggMetricsResponse { status: 401 }), Err(())] {
            let http = fake([response]);
            assert!(!post_topgg_stats(&http, BOT, "token", 1).await);
            assert_eq!(http.requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn invalid_configuration_never_sends_a_request() {
        let http = fake([Ok(TopggMetricsResponse { status: 200 })]);
        assert!(!post_topgg_stats(&http, "bot", "", 1).await);
        assert!(http.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn detailed_outcome_keeps_the_http_status_without_exposing_remote_body() {
        let http = fake([Ok(TopggMetricsResponse { status: 401 })]);
        assert_eq!(
            post_topgg_stats_with_shards(&http, BOT, "token", 166, 3).await,
            TopggMetricsOutcome::HttpFailure { status: 401 }
        );
        assert_eq!(
            http.requests.lock().unwrap()[0].body,
            Some(serde_json::json!({ "server_count": 166, "shard_count": 3 }))
        );
    }

    #[tokio::test]
    async fn command_sync_uses_only_bearer_v1_payload() {
        let http = fake([Ok(TopggMetricsResponse { status: 200 })]);
        assert!(
            sync_topgg_commands(&http, "token", vec![serde_json::json!({ "name": "join" })]).await
        );
        assert_eq!(
            http.requests.lock().unwrap().as_slice(),
            &[TopggMetricsRequest {
                url: V1_COMMANDS_URL.into(),
                method: TopggMetricsMethod::Put,
                authorization: "Bearer token".into(),
                body: Some(serde_json::json!([{ "name": "join" }])),
            }]
        );
    }
}
