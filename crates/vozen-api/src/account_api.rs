//! Read-only account status endpoint used by the Vozen site.
//!
//! It is deliberately a separate router so the runtime can compose it only when the authenticated
//! HTTP surface is explicitly enabled during the staged cutover.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use serde::Serialize;
use serde_json::json;
use vozen_store::{PremiumPassStatus, PremiumStatusView, SqliteStore};

use crate::premium_api::DiscordIdentityVerifier;

const STATUS_RATE_MAX: usize = 30;
const STATUS_RATE_WINDOW_MS: i64 = 10_000;
const RATE_MAX_ENTRIES: usize = 2_048;

type GuildNameResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct AccountApiConfig {
    /// Exact public site origin, normally `https://vozen.org`.
    pub origin: String,
    pub store: Arc<Mutex<SqliteStore>>,
    pub identity_verifier: Arc<dyn DiscordIdentityVerifier>,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// The Discord gateway can inject a cached name. No remote lookup happens on this request.
    pub resolve_guild_name: Option<GuildNameResolver>,
}

#[derive(Clone)]
struct AccountApiState {
    origin: HeaderValue,
    store: Arc<Mutex<SqliteStore>>,
    identity_verifier: Arc<dyn DiscordIdentityVerifier>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    resolve_guild_name: Option<GuildNameResolver>,
    rate: Arc<Mutex<HashMap<String, RateState>>>,
}

#[derive(Clone, Copy)]
struct RateState {
    count: usize,
    reset: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountApiConfigError {
    Origin,
}

impl std::fmt::Display for AccountApiConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("account API requires a valid exact site origin")
    }
}

impl std::error::Error for AccountApiConfigError {}

pub fn account_router(config: AccountApiConfig) -> Result<Router, AccountApiConfigError> {
    let origin =
        HeaderValue::from_str(&config.origin).map_err(|_| AccountApiConfigError::Origin)?;
    Ok(Router::new()
        .route("/api/me/premium", any(account_status))
        .with_state(AccountApiState {
            origin,
            store: config.store,
            identity_verifier: config.identity_verifier,
            now: config.now,
            resolve_guild_name: config.resolve_guild_name,
            rate: Arc::new(Mutex::new(HashMap::new())),
        }))
}

async fn account_status(
    State(state): State<AccountApiState>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if method == Method::OPTIONS {
        return preflight(&state);
    }
    if method != Method::GET {
        return text_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", &state);
    }
    let now = (state.now)();
    if rate_limited(&state, client_ip(&headers), now) {
        return json_response(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error":"rate_limited"}),
            &state,
        );
    }
    let Some(bearer) = bearer_token(&headers) else {
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({"error":"no_token"}),
            &state,
        );
    };
    let identity = match state.identity_verifier.resolve_identity(bearer).await {
        Ok(identity) => identity,
        Err(()) => {
            return json_response(
                StatusCode::UNAUTHORIZED,
                json!({"error":"invalid_token"}),
                &state,
            );
        }
    };
    let status = match state.store.lock() {
        Ok(store) => store.premium_status(&identity.id, now),
        Err(_) => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"internal"}),
                &state,
            );
        }
    };
    let Ok(status) = status else {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"internal"}),
            &state,
        );
    };
    let body = match serde_json::to_value(account_body(
        identity.id,
        identity.username,
        identity.avatar,
        identity.avatar_decoration_asset,
        status,
        &state,
    )) {
        Ok(body) => body,
        Err(_) => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"internal"}),
                &state,
            );
        }
    };
    json_response(StatusCode::OK, body, &state)
}

#[derive(Serialize)]
struct AccountBody {
    user: UserBody,
    plus: PlusBody,
    pass: Option<PassBody>,
}

#[derive(Serialize)]
struct UserBody {
    id: String,
    username: String,
    avatar: Option<String>,
    #[serde(rename = "avatarDecorationAsset")]
    avatar_decoration_asset: Option<String>,
}

#[derive(Serialize)]
struct PlusBody {
    active: bool,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
}

#[derive(Serialize)]
struct PassBody {
    seats: i64,
    used: i64,
    #[serde(rename = "expiresAt")]
    expires_at: i64,
    active: bool,
    servers: Vec<GuildBody>,
    #[serde(rename = "inactiveServers", skip_serializing_if = "Vec::is_empty")]
    inactive_servers: Vec<GuildBody>,
}

#[derive(Serialize)]
struct GuildBody {
    id: String,
    name: Option<String>,
}

fn account_body(
    id: String,
    username: String,
    avatar: Option<String>,
    avatar_decoration_asset: Option<String>,
    status: PremiumStatusView,
    state: &AccountApiState,
) -> AccountBody {
    AccountBody {
        user: UserBody {
            id,
            username,
            avatar,
            avatar_decoration_asset,
        },
        plus: PlusBody {
            active: status.plus_active,
            expires_at: status.plus_expires_at,
        },
        pass: status.pass.map(|pass| pass_body(pass, state)),
    }
}

