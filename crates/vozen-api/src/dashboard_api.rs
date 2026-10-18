//! Authenticated dashboard HTTP routes.
//!
//! The authorizer is deliberately separate from Discord's mutable gateway cache. A production
//! implementation must prove OAuth audience + `identify guilds`, Manage Guild/Administrator and
//! current bot presence before it returns `Allowed`; no configuration is read before that point.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use axum::{
    Router,
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use serde_json::{Value, json};
use vozen_store::{ChannelProfile, ChannelProfilePatch, GuildConfig, SqliteStore};

use crate::dashboard_validation::{
    ChannelProfileValidationOptions, InvalidChannelProfile, InvalidDashboardSetting,
    SanitizeChannelProfilePatch, SanitizeDashboardPatch, sanitize_channel_profile_patch,
    sanitize_dashboard_patch,
};

const MAX_DASHBOARD_BODY_BYTES: usize = 8_000;
const DASHBOARD_RATE_MAX: usize = 30;
const DASHBOARD_RATE_WINDOW_MS: i64 = 10_000;
const DASHBOARD_RATE_MAX_ENTRIES: usize = 2_048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManageableGuild {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardAccess<T> {
    Allowed(T),
    Unauthenticated,
    Forbidden,
    Unavailable,
}

#[async_trait]
pub trait DashboardAuthorizer: Send + Sync {
    /// Lists only guilds that pass all access checks; no raw Discord guild list is exposed.
    async fn manageable_guilds(&self, bearer: &str) -> DashboardAccess<Vec<ManageableGuild>>;
    /// Rechecks live access for each guild operation, even if a guild list was cached upstream.
    async fn authorize_guild(&self, bearer: &str, guild_id: &str) -> DashboardAccess<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DashboardOption {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unavailable: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DashboardOptions {
    pub channels: Vec<DashboardOption>,
    pub voices: Vec<DashboardOption>,
    pub locales: Vec<DashboardOption>,
    pub voice_channels: Vec<DashboardOption>,
    pub roles: Vec<DashboardOption>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardOptionsError {
    Unavailable,
}

/// Data obtained from the live Discord client only after authorization. Errors fail closed.
#[async_trait]
pub trait DashboardOptionsProvider: Send + Sync {
    async fn options_for_guild(
        &self,
        guild_id: &str,
    ) -> Result<DashboardOptions, DashboardOptionsError>;
}

pub struct DashboardApiConfig {
    pub origin: String,
    pub store: Arc<Mutex<SqliteStore>>,
    pub authorizer: Arc<dyn DashboardAuthorizer>,
    pub options: Arc<dyn DashboardOptionsProvider>,
}

#[derive(Clone)]
struct DashboardState {
    origin: HeaderValue,
    store: Arc<Mutex<SqliteStore>>,
    authorizer: Arc<dyn DashboardAuthorizer>,
    options: Arc<dyn DashboardOptionsProvider>,
    rate: Arc<Mutex<HashMap<String, RateState>>>,
}

#[derive(Clone, Copy)]
struct RateState {
    count: usize,
    reset: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardApiConfigError {
    Origin,
}

impl std::fmt::Display for DashboardApiConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("dashboard API requires a valid exact site origin")
    }
}

impl std::error::Error for DashboardApiConfigError {}

pub fn dashboard_router(config: DashboardApiConfig) -> Result<Router, DashboardApiConfigError> {
    let origin =
        HeaderValue::from_str(&config.origin).map_err(|_| DashboardApiConfigError::Origin)?;
    let state = DashboardState {
        origin,
        store: config.store,
        authorizer: config.authorizer,
        options: config.options,
        rate: Arc::new(Mutex::new(HashMap::new())),
    };
    Ok(Router::new()
        .route("/api/dashboard/guilds", get(list_guilds).options(preflight))
        .route(
            "/api/dashboard/guild/{guild_id}",
            get(get_guild).post(save_guild).options(preflight),
        )
        .route(
            "/api/dashboard/guild/{guild_id}/profile/{channel_id}",
            post(save_profile).delete(delete_profile).options(preflight),
        )
        .with_state(state))
}

async fn list_guilds(State(state): State<DashboardState>, headers: HeaderMap) -> Response {
    if rate_limited(&state, &headers) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", &state);
    }
    let Some(bearer) = bearer_token(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "no_token", &state);
    };
    match state.authorizer.manageable_guilds(bearer).await {
        DashboardAccess::Allowed(guilds) => {
            json_response(StatusCode::OK, json!({"guilds":guilds}), &state)
        }
        DashboardAccess::Unauthenticated => {
            error(StatusCode::UNAUTHORIZED, "invalid_token", &state)
        }
        DashboardAccess::Forbidden => error(StatusCode::FORBIDDEN, "forbidden", &state),
        DashboardAccess::Unavailable => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "authorization_unavailable",
            &state,
        ),
    }
}

