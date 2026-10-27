//! HTTP compatibility surface for the owner admin console.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::any,
};
use serde::Deserialize;
use serde_json::json;

use crate::admin_api::{AdminApi, AdminGrant, AdminGrantError, AdminRevoke};

const BODY_MAX_BYTES: usize = 4_000;
const API_RATE_MAX: usize = 30;
const API_RATE_WINDOW_MS: i64 = 10_000;
const LOGIN_RATE_MAX: usize = 6;
const LOGIN_RATE_WINDOW_MS: i64 = 10 * 60 * 1_000;
const RATE_MAX_ENTRIES: usize = 2_048;

pub struct AdminRouterConfig {
    pub origin: String,
    pub api: Arc<AdminApi>,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminRouterConfigError {
    Origin,
}

impl std::fmt::Display for AdminRouterConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("admin API requires a valid exact panel origin")
    }
}
impl std::error::Error for AdminRouterConfigError {}

#[derive(Clone)]
struct AdminState {
    origin: HeaderValue,
    api: Arc<AdminApi>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    rate: Arc<Mutex<HashMap<String, RateState>>>,
    login_rate: Arc<Mutex<HashMap<String, RateState>>>,
}

#[derive(Clone, Copy)]
struct RateState {
    count: usize,
    reset: i64,
}

pub fn admin_router(config: AdminRouterConfig) -> Result<Router, AdminRouterConfigError> {
    let origin =
        HeaderValue::from_str(&config.origin).map_err(|_| AdminRouterConfigError::Origin)?;
    Ok(Router::new()
        .route("/api/admin/login", any(admin_request))
        .route("/api/admin/passes", any(admin_request))
        .route("/api/admin/guilds", any(admin_request))
        .route("/api/admin/toptalkers", any(admin_request))
        .route("/api/admin/grant", any(admin_request))
        .route("/api/admin/revoke", any(admin_request))
        .layer(DefaultBodyLimit::max(BODY_MAX_BYTES))
        .with_state(AdminState {
            origin,
            api: config.api,
            now: config.now,
            rate: Arc::new(Mutex::new(HashMap::new())),
            login_rate: Arc::new(Mutex::new(HashMap::new())),
        }))
}

async fn admin_request(
    State(state): State<AdminState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !state.api.enabled() {
        return response(StatusCode::NOT_FOUND, json!({"error":"not_found"}), &state);
    }
    if method == Method::OPTIONS {
        return preflight(&state);
    }
    let path = uri.path();
    let bearer = bearer_token(&headers);
    if path == "/api/admin/login" {
        if method != Method::POST {
            return response(
                StatusCode::METHOD_NOT_ALLOWED,
                json!({"error":"method_not_allowed"}),
                &state,
            );
        }
        let now = (state.now)();
        if rate_limited(
            &state.login_rate,
            &headers,
            now,
            LOGIN_RATE_MAX,
            LOGIN_RATE_WINDOW_MS,
        ) {
            return response(
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error":"rate_limited"}),
                &state,
            );
        }
        return match state.api.login(bearer).await {
            Some(login) => response(
                StatusCode::OK,
                serde_json::to_value(login).unwrap_or_else(|_| json!({"error":"internal"})),
                &state,
            ),
            None => response(StatusCode::FORBIDDEN, json!({"error":"denied"}), &state),
        };
    }
    let now = (state.now)();
    if rate_limited(&state.rate, &headers, now, API_RATE_MAX, API_RATE_WINDOW_MS) {
        return response(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error":"rate_limited"}),
            &state,
        );
    }
    if state.api.authorize(bearer).is_none() {
        return response(StatusCode::FORBIDDEN, json!({"error":"forbidden"}), &state);
    }
    match (path, method) {
        ("/api/admin/passes", Method::GET) => match state.api.list_passes() {
            Ok(value) => response(
                StatusCode::OK,
                serde_json::to_value(value).unwrap_or_else(|_| json!({"error":"internal"})),
                &state,
            ),
            Err(_) => response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"internal"}),
                &state,
            ),
        },
        ("/api/admin/guilds", Method::GET) => match state.api.list_guilds() {
            Ok(guilds) => response(StatusCode::OK, json!({"guilds":guilds}), &state),
            Err(_) => response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"internal"}),
                &state,
            ),
        },
        ("/api/admin/toptalkers", Method::GET) => match state.api.list_top_talkers() {
            Ok(talkers) => response(StatusCode::OK, json!({"talkers":talkers}), &state),
            Err(_) => response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"internal"}),
                &state,
            ),
        },
        ("/api/admin/grant", Method::POST) => grant(body, &state),
        ("/api/admin/revoke", Method::POST) => revoke(body, &state),
        (
            "/api/admin/passes"
            | "/api/admin/guilds"
            | "/api/admin/toptalkers"
            | "/api/admin/grant"
            | "/api/admin/revoke",
            _,
        ) => response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"method_not_allowed"}),
            &state,
        ),
        _ => response(StatusCode::NOT_FOUND, json!({"error":"not_found"}), &state),
    }
}

