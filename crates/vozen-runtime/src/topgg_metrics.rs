//! Opt-in Top.gg server-count publishing.
//!
//! Listing availability is never part of Discord's critical path. Requests are bounded and all
//! failures become `false`, with the legacy API attempted only when Top.gg explicitly reports
//! that the v1 metrics endpoint is unavailable.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

const V1_METRICS_URL: &str = "https://top.gg/api/v1/projects/@me/metrics";
const V1_COMMANDS_URL: &str = "https://top.gg/api/v1/projects/@me/commands";
const LEGACY_METRICS_BASE_URL: &str = "https://top.gg/api/bots";
pub const TOPGG_POST_INTERVAL: Duration = Duration::from_secs(30 * 60);
const TOPGG_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopggMetricsRequest {
    pub url: String,
    pub method: TopggMetricsMethod,
    pub authorization: String,
    pub body: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopggMetricsMethod {
    Patch,
    Post,
    Put,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopggMetricsResponse {
    pub status: u16,
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
        let method = match request.method {
            TopggMetricsMethod::Patch => reqwest::Method::PATCH,
            TopggMetricsMethod::Post => reqwest::Method::POST,
            TopggMetricsMethod::Put => reqwest::Method::PUT,
        };
        let response = self
            .client
            .request(method, &request.url)
            .header(reqwest::header::AUTHORIZATION, request.authorization)
            .json(&request.body)
            .send()
            .await
            .map_err(|_| ())?;
        Ok(TopggMetricsResponse {
            status: response.status().as_u16(),
        })
    }
}

/// Sends only a non-negative exact count. Returns false for every remote error so an outage
/// cannot interfere with Discord startup or command handling.
pub async fn post_topgg_stats(
    http: &impl TopggMetricsHttp,
    bot_id: &str,
    token: &str,
    server_count: usize,
) -> bool {
    if token.trim().is_empty() || !is_discord_application_id(bot_id) {
        return false;
    }
    let v1 = http
        .send(TopggMetricsRequest {
            url: V1_METRICS_URL.into(),
            method: TopggMetricsMethod::Patch,
            authorization: format!("Bearer {token}"),
            body: serde_json::json!({ "server_count": server_count }),
        })
        .await;
    match v1 {
        Ok(response) if (200..300).contains(&response.status) => true,
        Ok(response) if matches!(response.status, 404 | 405) => http
            .send(TopggMetricsRequest {
                url: format!("{LEGACY_METRICS_BASE_URL}/{bot_id}/stats"),
                method: TopggMetricsMethod::Post,
                authorization: token.to_owned(),
                body: serde_json::json!({ "server_count": server_count }),
            })
            .await
            .is_ok_and(|response| (200..300).contains(&response.status)),
        Ok(_) | Err(()) => false,
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
        body: Value::Array(commands),
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
                body: serde_json::json!({ "server_count": 42 }),
            }]
        );
    }

    #[tokio::test]
    async fn only_missing_v1_endpoint_falls_back_to_legacy_auth() {
        let http = fake([
            Ok(TopggMetricsResponse { status: 404 }),
            Ok(TopggMetricsResponse { status: 200 }),
        ]);
        assert!(post_topgg_stats(&http, BOT, "token", 3).await);
        let requests = http.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, TopggMetricsMethod::Post);
        assert_eq!(requests[1].authorization, "token");
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
                body: serde_json::json!([{ "name": "join" }]),
            }]
        );
    }
}