async fn get_guild(
    State(state): State<DashboardState>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if rate_limited(&state, &headers) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", &state);
    }
    let Some(bearer) = bearer_token(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "no_token", &state);
    };
    if !valid_discord_id(&guild_id) {
        return error(StatusCode::BAD_REQUEST, "bad_guild", &state);
    }
    match authorize(&state, bearer, &guild_id).await {
        Ok(()) => match build_payload(&state, &guild_id).await {
            Ok(payload) => json_response(StatusCode::OK, payload, &state),
            Err(DashboardOptionsError::Unavailable) => {
                error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state)
            }
        },
        Err(response) => response,
    }
}

async fn save_guild(
    State(state): State<DashboardState>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if rate_limited(&state, &headers) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", &state);
    }
    let Some(bearer) = bearer_token(&headers).map(str::to_owned) else {
        return error(StatusCode::UNAUTHORIZED, "no_token", &state);
    };
    if !valid_discord_id(&guild_id) {
        return error(StatusCode::BAD_REQUEST, "bad_guild", &state);
    }
    if let Err(response) = authorize(&state, &bearer, &guild_id).await {
        return response;
    }
    let input = match read_json(request).await {
        Ok(input) => input,
        Err(JsonBodyError::TooLarge) => {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "too_large", &state);
        }
        Err(JsonBodyError::Invalid) => return error(StatusCode::BAD_REQUEST, "bad_json", &state),
    };
    let options = match state.options.options_for_guild(&guild_id).await {
        Ok(options) => options,
        Err(DashboardOptionsError::Unavailable) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state);
        }
    };
    let result = {
        let Ok(store) = state.store.lock() else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state);
        };
        let current = match store.guild_config(&guild_id) {
            Ok(config) => config,
            Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state),
        };
        sanitize_dashboard_patch(&input, &validation_options(&options), &current)
    };
    let SanitizeDashboardPatch::Valid(patch) = result else {
        let SanitizeDashboardPatch::Invalid(field) = result else {
            unreachable!()
        };
        return invalid_setting(field, &state);
    };
    let Ok(store) = state.store.lock() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state);
    };
    if store.update_guild_config(&guild_id, *patch).is_err() {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state);
    }
    drop(store);
    match build_payload_with_options(&state, &guild_id, options) {
        Ok(payload) => json_response(StatusCode::OK, payload, &state),
        Err(DashboardOptionsError::Unavailable) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state)
        }
    }
}