#[derive(Debug, Deserialize)]
struct GrantBody {
    kind: Option<String>,
    id: Option<String>,
    days: Option<i64>,
    seats: Option<i64>,
}

fn grant(body: Bytes, state: &AdminState) -> Response {
    let Ok(input) = serde_json::from_slice::<GrantBody>(&body) else {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"bad_request"}),
            state,
        );
    };
    let (Some(kind), Some(id), Some(days)) = (input.kind, input.id, input.days) else {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"bad_request"}),
            state,
        );
    };
    let grant = match kind.as_str() {
        "plus" => AdminGrant::Plus { id, days },
        "premium" => match input.seats {
            Some(seats) => AdminGrant::Premium { id, days, seats },
            None => return response(StatusCode::BAD_REQUEST, json!({"error":"bad_seats"}), state),
        },
        _ => return response(StatusCode::BAD_REQUEST, json!({"error":"bad_kind"}), state),
    };
    match state.api.grant(grant) {
        Ok(expires_at) => response(
            StatusCode::OK,
            json!({"ok":true,"expiresAt":expires_at}),
            state,
        ),
        Err(AdminGrantError::Store) => response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"internal"}),
            state,
        ),
        Err(error) => response(
            StatusCode::BAD_REQUEST,
            json!({"error":grant_error(error)}),
            state,
        ),
    }
}

fn revoke(body: Bytes, state: &AdminState) -> Response {
    #[derive(Deserialize)]
    struct RevokeBody {
        kind: Option<String>,
        id: Option<String>,
    }
    let Ok(input) = serde_json::from_slice::<RevokeBody>(&body) else {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"bad_request"}),
            state,
        );
    };
    let (Some(kind), Some(id)) = (input.kind, input.id) else {
        return response(
            StatusCode::BAD_REQUEST,
            json!({"error":"bad_request"}),
            state,
        );
    };
    let revoke = match kind.as_str() {
        "plus" => AdminRevoke::Plus { id },
        "premium" => AdminRevoke::Premium { id },
        _ => {
            return response(
                StatusCode::BAD_REQUEST,
                json!({"error":"bad_request"}),
                state,
            );
        }
    };
    match state.api.revoke(revoke) {
        Ok(ok) => response(StatusCode::OK, json!({"ok":ok}), state),
        Err(_) => response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"internal"}),
            state,
        ),
    }
}

fn grant_error(error: AdminGrantError) -> &'static str {
    match error {
        AdminGrantError::BadId => "bad_id",
        AdminGrantError::BadDays => "bad_days",
        AdminGrantError::BadSeats => "bad_seats",
        AdminGrantError::Store => "internal",
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

fn rate_limited(
    bucket: &Arc<Mutex<HashMap<String, RateState>>>,
    headers: &HeaderMap,
    now: i64,
    max: usize,
    window: i64,
) -> bool {
    let Ok(mut bucket) = bucket.lock() else {
        return true;
    };
    bucket.retain(|_, state| state.reset > now);
    let ip = client_ip(headers);
    if !bucket.contains_key(&ip)
        && bucket.len() >= RATE_MAX_ENTRIES
        && let Some(oldest) = bucket
            .iter()
            .min_by_key(|(_, state)| state.reset)
            .map(|(ip, _)| ip.clone())
    {
        bucket.remove(&oldest);
    }
    let state = bucket.entry(ip).or_insert(RateState {
        count: 0,
        reset: now + window,
    });
    state.count += 1;
    state.count > max
}

fn preflight(state: &AdminState) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    common_headers(response.headers_mut(), state);
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Authorization, Content-Type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    response
}

