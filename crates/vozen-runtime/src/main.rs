#![forbid(unsafe_code)]

//! Rust process entry point for the production cutover and the isolated migration canary.
//!
//! Production runs in `RUST_RUNTIME_MODE=full`, after every required canary is enabled and the
//! Node gateway has been deliberately retired. Shadow mode is a staging-only migration tool: it
//! keeps SQLite authoritative while individual Discord, HTTP and Postgres adapters are verified.
//! See `docs/RUST-MIGRATION-STAGING.md` for the staging-only flags, including
//! `RUST_RUNTIME_MODE=shadow`, `RUST_POSTGRES_MODE=shadow|mirror`,
//! `RUST_POSTGRES_REPLICA_OUTBOX=true` and `RUST_POSTGRES_VOICE_READ_CACHE=true`.

#[cfg(feature = "voice-driver")]
mod activity_poster;
mod admin_metrics;
mod autocomplete_sink;
mod automatic_translation_sink;
mod birthday_sink;
mod bot_stats_sink;
mod config_blockword_sink;
mod config_channel_sink;
mod config_default_voice_sink;
mod config_greet_language_sink;
mod config_language_sink;
mod config_numeric_sink;
mod config_queue_role_sink;
mod config_reset_sink;
mod config_role_sink;
mod config_show_sink;
mod config_toggle_sink;
#[cfg(feature = "voice-driver")]
mod core_voice_sink;
mod engine_router;
mod error_reporter;
mod file_export_sink;
mod game_list_sink;
mod game_score_sink;
#[cfg(feature = "voice-driver")]
mod gcloud_adapter;
#[cfg(feature = "voice-driver")]
mod gtts_adapter;
mod guild_lifecycle_sink;
mod guild_welcome_sink;
mod help_sink;
mod invite_sink;
#[cfg(feature = "voice-driver")]
mod kokoro_adapter;
#[cfg(feature = "voice-driver")]
mod live_transcription_sink;
mod loop_lag;
#[cfg(feature = "voice-driver")]
mod neural_adapter;
mod owner_command_sink;
mod piper_adapter;
mod postgres_import;
mod postgres_metrics;
mod postgres_outbox;
mod postgres_shadow;
mod postgres_voice_cache;
mod premium_sink;
mod privacy_sink;
mod pronunciation_sink;
mod redeem_sink;
mod runtime_mode;
mod server_stats_sink;
mod setup_wizard;
mod stats_sink;
#[cfg(feature = "voice-driver")]
mod streak_card;
mod streak_style;
mod top_speakers_sink;
mod topgg_metrics;
mod transcription_adapter;
mod transcription_control_sink;
mod transcription_sink;
mod translation_preference_sink;
mod translation_provider;
mod translation_text_sink;
mod ui;
mod uptime_sink;
mod voice_preference_sink;
mod vote_sink;