async fn save_profile(
    State(state): State<DashboardState>,
    Path((guild_id, channel_id)): Path<(String, String)>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if rate_limited(&state, &headers) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", &state);
    }
    let Some(bearer) = bearer_token(&headers).map(str::to_owned) else {
        return error(StatusCode::UNAUTHORIZED, "no_token", &state);
    };
    if !valid_discord_id(&guild_id) || !valid_discord_id(&channel_id) {
        return error(StatusCode::BAD_REQUEST, "bad_guild", &state);
    }
    if let Err(response) = authorize(&state, &bearer, &guild_id).await {
        return response;
    }
    let input = match read_json(request).await {
        Ok(input) => input,
        Err(JsonBodyError::TooLarge) => {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "too_large", &state);
        }
        Err(JsonBodyError::Invalid) => return error(StatusCode::BAD_REQUEST, "bad_json", &state),
    };
    let options = match state.options.options_for_guild(&guild_id).await {
        Ok(options) => options,
        Err(DashboardOptionsError::Unavailable) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state);
        }
    };
    if !options
        .channels
        .iter()
        .any(|channel| channel.id == channel_id)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_channel", &state);
    }
    let result = sanitize_channel_profile_patch(&input, &profile_validation_options(&options));
    let SanitizeChannelProfilePatch::Valid(update) = result else {
        let SanitizeChannelProfilePatch::Invalid(field) = result else {
            unreachable!()
        };
        return invalid_profile(field, &state);
    };
    let Ok(store) = state.store.lock() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state);
    };
    let current = match store.channel_profile(&guild_id, &channel_id) {
        Ok(profile) => profile
            .map(|profile| ChannelProfilePatch::from(&profile))
            .unwrap_or_default(),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state),
    };
    match store.save_channel_profile(&guild_id, &channel_id, &update.apply_to(current)) {
        Ok(true) => {}
        Ok(false) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"error":"invalid_profile","field":"limit"}),
                &state,
            );
        }
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state),
    }
    drop(store);
    match build_payload_with_options(&state, &guild_id, options) {
        Ok(payload) => json_response(StatusCode::OK, payload, &state),
        Err(DashboardOptionsError::Unavailable) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state)
        }
    }
}

async fn delete_profile(
    State(state): State<DashboardState>,
    Path((guild_id, channel_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if rate_limited(&state, &headers) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", &state);
    }
    let Some(bearer) = bearer_token(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "no_token", &state);
    };
    if !valid_discord_id(&guild_id) || !valid_discord_id(&channel_id) {
        return error(StatusCode::BAD_REQUEST, "bad_guild", &state);
    }
    if let Err(response) = authorize(&state, bearer, &guild_id).await {
        return response;
    }
    let Ok(store) = state.store.lock() else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state);
    };
    if store
        .delete_channel_profile(&guild_id, &channel_id)
        .is_err()
    {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &state);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    common_headers(response.headers_mut(), &state);
    response
}

async fn authorize(state: &DashboardState, bearer: &str, guild_id: &str) -> Result<(), Response> {
    match state.authorizer.authorize_guild(bearer, guild_id).await {
        DashboardAccess::Allowed(()) => Ok(()),
        DashboardAccess::Unauthenticated => {
            Err(error(StatusCode::UNAUTHORIZED, "invalid_token", state))
        }
        DashboardAccess::Forbidden => Err(error(StatusCode::FORBIDDEN, "forbidden", state)),
        DashboardAccess::Unavailable => Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "authorization_unavailable",
            state,
        )),
    }
}

async fn build_payload(
    state: &DashboardState,
    guild_id: &str,
) -> Result<Value, DashboardOptionsError> {
    build_payload_with_options(
        state,
        guild_id,
        state.options.options_for_guild(guild_id).await?,
    )
}

fn build_payload_with_options(
    state: &DashboardState,
    guild_id: &str,
    mut options: DashboardOptions,
) -> Result<Value, DashboardOptionsError> {
    let store = state
        .store
        .lock()
        .map_err(|_| DashboardOptionsError::Unavailable)?;
    let config = store
        .guild_config(guild_id)
        .map_err(|_| DashboardOptionsError::Unavailable)?;
    let profiles = store
        .list_channel_profiles(guild_id)
        .map_err(|_| DashboardOptionsError::Unavailable)?;
    drop(store);
    if let Some(id) = config.tts_channel_id.as_ref()
        && !options.channels.iter().any(|option| &option.id == id)
    {
        options.channels.insert(0, unavailable(id));
    }
    if !config.default_voice.is_empty()
        && !options
            .voices
            .iter()
            .any(|option| option.id == config.default_voice)
    {
        options.voices.insert(0, unavailable(&config.default_voice));
    }
    Ok(json!({
        "config": config_body(&config),
        "capabilities": {"ttsChannelId":true,"defaultVoice":true,"channelProfiles":true},
        "options": {"channels":options.channels,"voices":options.voices,"locales":options.locales,"voiceChannels":options.voice_channels,"roles":options.roles},
        "channelProfiles": profiles.into_iter().map(profile_body).collect::<Vec<_>>(),
    }))
}