fn response(status: StatusCode, body: serde_json::Value, state: &AdminState) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    common_headers(response.headers_mut(), state);
    response
}

fn common_headers(headers: &mut HeaderMap, state: &AdminState) {
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
    use crate::admin_api::{AdminApiConfig, AdminAuthorization, AdminAuthorizationResolver};
    use async_trait::async_trait;
    use axum::{body::Body, body::to_bytes, http::Request};
    use tower::ServiceExt;
    use vozen_store::SqliteStore;

    const OWNER: &str = "100000000000000001";
    const CLIENT: &str = "100000000000000003";
    const SECRET: &str = "sess-secret-abcdefghijklmnopqrstuvwxyz";
    const NOW: i64 = 1_700_000_000_000;

    struct Resolver;
    #[async_trait]
    impl AdminAuthorizationResolver for Resolver {
        async fn resolve_authorization(&self, bearer: &str) -> Option<AdminAuthorization> {
            (bearer == "owner-token").then(|| AdminAuthorization {
                user_id: OWNER.into(),
                application_id: CLIENT.into(),
            })
        }
    }

    fn router() -> Router {
        let api = Arc::new(AdminApi::new(AdminApiConfig {
            store: Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store"))),
            resolver: Arc::new(Resolver),
            now: Arc::new(|| NOW),
            admin_session_secret: Some(SECRET.into()),
            owner_id: Some(OWNER.into()),
            admin_client_id: Some(CLIENT.into()),
            session_ttl_seconds: None,
            log: Arc::new(|_| {}),
            resolve_guilds: None,
            local_day: Arc::new(|| "2026-07-23".into()),
        }));
        admin_router(AdminRouterConfig {
            origin: "https://admin.example.com".into(),
            api,
            now: Arc::new(|| NOW),
        })
        .expect("router")
    }

    #[tokio::test]
    async fn login_is_owner_only_and_passes_requires_the_signed_session() {
        let app = router();
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/login")
                    .header("authorization", "Bearer owner-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("login");
        assert_eq!(login.status(), StatusCode::OK);
        let login_body: serde_json::Value =
            serde_json::from_slice(&to_bytes(login.into_body(), BODY_MAX_BYTES).await.unwrap())
                .unwrap();
        let token = login_body["token"].as_str().expect("session token");

        let bare = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/admin/passes")
                    .header("authorization", "Bearer owner-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("bare session response");
        assert_eq!(bare.status(), StatusCode::FORBIDDEN);

        let passes = app
            .oneshot(
                Request::builder()
                    .uri("/api/admin/passes")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("passes");
        assert_eq!(passes.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(passes.into_body(), BODY_MAX_BYTES).await.unwrap())
                .unwrap();
        assert_eq!(body, json!({"plus":[],"passes":[],"pending":[]}));
    }

    #[tokio::test]
    async fn grant_and_revoke_keep_http_error_contract() {
        let app = router();
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/login")
                    .header("authorization", "Bearer owner-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(login.into_body(), BODY_MAX_BYTES).await.unwrap())
                .unwrap();
        let token = body["token"].as_str().unwrap().to_owned();
        let grant = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/grant")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"kind":"plus","id":"111","days":30}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grant.status(), StatusCode::OK);
        let bad = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/revoke")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"kind":"plus","id":"bad"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::OK);
        let bad_body: serde_json::Value =
            serde_json::from_slice(&to_bytes(bad.into_body(), BODY_MAX_BYTES).await.unwrap())
                .unwrap();
        assert_eq!(bad_body, json!({"ok":false}));
    }
}
