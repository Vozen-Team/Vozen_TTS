//! Server-side Discord bot installation flow for Vozen TTS.
//!
//! The browser receives only an opaque signed state. Discord client secrets,
//! authorization codes and the one-time replay record stay server-side.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use vozen_store::SqliteStore;

type HmacSha256 = Hmac<Sha256>;
const STATE_TTL_MS: i64 = 10 * 60 * 1_000;
const DISCORD_TOKEN_ENDPOINT: &str = "https://discord.com/api/v10/oauth2/token";
const DISCORD_AUTHORIZE_ENDPOINT: &str = "https://discord.com/oauth2/authorize";
const TTS_PERMISSIONS: &str = "326420745232";

#[derive(Clone)]
pub struct InstallApiConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub success_redirect: String,
    pub state_secret: String,
    pub store: Arc<Mutex<SqliteStore>>,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallApiConfigError {
    ClientId,
    ClientSecret,
    RedirectUri,
    SuccessRedirect,
    StateSecret,
}

impl std::fmt::Display for InstallApiConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ClientId => "TTS install OAuth requires a Discord client id",
            Self::ClientSecret => "TTS install OAuth requires a Discord client secret",
            Self::RedirectUri => "TTS install OAuth callback must be an HTTPS API URL",
            Self::SuccessRedirect => {
                "TTS install OAuth success redirect must be the Vozen dashboard"
            }
            Self::StateSecret => "TTS install OAuth state secret must be at least 32 characters",
        })
    }
}
impl std::error::Error for InstallApiConfigError {}

#[derive(Clone)]
struct InstallState {
    client_id: Arc<str>,
    client_secret: Arc<str>,
    redirect_uri: Arc<str>,
    success_redirect: Arc<str>,
    state_secret: Arc<str>,
    store: Arc<Mutex<SqliteStore>>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    client: Client,
}

pub fn install_router(config: InstallApiConfig) -> Result<Router, InstallApiConfigError> {
    validate_config(&config)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap_or_else(|_| Client::new());
    Ok(Router::new()
        .route("/api/install/tts/start", get(start))
        .route("/api/install/tts/callback", get(callback))
        .with_state(InstallState {
            client_id: Arc::from(config.client_id),
            client_secret: Arc::from(config.client_secret),
            redirect_uri: Arc::from(config.redirect_uri),
            success_redirect: Arc::from(config.success_redirect),
            state_secret: Arc::from(config.state_secret),
            store: config.store,
            now: config.now,
            client,
        }))
}

fn validate_config(config: &InstallApiConfig) -> Result<(), InstallApiConfigError> {
    if config.client_id.trim().is_empty() {
        return Err(InstallApiConfigError::ClientId);
    }
    if config.client_secret.trim().is_empty() {
        return Err(InstallApiConfigError::ClientSecret);
    }
    if config.state_secret.len() < 32 {
        return Err(InstallApiConfigError::StateSecret);
    }
    if !is_canonical_callback(&config.redirect_uri) {
        return Err(InstallApiConfigError::RedirectUri);
    }
    let success = url::Url::parse(&config.success_redirect)
        .map_err(|_| InstallApiConfigError::SuccessRedirect)?;
    if success.scheme() != "https"
        || !success
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("vozen.org"))
        || !matches!(success.path(), "/dashboard/" | "/dashboard")
    {
        return Err(InstallApiConfigError::SuccessRedirect);
    }
    Ok(())
}

fn is_canonical_callback(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.vozen.org"))
        && url.path() == "/api/install/tts/callback"
        && url.query().is_none()
        && url.fragment().is_none()
}

#[derive(Deserialize)]
struct StartQuery {
    source: Option<String>,
}

async fn start(
    State(state): State<InstallState>,
    Query(query): Query<StartQuery>,
) -> Result<Response, InstallResponseError> {
    let source = install_source(query.source.as_deref())
        .ok_or(InstallResponseError::BadRequest("invalid_install_source"))?;
    let now = (state.now)();
    let payload = format!(
        "{source}|{}|{}",
        now.saturating_add(STATE_TTL_MS),
        Uuid::new_v4()
    );
    let token = signed_state(&payload, &state.state_secret).expect("HMAC accepts any key length");
    let state_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()));
    let store = state
        .store
        .lock()
        .map_err(|_| InstallResponseError::Internal)?;
    // A best-effort sweep keeps the replay table bounded without a separate
    // scheduler. It cannot affect the newly-created state.
    let _ = store.purge_install_oauth_states(now);
    store
        .register_install_oauth_state(&state_hash, now.saturating_add(STATE_TTL_MS))
        .map_err(|_| InstallResponseError::Internal)?;
    let mut url = url::Url::parse(DISCORD_AUTHORIZE_ENDPOINT).expect("constant URL");
    url.query_pairs_mut()
        .append_pair("client_id", &state.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &state.redirect_uri)
        .append_pair("scope", "bot applications.commands")
        .append_pair("permissions", TTS_PERMISSIONS)
        .append_pair("integration_type", "0")
        .append_pair("state", &token);
    Ok(Redirect::temporary(url.as_str()).into_response())
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(state): State<InstallState>,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, InstallResponseError> {
    if query.error.is_some() {
        return Ok(redirect_outcome(&state.success_redirect, "cancelled"));
    }
    let code = query
        .code
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or(InstallResponseError::BadRequest("missing_code"))?;
    let state_token = query
        .state
        .as_deref()
        .ok_or(InstallResponseError::BadRequest("missing_state"))?;
    let payload = verify_state(state_token, &state.state_secret)
        .ok_or(InstallResponseError::BadRequest("invalid_state"))?;
    let (source, expires_at, _nonce) =
        parse_state(&payload).ok_or(InstallResponseError::BadRequest("invalid_state"))?;
    let now = (state.now)();
    if expires_at < now {
        return Err(InstallResponseError::BadRequest("expired_state"));
    }
    let state_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(state_token.as_bytes()));
    let consumed = state
        .store
        .lock()
        .map_err(|_| InstallResponseError::Internal)?
        .consume_install_oauth_state(&state_hash, now)
        .map_err(|_| InstallResponseError::Internal)?;
    if !consumed {
        return Err(InstallResponseError::BadRequest("state_replayed"));
    }
    let exchange = state
        .client
        .post(DISCORD_TOKEN_ENDPOINT)
        .form(&[
            ("client_id", state.client_id.as_ref()),
            ("client_secret", state.client_secret.as_ref()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", state.redirect_uri.as_ref()),
        ])
        .send()
        .await
        .map_err(|_| InstallResponseError::BadGateway)?;
    if !exchange.status().is_success() {
        return Ok(redirect_outcome(&state.success_redirect, "oauth_failed"));
    }
    let body: serde_json::Value = exchange
        .json()
        .await
        .map_err(|_| InstallResponseError::BadGateway)?;
    let guild_id = verified_guild_id(&body);
    if let Some(guild_id) = guild_id {
        state
            .store
            .lock()
            .map_err(|_| InstallResponseError::Internal)?
            .set_guild_install_source(guild_id, source, now)
            .map_err(|_| InstallResponseError::Internal)?;
    }
    Ok(redirect_outcome(
        &state.success_redirect,
        if guild_id.is_some() {
            "installed"
        } else {
            "guild_missing"
        },
    ))
}