fn unavailable(id: &str) -> DashboardOption {
    DashboardOption {
        id: id.to_owned(),
        label: id.to_owned(),
        unavailable: true,
    }
}

fn config_body(config: &GuildConfig) -> Value {
    json!({"autoread":config.autoread,"xsaid":config.xsaid,"autojoin":config.autojoin,"readBots":config.read_bots,"textInVoice":config.text_in_voice,"antispam":config.antispam,"streakAnnounce":config.streak_announce,"soundboard":config.soundboard,"greetOnJoin":config.greet_on_join,"translationEnabled":config.translation_enabled,"votePromos":config.vote_promos,"maxChars":config.max_chars,"ratePerMin":config.rate_per_min,"locale":config.locale,"ttsChannelId":config.tts_channel_id,"defaultVoice":config.default_voice,"priorityRoleId":config.priority_role_id,"blockedRoleId":config.blocked_role_id})
}

fn profile_body(profile: ChannelProfile) -> Value {
    json!({"guildId":profile.guild_id,"channelId":profile.channel_id,"autoRead":profile.auto_read,"translationEnabled":profile.translation_enabled,"defaultVoice":profile.default_voice,"engine":profile.engine.map(engine_name),"speed":profile.speed,"maxChars":profile.max_chars,"readBots":profile.read_bots,"voiceChannelId":profile.voice_channel_id,"locale":profile.locale,"effect":profile.effect})
}

fn engine_name(engine: vozen_store::UserEngine) -> &'static str {
    match engine {
        vozen_store::UserEngine::Google => "google",
        vozen_store::UserEngine::Piper => "piper",
        vozen_store::UserEngine::Kokoro => "kokoro",
        vozen_store::UserEngine::Gcloud => "gcloud",
    }
}

fn validation_options(
    options: &DashboardOptions,
) -> crate::dashboard_validation::DashboardValidationOptions {
    crate::dashboard_validation::DashboardValidationOptions {
        channel_ids: options
            .channels
            .iter()
            .map(|option| option.id.clone())
            .collect(),
        voice_ids: options
            .voices
            .iter()
            .map(|option| option.id.clone())
            .collect(),
        role_ids: options
            .roles
            .iter()
            .map(|option| option.id.clone())
            .collect(),
    }
}

fn profile_validation_options(options: &DashboardOptions) -> ChannelProfileValidationOptions {
    ChannelProfileValidationOptions {
        dashboard: validation_options(options),
        voice_channel_ids: options
            .voice_channels
            .iter()
            .map(|option| option.id.clone())
            .collect(),
    }
}

fn invalid_setting(field: InvalidDashboardSetting, state: &DashboardState) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        json!({"error":"invalid_setting","field":setting_field(field)}),
        state,
    )
}
fn invalid_profile(field: InvalidChannelProfile, state: &DashboardState) -> Response {
    json_response(
        StatusCode::BAD_REQUEST,
        json!({"error":"invalid_profile","field":profile_field(field)}),
        state,
    )
}
fn setting_field(field: InvalidDashboardSetting) -> &'static str {
    match field {
        InvalidDashboardSetting::TtsChannelId => "ttsChannelId",
        InvalidDashboardSetting::DefaultVoice => "defaultVoice",
        InvalidDashboardSetting::PriorityRoleId => "priorityRoleId",
        InvalidDashboardSetting::BlockedRoleId => "blockedRoleId",
    }
}
fn profile_field(field: InvalidChannelProfile) -> &'static str {
    match field {
        InvalidChannelProfile::AutoRead => "autoRead",
        InvalidChannelProfile::TranslationEnabled => "translationEnabled",
        InvalidChannelProfile::DefaultVoice => "defaultVoice",
        InvalidChannelProfile::Engine => "engine",
        InvalidChannelProfile::Speed => "speed",
        InvalidChannelProfile::MaxChars => "maxChars",
        InvalidChannelProfile::ReadBots => "readBots",
        InvalidChannelProfile::VoiceChannelId => "voiceChannelId",
        InvalidChannelProfile::Locale => "locale",
        InvalidChannelProfile::Effect => "effect",
    }
}