use std::{
    collections::HashMap,
    env,
    net::{IpAddr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use std::fs;

use thiserror::Error;
use vozen_api::{
    ProviderHealth as PublicProviderHealth, PublicStatusInput, PublicStatusProvider,
    RuntimeRouterConfig,
    account_api::AccountApiConfig,
    admin_api::{AdminApiConfig, AdminTalkerProfile, AdminTalkerProfileResolver},
    admin_router::AdminRouterConfig,
    dashboard_api::{
        DashboardApiConfig, DashboardOption, DashboardOptions, DashboardOptionsError,
        DashboardOptionsProvider,
    },
    dashboard_oauth::DiscordDashboardAuthorizer,
    discord_oauth::DiscordOAuthVerifier,
    install_api::InstallApiConfig,
    kofi_webhook::KofiWebhookConfig,
    map_public_status,
    premium_api::{ClaimHelpNotifier, DiscordClaimHelpNotifier, PremiumApiConfig},
    runtime_router,
    topgg_webhook::TopggWebhookConfig,
    web_analytics::CloudflareWebAnalyticsConfig,
};
use vozen_contracts::DiscordCommandCatalog;
use vozen_core::{SynthesisEngine, parse_kofi_shop_map};
use vozen_discord::{
    CommandRegistrationConfig, CompositeGatewayEventSink, CoreVoiceSettings,
    DiscordDashboardOptionsProvider, DiscordHttpCommandRegistrationClient, DiscordRuntimeConfig,
    DiscordRuntimeError, GatewayEventSink, GatewayState, locale_display_options, register_commands,
    run_discord_gateway_with_state_and_sink, voice_display_options, write_planned_rejoin_marker,
};
use vozen_store::{
    DEPARTURE_GRACE_MS, ProviderHealth as StoreProviderHealth, RuntimeBatchBuffer, SqliteStore,
    TopggSyncDetail, month_key_utc,
};

use crate::owner_command_sink::OwnerCommandRuntimeOptions;
use crate::runtime_mode::RuntimeMode;
use crate::topgg_metrics::{
    ReqwestTopggMetricsHttp, TOPGG_POST_INTERVAL, TopggMetricsTrigger,
    post_topgg_stats_with_shards, sync_topgg_commands, validate_topgg_v1_token,
};
use crate::transcription_adapter::TranscriptionRuntimeOptions;
use crate::transcription_control_sink::SttConsentRegistry;

const DISCORD_COMMAND_CONTRACT: &str = include_str!("../../../contracts/discord-commands.json");

// Keep this catalogue in lockstep with the generated Rust voice-display contract.
// The legacy runtime exposes one synthetic Google voice for every locale not covered by
// an installed Piper model. Games such as Guess the Language use this same public voice
// catalogue to decide how many distinct playable languages exist.
// Only the voice-driver build needs to expand Piper's installed catalogue with
// the legacy synthetic Google voices. Keep the helper available to its unit
// tests in the portable build as well.
#[allow(dead_code)]
const GTTS_SYNTHETIC_LOCALES: &[&str] = &[
    "ar_JO", "ca_ES", "cs_CZ", "cy_GB", "da_DK", "de_DE", "el_GR", "en_GB", "en_US", "es_ES",
    "es_MX", "fa_IR", "fi_FI", "fr_FR", "hu_HU", "is_IS", "it_IT", "ja_JP", "ka_GE", "kk_KZ",
    "lb_LU", "lv_LV", "ne_NP", "nl_BE", "nl_NL", "no_NO", "pl_PL", "pt_BR", "pt_PT", "ro_RO",
    "ru_RU", "sk_SK", "sl_SI", "sr_RS", "sv_SE", "sw_CD", "tr_TR", "uk_UA", "vi_VN", "zh_CN",
];

struct RuntimeConfig {
    discord_token: String,
    database_path: PathBuf,
    postgres_shadow: Option<postgres_shadow::PostgresShadowConfig>,
    postgres_replica_outbox: bool,
    postgres_voice_read_cache: bool,
    health_bind: Option<SocketAddr>,
    public_status: Option<PublicStatusConfig>,
    premium_http: Option<PremiumHttpConfig>,
    install_oauth: Option<TtsInstallOAuthRuntimeConfig>,
    topgg_webhook: Option<TopggWebhookRuntimeConfig>,
    topgg_metrics: Option<TopggMetricsRuntimeConfig>,
    vote_redemption_secret: Option<String>,
    owner_commands: Option<OwnerCommandRuntimeOptions>,
    core_voice: Option<CoreVoiceRuntimeOptions>,
    tts_file: Option<TtsFileRuntimeOptions>,
    transcription: Option<TranscriptionRuntimeOptions>,
    transcription_control: bool,
    #[cfg(feature = "voice-driver")]
    transcription_live: bool,
    translation_text: Option<TranslationTextRuntimeOptions>,
    translation_preferences: bool,
    voice_preferences: Option<VoicePreferenceRuntimeOptions>,
    config_default_voice: Option<ConfigDefaultVoiceRuntimeOptions>,
    autocomplete: Option<autocomplete_sink::AutocompleteRuntimeOptions>,
    config_channel: bool,
    config_queue_roles: bool,
    config_greet_language: bool,
    config_blockword: bool,
    pronunciation: bool,
    config_language: bool,
    config_toggles: bool,
    config_numeric: bool,
    config_role: bool,
    config_reset: bool,
    config_show: bool,
    uptime: bool,
    invite: bool,
    invite_client_id: Option<String>,
    help: bool,
    help_support_url: String,
    welcome: bool,
    vote: bool,
    vote_client_id: Option<String>,
    top_speakers: bool,
    birthday: bool,
    bot_stats: bool,
    server_stats: bool,
    stats: bool,
    premium: bool,
    redeem: bool,
    privacy: bool,
    game_list: bool,
    game_scores: bool,
    automatic_translation: Option<AutomaticTranslationRuntimeOptions>,
    dashboard: Option<DashboardRuntimeOptions>,
    admin: Option<AdminRuntimeOptions>,
}

struct PublicStatusConfig {
    incident: Option<String>,
}

struct PremiumHttpConfig {
    browser_api_enabled: bool,
    client_id: Option<String>,
    dashboard_client_ids: Vec<String>,
    origin: String,
    kofi_webhook_token: Option<String>,
    kofi_shop_map: Option<String>,
    claim_help_webhook_url: Option<String>,
    stripe_secret_key: Option<String>,
    stripe_publishable_key: Option<String>,
    stripe_webhook_secret: Option<String>,
    stripe_prices: Option<vozen_api::stripe_api::StripePriceIds>,
}

struct TopggWebhookRuntimeConfig {
    client_id: String,
    webhook_secret: String,
    redemption_secret: String,
}

struct TopggMetricsRuntimeConfig {
    client_id: String,
    token: String,
}

struct TtsInstallOAuthRuntimeConfig {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    success_redirect: String,
    state_secret: String,
}

/// Browser dashboard migration is separate from the command promotion. It stays disabled unless
/// the operator explicitly enables it, so a loopback Rust shadow process cannot take ownership
/// of account configuration accidentally.
struct DashboardRuntimeOptions {
    models_dir: PathBuf,
}

/// Owner console routes are a separate, explicit promotion. Missing values keep the API inert;
/// the runtime never invents an owner, session secret or OAuth audience.
struct AdminRuntimeOptions {
    panel_origin: String,
    session_secret: Option<String>,
    owner_id: Option<String>,
    client_id: Option<String>,
    web_analytics: Option<CloudflareWebAnalyticsConfig>,
}

struct RuntimeDashboardOptionsProvider {
    discord: DiscordDashboardOptionsProvider,
    voices: Vec<DashboardOption>,
    locales: Vec<DashboardOption>,
}

impl RuntimeDashboardOptionsProvider {
    fn new(gateway_state: GatewayState, models: Vec<String>) -> Self {
        Self {
            discord: DiscordDashboardOptionsProvider::new(gateway_state),
            voices: dashboard_options(voice_display_options(&models)),
            locales: dashboard_options(locale_display_options()),
        }
    }
}

#[async_trait::async_trait]
impl DashboardOptionsProvider for RuntimeDashboardOptionsProvider {
    async fn options_for_guild(
        &self,
        guild_id: &str,
    ) -> Result<DashboardOptions, DashboardOptionsError> {
        let options = self
            .discord
            .options_for_guild(guild_id)
            .await
            .ok_or(DashboardOptionsError::Unavailable)?;
        Ok(DashboardOptions {
            channels: dashboard_options(options.channels),
            voices: self.voices.clone(),
            locales: self.locales.clone(),
            voice_channels: dashboard_options(options.voice_channels),
            roles: dashboard_options(options.roles),
        })
    }
}

fn dashboard_options(options: Vec<vozen_discord::DiscordDashboardOption>) -> Vec<DashboardOption> {
    options
        .into_iter()
        .map(|option| DashboardOption {
            id: option.id,
            label: option.label,
            unavailable: false,
        })
        .collect()
}

/// Resolves the small profile card shown in the private owner console. The browser never receives
/// the bot credential; the existing gateway HTTP client performs at most ten lookups per request.
struct RuntimeAdminTalkerProfileResolver {
    gateway_state: GatewayState,
}

#[async_trait::async_trait]
impl AdminTalkerProfileResolver for RuntimeAdminTalkerProfileResolver {
    async fn resolve_talker_profiles(
        &self,
        user_ids: &[String],
    ) -> HashMap<String, AdminTalkerProfile> {
        let Some(http) = self.gateway_state.discord_http() else {
            return HashMap::new();
        };
        let mut profiles = HashMap::with_capacity(user_ids.len());
        for user_id in user_ids {
            let Ok(id) = user_id.parse::<u64>() else {
                continue;
            };
            let Ok(user) = serenity::model::id::UserId::new(id).to_user(&http).await else {
                continue;
            };
            let avatar = user.face();
            profiles.insert(
                user_id.clone(),
                AdminTalkerProfile {
                    username: user.global_name.unwrap_or(user.name),
                    avatar: Some(avatar),
                },
            );
        }
        profiles
    }
}

/// Explicit opt-in configuration for the first Rust-owned Discord voice commands.
///
/// It shares Node's established Piper values, but it is never inferred from their presence:
/// `RUST_CORE_VOICE_ENABLED=true` is required so a normal Rust shadow process cannot claim
/// interactions by accident.
#[derive(Clone)]
#[cfg_attr(not(feature = "voice-driver"), allow(dead_code))]
struct CoreVoiceRuntimeOptions {
    piper_path: PathBuf,
    models_dir: PathBuf,
    cache_dir: PathBuf,
    gtts_cache_dir: PathBuf,
    ffmpeg: PathBuf,
    openai_api_key: Option<String>,
    neural_cache_dir: PathBuf,
    gcloud_api_key: Option<String>,
    gcloud_cache_dir: PathBuf,
    gcloud_limits: vozen_tts::GcloudLimits,
    kokoro_command: Option<vozen_tts::KokoroCommand>,
    kokoro_cache_dir: PathBuf,
    kokoro_languages: Option<Vec<String>>,
    piper_concurrency: usize,
    queue_cap: usize,
    queue_enabled: bool,
    message_autoread: bool,
    randomizer_enabled: bool,
    cast_enabled: bool,
    setup_enabled: bool,
    speak_context_enabled: bool,
    game_play_enabled: bool,
    client_id: Option<String>,
    support_url: String,
    settings: CoreVoiceSettings,
}

/// File export deliberately has a separate flag from in-call playback: it never joins a call or
/// requires Songbird, and can therefore be canaried independently while Node remains authority
/// for every other command.
#[derive(Clone)]
struct TtsFileRuntimeOptions {
    piper_path: PathBuf,
    models_dir: PathBuf,
    cache_dir: PathBuf,
    piper_concurrency: usize,
    settings: CoreVoiceSettings,
}

/// `/voice` preference mutations do not need the audio driver, but `/voice set` must validate
/// against the exact Piper model catalogue available to this process. Keeping that catalogue in
/// startup configuration prevents a stale Discord option from becoming a stored preference.
struct VoicePreferenceRuntimeOptions {
    available_models: Vec<String>,
    default_speed: f64,
}

/// `/config default-voice` shares the discovered Piper catalogue but has an independent canary
/// so preference migrations cannot accidentally claim an admin command.
struct ConfigDefaultVoiceRuntimeOptions {
    available_models: Vec<String>,
}

/// This is separate from automatic channel translation and is disabled unless the exact flag
/// is enabled alongside the matching Node ownership boundary.
struct TranslationTextRuntimeOptions {
    provider: translation_provider::RuntimeTranslationProvider,
    text_enabled: bool,
    admin_enabled: bool,
    context_enabled: bool,
}

/// Automatic channel translation is a separate, explicitly promoted message path. It has no
/// relationship to speaking or `/translate text`, so it remains off until its own flag is set.
struct AutomaticTranslationRuntimeOptions {
    provider: translation_provider::RuntimeTranslationProvider,
}

impl RuntimeConfig {
    fn from_environment() -> Result<Self, RuntimeError> {
        let runtime_mode = RuntimeMode::from_environment()?;
        runtime_mode.validate_environment()?;
        let discord_token = env::var("DISCORD_TOKEN").map_err(|_| RuntimeError::MissingToken)?;
        if discord_token.trim().is_empty() {
            return Err(RuntimeError::MissingToken);
        }
        let database_path = env::var_os("DB_PATH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("./tts.db"));
        let postgres_shadow =
            postgres_shadow::PostgresShadowConfig::from_environment(runtime_mode)?;
        let postgres_replica_outbox = postgres_replica_outbox_enabled(
            env::var("RUST_POSTGRES_REPLICA_OUTBOX").ok().as_deref(),
        );
        if postgres_replica_outbox && postgres_shadow.is_none() {
            return Err(RuntimeError::PostgresReplicaRequiresPostgres);
        }
        let postgres_voice_read_cache = postgres_voice_read_cache_enabled(
            env::var("RUST_POSTGRES_VOICE_READ_CACHE").ok().as_deref(),
        );
        if postgres_voice_read_cache && (!postgres_replica_outbox || postgres_shadow.is_none()) {
            return Err(RuntimeError::PostgresVoiceReadCacheRequiresReplica);
        }
        let health_host = nonempty_env("HEALTH_HOST").unwrap_or_else(|| "127.0.0.1".to_owned());
        let health_ip = health_host
            .parse::<IpAddr>()
            .map_err(|_| RuntimeError::InvalidHealthHost)?;
        let health_bind = match env::var("HEALTH_PORT") {
            Ok(raw) if raw.trim().is_empty() => None,
            Ok(raw) => {
                let port = raw
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or(RuntimeError::InvalidHealthPort)?;
                Some(SocketAddr::new(health_ip, port))
            }
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => return Err(RuntimeError::InvalidHealthPort),
        };
        let premium_http = premium_http_from_environment()?;
        let install_oauth = tts_install_oauth_from_environment()?;
        if runtime_mode.is_full()
            && !browser_api_promoted(
                env::var("RUST_BROWSER_API_ENABLED").ok().as_deref(),
                premium_http.as_ref(),
            )
        {
            return Err(RuntimeError::FullRuntimeBrowserApiRequired);
        }
        let public_status = public_status_from_environment();
        let topgg_webhook = topgg_webhook_from_environment()?;
        let topgg_metrics = topgg_metrics_from_environment()?;
        let vote_redemption_secret = nonempty_env("VOTE_REDEMPTION_SECRET");
        let owner_commands = owner_commands_from_environment();
        if !full_owner_commands_ready(
            runtime_mode.is_full(),
            owner_commands_enabled(env::var("RUST_OWNER_COMMANDS_ENABLED").ok().as_deref()),
            owner_commands.as_ref(),
        ) {
            return Err(RuntimeError::FullRuntimeOwnerCommandsRequired);
        }
        let core_voice = core_voice_from_environment()?;
        let tts_file = tts_file_from_environment()?;
        let transcription = transcription_from_environment()?;
        let transcription_live_requested =
            live_transcription_enabled(env::var("RUST_TRANSCRIBE_LIVE_ENABLED").ok().as_deref());
        #[cfg(not(feature = "voice-driver"))]
        if transcription_live_requested {
            return Err(RuntimeError::LiveTranscriptionVoiceDriverRequired);
        }
        #[cfg(feature = "voice-driver")]
        let transcription_live = transcription_live_requested;
        let transcription_control = transcription_control_enabled(
            env::var("RUST_TRANSCRIBE_CONTROL_ENABLED").ok().as_deref(),
        ) || {
            #[cfg(feature = "voice-driver")]
            {
                transcription_live
            }
            #[cfg(not(feature = "voice-driver"))]
            {
                false
            }
        };
        let translation_text = translation_text_from_environment();
        let translation_preferences = translation_preferences_enabled(
            env::var("RUST_TRANSLATION_PREFERENCES_ENABLED")
                .ok()
                .as_deref(),
        );
        let voice_preferences = voice_preferences_from_environment()?;
        let config_default_voice = config_default_voice_from_environment()?;
        let autocomplete = autocomplete_from_environment(
            core_voice.as_ref(),
            voice_preferences.as_ref(),
            config_default_voice.as_ref(),
            config_language_enabled(env::var("RUST_CONFIG_LANGUAGE_ENABLED").ok().as_deref()),
            translation_preferences,
            pronunciation_enabled(env::var("RUST_PRONUNCIATION_ENABLED").ok().as_deref()),
            #[cfg(feature = "voice-driver")]
            transcription_live,
            #[cfg(not(feature = "voice-driver"))]
            false,
        )?;
        let config_channel =
            config_channel_enabled(env::var("RUST_CONFIG_CHANNEL_ENABLED").ok().as_deref());
        let config_queue_roles =
            config_queue_roles_enabled(env::var("RUST_CONFIG_QUEUE_ROLES_ENABLED").ok().as_deref());
        let config_greet_language = config_greet_language_enabled(
            env::var("RUST_CONFIG_GREET_LANGUAGE_ENABLED")
                .ok()
                .as_deref(),
        );
        let config_blockword =
            config_blockword_enabled(env::var("RUST_CONFIG_BLOCKWORD_ENABLED").ok().as_deref());
        let pronunciation =
            pronunciation_enabled(env::var("RUST_PRONUNCIATION_ENABLED").ok().as_deref());
        let config_language =
            config_language_enabled(env::var("RUST_CONFIG_LANGUAGE_ENABLED").ok().as_deref());
        let config_toggles =
            config_toggles_enabled(env::var("RUST_CONFIG_TOGGLES_ENABLED").ok().as_deref());
        let config_numeric =
            config_numeric_enabled(env::var("RUST_CONFIG_NUMERIC_ENABLED").ok().as_deref());
        let config_role = config_role_enabled(env::var("RUST_CONFIG_ROLE_ENABLED").ok().as_deref());
        let config_reset =
            config_reset_enabled(env::var("RUST_CONFIG_RESET_ENABLED").ok().as_deref());
        let config_show = config_show_enabled(env::var("RUST_CONFIG_SHOW_ENABLED").ok().as_deref());
        let public_commands =
            public_commands_enabled(env::var("RUST_PUBLIC_COMMANDS_ENABLED").ok().as_deref());
        let uptime =
            uptime_enabled(env::var("RUST_UPTIME_ENABLED").ok().as_deref()) || public_commands;
        let invite =
            invite_enabled(env::var("RUST_INVITE_ENABLED").ok().as_deref()) || public_commands;
        let invite_client_id = nonempty_env("CLIENT_ID");
        let help = help_enabled(env::var("RUST_HELP_ENABLED").ok().as_deref()) || public_commands;
        let help_support_url = nonempty_env("SUPPORT_URL")
            .unwrap_or_else(|| "https://discord.gg/4kYw2WUbNN".to_owned());
        let welcome = welcome_enabled(env::var("RUST_WELCOME_ENABLED").ok().as_deref());
        let vote = vote_enabled(env::var("RUST_VOTE_ENABLED").ok().as_deref()) || public_commands;
        let vote_client_id = nonempty_env("CLIENT_ID");
        let top_speakers =
            top_speakers_enabled(env::var("RUST_TOP_SPEAKERS_ENABLED").ok().as_deref())
                || public_commands;
        let birthday = birthday_enabled(env::var("RUST_BIRTHDAY_ENABLED").ok().as_deref());
        let bot_stats = bot_stats_enabled(env::var("RUST_BOT_STATS_ENABLED").ok().as_deref())
            || public_commands;
        let server_stats =
            server_stats_enabled(env::var("RUST_SERVER_STATS_ENABLED").ok().as_deref())
                || public_commands;
        let stats =
            stats_enabled(env::var("RUST_STATS_ENABLED").ok().as_deref()) || public_commands;
        let premium = premium_enabled(env::var("RUST_PREMIUM_ENABLED").ok().as_deref());
        let redeem = redeem_enabled(env::var("RUST_REDEEM_ENABLED").ok().as_deref());
        let privacy = privacy_enabled(env::var("RUST_PRIVACY_ENABLED").ok().as_deref());
        let game_list = game_list_enabled(env::var("RUST_GAME_LIST_ENABLED").ok().as_deref())
            || public_commands;
        let game_scores = game_scores_enabled(env::var("RUST_GAME_SCORES_ENABLED").ok().as_deref())
            || public_commands;
        let automatic_translation = automatic_translation_from_environment();
        let dashboard = dashboard_from_environment()?;
        let admin = admin_from_environment()?;
        if http_listener_required(
            health_bind,
            premium_http.is_some(),
            dashboard.is_some(),
            admin.is_some(),
            install_oauth.is_some(),
            topgg_webhook.is_some(),
            public_status.is_some(),
        ) {
            return Err(RuntimeError::HttpListenerRequired);
        }
        Ok(Self {
            discord_token,
            database_path,
            postgres_shadow,
            postgres_replica_outbox,
            postgres_voice_read_cache,
            health_bind,
            public_status,
            premium_http,
            install_oauth,
            topgg_webhook,
            topgg_metrics,
            vote_redemption_secret,
            owner_commands,
            core_voice,
            tts_file,
            transcription,
            transcription_control,
            #[cfg(feature = "voice-driver")]
            transcription_live,
            translation_text,
            translation_preferences,
            voice_preferences,
            config_default_voice,
            autocomplete,
            config_channel,
            config_queue_roles,
            config_greet_language,
            config_blockword,
            pronunciation,
            config_language,
            config_toggles,
            config_numeric,
            config_role,
            config_reset,
            config_show,
            uptime,
            invite,
            invite_client_id,
            help,
            help_support_url,
            welcome,
            vote,
            vote_client_id,
            top_speakers,
            birthday,
            bot_stats,
            server_stats,
            stats,
            premium,
            redeem,
            privacy,
            game_list,
            game_scores,
            automatic_translation,
            dashboard,
            admin,
        })
    }
}

fn core_voice_from_environment() -> Result<Option<CoreVoiceRuntimeOptions>, RuntimeError> {
    if !core_voice_enabled(env::var("RUST_CORE_VOICE_ENABLED").ok().as_deref()) {
        return Ok(None);
    }
    let default_engine = core_voice_default_engine(env::var("TTS_ENGINE").ok().as_deref())?;
    if default_engine == SynthesisEngine::Neural && nonempty_env("OPENAI_API_KEY").is_none() {
        return Err(RuntimeError::NeuralApiKeyRequired);
    }
    let default_voice =
        nonempty_env("DEFAULT_VOICE").unwrap_or_else(|| "en_US-amy-medium".to_owned());
    let default_speed = positive_number_from_environment("DEFAULT_SPEED", 1.0, false)?;
    let queue_cap = positive_number_from_environment("QUEUE_CAP", 20.0, true)? as usize;
    let piper_concurrency = positive_number_from_environment(
        "PIPER_MAX_CONCURRENCY",
        default_piper_concurrency() as f64,
        true,
    )? as usize;
    Ok(Some(CoreVoiceRuntimeOptions {
        piper_path: nonempty_env("PIPER_PATH")
            .unwrap_or_else(|| "piper".to_owned())
            .into(),
        models_dir: nonempty_env("MODELS_DIR")
            .unwrap_or_else(|| "./models".to_owned())
            .into(),
        cache_dir: nonempty_env("RUST_VOICE_CACHE_DIR")
            .unwrap_or_else(|| "./audio-cache/rust".to_owned())
            .into(),
        gtts_cache_dir: nonempty_env("RUST_GTTS_CACHE_DIR")
            .unwrap_or_else(|| "./audio-cache/rust-gtts".to_owned())
            .into(),
        ffmpeg: nonempty_env("FFMPEG_PATH")
            .unwrap_or_else(|| "ffmpeg".to_owned())
            .into(),
        openai_api_key: nonempty_env("OPENAI_API_KEY"),
        neural_cache_dir: nonempty_env("RUST_NEURAL_CACHE_DIR")
            .unwrap_or_else(|| "./audio-cache/rust-neural".to_owned())
            .into(),
        gcloud_api_key: nonempty_env("GOOGLE_TTS_API_KEY"),
        gcloud_cache_dir: nonempty_env("RUST_GCLOUD_CACHE_DIR")
            .unwrap_or_else(|| "./audio-cache/rust-gcloud".to_owned())
            .into(),
        gcloud_limits: vozen_tts::GcloudLimits {
            max_chars: positive_number_from_environment("GCLOUD_MAX_CHARS", 500.0, true)? as usize,
            plus_monthly: positive_number_from_environment(
                "GCLOUD_PLUS_MONTHLY_CHARS",
                100_000.0,
                true,
            )? as i64,
            pass3_monthly: positive_number_from_environment(
                "GCLOUD_PASS3_MONTHLY_CHARS",
                400_000.0,
                true,
            )? as i64,
            pass8_monthly: positive_number_from_environment(
                "GCLOUD_PASS8_MONTHLY_CHARS",
                1_000_000.0,
                true,
            )? as i64,
            daily_budget: non_negative_number_from_environment(
                "GCLOUD_DAILY_CHAR_BUDGET",
                300_000.0,
                true,
            )? as i64,
        },
        kokoro_command: resolve_kokoro_command(nonempty_env("KOKORO_CMD").as_deref()),
        kokoro_cache_dir: nonempty_env("RUST_KOKORO_CACHE_DIR")
            .unwrap_or_else(|| "./audio-cache/rust-kokoro".to_owned())
            .into(),
        kokoro_languages: kokoro_languages_from_environment(),
        piper_concurrency,
        queue_cap,
        queue_enabled: queue_enabled(env::var("RUST_QUEUE_ENABLED").ok().as_deref()),
        message_autoread: message_autoread_enabled(
            env::var("RUST_MESSAGE_AUTOREAD_ENABLED").ok().as_deref(),
        ),
        randomizer_enabled: randomizer_enabled(env::var("RUST_RANDOMIZER_ENABLED").ok().as_deref()),
        cast_enabled: cast_enabled(env::var("RUST_CAST_ENABLED").ok().as_deref()),
        setup_enabled: setup_enabled(env::var("RUST_SETUP_ENABLED").ok().as_deref()),
        speak_context_enabled: speak_context_enabled(
            env::var("RUST_SPEAK_CONTEXT_ENABLED").ok().as_deref(),
        ),
        game_play_enabled: game_play_enabled(env::var("RUST_GAME_PLAY_ENABLED").ok().as_deref()),
        client_id: nonempty_env("CLIENT_ID"),
        support_url: nonempty_env("SUPPORT_URL")
            .unwrap_or_else(|| "https://discord.gg/4kYw2WUbNN".to_owned()),
        settings: CoreVoiceSettings {
            available_models: Vec::new(),
            default_voice,
            default_speed,
            default_engine,
        },
    }))
}

fn kokoro_languages_from_environment() -> Option<Vec<String>> {
    let raw = env::var("KOKORO_LANGS").ok()?;
    let languages: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    (!languages.is_empty()).then_some(languages)
}

fn resolve_kokoro_command(explicit: Option<&str>) -> Option<vozen_tts::KokoroCommand> {
    if let Some(value) = explicit {
        return vozen_tts::parse_kokoro_command(value);
    }
    let root = env::current_dir().ok()?;
    let python = [
        root.join("tools")
            .join("kokoro-venv")
            .join("Scripts")
            .join("python.exe"),
        root.join("tools")
            .join("kokoro-venv")
            .join("bin")
            .join("python"),
    ]
    .into_iter()
    .find(|path| path.is_file())?;
    let required = [
        root.join("tools").join("kokoro_server.py"),
        root.join("tools").join("kokoro-v1.0.onnx"),
        root.join("tools").join("voices-v1.0.bin"),
    ];
    if required.iter().all(|path| Path::is_file(path)) {
        Some(vozen_tts::KokoroCommand {
            executable: python,
            args: vec![required[0].to_string_lossy().into_owned()],
        })
    } else {
        None
    }
}

fn randomizer_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn cast_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn setup_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn speak_context_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn game_play_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn tts_file_from_environment() -> Result<Option<TtsFileRuntimeOptions>, RuntimeError> {
    if !tts_file_enabled(env::var("RUST_TTS_FILE_ENABLED").ok().as_deref()) {
        return Ok(None);
    }
    // Private file export is intentionally Piper-backed and independent from the global
    // voice default. The main voice route may use Google/gTTS with Piper as its fallback.
    let default_voice =
        nonempty_env("DEFAULT_VOICE").unwrap_or_else(|| "en_US-amy-medium".to_owned());
    let default_speed = positive_number_from_environment("DEFAULT_SPEED", 1.0, false)?;
    let piper_concurrency = positive_number_from_environment(
        "PIPER_MAX_CONCURRENCY",
        default_piper_concurrency() as f64,
        true,
    )? as usize;
    Ok(Some(TtsFileRuntimeOptions {
        piper_path: nonempty_env("PIPER_PATH")
            .unwrap_or_else(|| "piper".to_owned())
            .into(),
        models_dir: nonempty_env("MODELS_DIR")
            .unwrap_or_else(|| "./models".to_owned())
            .into(),
        cache_dir: nonempty_env("RUST_TTS_FILE_CACHE_DIR")
            .unwrap_or_else(|| "./audio-cache/rust-file".to_owned())
            .into(),
        piper_concurrency,
        settings: CoreVoiceSettings {
            available_models: Vec::new(),
            default_voice,
            default_speed,
            default_engine: SynthesisEngine::Piper,
        },
    }))
}

fn transcription_from_environment() -> Result<Option<TranscriptionRuntimeOptions>, RuntimeError> {
    let message_enabled =
        transcription_enabled(env::var("RUST_TRANSCRIBE_MESSAGE_ENABLED").ok().as_deref());
    #[cfg(feature = "voice-driver")]
    let live_enabled =
        live_transcription_enabled(env::var("RUST_TRANSCRIBE_LIVE_ENABLED").ok().as_deref());
    #[cfg(not(feature = "voice-driver"))]
    let live_enabled = false;
    if !message_enabled && !live_enabled {
        return Ok(None);
    }
    let max_concurrency =
        positive_number_from_environment("STT_MAX_CONCURRENCY", 1.0, true)? as usize;
    let working_directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (python, script) = resolve_whisper_runtime(
        &working_directory,
        nonempty_env("WHISPER_PYTHON").as_deref(),
        nonempty_env("WHISPER_SCRIPT").as_deref(),
    );
    Ok(Some(TranscriptionRuntimeOptions {
        python,
        script,
        model: nonempty_env("WHISPER_MODEL"),
        ffmpeg: nonempty_env("FFMPEG_PATH")
            .unwrap_or_else(|| "ffmpeg".to_owned())
            .into(),
        max_concurrency,
    }))
}

/// Resolve the same optional Whisper venv layout as the Node runtime. Explicit environment
/// paths always win; otherwise a project-local venv is preferred so the Rust canaries use the
/// pinned `faster-whisper` environment installed by `tools/setup-whisper.*`.
fn resolve_whisper_runtime(
    working_directory: &Path,
    explicit_python: Option<&str>,
    explicit_script: Option<&str>,
) -> (PathBuf, PathBuf) {
    let python = explicit_python
        .map(PathBuf::from)
        .or_else(|| {
            [
                "tools/whisper-venv/bin/python",
                "tools/whisper-venv/Scripts/python.exe",
            ]
            .iter()
            .map(|candidate| working_directory.join(candidate))
            .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| PathBuf::from("python3"));
    let script = explicit_script
        .map(PathBuf::from)
        .or_else(|| {
            let candidate = working_directory.join("tools/whisper_sidecar.py");
            candidate.is_file().then_some(candidate)
        })
        .unwrap_or_else(|| PathBuf::from("tools/whisper_sidecar.py"));
    (python, script)
}

fn voice_preferences_from_environment()
-> Result<Option<VoicePreferenceRuntimeOptions>, RuntimeError> {
    if !voice_preferences_enabled(env::var("RUST_VOICE_PREFERENCES_ENABLED").ok().as_deref()) {
        return Ok(None);
    }
    let models_dir = nonempty_env("MODELS_DIR").unwrap_or_else(|| "./models".to_owned());
    let default_speed = positive_number_from_environment("DEFAULT_SPEED", 1.0, false)?;
    Ok(Some(VoicePreferenceRuntimeOptions {
        available_models: discover_piper_models(std::path::Path::new(&models_dir))?,
        default_speed,
    }))
}

fn config_default_voice_from_environment()
-> Result<Option<ConfigDefaultVoiceRuntimeOptions>, RuntimeError> {
    if !config_default_voice_enabled(
        env::var("RUST_CONFIG_DEFAULT_VOICE_ENABLED")
            .ok()
            .as_deref(),
    ) {
        return Ok(None);
    }
    let models_dir = nonempty_env("MODELS_DIR").unwrap_or_else(|| "./models".to_owned());
    Ok(Some(ConfigDefaultVoiceRuntimeOptions {
        available_models: discover_piper_models(std::path::Path::new(&models_dir))?,
    }))
}

fn autocomplete_from_environment(
    core_voice: Option<&CoreVoiceRuntimeOptions>,
    voice_preferences: Option<&VoicePreferenceRuntimeOptions>,
    config_default_voice: Option<&ConfigDefaultVoiceRuntimeOptions>,
    config_language: bool,
    translation_preferences: bool,
    pronunciation: bool,
    transcription_live: bool,
) -> Result<Option<autocomplete_sink::AutocompleteRuntimeOptions>, RuntimeError> {
    if !autocomplete_enabled(env::var("RUST_AUTOCOMPLETE_ENABLED").ok().as_deref()) {
        return Ok(None);
    }
    let needs_models =
        core_voice.is_some() || voice_preferences.is_some() || config_default_voice.is_some();
    let available_models = if needs_models {
        let models_dir = nonempty_env("MODELS_DIR").unwrap_or_else(|| "./models".to_owned());
        discover_piper_models(std::path::Path::new(&models_dir))?
    } else {
        Vec::new()
    };
    Ok(Some(autocomplete_sink::AutocompleteRuntimeOptions {
        available_models,
        core_voice: core_voice.is_some(),
        game_play: core_voice.is_some_and(|options| options.game_play_enabled),
        transcription_live,
        voice_preferences: voice_preferences.is_some(),
        config_default_voice: config_default_voice.is_some(),
        config_language,
        translation_preferences,
        pronunciation,
    }))
}

fn translation_text_from_environment() -> Option<TranslationTextRuntimeOptions> {
    let text_enabled =
        translation_text_enabled(env::var("RUST_TRANSLATE_TEXT_ENABLED").ok().as_deref());
    let admin_enabled =
        translation_admin_enabled(env::var("RUST_TRANSLATION_ADMIN_ENABLED").ok().as_deref());
    let context_enabled =
        translation_context_enabled(env::var("RUST_TRANSLATE_CONTEXT_ENABLED").ok().as_deref());
    (text_enabled || admin_enabled || context_enabled).then(|| TranslationTextRuntimeOptions {
        provider: translation_provider::RuntimeTranslationProvider::from_environment(),
        text_enabled,
        admin_enabled,
        context_enabled,
    })
}

fn automatic_translation_from_environment() -> Option<AutomaticTranslationRuntimeOptions> {
    automatic_translation_enabled(
        env::var("RUST_AUTOMATIC_TRANSLATION_ENABLED")
            .ok()
            .as_deref(),
    )
    .then(|| AutomaticTranslationRuntimeOptions {
        provider: translation_provider::RuntimeTranslationProvider::from_environment(),
    })
}

/// This deliberately matches Node's safe opt-in semantics: only literal `true` can make Rust
/// own a Discord interaction. `1`, `yes`, missing and spelling mistakes remain shadow-only.
fn core_voice_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// Bundles only read-only/control-plane handlers. Voice, live games, STT, payments and
/// destructive privacy actions deliberately remain on independent canaries.
fn public_commands_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// The Rust owner sink is inert unless the operator explicitly enables it and supplies both
/// identity guards. This mirrors the Node handler's fail-closed owner resolution.
fn owner_commands_from_environment() -> Option<OwnerCommandRuntimeOptions> {
    if !owner_commands_enabled(env::var("RUST_OWNER_COMMANDS_ENABLED").ok().as_deref()) {
        return None;
    }
    Some(OwnerCommandRuntimeOptions {
        owner_id: nonempty_env("OWNER_ID")?,
        owner_guild_id: nonempty_env("OWNER_GUILD_ID")?,
    })
}

fn owner_commands_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn full_owner_commands_ready(
    full_mode: bool,
    enabled: bool,
    options: Option<&OwnerCommandRuntimeOptions>,
) -> bool {
    !full_mode || !enabled || options.is_some()
}

async fn register_rust_commands_if_enabled(config: &RuntimeConfig) -> Result<(), RuntimeError> {
    if !register_commands_enabled(env::var("RUST_REGISTER_COMMANDS_ENABLED").ok().as_deref()) {
        return Ok(());
    }
    let application_id = nonempty_env("CLIENT_ID").ok_or(RuntimeError::MissingClientId)?;
    // R4 staging can scope public commands to one test guild. Leaving this empty preserves the
    // production global registration path; a staging process must opt in explicitly.
    let public_guild_id = nonempty_env("RUST_COMMANDS_GUILD_ID");
    let owner_guild_id = nonempty_env("OWNER_GUILD_ID");
    let state_path = nonempty_env("RUST_COMMANDS_STATE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            config
                .database_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("commands-state-rust.json")
        });
    let client = DiscordHttpCommandRegistrationClient::new(config.discord_token.clone())
        .map_err(|_| RuntimeError::CommandRegistration)?;
    register_commands(
        &client,
        &CommandRegistrationConfig {
            application_id,
            public_guild_id,
            state_path: Some(state_path),
            owner_guild_id,
        },
    )
    .await
    .map_err(|_| RuntimeError::CommandRegistration)?;
    Ok(())
}

fn register_commands_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn tts_file_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn transcription_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn transcription_control_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn live_transcription_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn core_voice_default_engine(raw: Option<&str>) -> Result<SynthesisEngine, RuntimeError> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(SynthesisEngine::Piper),
        Some(value) if value.eq_ignore_ascii_case("piper") => Ok(SynthesisEngine::Piper),
        Some(value) if value.eq_ignore_ascii_case("gtts") => Ok(SynthesisEngine::Default),
        Some(value) if value.eq_ignore_ascii_case("router") => Ok(SynthesisEngine::Default),
        Some(value) if value.eq_ignore_ascii_case("neural") => Ok(SynthesisEngine::Neural),
        Some(_) => Err(RuntimeError::RustVoiceRequiresPiperDefault),
    }
}

fn translation_text_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn translation_admin_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn translation_context_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn translation_preferences_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn voice_preferences_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_default_voice_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn autocomplete_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_channel_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_queue_roles_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_greet_language_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_blockword_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn pronunciation_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_language_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_toggles_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_numeric_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_role_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_show_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn config_reset_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn uptime_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn invite_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn help_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn welcome_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn vote_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn top_speakers_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn birthday_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn bot_stats_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn server_stats_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn stats_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn postgres_replica_outbox_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn postgres_voice_read_cache_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn premium_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn redeem_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn privacy_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn game_list_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn game_scores_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn automatic_translation_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn message_autoread_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn queue_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn dashboard_from_environment() -> Result<Option<DashboardRuntimeOptions>, RuntimeError> {
    if !dashboard_enabled(env::var("RUST_DASHBOARD_ENABLED").ok().as_deref()) {
        return Ok(None);
    }
    let models_dir: PathBuf = nonempty_env("MODELS_DIR")
        .unwrap_or_else(|| "./models".to_owned())
        .into();
    // Validate now. A dashboard with an empty model selection would silently reject every
    // existing default voice, so its operator must fix the local model directory first.
    let _ = discover_piper_models(&models_dir)?;
    Ok(Some(DashboardRuntimeOptions { models_dir }))
}

fn dashboard_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn admin_from_environment() -> Result<Option<AdminRuntimeOptions>, RuntimeError> {
    if !admin_enabled(env::var("RUST_ADMIN_API_ENABLED").ok().as_deref()) {
        return Ok(None);
    }
    let web_analytics = if env::var("CLOUDFLARE_WEB_ANALYTICS_ENABLED")
        .ok()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
    {
        Some(CloudflareWebAnalyticsConfig::new(
            nonempty_env("CLOUDFLARE_ACCOUNT_ID").ok_or(
                RuntimeError::MissingCloudflareWebAnalytics("CLOUDFLARE_ACCOUNT_ID"),
            )?,
            nonempty_env("CLOUDFLARE_ZONE_ID").ok_or(
                RuntimeError::MissingCloudflareWebAnalytics("CLOUDFLARE_ZONE_ID"),
            )?,
            nonempty_env("CLOUDFLARE_WEB_ANALYTICS_SITE_TAG").ok_or(
                RuntimeError::MissingCloudflareWebAnalytics("CLOUDFLARE_WEB_ANALYTICS_SITE_TAG"),
            )?,
            nonempty_env("CLOUDFLARE_WEB_ANALYTICS_TOKEN").ok_or(
                RuntimeError::MissingCloudflareWebAnalytics("CLOUDFLARE_WEB_ANALYTICS_TOKEN"),
            )?,
        )?)
    } else {
        None
    };
    Ok(Some(AdminRuntimeOptions {
        panel_origin: nonempty_env("ADMIN_PANEL_ORIGIN")
            .unwrap_or_else(|| "https://rexy40407.github.io".to_owned()),
        session_secret: nonempty_env("ADMIN_SESSION_SECRET"),
        owner_id: nonempty_env("OWNER_ID"),
        client_id: nonempty_env("ADMIN_CLIENT_ID").or_else(|| nonempty_env("CLIENT_ID")),
        web_analytics,
    }))
}

fn admin_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

fn default_piper_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().saturating_sub(1).max(1))
        .unwrap_or(1)
}