fn pass_body(pass: PremiumPassStatus, state: &AccountApiState) -> PassBody {
    let resolve = |id: String| GuildBody {
        name: state
            .resolve_guild_name
            .as_ref()
            .and_then(|resolve| resolve(&id)),
        id,
    };
    PassBody {
        seats: pass.seats,
        used: pass.used,
        expires_at: pass.expires_at,
        active: pass.active,
        servers: pass.guilds.into_iter().map(resolve).collect(),
        inactive_servers: pass.inactive_guilds.into_iter().map(resolve).collect(),
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

fn rate_limited(state: &AccountApiState, client_ip: String, now: i64) -> bool {
    let Ok(mut rate) = state.rate.lock() else {
        return true;
    };
    rate.retain(|_, value| value.reset > now);
    if !rate.contains_key(&client_ip) && rate.len() >= RATE_MAX_ENTRIES {
        let oldest = rate
            .iter()
            .min_by_key(|(_, value)| value.reset)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            rate.remove(&oldest);
        }
    }
    let entry = rate.entry(client_ip).or_insert(RateState {
        count: 0,
        reset: now + STATUS_RATE_WINDOW_MS,
    });
    entry.count += 1;
    entry.count > STATUS_RATE_MAX
}

fn preflight(state: &AccountApiState) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    common_headers(headers, state);
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Authorization"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    response
}

fn json_response(status: StatusCode, body: serde_json::Value, state: &AccountApiState) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    common_headers(response.headers_mut(), state);
    response
}

fn text_response(status: StatusCode, text: &'static str, state: &AccountApiState) -> Response {
    let mut response = (status, text).into_response();
    common_headers(response.headers_mut(), state);
    response
}

fn common_headers(headers: &mut HeaderMap, state: &AccountApiState) {
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, state.origin.clone());
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::{body::Body, body::to_bytes, http::Request};
    use tower::ServiceExt;
    use vozen_store::SqliteStore;

    use crate::premium_api::{ActivationIdentity, ActivationIdentityError, DiscordIdentity};

    struct Identities;

    #[async_trait]
    impl DiscordIdentityVerifier for Identities {
        async fn resolve_identity(&self, bearer: &str) -> Result<DiscordIdentity, ()> {
            (bearer == "valid")
                .then(|| DiscordIdentity {
                    id: "user".into(),
                    username: "Rexy".into(),
                    avatar: Some("avatar-hash".into()),
                    avatar_decoration_asset: Some("a_fed43ab12698df65902ba06727e20c0e".into()),
                })
                .ok_or(())
        }

        async fn resolve_activation_identity(
            &self,
            _bearer: &str,
        ) -> Result<ActivationIdentity, ActivationIdentityError> {
            Err(ActivationIdentityError::InvalidToken)
        }
    }

    fn router(store: Arc<Mutex<SqliteStore>>) -> Router {
        account_router(AccountApiConfig {
            origin: "https://vozen.org".into(),
            store,
            identity_verifier: Arc::new(Identities),
            now: Arc::new(|| 1_000),
            resolve_guild_name: Some(Arc::new(|id| (id == "guild").then(|| "Server".into()))),
        })
        .expect("router")
    }

    #[tokio::test]
    async fn returns_the_node_account_contract_from_verified_identity_only() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        {
            let store_guard = store.lock().unwrap();
            store_guard
                .grant_user_premium("user", 30, "kofi", 1_000)
                .expect("plus");
            store_guard
                .grant_guild_pass("user", 3, 30, "kofi", 1_000)
                .expect("pass");
            store_guard
                .activate_seat("user", "guild", 1_000)
                .expect("seat");
        }
        let response = router(store)
            .oneshot(
                Request::builder()
                    .uri("/api/me/premium")
                    .header("authorization", "Bearer valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 16_384).await.unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "user":{"id":"user","username":"Rexy","avatar":"avatar-hash","avatarDecorationAsset":"a_fed43ab12698df65902ba06727e20c0e"},
                "plus":{"active":true,"expiresAt":2_592_001_000i64},
                "pass":{"seats":3,"used":1,"expiresAt":2_592_001_000i64,"active":true,"servers":[{"id":"guild","name":"Server"}]}
            })
        );
    }

    #[tokio::test]
    async fn keeps_cors_preflight_and_token_errors_compatible() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let router = router(store);
        let preflight = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/api/me/premium")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            preflight
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .unwrap(),
            "GET, OPTIONS"
        );
        let missing = router
            .oneshot(
                Request::builder()
                    .uri("/api/me/premium")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            to_bytes(missing.into_body(), 1_024).await.unwrap().as_ref(),
            br#"{"error":"no_token"}"#
        );
    }
}