enum JsonBodyError {
    TooLarge,
    Invalid,
}

async fn read_json(request: Request) -> Result<Value, JsonBodyError> {
    let body = to_bytes(request.into_body(), MAX_DASHBOARD_BODY_BYTES)
        .await
        .map_err(|_| JsonBodyError::TooLarge)?;
    serde_json::from_slice(&body).map_err(|_| JsonBodyError::Invalid)
}
async fn preflight(State(state): State<DashboardState>) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    common_headers(response.headers_mut(), &state);
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Authorization, Content-Type"),
    );
    response
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
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

fn rate_limited(state: &DashboardState, headers: &HeaderMap) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    let Ok(mut rate) = state.rate.lock() else {
        return true;
    };
    rate.retain(|_, value| value.reset > now);
    let ip = client_ip(headers);
    if !rate.contains_key(&ip)
        && rate.len() >= DASHBOARD_RATE_MAX_ENTRIES
        && let Some(oldest) = rate
            .iter()
            .min_by_key(|(_, value)| value.reset)
            .map(|(ip, _)| ip.clone())
    {
        rate.remove(&oldest);
    }
    let entry = rate.entry(ip).or_insert(RateState {
        count: 0,
        reset: now + DASHBOARD_RATE_WINDOW_MS,
    });
    entry.count += 1;
    entry.count > DASHBOARD_RATE_MAX
}