fn redirect_outcome(base: &str, outcome: &str) -> Response {
    let mut url = url::Url::parse(base).expect("validated success URL");
    url.query_pairs_mut()
        .append_pair("installed", if outcome == "installed" { "1" } else { "0" })
        .append_pair("install", outcome);
    Redirect::to(url.as_str()).into_response()
}

fn install_source(value: Option<&str>) -> Option<&'static str> {
    match value.unwrap_or("home") {
        "home" => Some("home"),
        "tts-hero" => Some("tts-hero"),
        "tts-pricing" => Some("tts-pricing"),
        "commands" => Some("commands"),
        "topgg" => Some("topgg"),
        "discordbotlist" => Some("discordbotlist"),
        _ => None,
    }
}

fn signed_state(payload: &str, secret: &str) -> Option<String> {
    let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(encoded.as_bytes());
    Some(format!(
        "{encoded}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    ))
}

fn verify_state(token: &str, secret: &str) -> Option<String> {
    let (encoded, signature) = token.split_once('.')?;
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(encoded.as_bytes());
    mac.verify_slice(&signature).ok()?;
    String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()
}

fn parse_state(payload: &str) -> Option<(&str, i64, &str)> {
    let mut parts = payload.split('|');
    let source = install_source(Some(parts.next()?))?;
    let expires_at = parts.next()?.parse().ok()?;
    let nonce = parts.next()?;
    (parts.next().is_none() && !nonce.is_empty()).then_some((source, expires_at, nonce))
}

fn valid_snowflake(value: &str) -> bool {
    (17..=22).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn verified_guild_id(body: &serde_json::Value) -> Option<&str> {
    // Query parameters are controlled by the browser. Only the server-to-server
    // token exchange can attest which guild the bot was actually installed in.
    body.pointer("/guild/id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| valid_snowflake(value))
}

enum InstallResponseError {
    BadRequest(&'static str),
    BadGateway,
    Internal,
}
impl IntoResponse for InstallResponseError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::BadRequest(code) => (StatusCode::BAD_REQUEST, code),
            Self::BadGateway => (StatusCode::BAD_GATEWAY, "discord_unavailable"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        (
            status,
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(serde_json::json!({"error":code})),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installation_adds_only_manage_channels_to_existing_permissions() {
        let permissions: u64 = TTS_PERMISSIONS.parse().unwrap();
        assert_eq!(permissions, 326420745216 | 16);
        assert_eq!(permissions & 8, 0);
    }

    #[test]
    fn installed_guild_requires_the_trusted_token_response() {
        assert_eq!(
            verified_guild_id(&serde_json::json!({"guild":{"id":"123456789012345678"}})),
            Some("123456789012345678")
        );
        for body in [
            serde_json::json!({}),
            serde_json::json!({"guild_id":"123456789012345678"}),
            serde_json::json!({"guild":{"id":"invalid"}}),
            serde_json::json!({"guild":{"id":123456789012345678_u64}}),
        ] {
            assert_eq!(verified_guild_id(&body), None);
        }
    }
    #[test]
    fn signed_state_rejects_tampering_and_unapproved_sources() {
        let state = signed_state(
            "tts-hero|2000|nonce",
            "a-secret-with-at-least-thirty-two-chars",
        )
        .unwrap();
        assert_eq!(
            verify_state(&state, "a-secret-with-at-least-thirty-two-chars").as_deref(),
            Some("tts-hero|2000|nonce")
        );
        assert!(
            verify_state(
                &format!("{state}x"),
                "a-secret-with-at-least-thirty-two-chars"
            )
            .is_none()
        );
        assert_eq!(install_source(Some("https://attacker.invalid")), None);
        assert_eq!(
            install_source(Some("discordbotlist")),
            Some("discordbotlist")
        );
    }

    #[test]
    fn installation_callback_is_pinned_to_the_public_api_origin() {
        assert!(is_canonical_callback(
            "https://api.vozen.org/api/install/tts/callback"
        ));
        assert!(!is_canonical_callback(
            "https://api.vozen.org/api/install/tts/callback?next=https://evil.invalid"
        ));
        assert!(!is_canonical_callback(
            "https://evil.invalid/api/install/tts/callback"
        ));
    }
}