fn positive_number_from_environment(
    name: &'static str,
    fallback: f64,
    integer: bool,
) -> Result<f64, RuntimeError> {
    parse_positive_number(nonempty_env(name).as_deref(), fallback, integer)
        .ok_or(RuntimeError::InvalidCoreVoiceSetting(name))
}

fn non_negative_number_from_environment(
    name: &'static str,
    fallback: f64,
    integer: bool,
) -> Result<f64, RuntimeError> {
    let value = nonempty_env(name)
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(fallback);
    if value.is_finite() && value >= 0.0 && (!integer || value.fract() == 0.0) {
        Ok(value)
    } else {
        Err(RuntimeError::InvalidCoreVoiceSetting(name))
    }
}

fn parse_positive_number(raw: Option<&str>, fallback: f64, integer: bool) -> Option<f64> {
    let Some(raw) = raw else {
        return Some(fallback);
    };
    raw.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0 && (!integer || value.fract() == 0.0))
}

#[cfg(feature = "voice-driver")]
fn core_voice_event_sink(
    options: Option<CoreVoiceRuntimeOptions>,
    store: Arc<Mutex<SqliteStore>>,
    voice_read_store: Arc<Mutex<SqliteStore>>,
    gateway_state: GatewayState,
    runtime_batch: RuntimeBatchBuffer,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    let Some(mut options) = options else {
        return Ok(None);
    };
    let piper_models = piper_models_for_core_voice(
        options.settings.default_engine,
        &options.piper_path,
        &options.models_dir,
        &options.settings.default_voice,
    )?;
    options.settings.available_models =
        available_models_for_default_provider(piper_models, options.settings.default_engine);
    Ok(Some(Arc::new(
        core_voice_sink::CoreVoiceGatewaySink::new_with_runtime_batch_and_voice_read_store(
            store,
            voice_read_store,
            gateway_state,
            options,
            runtime_batch,
        ),
    )))
}

/// Piper is mandatory only when it is the selected default engine.  A staging
/// run using the Google fallback must remain able to exercise Discord voice
/// transport without first installing an unrelated local Piper model.
#[cfg(feature = "voice-driver")]
fn piper_models_for_core_voice(
    default_engine: SynthesisEngine,
    piper_path: &Path,
    models_dir: &Path,
    default_voice: &str,
) -> Result<Vec<String>, RuntimeError> {
    if default_engine == SynthesisEngine::Piper {
        return validate_piper_runtime(piper_path, models_dir, default_voice);
    }
    match discover_piper_models(models_dir) {
        Ok(models) => Ok(models),
        Err(RuntimeError::ModelsUnavailable) => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn tts_file_event_sink(
    options: Option<TtsFileRuntimeOptions>,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    let Some(mut options) = options else {
        return Ok(None);
    };
    options.settings.available_models = validate_piper_runtime(
        &options.piper_path,
        &options.models_dir,
        &options.settings.default_voice,
    )?;
    Ok(Some(Arc::new(
        file_export_sink::TtsFileGatewaySink::new(store, options)
            .map_err(|_| RuntimeError::TtsFileGateway)?,
    )))
}

fn transcription_event_sink(
    options: Option<TranscriptionRuntimeOptions>,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    // Live `/transcribe` reuses the adapter configuration, but must not accidentally promote the
    // separate message-context command when only `RUST_TRANSCRIBE_LIVE_ENABLED` is set.
    if !transcription_enabled(env::var("RUST_TRANSCRIBE_MESSAGE_ENABLED").ok().as_deref()) {
        return Ok(None);
    }
    let Some(options) = options else {
        return Ok(None);
    };
    let transcriber = transcription_adapter::AttachmentTranscriber::new(options)
        .map_err(|_| RuntimeError::TranscriptionGateway)?;
    Ok(Some(Arc::new(
        transcription_sink::TranscriptionGatewaySink::new(store, transcriber),
    )))
}

fn transcription_control_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
    consent_registry: SttConsentRegistry,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        transcription_control_sink::TranscriptionControlGatewaySink::new_with_registry(
            store,
            consent_registry,
        )
        .map_err(|_| RuntimeError::TranscriptionControlGateway)?,
    )))
}

#[cfg(feature = "voice-driver")]
fn transcription_live_event_sink(
    enabled: bool,
    options: Option<TranscriptionRuntimeOptions>,
    store: Arc<Mutex<SqliteStore>>,
    gateway_state: GatewayState,
    consent_registry: SttConsentRegistry,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    let options = options.ok_or(RuntimeError::TranscriptionGateway)?;
    let max_concurrency = options.max_concurrency.max(1);
    let transcriber = transcription_adapter::AttachmentTranscriber::new(options)
        .map_err(|_| RuntimeError::TranscriptionGateway)?;
    Ok(Some(Arc::new(
        live_transcription_sink::LiveTranscriptionGatewaySink::new(
            store,
            gateway_state,
            transcriber,
            max_concurrency,
            consent_registry,
        )
        .map_err(|_| RuntimeError::TranscriptionGateway)?,
    )))
}

fn owner_command_event_sink(
    options: Option<OwnerCommandRuntimeOptions>,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    let Some(options) = options else {
        return Ok(None);
    };
    Ok(Some(Arc::new(
        owner_command_sink::OwnerCommandGatewaySink::new(store, options)
            .map_err(|_| RuntimeError::OwnerCommandGateway)?,
    )))
}

fn translation_text_event_sink(
    options: Option<TranslationTextRuntimeOptions>,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    let Some(options) = options else {
        return Ok(None);
    };
    Ok(Some(Arc::new(
        translation_text_sink::TranslationTextGatewaySink::new(
            store,
            options.provider,
            options.text_enabled,
            options.admin_enabled,
            options.context_enabled,
        )
        .map_err(|_| RuntimeError::TranslationGateway)?,
    )))
}

fn translation_preference_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        translation_preference_sink::TranslationPreferenceGatewaySink::new(store)
            .map_err(|_| RuntimeError::TranslationPreferenceGateway)?,
    )))
}

fn voice_preference_event_sink(
    options: Option<VoicePreferenceRuntimeOptions>,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    let Some(options) = options else {
        return Ok(None);
    };
    Ok(Some(Arc::new(
        voice_preference_sink::VoicePreferenceGatewaySink::new(
            store,
            vozen_discord::VoicePreferenceSettings {
                available_models: options.available_models,
                default_speed: options.default_speed,
            },
        )
        .map_err(|_| RuntimeError::VoicePreferenceGateway)?,
    )))
}

fn pronunciation_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
    payments_enabled: bool,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        pronunciation_sink::PronunciationGatewaySink::new(
            store,
            nonempty_env("KOFI_URL").unwrap_or_default(),
            payments_enabled,
        )
        .map_err(|_| RuntimeError::PronunciationGateway)?,
    )))
}

fn config_language_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        config_language_sink::ConfigLanguageGatewaySink::new(store)
            .map_err(|_| RuntimeError::ConfigLanguageGateway)?,
    )))
}

fn config_toggle_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        config_toggle_sink::ConfigToggleGatewaySink::new(store)
            .map_err(|_| RuntimeError::ConfigToggleGateway)?,
    )))
}

fn config_numeric_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        config_numeric_sink::ConfigNumericGatewaySink::new(store)
            .map_err(|_| RuntimeError::ConfigNumericGateway)?,
    )))
}

fn config_role_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        config_role_sink::ConfigRoleGatewaySink::new(store)
            .map_err(|_| RuntimeError::ConfigRoleGateway)?,
    )))
}

fn config_default_voice_event_sink(
    options: Option<ConfigDefaultVoiceRuntimeOptions>,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    let Some(options) = options else {
        return Ok(None);
    };
    Ok(Some(Arc::new(
        config_default_voice_sink::ConfigDefaultVoiceGatewaySink::new(
            store,
            vozen_discord::ConfigDefaultVoiceSettings {
                available_models: options.available_models,
            },
        )
        .map_err(|_| RuntimeError::ConfigDefaultVoiceGateway)?,
    )))
}

fn autocomplete_event_sink(
    options: Option<autocomplete_sink::AutocompleteRuntimeOptions>,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    let Some(options) = options else {
        return Ok(None);
    };
    Ok(Some(Arc::new(
        autocomplete_sink::AutocompleteGatewaySink::new(store, options)
            .map_err(|_| RuntimeError::AutocompleteGateway)?,
    )))
}

fn config_channel_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        config_channel_sink::ConfigChannelGatewaySink::new(store)
            .map_err(|_| RuntimeError::ConfigChannelGateway)?,
    )))
}

fn config_queue_role_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        config_queue_role_sink::ConfigQueueRoleGatewaySink::new(store)
            .map_err(|_| RuntimeError::ConfigQueueRoleGateway)?,
    )))
}

fn config_greet_language_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        config_greet_language_sink::ConfigGreetLanguageGatewaySink::new(store)
            .map_err(|_| RuntimeError::ConfigGreetLanguageGateway)?,
    )))
}

fn config_blockword_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        config_blockword_sink::ConfigBlockwordGatewaySink::new(store)
            .map_err(|_| RuntimeError::ConfigBlockwordGateway)?,
    )))
}

fn config_show_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        config_show_sink::ConfigShowGatewaySink::new(store)
            .map_err(|_| RuntimeError::ConfigShowGateway)?,
    )))
}

fn config_reset_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        config_reset_sink::ConfigResetGatewaySink::new(store)
            .map_err(|_| RuntimeError::ConfigResetGateway)?,
    )))
}

fn uptime_event_sink(enabled: bool) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        uptime_sink::UptimeGatewaySink::new().map_err(|_| RuntimeError::UptimeGateway)?,
    )))
}

fn invite_event_sink(
    enabled: bool,
    client_id: Option<String>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        invite_sink::InviteGatewaySink::new(client_id).map_err(|_| RuntimeError::InviteGateway)?,
    )))
}

fn help_event_sink(
    enabled: bool,
    support_url: String,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        help_sink::HelpGatewaySink::new(support_url).map_err(|_| RuntimeError::HelpGateway)?,
    )))
}

fn vote_event_sink(
    enabled: bool,
    client_id: Option<String>,
    redemption_secret: Option<String>,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        vote_sink::VoteGatewaySink::new(client_id, redemption_secret, store)
            .map_err(|_| RuntimeError::VoteGateway)?,
    )))
}

fn top_speakers_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        top_speakers_sink::TopSpeakersGatewaySink::new(store)
            .map_err(|_| RuntimeError::TopSpeakersGateway)?,
    )))
}

fn privacy_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        privacy_sink::PrivacyGatewaySink::new(store).map_err(|_| RuntimeError::PrivacyGateway)?,
    )))
}

fn birthday_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        birthday_sink::BirthdayGatewaySink::new(store)
            .map_err(|_| RuntimeError::BirthdayGateway)?,
    )))
}

fn bot_stats_event_sink(
    enabled: bool,
    gateway_state: GatewayState,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        bot_stats_sink::BotStatsGatewaySink::new(gateway_state)
            .map_err(|_| RuntimeError::BotStatsGateway)?,
    )))
}

fn stats_event_sink(
    enabled: bool,
    gateway_state: GatewayState,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        stats_sink::StatsGatewaySink::new(gateway_state).map_err(|_| RuntimeError::StatsGateway)?,
    )))
}

fn server_stats_event_sink(
    enabled: bool,
    client_id: Option<String>,
    redemption_secret: Option<String>,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        server_stats_sink::ServerStatsGatewaySink::new(store, client_id, redemption_secret)
            .map_err(|_| RuntimeError::ServerStatsGateway)?,
    )))
}