fn valid_discord_id(id: &str) -> bool {
    (17..=20).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit())
}
fn error(status: StatusCode, code: &'static str, state: &DashboardState) -> Response {
    json_response(status, json!({"error":code}), state)
}
fn json_response(status: StatusCode, value: Value, state: &DashboardState) -> Response {
    let mut response = (status, axum::Json(value)).into_response();
    common_headers(response.headers_mut(), state);
    response
}
fn common_headers(headers: &mut HeaderMap, state: &DashboardState) {
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, state.origin.clone());
    // The public dashboard sends its OAuth bearer request with credentials
    // enabled so a future same-site session bridge can be used as well. Browsers
    // reject that cross-origin response unless this explicit opt-in is present,
    // including for 401s and the OPTIONS preflight.
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
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
    use axum::{
        body::Body,
        http::{Method, Request},
    };
    use tower::ServiceExt;

    const GUILD: &str = "999999999999999999";
    const CHANNEL: &str = "888888888888888888";

    struct Auth;
    #[async_trait]
    impl DashboardAuthorizer for Auth {
        async fn manageable_guilds(&self, bearer: &str) -> DashboardAccess<Vec<ManageableGuild>> {
            if bearer == "good" {
                DashboardAccess::Allowed(vec![ManageableGuild {
                    id: GUILD.into(),
                    name: "Mine".into(),
                    icon: None,
                }])
            } else {
                DashboardAccess::Unauthenticated
            }
        }
        async fn authorize_guild(&self, bearer: &str, guild_id: &str) -> DashboardAccess<()> {
            if bearer != "good" {
                DashboardAccess::Unauthenticated
            } else if guild_id == GUILD {
                DashboardAccess::Allowed(())
            } else {
                DashboardAccess::Forbidden
            }
        }
    }
    struct Options;
    #[async_trait]
    impl DashboardOptionsProvider for Options {
        async fn options_for_guild(
            &self,
            _guild_id: &str,
        ) -> Result<DashboardOptions, DashboardOptionsError> {
            Ok(DashboardOptions {
                channels: vec![DashboardOption {
                    id: CHANNEL.into(),
                    label: "#talk".into(),
                    unavailable: false,
                }],
                voices: vec![DashboardOption {
                    id: "en_US-amy-medium".into(),
                    label: "Amy".into(),
                    unavailable: false,
                }],
                voice_channels: vec![DashboardOption {
                    id: "777777777777777777".into(),
                    label: "Call".into(),
                    unavailable: false,
                }],
                ..DashboardOptions::default()
            })
        }
    }
    fn app() -> Router {
        dashboard_router(DashboardApiConfig {
            origin: "https://vozen.org".into(),
            store: Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store"))),
            authorizer: Arc::new(Auth),
            options: Arc::new(Options),
        })
        .expect("router")
    }
    fn request(method: Method, uri: &str, token: Option<&str>, body: &str) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_owned())).expect("request")
    }

    #[tokio::test]
    async fn access_precedes_config_and_post_returns_authoritative_state() {
        let app = app();
        let unauthenticated = app
            .clone()
            .oneshot(request(Method::GET, "/api/dashboard/guilds", None, ""))
            .await
            .expect("response");
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthenticated
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
            Some(&HeaderValue::from_static("true"))
        );
        assert_eq!(
            app.clone()
                .oneshot(request(
                    Method::GET,
                    "/api/dashboard/guild/999999999999999998",
                    Some("good"),
                    ""
                ))
                .await
                .expect("response")
                .status(),
            StatusCode::FORBIDDEN
        );
        let saved = app
            .oneshot(request(
                Method::POST,
                &format!("/api/dashboard/guild/{GUILD}"),
                Some("good"),
                &format!(r#"{{"ttsChannelId":"{CHANNEL}","defaultVoice":"en_US-amy-medium"}}"#),
            ))
            .await
            .expect("response");
        assert_eq!(saved.status(), StatusCode::OK);
        assert_eq!(
            saved.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("https://vozen.org"))
        );
        assert_eq!(
            saved
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
            Some(&HeaderValue::from_static("true"))
        );
    }

    #[tokio::test]
    async fn dashboard_preflight_allows_credentialed_browser_requests() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/dashboard/guilds")
                    .header(header::ORIGIN, "https://vozen.org")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
            Some(&HeaderValue::from_static("true"))
        );
    }

    #[tokio::test]
    async fn malformed_or_tampered_settings_do_not_write() {
        let app = app();
        let route = format!("/api/dashboard/guild/{GUILD}");
        let bad_guild = app
            .clone()
            .oneshot(request(
                Method::GET,
                "/api/dashboard/guild/not-a-discord-id",
                Some("good"),
                "",
            ))
            .await
            .expect("response");
        assert_eq!(bad_guild.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            to_bytes(bad_guild.into_body(), 1024)
                .await
                .expect("body")
                .as_ref(),
            br#"{"error":"bad_guild"}"#
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Method::POST, &route, Some("good"), "{no"))
                .await
                .expect("response")
                .status(),
            StatusCode::BAD_REQUEST
        );
        let too_large = "x".repeat(MAX_DASHBOARD_BODY_BYTES + 1);
        assert_eq!(
            app.clone()
                .oneshot(request(Method::POST, &route, Some("good"), &too_large))
                .await
                .expect("response")
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let invalid = app
            .oneshot(request(
                Method::POST,
                &route,
                Some("good"),
                r#"{"ttsChannelId":"forged"}"#,
            ))
            .await
            .expect("response");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn channel_profile_routes_keep_the_same_authorization_boundary() {
        let app = app();
        let route = format!("/api/dashboard/guild/{GUILD}/profile/{CHANNEL}");
        let forbidden = app
            .clone()
            .oneshot(request(
                Method::POST,
                &route,
                Some("bad"),
                r#"{"autoRead":true}"#,
            ))
            .await
            .expect("response");
        assert_eq!(forbidden.status(), StatusCode::UNAUTHORIZED);
        let saved = app
            .clone()
            .oneshot(request(
                Method::POST,
                &route,
                Some("good"),
                r#"{"autoRead":true,"engine":"piper","maxChars":500}"#,
            ))
            .await
            .expect("response");
        assert_eq!(saved.status(), StatusCode::OK);
        let deleted = app
            .oneshot(request(Method::DELETE, &route, Some("good"), ""))
            .await
            .expect("response");
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn dashboard_rate_limit_matches_node_window_and_limit() {
        let app = app();
        for _ in 0..DASHBOARD_RATE_MAX {
            let response = app
                .clone()
                .oneshot(request(
                    Method::GET,
                    "/api/dashboard/guilds",
                    Some("good"),
                    "",
                ))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
        }
        let limited = app
            .oneshot(request(
                Method::GET,
                "/api/dashboard/guilds",
                Some("good"),
                "",
            ))
            .await
            .expect("response");
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
