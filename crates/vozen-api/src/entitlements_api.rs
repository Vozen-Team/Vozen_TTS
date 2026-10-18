//! Authenticated cross-product entitlement read API.
//!
//! Billing remains owned by the existing Vozen store. Trusted product services
//! can explicitly assign an existing subscription seat after authorizing the user.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::IntoResponse,
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use vozen_store::{ActivateStatus, SqliteStore};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct EntitlementsConfig {
    pub store: Arc<Mutex<SqliteStore>>,
    pub service_secret: String,
}

#[derive(Clone)]
struct StateInner {
    store: Arc<Mutex<SqliteStore>>,
    service_secret: String,
    seen_nonces: Arc<Mutex<HashMap<String, i64>>>,
}

#[derive(Debug, Deserialize)]
struct ResolveRequest {
    subject_id: String,
    guild_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResolveResponse {
    product: &'static str,
    subject_id: String,
    scope: &'static str,
    plan: &'static str,
    guild_limit: u16,
    active: bool,
    expires_at_ms: Option<i64>,
    version: i64,
    issued_at_ms: i64,
}

pub fn entitlements_router(config: EntitlementsConfig) -> Router {
    Router::new()
        .route("/internal/v1/entitlements/resolve", post(resolve))
        .route("/internal/v1/entitlements/seats", post(seats))
        .layer(axum::extract::DefaultBodyLimit::max(2048))
        .with_state(StateInner {
            store: config.store,
            service_secret: config.service_secret,
            seen_nonces: Arc::new(Mutex::new(HashMap::new())),
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeatRequest {
    subject_id: String,
    guild_id: String,
    // Bound into the signed body: a resolve request can never be replayed as a write.
    operation: String,
}

#[derive(Debug, Serialize)]
struct SeatResponse {
    guild_premium: bool,
    pass_active: bool,
    seats: i64,
    used: i64,
}

async fn seats(
    State(state): State<StateInner>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    if !verify_request(&headers, &body, &state.service_secret, &state.seen_nonces) {
        return (StatusCode::UNAUTHORIZED, "invalid service signature").into_response();
    }
    let request: SeatRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid request").into_response(),
    };
    let valid_id = |value: &str| {
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    };
    if !valid_id(&request.subject_id)
        || !valid_id(&request.guild_id)
        || !matches!(request.operation.as_str(), "status" | "activate")
    {
        return (StatusCode::BAD_REQUEST, "invalid request").into_response();
    }
    let now = (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let result = state
        .store
        .lock()
        .map_err(|_| "unavailable")
        .and_then(|store| {
            seat_operation(
                &store,
                &request.subject_id,
                &request.guild_id,
                request.operation == "activate",
                now,
            )
        });
    match result {
        Ok(status) => axum::Json(status).into_response(),
        Err("no_seats") => (StatusCode::CONFLICT, "no_seats").into_response(),
        Err("no_pass") => (StatusCode::FORBIDDEN, "no_pass").into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "premium_unavailable").into_response(),
    }
}

fn seat_operation(
    store: &SqliteStore,
    user: &str,
    guild: &str,
    activate: bool,
    now: i64,
) -> Result<SeatResponse, &'static str> {
    let mut guild_premium = store
        .effective_guild_premium_expiry(guild, now)
        .map_err(|_| "unavailable")?
        .is_some();
    if activate && !guild_premium {
        let result = store
            .activate_seat(user, guild, now)
            .map_err(|_| "unavailable")?;
        match result.status {
            ActivateStatus::Ok | ActivateStatus::Already => guild_premium = true,
            ActivateStatus::NoSeats => return Err("no_seats"),
            ActivateStatus::NoPass | ActivateStatus::Expired => return Err("no_pass"),
        }
    }
    let pass = store
        .premium_status(user, now)
        .map_err(|_| "unavailable")?
        .pass;
    Ok(SeatResponse {
        guild_premium,
        pass_active: pass.as_ref().is_some_and(|pass| pass.active),
        seats: pass.as_ref().map_or(0, |pass| pass.seats),
        used: pass.as_ref().map_or(0, |pass| pass.used),
    })
}

async fn resolve(
    State(state): State<StateInner>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !verify_request(&headers, &body, &state.service_secret, &state.seen_nonces) {
        return (StatusCode::UNAUTHORIZED, "invalid service signature").into_response();
    }
    let request: ResolveRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid request").into_response(),
    };
    if request.subject_id.is_empty() || request.subject_id.len() > 64 {
        return (StatusCode::BAD_REQUEST, "invalid subject").into_response();
    }
    let now = OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    let result = state
        .store
        .lock()
        .ok()
        .and_then(|store| resolve_from_store(&store, &request, now).ok());
    match result {
        Some(response) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            serde_json::to_string(&response).unwrap(),
        )
            .into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "entitlement lookup failed",
        )
            .into_response(),
    }
}