fn game_list_event_sink(enabled: bool) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        game_list_sink::GameListGatewaySink::new().map_err(|_| RuntimeError::GameListGateway)?,
    )))
}

fn game_scores_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        game_score_sink::GameScoreGatewaySink::new(store)
            .map_err(|_| RuntimeError::GameScoresGateway)?,
    )))
}

fn premium_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
    payments_enabled: bool,
    client_id: Option<String>,
    redemption_secret: Option<String>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    let guild_sku_id = nonempty_env("PREMIUM_GUILD_SKU_ID").and_then(|value| value.parse().ok());
    let user_sku_id = nonempty_env("PREMIUM_USER_SKU_ID").and_then(|value| value.parse().ok());
    Ok(Some(Arc::new(
        premium_sink::PremiumGatewaySink::new(
            store,
            nonempty_env("KOFI_URL").unwrap_or_default(),
            payments_enabled,
            client_id,
            redemption_secret,
            guild_sku_id,
            user_sku_id,
        )
        .map_err(|_| RuntimeError::PremiumGateway)?,
    )))
}

fn redeem_event_sink(
    enabled: bool,
    store: Arc<Mutex<SqliteStore>>,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if !enabled {
        return Ok(None);
    }
    Ok(Some(Arc::new(
        redeem_sink::RedeemGatewaySink::new(store).map_err(|_| RuntimeError::RedeemGateway)?,
    )))
}

fn automatic_translation_event_sink(
    options: Option<AutomaticTranslationRuntimeOptions>,
    store: Arc<Mutex<SqliteStore>>,
    gateway_state: GatewayState,
) -> Option<Arc<dyn GatewayEventSink>> {
    options.map(|options| {
        Arc::new(
            automatic_translation_sink::AutomaticTranslationGatewaySink::new(
                store,
                gateway_state,
                options.provider,
            ),
        ) as Arc<dyn GatewayEventSink>
    })
}

#[cfg(not(feature = "voice-driver"))]
fn core_voice_event_sink(
    options: Option<CoreVoiceRuntimeOptions>,
    _store: Arc<Mutex<SqliteStore>>,
    _voice_read_store: Arc<Mutex<SqliteStore>>,
    _gateway_state: GatewayState,
    _runtime_batch: RuntimeBatchBuffer,
) -> Result<Option<Arc<dyn GatewayEventSink>>, RuntimeError> {
    if options.is_some() {
        return Err(RuntimeError::VoiceDriverRequired);
    }
    Ok(None)
}

fn discover_piper_models(models_dir: &std::path::Path) -> Result<Vec<String>, RuntimeError> {
    let entries = fs::read_dir(models_dir).map_err(|_| RuntimeError::ModelsUnavailable)?;
    let mut models = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "onnx"))
            .then(|| {
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .map(ToOwned::to_owned)
            })
            .flatten()
        })
        // Node intentionally excludes this legacy Piper model from selection/detection.
        .filter(|model| model != "pt_PT-tugao-medium")
        .collect::<Vec<_>>();
    models.sort_unstable();
    models.dedup();
    if models.is_empty() {
        return Err(RuntimeError::ModelsUnavailable);
    }
    Ok(models)
}

#[allow(dead_code)]
fn available_models_for_default_provider(
    mut piper_models: Vec<String>,
    default_engine: SynthesisEngine,
) -> Vec<String> {
    if default_engine != SynthesisEngine::Default {
        return piper_models;
    }
    for locale in GTTS_SYNTHETIC_LOCALES {
        let covered = piper_models
            .iter()
            .any(|model| model.split('-').next() == Some(*locale));
        if !covered {
            piper_models.push(format!("{locale}-google-medium"));
        }
    }
    piper_models.sort_unstable();
    piper_models.dedup();
    piper_models
}

fn validate_piper_runtime(
    piper_path: &Path,
    models_dir: &Path,
    default_voice: &str,
) -> Result<Vec<String>, RuntimeError> {
    if !piper_executable_available(piper_path) {
        return Err(RuntimeError::PiperExecutableUnavailable);
    }
    let models = discover_piper_models(models_dir)?;
    if !models.iter().any(|model| model == default_voice) {
        return Err(RuntimeError::DefaultVoiceUnavailable);
    }
    if !models_dir
        .join(format!("{default_voice}.onnx.json"))
        .is_file()
    {
        return Err(RuntimeError::DefaultVoiceConfigUnavailable);
    }
    Ok(models)
}

fn piper_executable_available(configured: &Path) -> bool {
    if configured.is_absolute() || configured.components().count() > 1 {
        return is_executable_file(configured);
    }
    if is_executable_file(configured) {
        return true;
    }
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|directory| {
        let candidate = directory.join(configured);
        if is_executable_file(&candidate) {
            return true;
        }
        #[cfg(windows)]
        {
            if configured.extension().is_none() {
                return ["exe", "cmd", "bat", "com"]
                    .iter()
                    .any(|extension| is_executable_file(&candidate.with_extension(extension)));
            }
        }
        false
    })
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn topgg_metrics_from_environment() -> Result<Option<TopggMetricsRuntimeConfig>, RuntimeError> {
    let Some(token) = nonempty_env("TOPGG_TOKEN") else {
        return Ok(None);
    };
    let client_id = nonempty_env("CLIENT_ID").ok_or(RuntimeError::MissingClientId)?;
    Ok(Some(TopggMetricsRuntimeConfig { client_id, token }))
}

/// The installation callback is opt-in because Discord must be configured
/// with the exact HTTPS callback before public CTAs point to it. All companion
/// values are required together; a partial configuration fails rather than
/// exposing a flow that cannot safely complete.
fn tts_install_oauth_from_environment() -> Result<Option<TtsInstallOAuthRuntimeConfig>, RuntimeError>
{
    if !env::var("RUST_TTS_INSTALL_OAUTH_ENABLED")
        .ok()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
    {
        return Ok(None);
    }
    let required = |name| nonempty_env(name).ok_or(RuntimeError::MissingTtsInstallOAuth(name));
    Ok(Some(TtsInstallOAuthRuntimeConfig {
        client_id: required("CLIENT_ID")?,
        client_secret: required("TTS_INSTALL_OAUTH_CLIENT_SECRET")?,
        redirect_uri: nonempty_env("TTS_INSTALL_OAUTH_REDIRECT_URI")
            .unwrap_or_else(|| "https://api.vozen.org/api/install/tts/callback".to_owned()),
        success_redirect: nonempty_env("TTS_INSTALL_OAUTH_SUCCESS_REDIRECT")
            .unwrap_or_else(|| "https://vozen.org/dashboard/".to_owned()),
        state_secret: required("TTS_INSTALL_OAUTH_STATE_SECRET")?,
    }))
}

/// A configured secret is an explicit request to serve this sensitive endpoint. It is never
/// inferred from a port or from the generic premium flag; missing companion values fail startup
/// once the HTTP listener is enabled instead of silently resetting reward eligibility.
fn topgg_webhook_from_environment() -> Result<Option<TopggWebhookRuntimeConfig>, RuntimeError> {
    let Some(webhook_secret) = nonempty_env("TOPGG_WEBHOOK_SECRET") else {
        return Ok(None);
    };
    let client_id = nonempty_env("CLIENT_ID").ok_or(RuntimeError::MissingClientId)?;
    let redemption_secret =
        nonempty_env("VOTE_REDEMPTION_SECRET").ok_or(RuntimeError::MissingVoteRedemptionSecret)?;
    Ok(Some(TopggWebhookRuntimeConfig {
        client_id,
        webhook_secret,
        redemption_secret,
    }))
}

/// Mirrors Node's deliberately strict public-status opt-in: only `true` enables a public route.
fn public_status_from_environment() -> Option<PublicStatusConfig> {
    public_status_enabled(env::var("PUBLIC_STATUS_ENABLED").ok().as_deref()).then_some(
        PublicStatusConfig {
            incident: nonempty_env("PUBLIC_STATUS_INCIDENT"),
        },
    )
}

fn public_status_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// Mirrors Node's dangerous-feature flag: only the literal value `true` enables the browser
/// premium API. A typo or a blank value must never expose an authenticated endpoint.
fn premium_http_from_environment() -> Result<Option<PremiumHttpConfig>, RuntimeError> {
    let browser_api_enabled = premium_http_enabled(env::var("PREMIUM_API_ENABLED").ok().as_deref());
    let stripe_secret_key = payments_enabled_from_environment()
        .then(|| nonempty_env("STRIPE_SECRET_KEY"))
        .flatten();
    let stripe_publishable_key = payments_enabled_from_environment()
        .then(|| nonempty_env("STRIPE_PUBLISHABLE_KEY"))
        .flatten();
    // Once Stripe is selected, an old Ko-fi secret cannot re-enable the retired checkout path.
    // Historical Ko-fi entitlements remain in SQLite and continue to work.
    let kofi_webhook_token = (!stripe_secret_key.is_some() && payments_enabled_from_environment())
        .then(|| nonempty_env("KOFI_WEBHOOK_TOKEN"))
        .flatten();
    let kofi_shop_map = kofi_webhook_token
        .as_ref()
        .and_then(|_| nonempty_env("KOFI_SHOP_MAP"));
    if !browser_api_enabled && kofi_webhook_token.is_none() {
        return Ok(None);
    }
    let ecosystem_client_id = nonempty_env("ECOSYSTEM_OAUTH_CLIENT_ID");
    let product_client_id = nonempty_env("CLIENT_ID");
    let client_id = if browser_api_enabled {
        Some(
            ecosystem_client_id
                .clone()
                .ok_or(RuntimeError::MissingClientId)?,
        )
    } else {
        ecosystem_client_id
            .clone()
            .or_else(|| product_client_id.clone())
    };
    let dashboard_client_ids = [ecosystem_client_id, product_client_id]
        .into_iter()
        .flatten()
        .fold(Vec::<String>::new(), |mut ids, id| {
            if !ids.iter().any(|existing| existing == &id) {
                ids.push(id);
            }
            ids
        });
    let origin = nonempty_env("PREMIUM_API_ORIGIN").unwrap_or_else(|| "https://vozen.org".into());
    let stripe_prices = [
        (
            "STRIPE_PRICE_PLUS_MONTHLY",
            nonempty_env("STRIPE_PRICE_PLUS_MONTHLY"),
        ),
        (
            "STRIPE_PRICE_PLUS_YEARLY",
            nonempty_env("STRIPE_PRICE_PLUS_YEARLY"),
        ),
        (
            "STRIPE_PRICE_PREMIUM_MONTHLY",
            nonempty_env("STRIPE_PRICE_PREMIUM_MONTHLY"),
        ),
        (
            "STRIPE_PRICE_PREMIUM_YEARLY",
            nonempty_env("STRIPE_PRICE_PREMIUM_YEARLY"),
        ),
        (
            "STRIPE_PRICE_MAX_MONTHLY",
            nonempty_env("STRIPE_PRICE_MAX_MONTHLY"),
        ),
        (
            "STRIPE_PRICE_MAX_YEARLY",
            nonempty_env("STRIPE_PRICE_MAX_YEARLY"),
        ),
    ];
    let stripe_prices = stripe_prices
        .iter()
        .all(|(_, value)| value.is_some())
        .then(|| vozen_api::stripe_api::StripePriceIds {
            plus_monthly: stripe_prices[0].1.clone().unwrap_or_default(),
            plus_yearly: stripe_prices[1].1.clone().unwrap_or_default(),
            premium_monthly: stripe_prices[2].1.clone().unwrap_or_default(),
            premium_yearly: stripe_prices[3].1.clone().unwrap_or_default(),
            max_monthly: stripe_prices[4].1.clone().unwrap_or_default(),
            max_yearly: stripe_prices[5].1.clone().unwrap_or_default(),
        });
    Ok(Some(PremiumHttpConfig {
        browser_api_enabled,
        client_id,
        dashboard_client_ids,
        origin,
        kofi_webhook_token,
        kofi_shop_map,
        claim_help_webhook_url: nonempty_env("CLAIM_HELP_WEBHOOK_URL")
            .or_else(|| nonempty_env("ERROR_WEBHOOK_URL")),
        stripe_secret_key,
        stripe_publishable_key,
        stripe_webhook_secret: payments_enabled_from_environment()
            .then(|| nonempty_env("STRIPE_WEBHOOK_SECRET"))
            .flatten(),
        stripe_prices,
    }))
}

fn premium_http_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// Payment providers are fail-closed. Stripe will be enabled explicitly once its checkout and
/// webhook contracts are ready; an unset flag must never expose a legacy Ko-fi path.
fn payments_enabled_from_environment() -> bool {
    premium_http_enabled(env::var("RUST_PAYMENTS_ENABLED").ok().as_deref())
}

