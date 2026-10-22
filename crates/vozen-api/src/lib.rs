#![forbid(unsafe_code)]

//! HTTP surface shared with vozen.org.
//!
//! The routers in this crate carry the Rust-side contracts for account OAuth, payments, dashboard,
//! admin and webhook traffic. Each sensitive router is still opt-in at runtime and fails closed
//! when its required verifier, secret or listener is absent.

use std::sync::Arc;

pub mod account_api;
pub mod admin_api;
pub mod admin_auth;
pub mod admin_router;
pub mod dashboard_api;
pub mod dashboard_oauth;
pub mod dashboard_validation;
pub mod discord_oauth;
pub mod entitlements_api;
pub mod install_api;
pub mod kofi_webhook;
pub mod premium_api;
pub mod stripe_api;
pub mod topgg_webhook;
pub mod web_analytics;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use thiserror::Error;

const INCIDENT_MAX_CHARS: usize = 240;
const OFFICIAL_ORIGINS: [&str; 2] = ["https://vozen.org", "https://www.vozen.org"];

pub type PublicStatusProvider = Arc<dyn Fn() -> PublicStatusResponse + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicStatusState {
    Operational,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicStatusInput {
    pub bot_ready: bool,
    pub database_ready: bool,
    pub provider_states: Vec<ProviderHealth>,
    pub incident_message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderHealth {
    Healthy,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublicStatusComponents {
    pub bot: PublicStatusState,
    pub database: PublicStatusState,
    pub providers: PublicStatusState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublicStatusResponse {
    pub status: PublicStatusState,
    pub components: PublicStatusComponents,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incident: Option<String>,
}

#[derive(Clone)]
struct AppState {
    public_status: Option<PublicStatusProvider>,
}

/// Maps the minimal internal health input to the public JSON contract. It intentionally omits
/// counts, Discord IDs, provider errors, quotas and raw incident data.
pub fn map_public_status(input: PublicStatusInput) -> PublicStatusResponse {
    let components = PublicStatusComponents {
        bot: component_state(input.bot_ready),
        database: component_state(input.database_ready),
        providers: provider_state(&input.provider_states),
    };
    let states = [components.bot, components.database, components.providers];
    let status = if states.contains(&PublicStatusState::Unavailable) {
        PublicStatusState::Unavailable
    } else if states.contains(&PublicStatusState::Degraded) {
        PublicStatusState::Degraded
    } else {
        PublicStatusState::Operational
    };
    PublicStatusResponse {
        status,
        components,
        incident: sanitise_public_incident(input.incident_message.as_deref()),
    }
}

/// Builds the safe public routes. Passing `None` keeps status opt-in: `/status` and
/// `/api/public/status` return the same JSON 404 response as the Node implementation.
pub fn public_router(public_status: Option<PublicStatusProvider>) -> Router {
    public_routes(public_status).fallback(fallback)
}

fn public_routes(public_status: Option<PublicStatusProvider>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/api/public/status", get(status))
        .with_state(AppState { public_status })
}

/// Configuration for the final Rust HTTP listener. Every sensitive surface remains explicitly
/// opt-in: omitting it means the route does not exist. This builder only composes routers; it
/// never binds a port, reads environment variables, or makes an outbound request.
pub struct RuntimeRouterConfig {
    pub public_status: Option<PublicStatusProvider>,
    pub account: Option<account_api::AccountApiConfig>,
    pub premium: Option<premium_api::PremiumApiConfig>,
    pub stripe: Option<stripe_api::StripeApiConfig>,
    pub dashboard: Option<dashboard_api::DashboardApiConfig>,
    pub install: Option<install_api::InstallApiConfig>,
    pub admin: Option<admin_router::AdminRouterConfig>,
    pub kofi_webhook: Option<kofi_webhook::KofiWebhookConfig>,
    pub topgg_webhook: Option<topgg_webhook::TopggWebhookConfig>,
}

#[derive(Debug, Error)]
pub enum RuntimeRouterError {
    #[error("account API configuration: {0}")]
    Account(#[from] account_api::AccountApiConfigError),
    #[error("premium API configuration: {0}")]
    Premium(#[from] premium_api::PremiumApiConfigError),
    #[error("dashboard API configuration: {0}")]
    Dashboard(#[from] dashboard_api::DashboardApiConfigError),
    #[error("TTS install API configuration: {0}")]
    Install(#[from] install_api::InstallApiConfigError),
    #[error("admin API configuration: {0}")]
    Admin(#[from] admin_router::AdminRouterConfigError),
    #[error("Ko-fi webhook configuration: {0}")]
    KofiWebhook(#[from] kofi_webhook::KofiWebhookConfigError),
    #[error("Top.gg webhook configuration: {0}")]
    TopggWebhook(#[from] topgg_webhook::TopggWebhookConfigError),
    #[error("Stripe configuration: {0}")]
    Stripe(#[from] stripe_api::StripeApiConfigError),
}

/// Composes the independently verified API surfaces for the cutover runtime. It avoids merging
/// the public fallback until every opted-in route has been installed, so a health fallback can
/// never shadow `/api/*` routes. Calling this is safe in shadow mode; serving its result is a
/// separate deployment decision.
pub fn runtime_router(config: RuntimeRouterConfig) -> Result<Router, RuntimeRouterError> {
    let mut router = public_routes(config.public_status);
    if let Some(account) = config.account {
        router = router.merge(account_api::account_router(account)?);
    }
    if let Some(premium) = config.premium {
        router = router.merge(premium_api::premium_router(premium)?);
    }
    if let Some(stripe) = config.stripe {
        router = router.merge(stripe_api::stripe_router(stripe)?);
    }
    if let Some(dashboard) = config.dashboard {
        router = router.merge(dashboard_api::dashboard_router(dashboard)?);
    }
    if let Some(install) = config.install {
        router = router.merge(install_api::install_router(install)?);
    }
    if let Some(admin) = config.admin {
        router = router.merge(admin_router::admin_router(admin)?);
    }
    if let Some(kofi_webhook) = config.kofi_webhook {
        router = router.merge(kofi_webhook::kofi_webhook_router(kofi_webhook)?);
    }
    if let Some(topgg_webhook) = config.topgg_webhook {
        router = router.merge(topgg_webhook::topgg_webhook_router(topgg_webhook)?);
    }
    Ok(router.fallback(fallback))
}

async fn health() -> Json<StatusBody> {
    Json(StatusBody { status: "ok" })
}

async fn status(
    headers: HeaderMap,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Response {
    let Some(provider) = state.public_status else {
        return not_found();
    };
    let mut response = Json(provider()).into_response();
    let response_headers = response.headers_mut();
    response_headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=30"),
    );
    if let Some(origin) = official_origin(&headers) {
        response_headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        response_headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    response
}

async fn fallback(_request: Request<Body>) -> Response {
    not_found()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(StatusBody {
            status: "not_found",
        }),
    )
        .into_response()
}

fn component_state(ready: bool) -> PublicStatusState {
    if ready {
        PublicStatusState::Operational
    } else {
        PublicStatusState::Unavailable
    }
}

fn provider_state(states: &[ProviderHealth]) -> PublicStatusState {
    if states.is_empty() {
        PublicStatusState::Unavailable
    } else if states.contains(&ProviderHealth::Degraded) {
        PublicStatusState::Degraded
    } else {
        PublicStatusState::Operational
    }
}

pub fn sanitise_public_incident(value: Option<&str>) -> Option<String> {
    let incident = value?.split_whitespace().collect::<Vec<_>>().join(" ");
    if incident.is_empty() {
        None
    } else {
        Some(incident.chars().take(INCIDENT_MAX_CHARS).collect())
    }
}

fn official_origin(headers: &HeaderMap) -> Option<HeaderValue> {
    let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
    if !OFFICIAL_ORIGINS.contains(&origin) {
        return None;
    }
    match origin {
        "https://vozen.org" => Some(HeaderValue::from_static("https://vozen.org")),
        "https://www.vozen.org" => Some(HeaderValue::from_static("https://www.vozen.org")),
        _ => None,
    }
}

#[derive(Serialize)]
struct StatusBody {
    status: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use tower::ServiceExt;

    #[tokio::test]
    async fn runtime_composition_keeps_only_explicitly_enabled_routes() {
        let app = runtime_router(RuntimeRouterConfig {
            public_status: None,
            account: None,
            premium: None,
            stripe: None,
            dashboard: None,
            install: None,
            admin: None,
            kofi_webhook: None,
            topgg_webhook: None,
        })
        .expect("compose public-only runtime");
        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("health response");
        assert_eq!(health.status(), StatusCode::OK);
        let unavailable_route = app
            .oneshot(
                Request::builder()
                    .uri("/api/activate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("fallback response");
        assert_eq!(unavailable_route.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn maps_public_status_without_sensitive_detail() {
        let status = map_public_status(PublicStatusInput {
            bot_ready: true,
            database_ready: true,
            provider_states: vec![ProviderHealth::Healthy, ProviderHealth::Degraded],
            incident_message: Some("  Provider\nmaintenance\t underway  ".into()),
        });
        assert_eq!(status.status, PublicStatusState::Degraded);
        assert_eq!(status.components.providers, PublicStatusState::Degraded);
        assert_eq!(
            status.incident.as_deref(),
            Some("Provider maintenance underway")
        );
    }

    #[tokio::test]
    async fn health_and_status_keep_the_node_public_contract() {
        let provider: PublicStatusProvider = Arc::new(|| {
            map_public_status(PublicStatusInput {
                bot_ready: true,
                database_ready: true,
                provider_states: vec![ProviderHealth::Healthy],
                incident_message: None,
            })
        });
        let app = public_router(Some(provider));

        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health?probe=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("health response");
        assert_eq!(health.status(), StatusCode::OK);
        let health_body = to_bytes(health.into_body(), usize::MAX)
            .await
            .expect("health body");
        assert_eq!(&health_body[..], br#"{"status":"ok"}"#);

        let status = app
            .oneshot(
                Request::builder()
                    .uri("/api/public/status")
                    .header(header::ORIGIN, "https://vozen.org")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("status response");
        assert_eq!(status.status(), StatusCode::OK);
        assert_eq!(
            status.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("https://vozen.org"))
        );
        assert_eq!(
            status.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static("public, max-age=30"))
        );
    }

    #[tokio::test]
    async fn status_stays_hidden_until_explicitly_enabled() {
        let response = public_router(None)
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(&body[..], br#"{"status":"not_found"}"#);
    }
}