fn resolve_from_store(
    store: &SqliteStore,
    request: &ResolveRequest,
    now: i64,
) -> Result<ResolveResponse, rusqlite::Error> {
    let status = store
        .premium_status(&request.subject_id, now)
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    let (plan, guild_limit, active, expires_at_ms, scope) =
        if let Some(guild_id) = request.guild_id.as_deref() {
            let guild_expiry = store
                .effective_guild_premium_expiry(guild_id, now)
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            let activated_pass = status.pass.as_ref().filter(|pass| {
                pass.active && pass.guilds.iter().any(|activated| activated == guild_id)
            });
            if let Some(guild_expiry) = guild_expiry {
                let seats = activated_pass
                    .map(|pass| u16::try_from(pass.seats.max(1)).unwrap_or(u16::MAX))
                    .unwrap_or(1);
                let expiry = activated_pass
                    .map(|pass| pass.expires_at.max(guild_expiry))
                    .unwrap_or(guild_expiry);
                ("premium", seats, true, Some(expiry), "guild")
            } else if status.plus_active {
                ("plus", 1, true, status.plus_expires_at, "guild")
            } else {
                ("free", 1, true, None, "guild")
            }
        } else if let Some(pass) = status.pass.filter(|pass| pass.active) {
            (
                "premium",
                u16::try_from(pass.seats.max(1)).unwrap_or(u16::MAX),
                true,
                Some(pass.expires_at),
                "user",
            )
        } else if status.plus_active {
            ("plus", 1, true, status.plus_expires_at, "user")
        } else {
            ("free", 1, true, None, "user")
        };
    Ok(ResolveResponse {
        product: "vozen",
        subject_id: request.subject_id.clone(),
        scope,
        plan,
        guild_limit,
        active,
        expires_at_ms,
        version: now,
        issued_at_ms: now,
    })
}