fn browser_api_promoted(raw: Option<&str>, premium_http: Option<&PremiumHttpConfig>) -> bool {
    raw.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
        && premium_http.is_some_and(|config| config.browser_api_enabled)
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn http_listener_required(
    health_bind: Option<SocketAddr>,
    premium_http: bool,
    dashboard: bool,
    admin: bool,
    install_oauth: bool,
    topgg_webhook: bool,
    public_status: bool,
) -> bool {
    health_bind.is_none()
        && (premium_http || dashboard || admin || install_oauth || topgg_webhook || public_status)
}

#[derive(Debug, Error)]
enum RuntimeError {
    #[error("invalid or incomplete Rust runtime mode: {0}")]
    RuntimeMode(#[from] runtime_mode::RuntimeModeError),
    #[error("Postgres configuration failed: {0}")]
    PostgresShadow(#[from] postgres_shadow::PostgresShadowError),
    #[error("RUST_POSTGRES_IMPORT_SQLITE=true requires RUST_POSTGRES_MODE=shadow or mirror")]
    PostgresImportRequiresPostgres,
    #[error("Postgres import failed: {0}")]
    PostgresImport(#[from] postgres_import::ImportError),
    #[error("Postgres voice-read cache failed: {0}")]
    PostgresVoiceCache(#[from] postgres_voice_cache::PostgresVoiceCacheError),
    #[error(
        "RUST_POSTGRES_VOICE_READ_CACHE=true requires RUST_POSTGRES_REPLICA_OUTBOX=true and RUST_POSTGRES_MODE=shadow or mirror"
    )]
    PostgresVoiceReadCacheRequiresReplica,
    #[error("DISCORD_TOKEN is required to start the Rust gateway")]
    MissingToken,
    #[error("RUST_IMAGE_SMOKE must not receive production Discord credentials")]
    ImageSmokeCredentials,
    #[error("HEALTH_PORT must be an integer from 1 to 65535")]
    InvalidHealthPort,
    #[error("HEALTH_HOST must be a valid IP address")]
    InvalidHealthHost,
    #[error("SINGLE_INSTANCE_PORT must be `off`, `0`, or an integer from 1 to 65535")]
    InvalidSingleInstancePort,
    #[error("another Vozen runtime already owns the single-instance lock")]
    SingleInstanceAlreadyRunning,
    #[error("could not acquire the Vozen single-instance lock")]
    SingleInstanceLockFailed,
    #[error("HEALTH_PORT is required when a Rust HTTP/API surface is enabled")]
    HttpListenerRequired,
    #[error(
        "RUST_RUNTIME_MODE=full requires RUST_BROWSER_API_ENABLED=true and PREMIUM_API_ENABLED=true"
    )]
    FullRuntimeBrowserApiRequired,
    #[error(
        "RUST_RUNTIME_MODE=full with RUST_OWNER_COMMANDS_ENABLED=true requires OWNER_ID and OWNER_GUILD_ID"
    )]
    FullRuntimeOwnerCommandsRequired,
    #[error(
        "CLIENT_ID is required when PREMIUM_API_ENABLED=true, RUST_TTS_INSTALL_OAUTH_ENABLED=true, TOPGG_TOKEN, or TOPGG_WEBHOOK_SECRET is configured"
    )]
    MissingClientId,
    #[error("{0} is required when RUST_TTS_INSTALL_OAUTH_ENABLED=true")]
    MissingTtsInstallOAuth(&'static str),
    #[error("{0} is required when CLOUDFLARE_WEB_ANALYTICS_ENABLED=true")]
    MissingCloudflareWebAnalytics(&'static str),
    #[error("Cloudflare Web Analytics configuration failed: {0}")]
    CloudflareWebAnalytics(#[from] vozen_api::web_analytics::CloudflareWebAnalyticsConfigError),
    #[error("Rust Discord command registration failed")]
    CommandRegistration,
    #[error("VOTE_REDEMPTION_SECRET is required when TOPGG_WEBHOOK_SECRET is configured")]
    MissingVoteRedemptionSecret,
    #[error("{0} must be a positive number (and an integer where required)")]
    InvalidCoreVoiceSetting(&'static str),
    #[error("RUST_CORE_VOICE_ENABLED=true requires a runtime built with --features voice-driver")]
    #[cfg_attr(feature = "voice-driver", allow(dead_code))]
    VoiceDriverRequired,
    #[error(
        "RUST_TRANSCRIBE_LIVE_ENABLED=true requires a runtime built with --features voice-driver"
    )]
    #[cfg_attr(feature = "voice-driver", allow(dead_code))]
    LiveTranscriptionVoiceDriverRequired,
    #[error("Rust voice features require TTS_ENGINE to be unset, piper, gtts, router, or neural")]
    RustVoiceRequiresPiperDefault,
    #[error("TTS_ENGINE=neural requires OPENAI_API_KEY")]
    NeuralApiKeyRequired,
    #[error("a Rust Piper feature requires at least one supported Piper .onnx model")]
    ModelsUnavailable,
    #[error("PIPER_PATH must point to an executable Piper file when Rust voice is enabled")]
    PiperExecutableUnavailable,
    #[error("DEFAULT_VOICE must name a supported Piper model when a Rust Piper feature is enabled")]
    DefaultVoiceUnavailable,
    #[error("DEFAULT_VOICE requires a matching .onnx.json Piper configuration file")]
    DefaultVoiceConfigUnavailable,
    #[error("private TTS file gateway initialisation failed")]
    TtsFileGateway,
    #[error("private translation gateway initialisation failed")]
    TranslationGateway,
    #[error("message transcription gateway initialisation failed")]
    TranscriptionGateway,
    #[error("transcription control gateway initialisation failed")]
    TranscriptionControlGateway,
    #[error("owner command gateway initialisation failed")]
    OwnerCommandGateway,
    #[error("translation preference gateway initialisation failed")]
    TranslationPreferenceGateway,
    #[error("voice preference gateway initialisation failed")]
    VoicePreferenceGateway,
    #[error("pronunciation gateway initialisation failed")]
    PronunciationGateway,
    #[error("config language gateway initialisation failed")]
    ConfigLanguageGateway,
    #[error("config toggle gateway initialisation failed")]
    ConfigToggleGateway,
    #[error("config numeric gateway initialisation failed")]
    ConfigNumericGateway,
    #[error("config role gateway initialisation failed")]
    ConfigRoleGateway,
    #[error("config show gateway initialisation failed")]
    ConfigShowGateway,
    #[error("config reset gateway initialisation failed")]
    ConfigResetGateway,
    #[error("uptime gateway initialisation failed")]
    UptimeGateway,
    #[error("invite gateway initialisation failed")]
    InviteGateway,
    #[error("help gateway initialisation failed")]
    HelpGateway,
    #[error("vote gateway initialisation failed")]
    VoteGateway,
    #[error("top-speakers gateway initialisation failed")]
    TopSpeakersGateway,
    #[error("birthday gateway initialisation failed")]
    BirthdayGateway,
    #[error("bot-stats gateway initialisation failed")]
    BotStatsGateway,
    #[error("server-stats gateway initialisation failed")]
    ServerStatsGateway,
    #[error("stats gateway initialisation failed")]
    StatsGateway,
    #[error("premium gateway initialisation failed")]
    PremiumGateway,
    #[error("redeem gateway initialisation failed")]
    RedeemGateway,
    #[error("privacy gateway initialisation failed")]
    PrivacyGateway,
    #[error("game-list gateway initialisation failed")]
    GameListGateway,
    #[error("game-scores gateway initialisation failed")]
    GameScoresGateway,
    #[error("config default voice gateway initialisation failed")]
    ConfigDefaultVoiceGateway,
    #[error("autocomplete gateway initialisation failed")]
    AutocompleteGateway,
    #[error("config channel gateway initialisation failed")]
    ConfigChannelGateway,
    #[error("config queue role gateway initialisation failed")]
    ConfigQueueRoleGateway,
    #[error("config greet language gateway initialisation failed")]
    ConfigGreetLanguageGateway,
    #[error("config blockword gateway initialisation failed")]
    ConfigBlockwordGateway,
    #[error("Discord OAuth client initialisation failed")]
    OAuthClient,
    #[error("RUST_DASHBOARD_ENABLED=true requires PREMIUM_API_ENABLED=true")]
    DashboardRequiresPremiumHttp,
    #[error("RUST_ADMIN_API_ENABLED=true requires PREMIUM_API_ENABLED=true")]
    AdminRequiresPremiumHttp,
    #[error("RUST_DASHBOARD_ENABLED/RUST_ADMIN_API_ENABLED require PREMIUM_API_ENABLED=true")]
    DashboardOrAdminRequiresPremiumApi,
    #[error("RUST_POSTGRES_REPLICA_OUTBOX=true requires RUST_POSTGRES_MODE=shadow or mirror")]
    PostgresReplicaRequiresPostgres,
    #[error("SQLite startup failed: {0}")]
    Store(#[from] vozen_store::StoreError),
    #[error("SQLite store lock was poisoned")]
    StoreLock,
    #[error("Discord gateway failed: {0}")]
    Discord(#[from] DiscordRuntimeError),
    #[error("HTTP route construction failed: {0}")]
    Router(#[from] vozen_api::RuntimeRouterError),
    #[error("health listener failed: {0}")]
    HealthListener(#[from] std::io::Error),
    #[error("audio cache startup retention failed: {0}")]
    AudioCacheRetention(std::io::Error),
}

#[tokio::main]
async fn main() {
    let error_reporter = error_reporter::ErrorReporter::from_environment();
    if let Err(error) = run().await {
        // Runtime errors intentionally never contain the Discord token or an OAuth bearer token.
        eprintln!("vozen runtime startup failed: {error}");
        error_reporter.report(&error.to_string(), "runtime").await;
        std::process::exit(1);
    }
}

async fn run() -> Result<(), RuntimeError> {
    if image_smoke_enabled() {
        return run_image_smoke().await;
    }
    let config = RuntimeConfig::from_environment()?;
    // Share the same loopback guard as the Node supervisor. A Rust cutover must never silently
    // create a second gateway session with the same Discord token while Node is still alive.
    let _instance_guard = acquire_single_instance_lock()?;
    // Opening the store verifies/migrates the exact Node SQLite schema before the Rust gateway
    // does any work. Keep the handle alive for the whole process; future adapters share it.
    let store = Arc::new(Mutex::new(SqliteStore::open(&config.database_path)?));
    store
        .lock()
        .map_err(|_| RuntimeError::StoreLock)?
        .verify_integrity()?;
    // The local SQLite store remains the compatibility fallback. In `shadow` it is authoritative;
    // in `mirror` the same handlers stay local while configuration reads are refreshed from the
    // private Postgres snapshot and durable changes are delivered asynchronously.
    let postgres_shadow = if let Some(postgres) = config.postgres_shadow.as_ref() {
        let runtime = postgres_shadow::PostgresShadowRuntime::connect(postgres).await?;
        let mode = match postgres.mode() {
            postgres_shadow::PostgresMode::Shadow => "shadow",
            postgres_shadow::PostgresMode::Mirror => "mirror",
        };
        eprintln!("[postgres] {mode} preflight passed; local SQLite fallback remains available");
        Some(runtime)
    } else {
        None
    };
    let supabase_metrics = postgres_shadow.as_ref().map(|postgres| {
        let cache = postgres_metrics::new_cache();
        postgres_metrics::spawn(
            postgres.pool(),
            cache.clone(),
            postgres_metrics::database_capacity_from_environment(),
        );
        cache
    });
    if env::var("RUST_POSTGRES_IMPORT_SQLITE")
        .ok()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
    {
        let postgres = postgres_shadow
            .as_ref()
            .ok_or(RuntimeError::PostgresImportRequiresPostgres)?;
        let reports =
            postgres_import::import_and_reconcile(&postgres.pool(), &config.database_path).await?;
        eprintln!(
            "[postgres] SQLite import reconciled {} tables",
            reports.len()
        );
    }
    let postgres_voice_read_store = if config.postgres_voice_read_cache {
        let postgres = postgres_shadow
            .as_ref()
            .ok_or(RuntimeError::PostgresVoiceReadCacheRequiresReplica)?;
        match postgres_voice_cache::load(&postgres.pool()).await {
            Ok(cache) => {
                postgres_voice_cache::spawn(postgres.pool(), cache.clone());
                eprintln!("[postgres] voice reads use the refreshed local Postgres cache");
                Some(cache)
            }
            Err(error) => {
                // The Postgres cache is an optimization layered over the authoritative local
                // SQLite store. A remote schema/data hiccup must not take the Discord gateway
                // offline; keep the local store as the voice-read source until the next restart.
                eprintln!(
                    "[postgres] voice-read cache unavailable; using local SQLite fallback: {error}"
                );
                None
            }
        }
    } else {
        None
    };
    let runtime_batch_buffer = RuntimeBatchBuffer::default();
    if config.postgres_replica_outbox
        && let Some(postgres) = postgres_shadow.as_ref()
    {
        store
            .lock()
            .map_err(|_| RuntimeError::StoreLock)?
            .enable_postgres_replica_outbox()?;
        eprintln!("[postgres] durable SQLite change capture enabled for mirror");
        postgres_outbox::spawn(postgres.pool(), store.clone(), runtime_batch_buffer.clone());
    } else {
        // Replica triggers survive restarts. Stop capture when running on SQLite alone,
        // while preserving already queued batches for an explicit reconciliation.
        store
            .lock()
            .map_err(|_| RuntimeError::StoreLock)?
            .configure_postgres_replica_outbox(false)?;
    }
    run_startup_data_hygiene(&config.database_path);
    start_audio_cache_retention(audio_cache_directories(&config))
        .await
        .map_err(RuntimeError::AudioCacheRetention)?;
    let ffmpeg_path = nonempty_env("FFMPEG_PATH").unwrap_or_else(|| "ffmpeg".to_owned());
    match transcription_adapter::check_ffmpeg(std::path::Path::new(&ffmpeg_path)).await {
        transcription_adapter::FfmpegHealth::Available { version } => {
            eprintln!("[health] ffmpeg OK ({version})");
        }
        transcription_adapter::FfmpegHealth::Unavailable { reason } => {
            eprintln!("[health] ffmpeg unavailable ({reason}); voice playback may fail");
        }
    }
    if let Some(redemption_secret) = config.vote_redemption_secret.as_deref() {
        store
            .lock()
            .map_err(|_| RuntimeError::StoreLock)?
            .initialize_vote_redemption_ledger(redemption_secret)?;
    }
    register_rust_commands_if_enabled(&config).await?;
    // Retention is best effort: a one-off SQLite lock must not take down Discord, and the next
    // daily pass retries. The pseudonymous 30-day vote ledger is purged by its own retention job.
    spawn_vote_retention(store.clone());
    // Google HD counters are cost-control metadata, not personal message content. Keep the same
    // bounded monthly retention as the Node runtime without letting an old row block startup.
    spawn_gcloud_retention(store.clone());
    spawn_guild_retention(store.clone());
    // Ko-fi pending purchases are temporary attribution records. Keep the Node retention
    // boundary (90 days) so abandoned or already-claimed rows do not accumulate indefinitely.
    spawn_kofi_pending_retention(store.clone());
    if config.admin.is_some() {
        spawn_admin_metric_history(config.database_path.clone(), supabase_metrics.clone());
    }
    // This handle is intentionally process-scoped. The dashboard/rejoin adapters receive a
    // clone later; they never infer bot presence from a stale database row.
    let gateway_state = GatewayState::default();
    if config.admin.is_some() {
        spawn_admin_member_history(store.clone(), gateway_state.clone());
    }
    loop_lag::spawn(gateway_state.metrics().as_ref().clone());
    let topgg_trigger = if let Some(topgg_metrics) = config.topgg_metrics {
        let trigger = TopggMetricsTrigger::default();
        spawn_topgg_metrics(
            topgg_metrics,
            gateway_state.clone(),
            store.clone(),
            trigger.clone(),
        );
        Some(trigger)
    } else {
        // A missing token must be visible to the owner panel rather than looking like a quiet
        // integration that has simply never had a guild lifecycle event.
        if let Ok(store) = store.lock() {
            let _ = store.record_topgg_sync_attempt(
                system_now_ms(),
                None,
                None,
                false,
                TopggSyncDetail::Unconfigured,
            );
        }
        None
    };
    // Only a runtime that owns Rust voice sessions may write the shared restart marker. A
    // shadow process must never authorize the still-live Node process to reconnect calls.
    let write_rejoin_marker_on_shutdown = config.core_voice.is_some();
    let mut event_sinks: Vec<Arc<dyn GatewayEventSink>> = Vec::new();
    if setup_enabled(env::var("RUST_SETUP_ENABLED").ok().as_deref()) {
        event_sinks.push(Arc::new(setup_wizard::SetupGatewaySink::new(
            store.clone(),
            gateway_state.clone(),
        )));
    }
    let consent_registry = SttConsentRegistry::default();
    // This sink only records departure markers and clears them on guild_create; it does not
    // consume messages or interactions, so it is safe while Node remains authoritative.
    event_sinks.push(Arc::new(
        guild_lifecycle_sink::GuildLifecycleGatewaySink::new(store.clone(), topgg_trigger),
    ));
    if config.welcome {
        event_sinks.push(Arc::new(guild_welcome_sink::GuildWelcomeGatewaySink));
    }
    if let Some(sink) = owner_command_event_sink(config.owner_commands, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = core_voice_event_sink(
        config.core_voice,
        store.clone(),
        postgres_voice_read_store.unwrap_or_else(|| store.clone()),
        gateway_state.clone(),
        runtime_batch_buffer.clone(),
    )? {
        event_sinks.push(sink);
    }
    if let Some(sink) = autocomplete_event_sink(config.autocomplete, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = tts_file_event_sink(config.tts_file, store.clone())? {
        event_sinks.push(sink);
    }
    #[cfg(feature = "voice-driver")]
    if let Some(sink) = transcription_live_event_sink(
        config.transcription_live,
        config.transcription.clone(),
        store.clone(),
        gateway_state.clone(),
        consent_registry.clone(),
    )? {
        event_sinks.push(sink);
    }
    if let Some(sink) = transcription_event_sink(config.transcription, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = transcription_control_event_sink(
        config.transcription_control,
        store.clone(),
        consent_registry.clone(),
    )? {
        event_sinks.push(sink);
    }
    if let Some(sink) = translation_text_event_sink(config.translation_text, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) =
        translation_preference_event_sink(config.translation_preferences, store.clone())?
    {
        event_sinks.push(sink);
    }
    if let Some(sink) = voice_preference_event_sink(config.voice_preferences, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = pronunciation_event_sink(
        config.pronunciation,
        store.clone(),
        payments_enabled_from_environment(),
    )? {
        event_sinks.push(sink);
    }
    if let Some(sink) = config_language_event_sink(config.config_language, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = config_toggle_event_sink(config.config_toggles, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = config_numeric_event_sink(config.config_numeric, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = config_role_event_sink(config.config_role, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = config_default_voice_event_sink(config.config_default_voice, store.clone())?
    {
        event_sinks.push(sink);
    }
    if let Some(sink) = config_channel_event_sink(config.config_channel, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = config_queue_role_event_sink(config.config_queue_roles, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) =
        config_greet_language_event_sink(config.config_greet_language, store.clone())?
    {
        event_sinks.push(sink);
    }
    if let Some(sink) = config_blockword_event_sink(config.config_blockword, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = config_show_event_sink(config.config_show, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = config_reset_event_sink(config.config_reset, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = uptime_event_sink(config.uptime)? {
        event_sinks.push(sink);
    }
    if let Some(sink) = invite_event_sink(config.invite, config.invite_client_id.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = help_event_sink(config.help, config.help_support_url.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = vote_event_sink(
        config.vote,
        config.vote_client_id.clone(),
        config.vote_redemption_secret.clone(),
        store.clone(),
    )? {
        event_sinks.push(sink);
    }
    if let Some(sink) = top_speakers_event_sink(config.top_speakers, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = birthday_event_sink(config.birthday, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = bot_stats_event_sink(config.bot_stats, gateway_state.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = server_stats_event_sink(
        config.server_stats,
        config.vote_client_id.clone(),
        config.vote_redemption_secret.clone(),
        store.clone(),
    )? {
        event_sinks.push(sink);
    }
    if let Some(sink) = stats_event_sink(config.stats, gateway_state.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = premium_event_sink(
        config.premium,
        store.clone(),
        payments_enabled_from_environment(),
        config.vote_client_id.clone(),
        config.vote_redemption_secret.clone(),
    )? {
        event_sinks.push(sink);
    }
    if let Some(sink) = redeem_event_sink(config.redeem, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = privacy_event_sink(config.privacy, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = game_list_event_sink(config.game_list)? {
        event_sinks.push(sink);
    }
    if let Some(sink) = game_scores_event_sink(config.game_scores, store.clone())? {
        event_sinks.push(sink);
    }
    if let Some(sink) = automatic_translation_event_sink(
        config.automatic_translation,
        store.clone(),
        gateway_state.clone(),
    ) {
        event_sinks.push(sink);
    }
    let event_sink = match event_sinks.len() {
        0 => None,
        1 => event_sinks.into_iter().next(),
        _ => {
            Some(Arc::new(CompositeGatewayEventSink::new(event_sinks)) as Arc<dyn GatewayEventSink>)
        }
    };
    let bot_token = config.discord_token.clone();
    let gateway = run_discord_gateway_with_state_and_sink(
        DiscordRuntimeConfig::from_token(config.discord_token)?,
        gateway_state.clone(),
        event_sink,
    );

    let Some(health_bind) = config.health_bind else {
        if write_rejoin_marker_on_shutdown {
            tokio::select! {
                result = gateway => return result.map_err(RuntimeError::from),
                _ = wait_for_clean_shutdown_signal() => {
                    write_current_rejoin_marker(&gateway_state);
                    return Ok(());
                }
            }
        }
        return gateway.await.map_err(RuntimeError::from);
    };
    let app = build_http_router(
        config.database_path.clone(),
        config.premium_http,
        config.install_oauth,
        config.dashboard,
        config.admin,
        config.topgg_webhook,
        HttpRouterRuntimeOptions {
            public_status: config.public_status,
            bot_token,
        },
        store,
        gateway_state.clone(),
        supabase_metrics.clone(),
    )?;
    let listener = tokio::net::TcpListener::bind(health_bind).await?;
    if write_rejoin_marker_on_shutdown {
        tokio::select! {
            result = gateway => result.map_err(RuntimeError::from),
            result = axum::serve(listener, app) => result.map_err(RuntimeError::from),
            _ = wait_for_clean_shutdown_signal() => {
                write_current_rejoin_marker(&gateway_state);
                Ok(())
            }
        }
    } else {
        tokio::select! {
            result = gateway => result.map_err(RuntimeError::from),
            result = axum::serve(listener, app) => result.map_err(RuntimeError::from),
        }
    }
}

/// Starts only the local health surface used by the production-image smoke test.
///
/// This mode is deliberately opt-in and credential-free. It opens the same SQLite store and
/// constructs the same HTTP router as the normal runtime, but never creates a Discord gateway or
/// acquires the production single-instance lock. A normal invocation still goes through
/// `RuntimeConfig::from_environment`, where a missing token remains a hard startup failure.
async fn run_image_smoke() -> Result<(), RuntimeError> {
    if nonempty_env("DISCORD_TOKEN").is_some() {
        return Err(RuntimeError::ImageSmokeCredentials);
    }

    let health_host = nonempty_env("HEALTH_HOST").unwrap_or_else(|| "127.0.0.1".to_owned());
    let health_ip = health_host
        .parse::<IpAddr>()
        .map_err(|_| RuntimeError::InvalidHealthHost)?;
    let health_port = env::var("HEALTH_PORT")
        .ok()
        .filter(|raw| !raw.trim().is_empty())
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or(RuntimeError::InvalidHealthPort)?;
    let health_bind = SocketAddr::new(health_ip, health_port);

    let configured_db_path = env::var_os("DB_PATH").map(PathBuf::from);
    let database_path = configured_db_path.clone().unwrap_or_else(|| {
        PathBuf::from(format!(
            "/tmp/vozen-image-smoke-{}.sqlite",
            std::process::id()
        ))
    });
    let store = Arc::new(Mutex::new(SqliteStore::open(&database_path)?));
    store
        .lock()
        .map_err(|_| RuntimeError::StoreLock)?
        .verify_integrity()?;

    let app = build_http_router(
        database_path.clone(),
        None,
        None,
        None,
        None,
        None,
        HttpRouterRuntimeOptions {
            public_status: None,
            bot_token: String::new(),
        },
        store.clone(),
        GatewayState::default(),
        None,
    )?;
    let listener = tokio::net::TcpListener::bind(health_bind).await?;
    eprintln!(
        "[image-smoke] SQLite and local health listener ready on http://{health_bind}/health"
    );

    let result = tokio::select! {
        result = axum::serve(listener, app) => result.map_err(RuntimeError::HealthListener),
        _ = wait_for_clean_shutdown_signal() => Ok(()),
    };

    // Never delete a caller-provided DB_PATH. The default smoke database is disposable and lives
    // under /tmp, so clean it (including SQLite's optional sidecars) after a graceful stop.
    if configured_db_path.is_none() {
        drop(store);
        let _ = fs::remove_file(&database_path);
        let _ = fs::remove_file(database_path.with_extension("sqlite-wal"));
        let _ = fs::remove_file(database_path.with_extension("sqlite-shm"));
    }
    result
}

fn image_smoke_enabled() -> bool {
    image_smoke_flag_enabled(env::var("RUST_IMAGE_SMOKE").ok().as_deref())
}

fn image_smoke_flag_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// Waits for an administrator-initiated process stop. SIGTERM covers systemd/VPS deployments;
/// Ctrl+C keeps the local Windows development workflow equivalent. A forced crash never reaches
/// this function and therefore cannot authorize a normal voice-session recovery.
async fn wait_for_clean_shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn write_current_rejoin_marker(gateway_state: &GatewayState) {
    let directory = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let _ = write_planned_rejoin_marker(
        gateway_state
            .bot_voice_sessions()
            .into_iter()
            .map(|(guild_id, _)| guild_id),
        &directory,
    );
}

struct HttpRouterRuntimeOptions {
    public_status: Option<PublicStatusConfig>,
    bot_token: String,
}

#[allow(clippy::too_many_arguments)]
fn build_http_router(
    database_path: PathBuf,
    premium_http: Option<PremiumHttpConfig>,
    install_oauth: Option<TtsInstallOAuthRuntimeConfig>,
    dashboard: Option<DashboardRuntimeOptions>,
    admin: Option<AdminRuntimeOptions>,
    topgg_webhook: Option<TopggWebhookRuntimeConfig>,
    runtime_options: HttpRouterRuntimeOptions,
    store: Arc<Mutex<SqliteStore>>,
    gateway_state: GatewayState,
    supabase_metrics: Option<postgres_metrics::SharedSupabaseMetrics>,
) -> Result<axum::Router, RuntimeError> {
    let runtime_metrics = gateway_state.metrics();
    let public_status = runtime_options.public_status.map(|config| {
        public_status_provider(store.clone(), gateway_state.clone(), config.incident)
    });
    let Some(config) = premium_http else {
        if dashboard.is_some() {
            return Err(RuntimeError::DashboardRequiresPremiumHttp);
        }
        if admin.is_some() {
            return Err(RuntimeError::AdminRequiresPremiumHttp);
        }
        let install = install_oauth.map(|install| InstallApiConfig {
            client_id: install.client_id,
            client_secret: install.client_secret,
            redirect_uri: install.redirect_uri,
            success_redirect: install.success_redirect,
            state_secret: install.state_secret,
            store: store.clone(),
            now: Arc::new(system_now_ms),
        });
        return runtime_router(RuntimeRouterConfig {
            public_status,
            account: None,
            premium: None,
            install,
            stripe: None,
            dashboard: None,
            admin: None,
            kofi_webhook: None,
            topgg_webhook: topgg_webhook.map(|config| TopggWebhookConfig {
                webhook_secret: config.webhook_secret,
                redemption_secret: config.redemption_secret,
                expected_bot_id: config.client_id,
                store,
                metrics: Some(runtime_metrics.clone()),
                now: Arc::new(system_now_ms),
            }),
        })
        .map_err(RuntimeError::from);
    };

    if (dashboard.is_some() || admin.is_some()) && !config.browser_api_enabled {
        return Err(RuntimeError::DashboardOrAdminRequiresPremiumApi);
    }
    let verifier = config
        .client_id
        .clone()
        .map(|client_id| {
            DiscordOAuthVerifier::production(client_id, Some(runtime_options.bot_token.clone()))
                .map(Arc::new)
                .map_err(|_| RuntimeError::OAuthClient)
        })
        .transpose()?;
    let now = Arc::new(system_now_ms);
    let kofi_webhook =
        config
            .kofi_webhook_token
            .clone()
            .map(|verification_token| KofiWebhookConfig {
                verification_token,
                store: store.clone(),
                shop_map: parse_kofi_shop_map(config.kofi_shop_map.as_deref()),
                now: now.clone(),
                on_unmapped_shop: None,
            });
    let dashboard = dashboard
        .map(|dashboard| {
            let models = discover_piper_models(&dashboard.models_dir)?;
            let authorization_state = gateway_state.clone();
            let dashboard_client_ids = if config.dashboard_client_ids.is_empty() {
                config.client_id.clone().into_iter().collect()
            } else {
                config.dashboard_client_ids.clone()
            };
            if dashboard_client_ids.is_empty() {
                return Err(RuntimeError::MissingClientId);
            }
            let authorizer = DiscordDashboardAuthorizer::production_with_client_ids(
                dashboard_client_ids,
                move |guild_id| authorization_state.bot_has_guild(guild_id),
            )
            .map_err(|_| RuntimeError::OAuthClient)?;
            Ok::<DashboardApiConfig, RuntimeError>(DashboardApiConfig {
                origin: config.origin.clone(),
                store: store.clone(),
                authorizer: Arc::new(authorizer),
                options: Arc::new(RuntimeDashboardOptionsProvider::new(
                    gateway_state.clone(),
                    models,
                )),
            })
        })
        .transpose()?;
    let admin = admin
        .map(|admin| {
            let verifier = verifier
                .clone()
                .ok_or(RuntimeError::DashboardOrAdminRequiresPremiumApi)?;
            let gateway_state = gateway_state.clone();
            let metrics_gateway_state = gateway_state.clone();
            let profile_gateway_state = gateway_state.clone();
            let api = vozen_api::admin_api::AdminApi::new(AdminApiConfig {
                store: store.clone(),
                resolver: verifier,
                now: now.clone(),
                admin_session_secret: admin.session_secret,
                owner_id: admin.owner_id,
                admin_client_id: admin.client_id,
                session_ttl_seconds: None,
                log: Arc::new(|message| eprintln!("{message}")),
                resolve_guilds: Some(Arc::new(move || {
                    gateway_state
                        .guild_snapshots()
                        .into_iter()
                        .map(|guild| vozen_api::admin_api::AdminGuildBrief {
                            id: guild.id,
                            name: guild.name,
                            icon: guild.icon,
                            member_count: i64::try_from(guild.member_count).unwrap_or(i64::MAX),
                            joined_timestamp: Some(guild.joined_timestamp.saturating_mul(1_000)),
                        })
                        .collect()
                })),
                resolve_talker_profiles: Some(Arc::new(RuntimeAdminTalkerProfileResolver {
                    gateway_state: profile_gateway_state,
                })),
                local_day: Arc::new(system_local_day),
                system_metrics: Some(Arc::new({
                    let database_path = database_path.clone();
                    let gateway_state = metrics_gateway_state;
                    let supabase_metrics = supabase_metrics.clone();
                    let store = store.clone();
                    move || {
                        let mirror_enabled = supabase_metrics.is_some();
                        let supabase = supabase_metrics
                            .as_ref()
                            .and_then(|cache| cache.read().ok().and_then(|value| value.clone()));
                        let active_voice_servers = gateway_state
                            .bot_voice_sessions()
                            .into_iter()
                            .map(
                                |(guild_id, _)| vozen_api::admin_api::AdminActiveVoiceServer {
                                    name: gateway_state
                                        .guild_name(&guild_id)
                                        .unwrap_or_else(|| "Servidor a sincronizar".into()),
                                },
                            )
                            .collect();
                        admin_metrics::snapshot_with_supabase(
                            &database_path,
                            active_voice_servers,
                            supabase,
                            if mirror_enabled {
                                store
                                    .lock()
                                    .ok()
                                    .and_then(|value| value.runtime_outbox_metrics().ok())
                            } else {
                                None
                            },
                        )
                    }
                })),
            });
            Ok::<AdminRouterConfig, RuntimeError>(AdminRouterConfig {
                origin: admin.panel_origin,
                api: Arc::new(api),
                now: now.clone(),
                web_analytics: admin.web_analytics,
            })
        })
        .transpose()?;
    let (account, premium) = if config.browser_api_enabled {
        let verifier = verifier.clone().ok_or(RuntimeError::MissingClientId)?;
        (
            Some(AccountApiConfig {
                origin: config.origin.clone(),
                store: store.clone(),
                identity_verifier: verifier.clone(),
                now: now.clone(),
                // Guild names are sourced only from the current gateway process; a missing cache
                // entry stays `null` rather than causing an outbound lookup or leaking old data.
                resolve_guild_name: Some(Arc::new({
                    let gateway_state = gateway_state.clone();
                    move |guild_id| gateway_state.guild_name(guild_id)
                })),
            }),
            Some(PremiumApiConfig {
                origin: config.origin.clone(),
                kofi_webhook_token: config.kofi_webhook_token.clone(),
                store: store.clone(),
                identity_verifier: verifier,
                now: now.clone(),
                claim_help_notifier: if config.kofi_webhook_token.is_some() {
                    config
                        .claim_help_webhook_url
                        .clone()
                        .map(DiscordClaimHelpNotifier::new)
                        .map(|notifier| Arc::new(notifier) as Arc<dyn ClaimHelpNotifier>)
                } else {
                    None
                },
            }),
        )
    } else {
        (None, None)
    };
    let stripe = if config.browser_api_enabled {
        match (
            config.stripe_secret_key.clone(),
            config.stripe_publishable_key.clone(),
            config.stripe_webhook_secret.clone(),
            config.stripe_prices.clone(),
            verifier.clone(),
        ) {
            (
                Some(secret_key),
                Some(publishable_key),
                Some(webhook_secret),
                Some(prices),
                Some(identity_verifier),
            ) => Some(vozen_api::stripe_api::StripeApiConfig {
                origin: config.origin.clone(),
                // Pausing new charges must leave renewal webhooks and existing billing intact.
                purchases_enabled: premium_http_enabled(
                    env::var("RUST_NEW_PAYMENTS_ENABLED").ok().as_deref(),
                ),
                secret_key,
                publishable_key,
                webhook_secret,
                prices,
                store: store.clone(),
                identity_verifier,
                now: now.clone(),
            }),
            _ => None,
        }
    } else {
        None
    };
    let install = install_oauth.map(|install| InstallApiConfig {
        client_id: install.client_id,
        client_secret: install.client_secret,
        redirect_uri: install.redirect_uri,
        success_redirect: install.success_redirect,
        state_secret: install.state_secret,
        store: store.clone(),
        now: now.clone(),
    });
    runtime_router(RuntimeRouterConfig {
        public_status,
        account,
        premium,
        stripe,
        install,
        // Only `RUST_DASHBOARD_ENABLED=true` produces this route. Its authorizer rechecks
        // OAuth audience/scope, Manage Guild and current bot presence before the options
        // provider asks Discord for the bot's current authorised channels and roles.
        dashboard,
        admin,
        kofi_webhook,
        topgg_webhook: topgg_webhook.map(|config| TopggWebhookConfig {
            webhook_secret: config.webhook_secret,
            redemption_secret: config.redemption_secret,
            expected_bot_id: config.client_id,
            store: store.clone(),
            metrics: Some(runtime_metrics),
            now: Arc::new(system_now_ms),
        }),
    })
    .map_err(RuntimeError::from)
}

/// Produces the same coarse public status shape as Node. Any SQLite problem becomes an
/// unavailable database/providers component; provider names and errors never leave the process.
fn public_status_provider(
    store: Arc<Mutex<SqliteStore>>,
    gateway_state: GatewayState,
    incident: Option<String>,
) -> PublicStatusProvider {
    Arc::new(move || {
        let (database_ready, provider_states) = match store.lock() {
            Ok(store) => match store.list_provider_health() {
                Ok(rows) => (
                    true,
                    rows.into_iter()
                        .map(|row| match row.health {
                            StoreProviderHealth::Healthy => PublicProviderHealth::Healthy,
                            StoreProviderHealth::Degraded => PublicProviderHealth::Degraded,
                        })
                        .collect(),
                ),
                Err(_) => (false, Vec::new()),
            },
            Err(_) => (false, Vec::new()),
        };
        map_public_status(PublicStatusInput {
            bot_ready: gateway_state.is_ready(),
            database_ready,
            provider_states,
            incident_message: incident.clone(),
        })
    })
}

fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn acquire_single_instance_lock() -> Result<Option<TcpListener>, RuntimeError> {
    let Some(port) = parse_single_instance_port(env::var("SINGLE_INSTANCE_PORT").ok().as_deref())?
    else {
        return Ok(None);
    };
    TcpListener::bind(("127.0.0.1", port))
        .map(Some)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                RuntimeError::SingleInstanceAlreadyRunning
            } else {
                RuntimeError::SingleInstanceLockFailed
            }
        })
}

fn parse_single_instance_port(raw: Option<&str>) -> Result<Option<u16>, RuntimeError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Some(59_595));
    };
    if raw.eq_ignore_ascii_case("off") || raw == "0" {
        return Ok(None);
    }
    let port = raw
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(RuntimeError::InvalidSingleInstancePort)?;
    Ok(Some(port))
}

/// Reconciles files that cannot be cleaned up transactionally after a hard process stop. The
/// operation is deliberately best-effort and narrow: only Rust/Vozen-owned STT prefixes and the
/// exact legacy `voice-clones` directory beside the configured database are touched.
fn run_startup_data_hygiene(database_path: &std::path::Path) {
    let removed = transcription_adapter::sweep_orphan_stt_temps(&env::temp_dir(), system_now_ms());
    if removed > 0 {
        eprintln!("[retention] removed {removed} orphaned STT workspace(s)");
    }

    let Some(parent) = database_path.parent() else {
        return;
    };
    let legacy_voice_clones = parent.join("voice-clones");
    if !legacy_voice_clones.is_dir() {
        return;
    }
    if let Err(error) = fs::remove_dir_all(&legacy_voice_clones) {
        eprintln!("[retention] legacy voice-clones cleanup failed: {error}");
    } else {
        eprintln!("[retention] removed legacy voice-clones directory");
    }
}

fn system_local_day() -> String {
    time::OffsetDateTime::now_utc().date().to_string()
}

fn purge_vote_retention(
    store: &SqliteStore,
    now: i64,
) -> Result<(usize, usize), vozen_store::StoreError> {
    let rewards = store.purge_expired_vote_rewards(now)?;
    let events = store.purge_expired_topgg_events(now)?;
    Ok((rewards, events))
}

fn audio_cache_directories(config: &RuntimeConfig) -> Vec<PathBuf> {
    // Also sweep disabled providers: their old artifacts still require retention.
    let mut directories: Vec<PathBuf> = [
        ("RUST_VOICE_CACHE_DIR", "./audio-cache/rust"),
        ("RUST_GTTS_CACHE_DIR", "./audio-cache/rust-gtts"),
        ("RUST_NEURAL_CACHE_DIR", "./audio-cache/rust-neural"),
        ("RUST_GCLOUD_CACHE_DIR", "./audio-cache/rust-gcloud"),
        ("RUST_KOKORO_CACHE_DIR", "./audio-cache/rust-kokoro"),
        ("RUST_TTS_FILE_CACHE_DIR", "./audio-cache/rust-file"),
    ]
    .into_iter()
    .map(|(key, default)| PathBuf::from(nonempty_env(key).unwrap_or_else(|| default.to_owned())))
    .collect();
    if let Some(voice) = config.core_voice.as_ref() {
        directories.extend([
            voice.cache_dir.clone(),
            voice.gtts_cache_dir.clone(),
            voice.neural_cache_dir.clone(),
            voice.gcloud_cache_dir.clone(),
            voice.kokoro_cache_dir.clone(),
        ]);
    }
    if let Some(file) = config.tts_file.as_ref() {
        directories.push(file.cache_dir.clone());
    }
    directories.sort();
    directories.dedup();
    directories
}

async fn sweep_audio_caches(directories: &[PathBuf]) -> Result<usize, std::io::Error> {
    let now = SystemTime::now();
    let mut removed = 0;
    let mut failure = None;
    for directory in directories {
        match vozen_tts::purge_expired_audio_cache(directory, now).await {
            Ok(count) => removed += count,
            Err(error) => failure = Some(error),
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(removed),
    }
}

async fn start_audio_cache_retention(directories: Vec<PathBuf>) -> Result<(), std::io::Error> {
    // Refuse startup if pre-existing expired speech cannot be removed. No Discord session exists yet.
    let removed = sweep_audio_caches(&directories).await?;
    eprintln!(
        "[retention] audio cache startup: {removed} expired payload(s) removed; max age 7 days; sweep hourly"
    );
    spawn_audio_cache_retention(directories, vozen_tts::AUDIO_CACHE_SWEEP_INTERVAL);
    Ok(())
}

fn spawn_audio_cache_retention(
    directories: Vec<PathBuf>,
    period: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // Startup sweep already handled the immediate tick.
        loop {
            interval.tick().await;
            match sweep_audio_caches(&directories).await {
                Ok(removed) => {
                    eprintln!("[retention] audio cache sweep: {removed} expired payload(s) removed")
                }
                Err(error) => {
                    eprintln!("[retention] audio cache sweep failed; retry next hour: {error}")
                }
            }
        }
    })
}

fn spawn_vote_retention(store: Arc<Mutex<SqliteStore>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        loop {
            interval.tick().await;
            if let Ok(store) = store.lock() {
                let _ = purge_vote_retention(&store, system_now_ms());
            }
        }
    });
}

const GCLOUD_RETENTION_MS: i64 = 92 * 86_400_000;

fn purge_gcloud_retention(store: &SqliteStore, now: i64) -> Result<usize, vozen_store::StoreError> {
    let cutoff = month_key_utc(now.saturating_sub(GCLOUD_RETENTION_MS));
    store.purge_old_gcloud_usage(&cutoff)
}

fn spawn_gcloud_retention(store: Arc<Mutex<SqliteStore>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        loop {
            interval.tick().await;
            if let Ok(store) = store.lock() {
                let _ = purge_gcloud_retention(&store, system_now_ms());
            }
        }
    });
}

fn spawn_guild_retention(store: Arc<Mutex<SqliteStore>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        loop {
            interval.tick().await;
            if let Ok(store) = store.lock() {
                let _ = store.purge_departed_guilds(system_now_ms(), DEPARTURE_GRACE_MS);
            }
        }
    });
}

/// Daily storage readings are written independently of console visits, so the owner sees an
/// honest seven-day history even when nobody opened the dashboard that day.
fn spawn_admin_metric_history(
    database_path: PathBuf,
    supabase_metrics: Option<postgres_metrics::SharedSupabaseMetrics>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(6 * 60 * 60));
        loop {
            interval.tick().await;
            let supabase = supabase_metrics
                .as_ref()
                .and_then(|cache| cache.read().ok().and_then(|value| value.clone()));
            admin_metrics::record_daily_history(&database_path, supabase);
        }
    });
}

/// Records one aggregate guild-member total per UTC day independently of dashboard visits. The
/// first minute gives Discord time to deliver Guild Create snapshots after READY; later polls
/// update the same day with the newest live total. Empty or not-yet-ready gateway state is never
/// persisted as a misleading zero.
fn spawn_admin_member_history(store: Arc<Mutex<SqliteStore>>, gateway_state: GatewayState) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let mut interval = tokio::time::interval(Duration::from_secs(15 * 60));
        loop {
            interval.tick().await;
            if !gateway_state.is_ready() {
                continue;
            }
            let snapshots = gateway_state.guild_snapshots();
            if snapshots.is_empty() {
                continue;
            }
            let member_count = snapshots.iter().fold(0_i64, |total, guild| {
                total.saturating_add(i64::try_from(guild.member_count).unwrap_or(i64::MAX))
            });
            if let Ok(store) = store.lock()
                && let Err(error) = store.record_admin_member_total(
                    &system_local_day(),
                    member_count,
                    system_now_ms(),
                )
            {
                eprintln!("[admin] member history snapshot failed: {error}");
            }
        }
    });
}

const KOFI_PENDING_RETENTION_MS: i64 = 90 * 86_400_000;

fn purge_kofi_pending_retention(
    store: &SqliteStore,
    now: i64,
) -> Result<usize, vozen_store::StoreError> {
    store.purge_old_kofi_pending(now.saturating_sub(KOFI_PENDING_RETENTION_MS))
}

fn spawn_kofi_pending_retention(store: Arc<Mutex<SqliteStore>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        loop {
            interval.tick().await;
            if let Ok(store) = store.lock() {
                let _ = purge_kofi_pending_retention(&store, system_now_ms());
            }
        }
    });
}

fn spawn_topgg_metrics(
    config: TopggMetricsRuntimeConfig,
    gateway_state: GatewayState,
    store: Arc<Mutex<SqliteStore>>,
    trigger: TopggMetricsTrigger,
) {
    tokio::spawn(async move {
        let Ok(http) = ReqwestTopggMetricsHttp::new() else {
            // The listing is optional, but a local client construction failure must not make the
            // broken setup invisible to an operator.
            if let Ok(store) = store.lock() {
                let _ = store.record_topgg_sync_attempt(
                    system_now_ms(),
                    None,
                    None,
                    false,
                    TopggSyncDetail::TransportFailure,
                );
            }
            return;
        };
        // Node starts Top.gg work from ClientReady. Do not publish a transient zero while the
        // gateway is still establishing its authoritative guild cache.
        while !gateway_state.is_ready() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let mut interval = tokio::time::interval(TOPGG_POST_INTERVAL);
        let mut commands_synced = false;
        loop {
            tokio::select! {
                _ = interval.tick() => {},
                _ = trigger.notified() => {},
            }
            let server_count = gateway_state.guild_count();
            // Verify the v1 Bearer credential first. In particular, this prevents a legacy or
            // revoked token from being mistaken for a successful posting path.
            let validation = validate_topgg_v1_token(&http, &config.token).await;
            let (outcome, sent_server_count) = if validation.succeeded() {
                if !commands_synced && let Some(commands) = public_topgg_commands() {
                    commands_synced = sync_topgg_commands(&http, &config.token, commands).await;
                }
                (
                    post_topgg_stats_with_shards(
                        &http,
                        &config.client_id,
                        &config.token,
                        server_count,
                        1,
                    )
                    .await,
                    Some(server_count),
                )
            } else {
                (validation, None)
            };
            if let Ok(store) = store.lock() {
                let _ = store.record_topgg_sync_attempt(
                    system_now_ms(),
                    outcome.status(),
                    sent_server_count,
                    outcome.succeeded(),
                    outcome.detail(),
                );
            }
            if !outcome.succeeded() {
                eprintln!(
                    "[topgg] v1 validation or metrics publish failed: detail={}, status={:?}, server_count={server_count}",
                    outcome.detail().as_storage(),
                    outcome.status(),
                );
            }
        }
    });
}

fn public_topgg_commands() -> Option<Vec<serde_json::Value>> {
    DiscordCommandCatalog::from_json(DISCORD_COMMAND_CONTRACT)
        .ok()?
        .public_registration_payload()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn audio_retention_cleans_startup_and_then_idle_caches() {
        let root =
            std::env::temp_dir().join(format!("vozen-runtime-retention-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let old = SystemTime::now() - vozen_tts::AUDIO_CACHE_MAX_AGE - Duration::from_secs(60);
        let paths: Vec<_> = (0..6).map(|index| root.join(index.to_string())).collect();
        for directory in &paths {
            std::fs::create_dir(directory).unwrap();
            let file = directory.join("startup.wav");
            std::fs::write(&file, b"audio").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        start_audio_cache_retention(paths.clone()).await.unwrap();
        assert!(
            paths
                .iter()
                .all(|directory| !directory.join("startup.wav").exists())
        );
        let idle = paths[0].join("idle.wav");
        std::fs::write(&idle, b"audio").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&idle)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        let task = spawn_audio_cache_retention(paths, Duration::from_millis(20));
        tokio::time::timeout(Duration::from_secs(5), async {
            while idle.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn audio_retention_refuses_startup_on_invalid_root() {
        let file =
            std::env::temp_dir().join(format!("vozen-runtime-retention-{}", uuid::Uuid::new_v4()));
        std::fs::write(&file, b"not a directory").unwrap();
        assert!(
            start_audio_cache_retention(vec![file.clone()])
                .await
                .is_err()
        );
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn runtime_errors_without_a_token() {
        assert!(matches!(
            DiscordRuntimeConfig::from_token(String::new()),
            Err(DiscordRuntimeError::MissingToken)
        ));
    }

    #[test]
    fn health_port_is_loopback_only_when_constructed() {
        let address = SocketAddr::from(([127, 0, 0, 1], 8080));
        assert!(address.ip().is_loopback());
        assert_eq!(address.port(), 8080);
    }

    #[test]
    fn image_smoke_mode_is_exactly_opt_in() {
        assert!(image_smoke_flag_enabled(Some("true")));
        assert!(image_smoke_flag_enabled(Some(" TRUE ")));
        assert!(!image_smoke_flag_enabled(Some("1")));
        assert!(!image_smoke_flag_enabled(Some("yes")));
        assert!(!image_smoke_flag_enabled(None));
    }

    #[test]
    fn single_instance_guard_defaults_shared_with_node_and_has_explicit_disable() {
        assert_eq!(
            parse_single_instance_port(None).expect("default"),
            Some(59_595)
        );
        assert_eq!(
            parse_single_instance_port(Some(" 59596 ")).expect("custom"),
            Some(59_596)
        );
        assert_eq!(parse_single_instance_port(Some("off")).expect("off"), None);
        assert_eq!(parse_single_instance_port(Some("0")).expect("zero"), None);
        assert!(matches!(
            parse_single_instance_port(Some("not-a-port")),
            Err(RuntimeError::InvalidSingleInstancePort)
        ));
    }

    #[test]
    fn opted_in_http_surfaces_require_a_listener() {
        assert!(http_listener_required(
            None, true, false, false, false, false, false
        ));
        assert!(http_listener_required(
            None, false, true, false, false, false, false
        ));
        assert!(http_listener_required(
            None, false, false, false, false, true, false
        ));
        assert!(!http_listener_required(
            Some(SocketAddr::from(([127, 0, 0, 1], 8080))),
            true,
            true,
            true,
            true,
            true,
            true,
        ));
        assert!(!http_listener_required(
            None, false, false, false, false, false, false
        ));
    }

    #[test]
    fn premium_http_flag_is_exactly_opt_in() {
        assert!(premium_http_enabled(Some("true")));
        assert!(premium_http_enabled(Some("TRUE")));
        assert!(!premium_http_enabled(Some("1")));
        assert!(!premium_http_enabled(Some("yes")));
        assert!(!premium_http_enabled(None));
    }

    #[test]
    fn postgres_replica_outbox_is_exactly_opt_in() {
        assert!(postgres_replica_outbox_enabled(Some("true")));
        assert!(postgres_replica_outbox_enabled(Some(" TRUE ")));
        assert!(!postgres_replica_outbox_enabled(Some("1")));
        assert!(!postgres_replica_outbox_enabled(Some("yes")));
        assert!(!postgres_replica_outbox_enabled(None));
    }

    #[test]
    fn full_mode_requires_the_explicit_browser_api_promotion_and_real_api_config() {
        let config = PremiumHttpConfig {
            browser_api_enabled: true,
            client_id: Some("client".into()),
            dashboard_client_ids: vec!["client".into()],
            origin: "https://vozen.org".into(),
            kofi_webhook_token: None,
            kofi_shop_map: None,
            claim_help_webhook_url: None,
            stripe_secret_key: None,
            stripe_publishable_key: None,
            stripe_webhook_secret: None,
            stripe_prices: None,
        };
        assert!(browser_api_promoted(Some("true"), Some(&config)));
        assert!(browser_api_promoted(Some(" TRUE "), Some(&config)));
        assert!(!browser_api_promoted(Some("false"), Some(&config)));
        assert!(!browser_api_promoted(None, Some(&config)));
        assert!(!browser_api_promoted(
            Some("true"),
            Some(&PremiumHttpConfig {
                browser_api_enabled: false,
                ..config
            })
        ));
        assert!(!browser_api_promoted(Some("true"), None));
    }

    #[test]
    fn public_status_flag_is_exactly_opt_in() {
        assert!(public_status_enabled(Some("true")));
        assert!(public_status_enabled(Some("TRUE")));
        assert!(!public_status_enabled(Some("1")));
        assert!(!public_status_enabled(Some("yes")));
        assert!(!public_status_enabled(None));
    }

    #[test]
    fn rust_voice_promotion_is_exactly_opt_in() {
        assert!(core_voice_enabled(Some("true")));
        assert!(core_voice_enabled(Some(" TRUE ")));
        assert!(!core_voice_enabled(Some("1")));
        assert!(!core_voice_enabled(Some("yes")));
        assert!(!core_voice_enabled(None));
    }

    #[test]
    fn setup_promotion_is_exactly_opt_in() {
        assert!(setup_enabled(Some("true")));
        assert!(setup_enabled(Some(" TRUE ")));
        assert!(!setup_enabled(Some("1")));
        assert!(!setup_enabled(Some("yes")));
        assert!(!setup_enabled(None));
    }

    #[test]
    fn autocomplete_promotion_is_exactly_opt_in() {
        assert!(autocomplete_enabled(Some("true")));
        assert!(autocomplete_enabled(Some(" TRUE ")));
        assert!(!autocomplete_enabled(Some("1")));
        assert!(!autocomplete_enabled(Some("yes")));
        assert!(!autocomplete_enabled(None));
    }

    #[test]
    fn private_file_promotion_is_exactly_opt_in_and_independent_of_calls() {
        assert!(tts_file_enabled(Some("true")));
        assert!(tts_file_enabled(Some(" TRUE ")));
        assert!(!tts_file_enabled(Some("1")));
        assert!(!tts_file_enabled(Some("yes")));
        assert!(!tts_file_enabled(None));
    }

    #[test]
    fn message_transcription_promotion_is_exactly_opt_in() {
        assert!(transcription_enabled(Some("true")));
        assert!(transcription_enabled(Some(" TRUE ")));
        assert!(!transcription_enabled(Some("1")));
        assert!(!transcription_enabled(Some("yes")));
        assert!(!transcription_enabled(None));
    }

    #[test]
    fn transcription_control_promotion_is_exactly_opt_in() {
        assert!(transcription_control_enabled(Some("true")));
        assert!(transcription_control_enabled(Some(" TRUE ")));
        assert!(!transcription_control_enabled(Some("1")));
        assert!(!transcription_control_enabled(Some("yes")));
        assert!(!transcription_control_enabled(None));
    }

    #[test]
    fn live_transcription_promotion_is_exactly_opt_in() {
        assert!(live_transcription_enabled(Some("true")));
        assert!(live_transcription_enabled(Some(" TRUE ")));
        assert!(!live_transcription_enabled(Some("1")));
        assert!(!live_transcription_enabled(Some("yes")));
        assert!(!live_transcription_enabled(None));
    }

    #[test]
    fn whisper_runtime_prefers_the_pinned_project_venv() {
        let root = env::temp_dir().join(format!("vozen-whisper-resolve-{}", std::process::id()));
        let python = root.join("tools/whisper-venv/bin/python");
        let script = root.join("tools/whisper_sidecar.py");
        fs::create_dir_all(python.parent().expect("python parent")).expect("venv directory");
        fs::write(&python, b"python").expect("python marker");
        fs::write(&script, b"sidecar").expect("script marker");

        let resolved = resolve_whisper_runtime(&root, None, None);
        assert_eq!(resolved, (python, script));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn whisper_runtime_keeps_explicit_paths_and_safe_fallbacks() {
        let root = env::temp_dir().join(format!("vozen-whisper-fallback-{}", std::process::id()));
        fs::create_dir_all(&root).expect("test root");
        assert_eq!(
            resolve_whisper_runtime(&root, Some("custom-python"), Some("custom-sidecar.py")),
            (
                PathBuf::from("custom-python"),
                PathBuf::from("custom-sidecar.py")
            )
        );
        assert_eq!(
            resolve_whisper_runtime(&root, None, None),
            (
                PathBuf::from("python3"),
                PathBuf::from("tools/whisper_sidecar.py")
            )
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn piper_runtime_rejects_a_directory_masquerading_as_the_executable() {
        let root = env::temp_dir().join(format!(
            "vozen-piper-directory-regression-{}",
            std::process::id()
        ));
        let piper_directory = root.join("piper");
        let models = root.join("models");
        fs::create_dir_all(&piper_directory).expect("piper directory");
        fs::create_dir_all(&models).expect("models directory");
        fs::write(models.join("en_US-amy-medium.onnx"), b"model").expect("model");
        fs::write(models.join("en_US-amy-medium.onnx.json"), b"{}").expect("model config");

        assert!(matches!(
            validate_piper_runtime(&piper_directory, &models, "en_US-amy-medium"),
            Err(RuntimeError::PiperExecutableUnavailable)
        ));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn piper_runtime_requires_the_default_model_configuration_pair() {
        let root = env::temp_dir().join(format!(
            "vozen-piper-config-regression-{}",
            std::process::id()
        ));
        let piper = root.join(if cfg!(windows) { "piper.exe" } else { "piper" });
        let models = root.join("models");
        fs::create_dir_all(&models).expect("models directory");
        fs::write(&piper, b"executable").expect("piper marker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&piper, fs::Permissions::from_mode(0o755)).expect("executable bit");
        }
        fs::write(models.join("en_US-amy-medium.onnx"), b"model").expect("model");

        assert!(matches!(
            validate_piper_runtime(&piper, &models, "en_US-amy-medium"),
            Err(RuntimeError::DefaultVoiceConfigUnavailable)
        ));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn piper_runtime_accepts_an_executable_and_complete_default_model_pair() {
        let root = env::temp_dir().join(format!(
            "vozen-piper-complete-regression-{}",
            std::process::id()
        ));
        let piper = root.join(if cfg!(windows) { "piper.exe" } else { "piper" });
        let models = root.join("models");
        fs::create_dir_all(&models).expect("models directory");
        fs::write(&piper, b"executable").expect("piper marker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&piper, fs::Permissions::from_mode(0o755)).expect("executable bit");
        }
        fs::write(models.join("en_US-amy-medium.onnx"), b"model").expect("model");
        fs::write(models.join("en_US-amy-medium.onnx.json"), b"{}").expect("model config");

        assert_eq!(
            validate_piper_runtime(&piper, &models, "en_US-amy-medium")
                .expect("complete Piper runtime"),
            vec!["en_US-amy-medium"]
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn google_default_restores_the_legacy_synthetic_voice_catalogue_for_games() {
        use vozen_discord::{GameDriverAction, GameDriverFactory, GuessLanguageDriverAction};

        let models = available_models_for_default_provider(
            vec!["en_US-amy-medium".into()],
            SynthesisEngine::Default,
        );
        assert_eq!(models.len(), GTTS_SYNTHETIC_LOCALES.len());
        assert!(models.iter().any(|model| model == "pt_PT-google-medium"));
        assert!(models.iter().any(|model| model == "de_DE-google-medium"));
        assert_eq!(
            models
                .iter()
                .filter(|model| model.starts_with("en_US-"))
                .count(),
            1,
            "an installed Piper locale must not gain a duplicate synthetic voice"
        );

        let factory = GameDriverFactory::new(models, "en_US-amy-medium", "en");
        let mut driver = factory
            .create("guess-language", None, 42)
            .expect("Guess the Language driver");
        let actions = driver.on_start(0);
        assert!(matches!(
            actions.as_slice(),
            [
                GameDriverAction::Announcement(intro),
                GameDriverAction::GuessLanguage(GuessLanguageDriverAction::RoundOpened {
                    round: 1,
                    total: 5,
                    ..
                })
            ] if intro.parameters.get("rounds").map(String::as_str) == Some("5")
        ));
    }

    #[test]
    fn local_or_neural_defaults_do_not_advertise_google_only_models() {
        let piper = vec!["en_US-amy-medium".to_owned()];
        assert_eq!(
            available_models_for_default_provider(piper.clone(), SynthesisEngine::Piper),
            piper
        );
        assert_eq!(
            available_models_for_default_provider(
                vec!["en_US-amy-medium".to_owned()],
                SynthesisEngine::Neural
            ),
            vec!["en_US-amy-medium"]
        );
    }

    #[cfg(feature = "voice-driver")]
    #[test]
    fn google_voice_canary_does_not_require_a_piper_installation() {
        let missing = std::path::Path::new("__vozen_missing_staging_piper__");
        assert_eq!(
            piper_models_for_core_voice(
                SynthesisEngine::Default,
                missing,
                missing,
                "en_US-google-medium",
            )
            .expect("Google voice must run without Piper"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn core_voice_can_select_each_supported_operator_default() {
        assert_eq!(
            core_voice_default_engine(Some("gtts")).expect("gtts default"),
            SynthesisEngine::Default
        );
        assert_eq!(
            core_voice_default_engine(Some("PIPER")).expect("piper default"),
            SynthesisEngine::Piper
        );
        assert_eq!(
            core_voice_default_engine(Some("router")).expect("router default"),
            SynthesisEngine::Default
        );
        assert_eq!(
            core_voice_default_engine(Some("NEURAL")).expect("neural default"),
            SynthesisEngine::Neural
        );
    }

    #[test]
    fn private_translation_promotion_is_exactly_opt_in_and_independent_of_calls() {
        assert!(translation_text_enabled(Some("true")));
        assert!(translation_text_enabled(Some(" TRUE ")));
        assert!(!translation_text_enabled(Some("1")));
        assert!(!translation_text_enabled(Some("yes")));
        assert!(!translation_text_enabled(None));
    }

    #[test]
    fn welcome_promotion_is_exactly_opt_in() {
        assert!(welcome_enabled(Some("true")));
        assert!(welcome_enabled(Some(" TRUE ")));
        assert!(!welcome_enabled(Some("1")));
        assert!(!welcome_enabled(Some("yes")));
        assert!(!welcome_enabled(None));
    }

    #[test]
    fn translation_admin_promotion_is_exactly_opt_in_and_separate_from_text() {
        assert!(translation_admin_enabled(Some("true")));
        assert!(translation_admin_enabled(Some(" TRUE ")));
        assert!(!translation_admin_enabled(Some("1")));
        assert!(!translation_admin_enabled(Some("yes")));
        assert!(!translation_admin_enabled(None));
    }

    #[test]
    fn translation_preference_promotion_is_exactly_opt_in_and_independent_of_text() {
        assert!(translation_preferences_enabled(Some("true")));
        assert!(translation_preferences_enabled(Some(" TRUE ")));
        assert!(!translation_preferences_enabled(Some("1")));
        assert!(!translation_preferences_enabled(Some("yes")));
        assert!(!translation_preferences_enabled(None));
    }

    #[test]
    fn voice_preference_promotion_is_exactly_opt_in_and_independent_of_voice_driver() {
        assert!(voice_preferences_enabled(Some("true")));
        assert!(voice_preferences_enabled(Some(" TRUE ")));
        assert!(!voice_preferences_enabled(Some("1")));
        assert!(!voice_preferences_enabled(Some("yes")));
        assert!(!voice_preferences_enabled(None));
    }

    #[test]
    fn pronunciation_promotion_is_exactly_opt_in() {
        assert!(pronunciation_enabled(Some("true")));
        assert!(pronunciation_enabled(Some(" TRUE ")));
        assert!(!pronunciation_enabled(Some("1")));
        assert!(!pronunciation_enabled(Some("yes")));
        assert!(!pronunciation_enabled(None));
    }

    #[test]
    fn config_language_promotion_is_exactly_opt_in() {
        assert!(config_language_enabled(Some("true")));
        assert!(config_language_enabled(Some(" TRUE ")));
        assert!(!config_language_enabled(Some("1")));
        assert!(!config_language_enabled(Some("yes")));
        assert!(!config_language_enabled(None));
    }

    #[test]
    fn config_toggle_promotion_is_exactly_opt_in() {
        assert!(config_toggles_enabled(Some("true")));
        assert!(config_toggles_enabled(Some(" TRUE ")));
        assert!(!config_toggles_enabled(Some("1")));
        assert!(!config_toggles_enabled(Some("yes")));
        assert!(!config_toggles_enabled(None));
    }

    #[test]
    fn numeric_and_role_config_promotions_are_exactly_opt_in() {
        for enabled in [
            config_numeric_enabled as fn(Option<&str>) -> bool,
            config_role_enabled as fn(Option<&str>) -> bool,
        ] {
            assert!(enabled(Some("true")));
            assert!(enabled(Some(" TRUE ")));
            assert!(!enabled(Some("1")));
            assert!(!enabled(Some("yes")));
            assert!(!enabled(None));
        }
    }

    #[test]
    fn default_voice_config_promotion_is_exactly_opt_in() {
        assert!(config_default_voice_enabled(Some("true")));
        assert!(config_default_voice_enabled(Some(" TRUE ")));
        assert!(!config_default_voice_enabled(Some("1")));
        assert!(!config_default_voice_enabled(Some("yes")));
        assert!(!config_default_voice_enabled(None));
    }

    #[test]
    fn config_channel_promotion_is_exactly_opt_in() {
        assert!(config_channel_enabled(Some("true")));
        assert!(config_channel_enabled(Some(" TRUE ")));
        assert!(!config_channel_enabled(Some("1")));
        assert!(!config_channel_enabled(Some("yes")));
        assert!(!config_channel_enabled(None));
    }

    #[test]
    fn config_queue_roles_promotion_is_exactly_opt_in() {
        assert!(config_queue_roles_enabled(Some("true")));
        assert!(config_queue_roles_enabled(Some(" TRUE ")));
        assert!(!config_queue_roles_enabled(Some("1")));
        assert!(!config_queue_roles_enabled(Some("yes")));
        assert!(!config_queue_roles_enabled(None));
    }

    #[test]
    fn config_greet_language_promotion_is_exactly_opt_in() {
        assert!(config_greet_language_enabled(Some("true")));
        assert!(config_greet_language_enabled(Some(" TRUE ")));
        assert!(!config_greet_language_enabled(Some("1")));
        assert!(!config_greet_language_enabled(Some("yes")));
        assert!(!config_greet_language_enabled(None));
    }

    #[test]
    fn config_blockword_promotion_is_exactly_opt_in() {
        assert!(config_blockword_enabled(Some("true")));
        assert!(config_blockword_enabled(Some(" TRUE ")));
        assert!(!config_blockword_enabled(Some("1")));
        assert!(!config_blockword_enabled(Some("yes")));
        assert!(!config_blockword_enabled(None));
    }

    #[test]
    fn config_show_promotion_is_exactly_opt_in() {
        assert!(config_show_enabled(Some("true")));
        assert!(config_show_enabled(Some(" TRUE ")));
        assert!(!config_show_enabled(Some("1")));
        assert!(!config_show_enabled(Some("yes")));
        assert!(!config_show_enabled(None));
    }

    #[test]
    fn config_reset_promotion_is_exactly_opt_in() {
        assert!(config_reset_enabled(Some("true")));
        assert!(config_reset_enabled(Some(" TRUE ")));
        assert!(!config_reset_enabled(Some("1")));
        assert!(!config_reset_enabled(Some("yes")));
        assert!(!config_reset_enabled(None));
    }

    #[test]
    fn uptime_promotion_is_exactly_opt_in() {
        assert!(uptime_enabled(Some("true")));
        assert!(uptime_enabled(Some(" TRUE ")));
        assert!(!uptime_enabled(Some("1")));
        assert!(!uptime_enabled(Some("yes")));
        assert!(!uptime_enabled(None));
    }

    #[test]
    fn invite_promotion_is_exactly_opt_in() {
        assert!(invite_enabled(Some("true")));
        assert!(invite_enabled(Some(" TRUE ")));
        assert!(!invite_enabled(Some("1")));
        assert!(!invite_enabled(Some("yes")));
        assert!(!invite_enabled(None));
    }

    #[test]
    fn help_promotion_is_exactly_opt_in() {
        assert!(help_enabled(Some("true")));
        assert!(help_enabled(Some(" TRUE ")));
        assert!(!help_enabled(Some("1")));
        assert!(!help_enabled(Some("yes")));
        assert!(!help_enabled(None));
    }

    #[test]
    fn vote_promotion_is_exactly_opt_in() {
        assert!(vote_enabled(Some("true")));
        assert!(vote_enabled(Some(" TRUE ")));
        assert!(!vote_enabled(Some("1")));
        assert!(!vote_enabled(Some("yes")));
        assert!(!vote_enabled(None));
    }

    #[test]
    fn top_speakers_promotion_is_exactly_opt_in() {
        assert!(top_speakers_enabled(Some("true")));
        assert!(top_speakers_enabled(Some(" TRUE ")));
        assert!(!top_speakers_enabled(Some("1")));
        assert!(!top_speakers_enabled(Some("yes")));
        assert!(!top_speakers_enabled(None));
    }

    #[test]
    fn privacy_promotion_is_exactly_opt_in() {
        assert!(privacy_enabled(Some("true")));
        assert!(privacy_enabled(Some(" TRUE ")));
        assert!(!privacy_enabled(Some("1")));
        assert!(!privacy_enabled(Some("yes")));
        assert!(!privacy_enabled(None));
    }

    #[test]
    fn birthday_promotion_is_exactly_opt_in() {
        assert!(birthday_enabled(Some("true")));
        assert!(birthday_enabled(Some(" TRUE ")));
        assert!(!birthday_enabled(Some("1")));
        assert!(!birthday_enabled(Some("yes")));
        assert!(!birthday_enabled(None));
    }

    #[test]
    fn bot_stats_promotion_is_exactly_opt_in() {
        assert!(bot_stats_enabled(Some("true")));
        assert!(bot_stats_enabled(Some(" TRUE ")));
        assert!(!bot_stats_enabled(Some("1")));
        assert!(!bot_stats_enabled(Some("yes")));
        assert!(!bot_stats_enabled(None));
    }

    #[test]
    fn server_stats_promotion_is_exactly_opt_in() {
        assert!(server_stats_enabled(Some("true")));
        assert!(server_stats_enabled(Some(" TRUE ")));
        assert!(!server_stats_enabled(Some("1")));
        assert!(!server_stats_enabled(Some("yes")));
        assert!(!server_stats_enabled(None));
    }

    #[test]
    fn stats_promotion_is_exactly_opt_in() {
        assert!(stats_enabled(Some("true")));
        assert!(stats_enabled(Some(" TRUE ")));
        assert!(!stats_enabled(Some("1")));
        assert!(!stats_enabled(Some("yes")));
        assert!(!stats_enabled(None));
    }

    #[test]
    fn game_list_promotion_is_exactly_opt_in() {
        assert!(game_list_enabled(Some("true")));
        assert!(game_list_enabled(Some(" TRUE ")));
        assert!(!game_list_enabled(Some("1")));
        assert!(!game_list_enabled(Some("yes")));
        assert!(!game_list_enabled(None));
    }

    #[test]
    fn game_scores_promotion_is_exactly_opt_in() {
        assert!(game_scores_enabled(Some("true")));
        assert!(game_scores_enabled(Some(" TRUE ")));
        assert!(!game_scores_enabled(Some("1")));
        assert!(!game_scores_enabled(Some("yes")));
        assert!(!game_scores_enabled(None));
    }

    #[test]
    fn public_command_bundle_is_exactly_opt_in() {
        assert!(public_commands_enabled(Some("true")));
        assert!(public_commands_enabled(Some(" TRUE ")));
        assert!(!public_commands_enabled(Some("1")));
        assert!(!public_commands_enabled(Some("yes")));
        assert!(!public_commands_enabled(None));
    }

    #[test]
    fn rust_command_registration_is_exactly_opt_in() {
        assert!(register_commands_enabled(Some("true")));
        assert!(register_commands_enabled(Some(" TRUE ")));
        assert!(!register_commands_enabled(Some("1")));
        assert!(!register_commands_enabled(Some("yes")));
        assert!(!register_commands_enabled(None));
    }

    #[test]
    fn owner_commands_are_exactly_opt_in() {
        assert!(owner_commands_enabled(Some("true")));
        assert!(owner_commands_enabled(Some(" TRUE ")));
        assert!(!owner_commands_enabled(Some("1")));
        assert!(!owner_commands_enabled(Some("yes")));
        assert!(!owner_commands_enabled(None));
    }

    #[test]
    fn full_mode_never_claims_owner_commands_without_both_identity_guards() {
        let options = OwnerCommandRuntimeOptions {
            owner_id: "123456789012345678".into(),
            owner_guild_id: "234567890123456789".into(),
        };
        assert!(!full_owner_commands_ready(true, true, None));
        assert!(full_owner_commands_ready(true, true, Some(&options)));
        assert!(full_owner_commands_ready(false, true, None));
        assert!(full_owner_commands_ready(true, false, None));
    }

    #[test]
    fn automatic_translation_promotion_is_exactly_opt_in_and_independent_of_other_paths() {
        assert!(automatic_translation_enabled(Some("true")));
        assert!(automatic_translation_enabled(Some(" TRUE ")));
        assert!(!automatic_translation_enabled(Some("1")));
        assert!(!automatic_translation_enabled(Some("yes")));
        assert!(!automatic_translation_enabled(None));
    }

    #[test]
    fn rust_dashboard_promotion_is_exactly_opt_in() {
        assert!(dashboard_enabled(Some("true")));
        assert!(dashboard_enabled(Some(" TRUE ")));
        assert!(!dashboard_enabled(Some("1")));
        assert!(!dashboard_enabled(Some("yes")));
        assert!(!dashboard_enabled(None));
    }

    #[test]
    fn rust_admin_promotion_is_exactly_opt_in() {
        assert!(admin_enabled(Some("true")));
        assert!(admin_enabled(Some(" TRUE ")));
        assert!(!admin_enabled(Some("1")));
        assert!(!admin_enabled(Some("yes")));
        assert!(!admin_enabled(None));
    }

    #[test]
    fn admin_promotion_fails_closed_without_the_premium_http_listener() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let result = build_http_router(
            PathBuf::from("/tmp/vozen-test.sqlite"),
            None,
            None,
            None,
            Some(AdminRuntimeOptions {
                panel_origin: "https://admin.example".into(),
                session_secret: Some("01234567890123456789012345678901".into()),
                owner_id: Some("123456789012345678".into()),
                client_id: Some("123456789012345678".into()),
                web_analytics: None,
            }),
            None,
            HttpRouterRuntimeOptions {
                public_status: None,
                bot_token: String::new(),
            },
            store,
            GatewayState::default(),
            None,
        );
        assert!(matches!(
            result,
            Err(RuntimeError::AdminRequiresPremiumHttp)
        ));
    }

    #[tokio::test]
    async fn kofi_webhook_can_run_without_the_browser_premium_api() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let app = build_http_router(
            PathBuf::from("/tmp/vozen-test.sqlite"),
            Some(PremiumHttpConfig {
                browser_api_enabled: false,
                client_id: None,
                dashboard_client_ids: Vec::new(),
                origin: "https://vozen.org".into(),
                kofi_webhook_token: Some("token".into()),
                kofi_shop_map: None,
                claim_help_webhook_url: None,
                stripe_secret_key: None,
                stripe_publishable_key: None,
                stripe_webhook_secret: None,
                stripe_prices: None,
            }),
            None,
            None,
            None,
            None,
            HttpRouterRuntimeOptions {
                public_status: None,
                bot_token: String::new(),
            },
            store,
            GatewayState::default(),
            None,
        )
        .expect("kofi-only router");
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .expect("response");
        // The route is present even without the browser Premium API; an unsigned
        // webhook request is rejected by the Ko-fi verifier before payload parsing.
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn rust_message_autoread_is_exactly_opt_in() {
        assert!(message_autoread_enabled(Some("true")));
        assert!(message_autoread_enabled(Some(" TRUE ")));
        assert!(!message_autoread_enabled(Some("1")));
        assert!(!message_autoread_enabled(Some("yes")));
        assert!(!message_autoread_enabled(None));
    }

    #[test]
    fn rust_speak_context_is_exactly_opt_in() {
        assert!(speak_context_enabled(Some("true")));
        assert!(speak_context_enabled(Some(" TRUE ")));
        assert!(!speak_context_enabled(Some("1")));
        assert!(!speak_context_enabled(Some("yes")));
        assert!(!speak_context_enabled(None));
    }

    #[test]
    fn rust_translate_context_is_exactly_opt_in() {
        assert!(translation_context_enabled(Some("true")));
        assert!(translation_context_enabled(Some(" TRUE ")));
        assert!(!translation_context_enabled(Some("1")));
        assert!(!translation_context_enabled(Some("yes")));
        assert!(!translation_context_enabled(None));
    }

    #[test]
    fn rust_queue_promotion_is_exactly_opt_in() {
        assert!(queue_enabled(Some("true")));
        assert!(queue_enabled(Some(" TRUE ")));
        assert!(!queue_enabled(Some("1")));
        assert!(!queue_enabled(Some("yes")));
        assert!(!queue_enabled(None));
    }

    #[tokio::test]
    async fn dashboard_options_fail_closed_without_a_ready_gateway_connection() {
        let provider = RuntimeDashboardOptionsProvider::new(
            GatewayState::default(),
            vec!["en_US-amy-medium".into()],
        );
        assert!(matches!(
            provider.options_for_guild("123456789012345678").await,
            Err(DashboardOptionsError::Unavailable)
        ));
    }

    #[test]
    fn rust_voice_numeric_configuration_rejects_dead_or_fractional_queue_limits() {
        assert_eq!(parse_positive_number(None, 20.0, true), Some(20.0));
        assert_eq!(parse_positive_number(Some("2.5"), 20.0, false), Some(2.5));
        assert_eq!(parse_positive_number(Some("2.5"), 20.0, true), None);
        assert_eq!(parse_positive_number(Some("0"), 20.0, true), None);
        assert_eq!(parse_positive_number(Some("wat"), 20.0, true), None);
        assert!(default_piper_concurrency() >= 1);
    }

    #[test]
    fn public_status_fails_closed_until_gateway_ready_and_never_leaks_provider_detail() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let state = GatewayState::default();
        let response = public_status_provider(store, state, Some("  planned\nwork  ".into()))();
        assert_eq!(response.status, vozen_api::PublicStatusState::Unavailable);
        assert_eq!(response.incident.as_deref(), Some("planned work"));
        assert_eq!(
            response.components.bot,
            vozen_api::PublicStatusState::Unavailable
        );
        assert_eq!(
            response.components.database,
            vozen_api::PublicStatusState::Operational
        );
    }

    #[test]
    fn retention_removes_expired_vote_records_delivery_ids_and_pseudonymous_ledger() {
        let store = SqliteStore::open_in_memory().expect("store");
        let secret = "0123456789abcdef0123456789abcdef";
        let user = "12345678901234567";
        store
            .claim_topgg_vote_reward(Some("delivery"), user, 1_000, secret)
            .expect("reward");
        let (rewards, events) = purge_vote_retention(
            &store,
            1_000 + vozen_store::VOTE_REWARD_MS + vozen_store::TOPGG_EVENT_RETENTION_MS + 1,
        )
        .expect("purge");
        // `events` combines the delivery-id deduplication row and the
        // pseudonymous rolling-window ledger row. Both have the same 30-day
        // retention boundary and must disappear together.
        assert_eq!((rewards, events), (1, 2));
        assert!(
            store
                .vote_reward_status(
                    user,
                    secret,
                    1_000 + vozen_store::VOTE_REWARD_MS + vozen_store::TOPGG_EVENT_RETENTION_MS + 1,
                )
                .expect("status")
                .eligible
        );
    }

    #[test]
    fn gcloud_retention_removes_only_months_older_than_the_node_ttl() {
        let store = SqliteStore::open_in_memory().expect("store");
        let now = 1_753_372_800_000_i64; // 2025-07-01T00:00:00Z
        store
            .add_gcloud_monthly_chars(
                vozen_store::GcloudUsageScope::Guild,
                "guild-old",
                "2025-03",
                10,
            )
            .expect("old usage");
        store
            .add_gcloud_monthly_chars(
                vozen_store::GcloudUsageScope::Guild,
                "guild-recent",
                "2025-05",
                20,
            )
            .expect("recent usage");

        let removed = purge_gcloud_retention(&store, now).expect("purge");
        assert_eq!(removed, 1);
        assert_eq!(
            store
                .gcloud_monthly_chars(vozen_store::GcloudUsageScope::Guild, "guild-old", "2025-03",)
                .expect("old lookup"),
            0
        );
        assert_eq!(
            store
                .gcloud_monthly_chars(
                    vozen_store::GcloudUsageScope::Guild,
                    "guild-recent",
                    "2025-05",
                )
                .expect("recent lookup"),
            20
        );
    }

    #[test]
    fn topgg_sync_uses_only_the_public_command_contract() {
        let commands = public_topgg_commands().expect("public commands");
        let names = commands
            .iter()
            .filter_map(|command| command.get("name").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        assert!(names.contains(&"join"));
        assert!(!names.contains(&"vozen-grant"));
        assert!(!names.contains(&"dev"));
    }
}