fn verify_request(
    headers: &HeaderMap,
    body: &[u8],
    secret: &str,
    seen_nonces: &Arc<Mutex<HashMap<String, i64>>>,
) -> bool {
    let timestamp = match headers
        .get("x-vozen-timestamp")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
    {
        Some(value) => value,
        None => return false,
    };
    let nonce = match headers.get("x-vozen-nonce").and_then(|v| v.to_str().ok()) {
        Some(value) if !value.is_empty() && value.len() <= 128 => value,
        _ => return false,
    };
    let signature = match headers
        .get("x-vozen-signature")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("v1="))
    {
        Some(value) => value,
        None => return false,
    };
    let now = OffsetDateTime::now_utc().unix_timestamp();
    if now.abs_diff(timestamp) > 60 {
        return false;
    }
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    let body_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(body));
    mac.update(format!("{timestamp}\n{nonce}\n{body_hash}").as_bytes());
    let expected = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    let valid: bool =
        subtle::ConstantTimeEq::ct_eq(expected.as_bytes(), signature.as_bytes()).into();
    if !valid {
        return false;
    }
    let mut seen = match seen_nonces.lock() {
        Ok(seen) => seen,
        Err(_) => return false,
    };
    seen.retain(|_, seen_at| now.saturating_sub(*seen_at) <= 60);
    if seen.contains_key(nonce) {
        return false;
    }
    seen.insert(nonce.to_owned(), now);
    // Keep the replay cache bounded even if an attacker rotates nonces rapidly.
    if seen.len() > 4_096
        && let Some(oldest) = seen
            .iter()
            .min_by_key(|(_, seen_at)| **seen_at)
            .map(|(nonce, _)| nonce.clone())
    {
        seen.remove(&oldest);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::collections::HashMap;

    fn paid_stripe_store(plan: &str, seats: i64, now: i64, end: i64) -> SqliteStore {
        use vozen_store::{StripeEventInput, StripePaidPlan};
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .apply_stripe_event_once(
                "checkout",
                &StripeEventInput::Checkout {
                    subscription_id: "sub_lab".into(),
                    customer_id: "cus_lab".into(),
                    user_id: "owner".into(),
                    plan: plan.into(),
                    seats,
                },
                now,
            )
            .unwrap();
        assert!(store.premium_status("owner", now).unwrap().pass.is_none());
        store
            .apply_stripe_event_once(
                "invoice",
                &StripeEventInput::InvoicePaidPlan {
                    subscription_id: "sub_lab".into(),
                    period_end: Some(end),
                    paid: StripePaidPlan {
                        invoice_id: "in_lab".into(),
                        issued_at: now,
                        starts_at: now,
                        plan: plan.into(),
                        seats,
                        canonical_previous_invoice: None,
                    },
                },
                now,
            )
            .unwrap();
        store
    }

    #[test]
    fn stripe_paid_projection_activates_exact_seats_and_keeps_guilds_isolated() {
        let now = 1_000_000;
        for (plan, seats) in [("premium", 2), ("max", 5)] {
            let store = paid_stripe_store(plan, seats, now, now + 60_000);
            let status = seat_operation(&store, "owner", "a", false, now).unwrap();
            assert!(status.pass_active && !status.guild_premium);
            assert_eq!((status.seats, status.used), (seats, 0));
            for n in 0..seats {
                let guild = format!("guild-{n}");
                let activated = seat_operation(&store, "owner", &guild, true, now).unwrap();
                assert!(activated.guild_premium);
                assert_eq!(activated.used, n + 1);
                assert_eq!(
                    seat_operation(&store, "owner", &guild, true, now)
                        .unwrap()
                        .used,
                    n + 1
                );
            }
            assert_eq!(
                seat_operation(&store, "owner", "overflow", true, now).unwrap_err(),
                "no_seats"
            );
            assert_eq!(
                seat_operation(&store, "other", "unfunded", true, now).unwrap_err(),
                "no_pass"
            );
            let resolved = resolve_from_store(
                &store,
                &ResolveRequest {
                    subject_id: "other-admin".into(),
                    guild_id: Some("guild-0".into()),
                },
                now,
            )
            .unwrap();
            assert_eq!(resolved.plan, "premium");
            let free = resolve_from_store(
                &store,
                &ResolveRequest {
                    subject_id: "other-admin".into(),
                    guild_id: Some("unfunded".into()),
                },
                now,
            )
            .unwrap();
            assert_eq!(free.plan, "free");
            let expired = seat_operation(&store, "owner", "guild-0", false, now + 60_000).unwrap();
            assert!(!expired.pass_active && !expired.guild_premium);
        }
    }

    #[test]
    fn stripe_plus_does_not_supply_server_seats() {
        let now = 1_000_000;
        let store = paid_stripe_store("plus", 1, now, now + 60_000);
        assert_eq!(
            seat_operation(&store, "owner", "g", true, now).unwrap_err(),
            "no_pass"
        );
        let resolved = resolve_from_store(
            &store,
            &ResolveRequest {
                subject_id: "owner".into(),
                guild_id: None,
            },
            now,
        )
        .unwrap();
        assert_eq!(resolved.plan, "plus");
    }

    #[test]
    fn signed_request_rejects_extreme_timestamps_without_panicking() {
        for timestamp in [i64::MIN, i64::MAX] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "x-vozen-timestamp",
                HeaderValue::from_str(&timestamp.to_string()).unwrap(),
            );
            headers.insert("x-vozen-nonce", HeaderValue::from_static("n"));
            headers.insert("x-vozen-signature", HeaderValue::from_static("v1=bad"));
            assert!(!verify_request(
                &headers,
                b"{}",
                "secret",
                &Arc::new(Mutex::new(HashMap::new()))
            ));
        }
    }

    #[test]
    fn seat_activation_is_explicit_bounded_and_idempotent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = 1_000;
        store.grant_guild_pass("owner", 3, 30, "test", now).unwrap();
        let status = seat_operation(&store, "owner", "a", false, now).unwrap();
        assert_eq!(status.used, 0);
        assert!(!status.guild_premium);
        assert_eq!(
            seat_operation(&store, "owner", "a", true, now)
                .unwrap()
                .used,
            1
        );
        assert_eq!(
            seat_operation(&store, "owner", "a", true, now)
                .unwrap()
                .used,
            1
        );
        for guild in ["b", "c"] {
            seat_operation(&store, "owner", guild, true, now).unwrap();
        }
        assert!(seat_operation(&store, "owner", "d", true, now).is_err());
        assert_eq!(
            store
                .premium_status("owner", now)
                .unwrap()
                .pass
                .unwrap()
                .used,
            3
        );
        assert!(seat_operation(&store, "free", "d", true, now).is_err());
        // Already funded by another account: do not spend the caller's seat.
        store.grant_guild_pass("other", 5, 30, "test", now).unwrap();
        let shared = seat_operation(&store, "other", "a", true, now).unwrap();
        assert!(shared.guild_premium);
        assert_eq!(shared.used, 0);
        assert!(seat_operation(&store, "owner", "e", true, now + 31 * 86_400_000).is_err());
    }

    #[tokio::test]
    async fn seat_http_rejects_unsigned_and_non_seat_requests() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        for signed in [false, true] {
            let store = SqliteStore::open_in_memory().unwrap();
            let app = entitlements_router(EntitlementsConfig {
                store: Arc::new(Mutex::new(store)),
                service_secret: "test-secret".into(),
            });
            // A signed resolve payload lacks the mandatory seat operation.
            let body = br#"{"subject_id":"u","guild_id":"g"}"#;
            let mut request = Request::builder()
                .method("POST")
                .uri("/internal/v1/entitlements/seats");
            if signed {
                let now = OffsetDateTime::now_utc().unix_timestamp();
                let hash = URL_SAFE_NO_PAD.encode(Sha256::digest(body));
                let mut mac = HmacSha256::new_from_slice(b"test-secret").unwrap();
                mac.update(format!("{now}\nseat-test\n{hash}").as_bytes());
                request = request
                    .header("x-vozen-timestamp", now.to_string())
                    .header("x-vozen-nonce", "seat-test")
                    .header(
                        "x-vozen-signature",
                        format!("v1={}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())),
                    );
            }
            let result = app
                .oneshot(request.body(Body::from(body.to_vec())).unwrap())
                .await
                .unwrap();
            assert_eq!(
                result.status(),
                if signed {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::UNAUTHORIZED
                }
            );
        }
    }

    #[test]
    fn signed_request_rejects_stale_timestamp() {
        let mut headers = HeaderMap::new();
        headers.insert("x-vozen-timestamp", HeaderValue::from_static("1"));
        headers.insert("x-vozen-nonce", HeaderValue::from_static("n"));
        headers.insert("x-vozen-signature", HeaderValue::from_static("v1=bad"));
        assert!(!verify_request(
            &headers,
            b"{}",
            "secret",
            &Arc::new(Mutex::new(HashMap::new()))
        ));
    }

    #[test]
    fn signed_nonce_is_single_use_within_the_timestamp_window() {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let body = br#"{"subject_id":"u"}"#;
        let nonce = "nonce-1";
        let body_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(body));
        let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
        mac.update(format!("{now}\n{nonce}\n{body_hash}").as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-vozen-timestamp",
            HeaderValue::from_str(&now.to_string()).unwrap(),
        );
        headers.insert("x-vozen-nonce", HeaderValue::from_static(nonce));
        headers.insert(
            "x-vozen-signature",
            HeaderValue::from_str(&format!("v1={signature}")).unwrap(),
        );
        let cache = Arc::new(Mutex::new(HashMap::new()));
        assert!(verify_request(&headers, body, "secret", &cache));
        assert!(!verify_request(&headers, body, "secret", &cache));
    }

    #[test]
    fn guild_resolution_keeps_user_plus_and_activated_pass_entitlements() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = OffsetDateTime::now_utc().unix_timestamp_nanos() as i64 / 1_000_000;
        store
            .grant_user_premium("plus-user", 30, "test", now)
            .unwrap();
        let plus = resolve_from_store(
            &store,
            &ResolveRequest {
                subject_id: "plus-user".into(),
                guild_id: Some("guild".into()),
            },
            now,
        )
        .unwrap();
        assert_eq!(plus.plan, "plus");
        assert_eq!(plus.scope, "guild");

        store
            .grant_guild_pass("premium-user", 3, 30, "test", now)
            .unwrap();
        store.activate_seat("premium-user", "guild", now).unwrap();
        let premium = resolve_from_store(
            &store,
            &ResolveRequest {
                subject_id: "premium-user".into(),
                guild_id: Some("guild".into()),
            },
            now,
        )
        .unwrap();
        assert_eq!(premium.plan, "premium");
        assert_eq!(premium.guild_limit, 3);
    }

    #[test]
    fn guild_seat_unlocks_premium_for_every_administrator_of_that_guild() {
        let store = SqliteStore::open_in_memory().unwrap();
        let now = OffsetDateTime::now_utc().unix_timestamp_nanos() as i64 / 1_000_000;
        store
            .grant_guild_pass("subscription-owner", 3, 30, "test", now)
            .unwrap();
        store
            .activate_seat("subscription-owner", "funded-guild", now)
            .unwrap();

        let other_admin = resolve_from_store(
            &store,
            &ResolveRequest {
                subject_id: "different-guild-admin".into(),
                guild_id: Some("funded-guild".into()),
            },
            now,
        )
        .unwrap();
        assert_eq!(other_admin.plan, "premium");
        assert_eq!(other_admin.scope, "guild");

        let other_guild = resolve_from_store(
            &store,
            &ResolveRequest {
                subject_id: "different-guild-admin".into(),
                guild_id: Some("unfunded-guild".into()),
            },
            now,
        )
        .unwrap();
        assert_eq!(other_guild.plan, "free");
    }
}
