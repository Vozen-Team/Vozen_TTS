#![forbid(unsafe_code)]

//! Discord gateway adapter for the Rust migration.
//!
//! This crate owns only connection lifecycle and the minimal intent set. It deliberately does
//! not register commands on startup, start voice sessions, or send user content until those
//! operations have their Node parity contracts and tests.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    sync::{
        Arc, LazyLock, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serenity::{
    async_trait,
    client::{Client, Context, EventHandler},
    gateway::{ActivityData, ConnectionStage, ShardManager, ShardStageUpdateEvent},
    model::{
        gateway::{GatewayIntents, Ready},
        id::ChannelId,
        user::OnlineStatus,
    },
};
use songbird::serenity::SerenityInit;
use thiserror::Error;
use vozen_contracts::{ContractError, DiscordCommandCatalog};
use vozen_core::RuntimeMetrics;

mod attachment_transcription;
mod automatic_translation_service;
mod birthday_command;
mod bot_stats_command;
mod cast;
mod chess_driver;
mod command_registration;
mod command_routing;
mod command_speech_pipeline;
mod config_blockword_command;
mod config_blockword_service;
mod config_channel_command;
mod config_channel_service;
mod config_default_voice_command;
mod config_default_voice_service;
mod config_greet_language_command;
mod config_greet_language_service;
mod config_language_command;
mod config_language_service;
mod config_numeric_command;
mod config_numeric_service;
mod config_queue_role_command;
mod config_queue_role_service;
mod config_reset_command;
mod config_reset_service;
mod config_role_command;
mod config_role_service;
mod config_show_command;
mod config_show_service;
mod config_toggle_command;
mod config_toggle_service;
mod core_voice_command;
mod core_voice_executor;
mod core_voice_interaction;
mod core_voice_response;
mod core_voice_service;
mod dashboard_options;
mod explicit_translation;
mod file_export_command;
mod file_export_service;
mod game_action;
mod game_catalog;
mod game_command;
mod game_content;
mod game_coordinator;
mod game_driver_factory;
mod game_list_command;
mod game_manager;
mod game_play_admission;
mod game_score_command;
mod game_session;
mod gateway_composite;
mod gateway_watch;
mod greeting;
mod guess_language_driver;
mod guild_synthesis_coordinator;
mod hangman_driver;
mod heads_or_tails_coordinator;
mod heads_or_tails_driver;
mod heads_or_tails_session;
mod help_command;
mod interaction_dispatch;
mod invite_command;
mod joke_text;
mod laugh_text;
mod message_admission;
mod message_interaction;
mod message_media;
mod message_pipeline;
mod message_voice_service;
mod microfun_text;
mod numeric_quiz_driver;
mod owner_command;
mod pickup_text;
mod planned_rejoin;
mod premium_command;
mod privacy_command;
mod pronunciation_command;
mod pronunciation_service;
mod queue_command;
mod queue_control;
mod randomizer;
mod redeem_command;
mod reflexes_driver;
mod rejoin_service;
mod roulette_driver;
mod server_stats_command;
mod setup_command;
mod songbird_transport;
mod sound_text;
mod speak_message_command;
mod speech_preparation;
mod speech_telemetry;
mod stats_command;
mod text_quiz_driver;
mod tictactoe_driver;
mod top_speakers_command;
mod transcribe_message_command;
mod transcription_command;
mod translate_message_command;
mod translation_command;
mod translation_preference_command;
mod translation_reaction;
mod uptime_command;
mod utterance_collector;
mod voice_data_cache;
mod voice_display;
mod voice_i18n;
mod voice_playback;
mod voice_preference_command;
mod voice_preference_service;
mod voice_receiver;
mod voice_session;
mod vote_command;
mod vozen_says_driver;
mod word_chain_driver;
mod wordle_driver;

pub use attachment_transcription::{
    AttachmentAdmission, AttachmentRejectReason, AttachmentTranscriptionLimits,
    DiscordAudioAttachment, admit_discord_audio_attachment, bound_transcript_text,
    within_attachment_duration,
};
pub use automatic_translation_service::{
    AutomaticTranslationDelivery, AutomaticTranslationInvocation, AutomaticTranslationOutcome,
    AutomaticTranslationService, MAX_AUTOMATIC_TRANSLATION_IN_FLIGHT,
};
pub use birthday_command::{BirthdayCommand, BirthdayCommandError, parse_birthday_command};
pub use bot_stats_command::{BotStatsCommand, BotStatsCommandError, parse_bot_stats_command};
pub use cast::{
    CAST_LANGUAGE_CHOICES, CAST_MAX_MEMBERS, CAST_THEMES, CAST_WAIT_MS, CastAction, CastAssignment,
    CastEntry, CastLanguage, CastMember, CastSession, CastTheme, assign_cast, build_cast_speech,
    cast_theme_by_key, chunk_cast_speech, parse_cast_component_id,
};
pub use chess_driver::{ChessDriver, ChessDriverAction, ChessGameDriver};
pub use command_registration::{
    CommandRegistrationClient, CommandRegistrationConfig, CommandRegistrationError,
    CommandRegistrationOutcome, DiscordHttpCommandRegistrationClient, register_commands,
};
pub use command_routing::{CommandArea, command_area, route_command};
pub use command_speech_pipeline::{
    CommandSpeechInput, CommandSpeechOutcome, CommandSpeechPipeline,
};
pub use config_blockword_command::{
    ConfigBlockwordAction, ConfigBlockwordCommand, ConfigBlockwordCommandError,
    parse_config_blockword_command,
};
pub use config_blockword_service::{
    ConfigBlockwordFailure, ConfigBlockwordInvocation, ConfigBlockwordOutcome,
    ConfigBlockwordService,
};
pub use config_channel_command::{
    ConfigChannelCommand, ConfigChannelCommandError, parse_config_channel_command,
};
pub use config_channel_service::{
    ConfigChannelFailure, ConfigChannelInvocation, ConfigChannelOutcome, ConfigChannelService,
};
pub use config_default_voice_command::{
    ConfigDefaultVoiceCommand, ConfigDefaultVoiceCommandError, parse_config_default_voice_command,
};
pub use config_default_voice_service::{
    ConfigDefaultVoiceFailure, ConfigDefaultVoiceInvocation, ConfigDefaultVoiceOutcome,
    ConfigDefaultVoiceService, ConfigDefaultVoiceSettings,
};
pub use config_greet_language_command::{
    ConfigGreetLanguageCommand, ConfigGreetLanguageCommandError,
    parse_config_greet_language_command,
};
pub use config_greet_language_service::{
    ConfigGreetLanguageFailure, ConfigGreetLanguageInvocation, ConfigGreetLanguageOutcome,
    ConfigGreetLanguageService,
};
pub use config_language_command::{
    ConfigLanguageCommand, ConfigLanguageCommandError, parse_config_language_command,
};
pub use config_language_service::{
    ConfigLanguageInvocation, ConfigLanguageOutcome, ConfigLanguageService,
};
pub use config_numeric_command::{
    ConfigNumericCommand, ConfigNumericCommandError, ConfigNumericSetting,
    parse_config_numeric_command,
};
pub use config_numeric_service::{
    ConfigNumericFailure, ConfigNumericInvocation, ConfigNumericOutcome, ConfigNumericService,
};
pub use config_queue_role_command::{
    ConfigQueueRoleCommand, ConfigQueueRoleCommandError, ConfigQueueRoleSetting,
    parse_config_queue_role_command,
};
pub use config_queue_role_service::{
    ConfigQueueRoleFailure, ConfigQueueRoleInvocation, ConfigQueueRoleOutcome,
    ConfigQueueRoleService,
};
pub use config_reset_command::{
    ConfigResetCommand, ConfigResetCommandError, parse_config_reset_command,
};
pub use config_reset_service::{
    ConfigResetFailure, ConfigResetInvocation, ConfigResetOutcome, ConfigResetService,
};
pub use config_role_command::{
    ConfigRoleCommand, ConfigRoleCommandError, parse_config_role_command,
};
pub use config_role_service::{
    ConfigRoleFailure, ConfigRoleInvocation, ConfigRoleOutcome, ConfigRoleService,
};
pub use config_show_command::{
    ConfigShowCommand, ConfigShowCommandError, parse_config_show_command,
};
pub use config_show_service::{
    ConfigShowFailure, ConfigShowInvocation, ConfigShowOutcome, ConfigShowService,
};
pub use config_toggle_command::{
    ConfigToggle, ConfigToggleCommand, ConfigToggleCommandError, parse_config_toggle_command,
};
pub use config_toggle_service::{
    ConfigToggleFailure, ConfigToggleInvocation, ConfigToggleOutcome, ConfigToggleService,
};
pub use core_voice_command::{CoreVoiceCommand, CoreVoiceCommandError, parse_promoted_core_voice};
pub use core_voice_executor::{
    CoreVoiceExecutionError, CoreVoiceInteractionExecution, CoreVoiceInteractionExecutor,
};
pub use core_voice_interaction::CoreVoiceInteractionFacts;
pub use core_voice_response::{CoreVoiceResponse, core_voice_response};
pub use core_voice_service::{
    CommandPlaybackError, CommandPlaybackState, CommandSpeechSynthesizer, CommandSynthesisError,
    CommandVoicePlayback, CoreJokeOutcome, CoreJokeResult, CoreMicroFunResult,
    CorePlaybackControlOutcome, CorePreviewOutcome, CoreRizzOutcome, CoreRizzResult,
    CoreSoundOutcome, CoreSoundResult, CoreTtsOutcome, CoreVoiceInvocation, CoreVoiceOutcome,
    CoreVoiceService, CoreVoiceSettings,
};
pub use dashboard_options::{
    DiscordDashboardOption, DiscordDashboardOptions, DiscordDashboardOptionsProvider,
    locale_display_options, voice_display_options,
};
pub use explicit_translation::{
    ExplicitTranslationInvocation, ExplicitTranslationOutcome, ExplicitTranslationProvider,
    ExplicitTranslationService, FREE_GUILD_TRANSLATION_LIMIT, FREE_USER_TRANSLATION_LIMIT,
    PREMIUM_GUILD_TRANSLATION_LIMIT, PREMIUM_USER_TRANSLATION_LIMIT, USER_APP_TRANSLATION_SCOPE,
};
pub use file_export_command::{TtsFileCommand, TtsFileCommandError, parse_tts_file_command};
pub use file_export_service::{
    MAX_TTS_FILE_CHARS, TtsFileExportInvocation, TtsFileExportOutcome, TtsFileExportService,
};
pub use game_action::{
    GameSpeech, GameStanding, RenderedGameAction, RenderedGameSegment, RenderedTextPart,
    render_game_action, render_game_finish,
};
pub use game_catalog::{GAME_CATALOG, GameDefinition, game_by_id};
pub use game_command::{
    GameCommandError, GamePlayCommand, GameStopCommand, parse_game_play_command,
    parse_game_stop_command,
};
pub use game_content::{GameContent, game_content};
pub use game_coordinator::{
    GameCoordinator, GameCoordinatorError, GamePlayRequest, GameStartOutcome,
};
pub use game_driver_factory::{GameDriverFactory, GameFactoryError};
pub use game_list_command::{GameListCommand, GameListCommandError, parse_game_list_command};
pub use game_manager::{
    GameAnnouncementAction, GameDriver, GameDriverAction, GameManager, GameManagerEvent,
    GameMessage,
};
pub use game_play_admission::{
    GamePlayAdmission, GamePlayAdmissionFacts, admit_game_play, game_definition,
};
pub use game_score_command::{GameScoreCommand, GameScoreCommandError, parse_game_score_command};
pub use game_session::{GameScore, GameSession, GameSessionStore, GameStopDenied, StartGameResult};
pub use gateway_composite::CompositeGatewayEventSink;
pub use greeting::{Greeting, build_greeting, is_join_into_channel};
pub use guess_language_driver::{
    GuessLanguageDriver, GuessLanguageDriverAction, GuessLanguageGameDriver,
};
pub use guild_synthesis_coordinator::GuildSynthesisCoordinator;
pub use hangman_driver::{HangmanDriver, HangmanDriverAction, HangmanGameDriver};
pub use heads_or_tails_coordinator::{
    GUESS_WINDOW_MS, HeadsOrTailsAction, HeadsOrTailsCoordinator, NEXT_ROUND_DELAY_MS,
};
pub use heads_or_tails_driver::{
    HeadsOrTailsDriver, HeadsOrTailsDriverAction, HeadsOrTailsGameDriver,
};
pub use heads_or_tails_session::{HeadsOrTailsMessage, HeadsOrTailsSession, HeadsOrTailsStart};
pub use help_command::{HelpCommand, HelpCommandError, parse_help_command};
pub use interaction_dispatch::{
    DispatchOutcome, InteractionDispatchError, InteractionHandler, dispatch_interaction,
};
pub use invite_command::{InviteCommand, InviteCommandError, parse_invite_command};
pub use joke_text::{JOKE_LANGUAGES, JokeLanguage, joke_lang_by_key, pick_joke};
pub use laugh_text::{laughter_for_model, laughter_for_prefix};
pub use message_admission::{DiscordMessageFacts, admit_discord_message, should_attempt_autojoin};
pub(crate) use message_admission::{MessageAdmissionData, admit_discord_message_with_data};
pub use message_interaction::DiscordMessageFactsOwned;
pub use message_media::collect_message_media;
pub use message_pipeline::{MessagePipelineOutcome, MessageSpeechPipeline};
pub use message_voice_service::{MessageVoiceInvocation, MessageVoiceOutcome, MessageVoiceService};
pub use microfun_text::{MicroFunKind, pick_microfun};
pub use numeric_quiz_driver::{
    MathRound, NumericQuizAction, NumericQuizDriver, NumericQuizGameDriver, NumericQuizMode,
};
pub use owner_command::{OwnerCommand, OwnerCommandError, OwnerPlan, parse_owner_command};
pub use pickup_text::{line_counts as pickup_line_counts, pick_line};
pub use planned_rejoin::{
    MAX_PLANNED_REJOIN_AGE, PLANNED_REJOIN_MARKER, PlannedRejoinScope, RejoinChannelState,
    RejoinPlan, consume_planned_rejoin_marker, plan_rejoin, write_planned_rejoin_marker,
};
pub use premium_command::{
    PremiumCommand, PremiumCommandError, parse_premium_command, parse_premium_info_command,
};
pub use privacy_command::{PrivacyCommandError, PrivacyEraseCommand, parse_privacy_erase_command};
pub use pronunciation_command::{
    PronunciationCommand, PronunciationCommandError, PronunciationScope,
    parse_pronunciation_command,
};
pub use pronunciation_service::{
    PronunciationInvocation, PronunciationOutcome, PronunciationService,
};
pub use queue_command::{QueueCommand, QueueCommandError, parse_queue_command};
pub use queue_control::{
    QueueControlInvocation, QueueControlOutcome, QueueControlPlayback, QueueControlService,
};
pub use randomizer::{
    MAX_DIRECT_INPUT_CHARS, MAX_MODAL_OPTIONS, MAX_OPTION_CHARS, MAX_OPTIONS, MIN_OPTIONS,
    RandomizerCommand, RandomizerCommandError, RandomizerInteractionError, RandomizerSession,
    SESSION_TTL_MS, parse_amount_component_id, parse_direct_options, parse_fill_component_id,
    parse_modal_options, parse_randomizer_command, pick_option, randomizer_model,
};
pub use redeem_command::{RedeemCommand, RedeemCommandError, parse_redeem_command};
pub use reflexes_driver::{ReflexesDriver, ReflexesDriverAction, ReflexesGameDriver};
pub use rejoin_service::{PlannedRejoinError, PlannedRejoinOutcome, PlannedRejoinService};
pub use roulette_driver::{RouletteDriverAction, RouletteGameDriver};
pub use server_stats_command::{
    ServerStatsCommand, ServerStatsCommandError, parse_server_stats_command,
};
pub use setup_command::{SetupCommand, SetupCommandError, parse_setup_command};
pub use songbird_transport::SongbirdVoiceSessionTransport;
pub use sound_text::{SOUNDS, SoundClip, sound_by_key, sound_list};
pub use speak_message_command::{
    SPEAK_MESSAGE_COMMAND, SpeakMessageCommand, SpeakMessageCommandError,
    parse_speak_message_command,
};
pub use speech_preparation::{
    MessagePreparationInput, MessagePreparationOutcome, MessageSpeechDraft, PreparedMessageSpeech,
    begin_message_speech, finish_message_speech, gcloud_budget_for, prepare_message_speech,
};
pub(crate) use speech_preparation::{
    VoicePreparationData, begin_message_speech_with_data, finish_message_speech_with_data,
};
pub use stats_command::{StatsCommand, StatsCommandError, parse_stats_command};
pub use text_quiz_driver::{
    TextQuizDriver, TextQuizDriverAction, TextQuizGameDriver, TextQuizMode,
};
pub use tictactoe_driver::{TicTacToeDriver, TicTacToeDriverAction, TicTacToeGameDriver};
pub use top_speakers_command::{
    TopSpeakersCommand, TopSpeakersCommandError, parse_top_speakers_command,
};
pub use transcribe_message_command::{
    TRANSCRIBE_MESSAGE_COMMAND, TranscribeMessageCommand, TranscribeMessageCommandError,
    parse_transcribe_message_command,
};
pub use transcription_command::{
    TranscriptionControlCommand, TranscriptionControlCommandError, TranscriptionSessionCommand,
    parse_transcription_control_command, parse_transcription_session_command,
};
pub use translate_message_command::{
    TRANSLATE_MESSAGE_COMMAND, TranslateMessageCommand, TranslateMessageCommandError,
    parse_translate_message_command,
};
pub use translation_command::{
    TranslatePreviewCommand, TranslatePreviewCommandError, TranslateTextCommand,
    TranslateTextCommandError, TranslationAdminCommand, TranslationAdminCommandError,
    parse_translate_preview_command, parse_translate_text_command, parse_translation_admin_command,
};
pub use translation_preference_command::{
    TranslationPreferenceCommand, TranslationPreferenceCommandError,
    parse_translation_preference_command,
};
pub use translation_reaction::reaction_target_locale;
pub use uptime_command::{UptimeCommand, UptimeCommandError, parse_uptime_command};
pub use utterance_collector::{Utterance, UtteranceCollector};
pub(crate) use voice_data_cache::VoiceDataCache;
pub use voice_display::{VoiceDisplayCatalog, VoiceDisplayError};
pub use voice_i18n::{VoiceResponseLocalizer, VoiceResponseLocalizerError};
pub use voice_playback::{
    SongbirdCommandPlayback, VoicePlaybackError, join_and_enqueue_wav, leave_voice,
};
pub use voice_preference_command::{
    VoicePreferenceCommand, VoicePreferenceCommandError, parse_voice_preference_command,
};
pub use voice_preference_service::{
    VoicePreferenceInvocation, VoicePreferenceOutcome, VoicePreferenceService,
    VoicePreferenceSettings, sanitize_speaker_name,
};
#[cfg(feature = "voice-driver")]
pub use voice_receiver::SongbirdVoiceReceiver;
pub use voice_receiver::{ReceivedUtterance, VoiceReceiver};
pub use voice_session::{
    JoinVoiceOutcome, LeaveVoiceOutcome, VoiceSessionService, VoiceSessionTransport,
    VoiceSessionTransportError,
};
pub use vote_command::{VoteCommand, VoteCommandError, parse_vote_command};
pub use vozen_says_driver::{VozenSaysDriver, VozenSaysDriverAction, VozenSaysGameDriver};
pub use word_chain_driver::{WordChainDriver, WordChainDriverAction, WordChainGameDriver};
pub use wordle_driver::{WordleDriver, WordleDriverAction, WordleGameDriver};

/// Optional event boundary used while command/message paths are promoted from the Node runtime.
/// The gateway itself remains responsible only for Discord connection state; implementations own
/// their response deadline, content handling and error accounting.
#[async_trait]
pub trait GatewayEventSink: Send + Sync {
    /// Runs after this process has received READY and published its transient bot identity.
    /// Implementations must treat it as an availability hook: a failure cannot prevent the
    /// gateway from accepting later events, and reconnects may deliver READY again.
    async fn on_ready(&self, _context: Context) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    /// Runs when Discord reports a Premium App entitlement change. The event carries no
    /// user-controlled message content; sinks that use native monetization should reconcile the
    /// complete entitlement list rather than trusting the single event as an authoritative set.
    async fn on_entitlement_change(
        &self,
        _context: Context,
    ) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    /// Runs when Discord confirms that the bot is present in a guild again. Lifecycle sinks use
    /// this to cancel a pending departure purge; other sinks deliberately ignore the hook.
    async fn on_guild_create(&self, _guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    /// Rich Guild Create hook for slices that need transient channel/locale facts. The legacy ID
    /// hook remains the default so lifecycle sinks can keep their narrow contract.
    async fn on_guild_create_details(
        &self,
        context: Context,
        guild: serenity::model::guild::Guild,
    ) -> Result<(), GatewayEventDispatchError> {
        self.on_guild_create(&guild.id.get().to_string()).await?;
        let _ = context;
        Ok(())
    }

    async fn on_message(
        &self,
        context: Context,
        message: serenity::model::channel::Message,
    ) -> Result<(), GatewayEventDispatchError>;

    /// Runs when a user adds a reaction. Reaction-based slices must re-fetch the target message
    /// and fail closed when its author/content cannot be verified.
    async fn on_reaction_add(
        &self,
        _context: Context,
        _reaction: serenity::model::channel::Reaction,
    ) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    async fn on_interaction(
        &self,
        context: Context,
        interaction: serenity::model::application::Interaction,
    ) -> Result<(), GatewayEventDispatchError>;

    /// Runs after the transient gateway voice map has been updated. Sinks that own a live
    /// session may use this to enforce call-membership and auto-stop policies; other sinks keep
    /// the default no-op so adding the hook does not widen their authority.
    async fn on_voice_state_update(
        &self,
        _context: Context,
        _old: Option<serenity::model::voice::VoiceState>,
        _new: serenity::model::voice::VoiceState,
    ) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    async fn on_guild_delete(&self, guild_id: &str) -> Result<(), GatewayEventDispatchError>;
}

/// A sink error is intentionally content-free. Gateway callbacks discard it after the sink has
/// recorded safe operational context, so a malformed message can never crash or expose text from
/// the shard task.
#[derive(Debug, Error)]
#[error("promoted gateway event handler failed")]
pub struct GatewayEventDispatchError;

/// Live metadata received from Discord's Guild Create event. This is deliberately transient:
/// admin views may display it, but it is never persisted or used as an authorization claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayGuildSnapshot {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub member_count: u64,
    pub joined_timestamp: i64,
}

/// Minimal gateway facts used by the Rust adapters. It intentionally contains neither message
/// content, profiles nor tokens: the only live voice data is a transient guild/user/channel ID
/// mapping required to enforce same-call speech admission without Serenity's global member cache.
#[derive(Clone, Default)]
pub struct GatewayState {
    ready: Arc<AtomicBool>,
    metrics: Arc<RuntimeMetrics>,
    bot_user_id: Arc<RwLock<Option<String>>>,
    guild_ids: Arc<RwLock<BTreeSet<String>>>,
    guild_names: Arc<RwLock<BTreeMap<String, String>>>,
    guild_snapshots: Arc<RwLock<BTreeMap<String, GatewayGuildSnapshot>>>,
    voice_channels: Arc<RwLock<BTreeMap<String, BTreeMap<String, String>>>>,
    voice_bots: Arc<RwLock<BTreeMap<String, BTreeMap<String, bool>>>>,
    shard_stages: Arc<RwLock<BTreeMap<u64, ConnectionStage>>>,
    voice_drops_pending_reconnect: Arc<RwLock<BTreeSet<String>>>,
    /// HTTP is retained after READY only for low-frequency, authorized dashboard option lookups.
    /// It contains no message content or cached guild/member state.
    http: Arc<RwLock<Option<Arc<serenity::http::Http>>>>,
}

fn effective_old_voice_channel_id(
    old: Option<&serenity::model::voice::VoiceState>,
    remembered_channel_id: Option<&str>,
) -> Option<String> {
    old.and_then(|state| state.channel_id.map(|id| id.get().to_string()))
        .or_else(|| remembered_channel_id.map(ToOwned::to_owned))
}

impl GatewayState {
    /// Number of Rust-owned speech items accepted by the playback queue since process start.
    /// This deliberately mirrors the public Node metric's lifecycle (process-local and reset on
    /// restart) without persisting message content or user identifiers.
    pub fn messages_spoken(&self) -> u64 {
        self.metrics.snapshot().messages_spoken
    }

    /// Records a speech item explicitly. The Songbird adapter normally updates this counter from
    /// its `Playable` event; this hook remains useful for deterministic integration tests. The
    /// metric is process-local observability, not a durable usage record.
    pub fn record_message_spoken(&self) {
        self.metrics.record_message_spoken();
    }

    /// Shares the process-local speech counter with the Songbird adapter so it can record a
    /// track only once it becomes playable. The atomic contains no content or identity data.
    pub fn message_counter(&self) -> Arc<AtomicU64> {
        self.metrics.message_counter()
    }

    /// Shares process-local synthesis and gateway observability with runtime adapters.
    pub fn metrics(&self) -> Arc<RuntimeMetrics> {
        self.metrics.clone()
    }

    /// Whether this process received Discord's READY event. The value contains no guild or user
    /// identifiers and is safe to consume in the coarse public status mapper.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn bot_has_guild(&self, guild_id: &str) -> bool {
        self.guild_ids
            .read()
            .is_ok_and(|guild_ids| guild_ids.contains(guild_id))
    }

    pub fn guild_ids(&self) -> Vec<String> {
        self.guild_ids
            .read()
            .map(|guild_ids| guild_ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Current gateway membership count for observability only. It never causes an outbound
    /// guild fetch, so startup can report zero until Discord sends READY.
    pub fn guild_count(&self) -> usize {
        self.guild_ids
            .read()
            .map(|guild_ids| guild_ids.len())
            .unwrap_or(0)
    }

    /// Returns a live gateway-cached name for a guild. This cache is intentionally best-effort:
    /// callers must tolerate `None` until Discord has supplied a Guild Create event, rather than
    /// performing a request or returning a stale persisted name.
    pub fn guild_name(&self, guild_id: &str) -> Option<String> {
        self.guild_names
            .read()
            .ok()
            .and_then(|guild_names| guild_names.get(guild_id).cloned())
    }

    /// Current Guild Create metadata for the admin console. Missing metadata is expected during
    /// the short READY -> Guild Create window and must be treated as unavailable by callers.
    pub fn guild_snapshots(&self) -> Vec<GatewayGuildSnapshot> {
        self.guild_snapshots
            .read()
            .map(|snapshots| snapshots.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Returns Vozen's current voice channel for a guild. It is absent until the READY identity
    /// and a voice state are both known, so message admission always fails closed during startup.
    pub fn bot_voice_channel_id(&self, guild_id: &str) -> Option<String> {
        let bot_user_id = self.bot_user_id.read().ok()?.clone()?;
        self.voice_channel_id(guild_id, &bot_user_id)
    }

    /// Snapshots only Vozen's current guild/channel pairs for a clean shutdown marker. The
    /// result is derived exclusively from this gateway process and never from SQLite, so a
    /// historical row cannot turn a later restart into an unexpected rejoin.
    pub fn bot_voice_sessions(&self) -> Vec<(String, String)> {
        let Some(bot_user_id) = self.bot_user_id() else {
            return Vec::new();
        };
        self.voice_channels
            .read()
            .map(|guilds| {
                guilds
                    .iter()
                    .filter_map(|(guild_id, users)| {
                        users
                            .get(&bot_user_id)
                            .cloned()
                            .map(|channel_id| (guild_id.clone(), channel_id))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Current bot identity received from Discord READY. It is transient process state, never a
    /// persisted credential or application configuration value.
    pub fn bot_user_id(&self) -> Option<String> {
        self.bot_user_id.read().ok()?.clone()
    }

    /// Returns the current voice channel only if the gateway has seen a state for this exact
    /// guild/user pair. Missing state intentionally fails closed in the speech admission layer.
    pub fn voice_channel_id(&self, guild_id: &str, user_id: &str) -> Option<String> {
        self.voice_channels.read().ok().and_then(|guilds| {
            guilds
                .get(guild_id)
                .and_then(|users| users.get(user_id))
                .cloned()
        })
    }

    /// Returns only the transient user IDs currently recorded in a voice channel. The gateway
    /// receives these IDs from Discord voice-state events; names and member roles are deliberately
    /// not retained here and must be resolved separately by an authorized interaction handler.
    pub fn voice_member_ids(&self, guild_id: &str, channel_id: &str) -> Vec<String> {
        self.voice_channels
            .read()
            .ok()
            .and_then(|guilds| guilds.get(guild_id).cloned())
            .map(|users| {
                users
                    .into_iter()
                    .filter_map(|(user_id, current_channel)| {
                        (current_channel == channel_id).then_some(user_id)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Counts non-bot members currently observed in a voice channel. Unknown members are
    /// treated as humans, matching discord.js' count without retaining profiles or content.
    pub fn human_voice_member_count(&self, guild_id: &str, channel_id: &str) -> usize {
        let Ok(guilds) = self.voice_channels.read() else {
            return 0;
        };
        let Some(users) = guilds.get(guild_id) else {
            return 0;
        };
        let bot_flags = self
            .voice_bots
            .read()
            .ok()
            .and_then(|guilds| guilds.get(guild_id).cloned())
            .unwrap_or_default();
        users
            .iter()
            .filter(|(user_id, current_channel)| {
                current_channel.as_str() == channel_id
                    && self.bot_user_id().as_deref() != Some(user_id.as_str())
                    && !bot_flags.get(*user_id).copied().unwrap_or(false)
            })
            .count()
    }

    /// Gives owner-only adapters access to Discord's authenticated HTTP client for a bounded,
    /// low-frequency profile lookup. This does not expose the client on a public HTTP route.
    pub fn discord_http(&self) -> Option<Arc<serenity::http::Http>> {
        self.http.read().ok()?.clone()
    }

    fn replace_guilds(&self, guild_ids: impl IntoIterator<Item = String>) {
        let guild_ids = guild_ids.into_iter().collect::<BTreeSet<_>>();
        if let Ok(mut current) = self.guild_ids.write() {
            *current = guild_ids.clone();
        }
        if let Ok(mut guild_names) = self.guild_names.write() {
            guild_names.retain(|guild_id, _| guild_ids.contains(guild_id));
        }
        if let Ok(mut snapshots) = self.guild_snapshots.write() {
            snapshots.retain(|guild_id, _| guild_ids.contains(guild_id));
        }
    }

    fn remember_bot_user(&self, user_id: String) {
        if let Ok(mut bot_user_id) = self.bot_user_id.write() {
            *bot_user_id = Some(user_id);
        }
    }

    fn mark_ready(&self) {
        let ready = self
            .shard_stages
            .read()
            .map(|stages| {
                stages.is_empty()
                    || stages
                        .values()
                        .all(|stage| matches!(stage, ConnectionStage::Connected))
            })
            .unwrap_or(false);
        self.ready.store(ready, Ordering::Release);
    }

    fn mark_shard_stage(&self, shard_id: u64, stage: ConnectionStage) {
        let all_connected = if let Ok(mut stages) = self.shard_stages.write() {
            stages.insert(shard_id, stage);
            !stages.is_empty()
                && stages
                    .values()
                    .all(|current| matches!(current, ConnectionStage::Connected))
        } else {
            false
        };
        self.ready.store(all_connected, Ordering::Release);
    }

    /// Serenity may remove a runner during restart without delivering another stage event.
    /// Missing known shards must remain unavailable until their live runners reconnect.
    fn reconcile_live_shards(&self, live_stages: BTreeMap<u64, ConnectionStage>) {
        let all_connected = if let Ok(mut stages) = self.shard_stages.write() {
            for (id, stage) in stages.iter_mut() {
                *stage = live_stages
                    .get(id)
                    .copied()
                    .unwrap_or(ConnectionStage::Disconnected);
            }
            stages.extend(live_stages);
            !stages.is_empty()
                && stages
                    .values()
                    .all(|stage| matches!(stage, ConnectionStage::Connected))
        } else {
            false
        };
        self.ready.store(all_connected, Ordering::Release);
    }

    /// Sets only the bot's own transient voice fact. Used by `/join` and `/leave` to close the
    /// gap before Discord sends the subsequent voice-state gateway update.
    pub(crate) fn set_bot_voice_channel(&self, guild_id: &str, channel_id: Option<String>) {
        let Some(bot_user_id) = self.bot_user_id.read().ok().and_then(|id| id.clone()) else {
            return;
        };
        self.update_voice_state(guild_id, &bot_user_id, channel_id);
    }

    fn remember_guild(&self, guild_id: String, guild_name: String) {
        if let Ok(mut current) = self.guild_ids.write() {
            current.insert(guild_id.clone());
        }
        if let Ok(mut guild_names) = self.guild_names.write() {
            guild_names.insert(guild_id, guild_name);
        }
    }

    fn remember_guild_snapshot(&self, snapshot: GatewayGuildSnapshot) {
        self.remember_guild(snapshot.id.clone(), snapshot.name.clone());
        if let Ok(mut snapshots) = self.guild_snapshots.write() {
            snapshots.insert(snapshot.id.clone(), snapshot);
        }
    }

    fn remember_http(&self, http: Arc<serenity::http::Http>) {
        if let Ok(mut current) = self.http.write() {
            *current = Some(http);
        }
    }

    fn replace_guild_voice_states(&self, guild: &serenity::model::guild::Guild) {
        let voice_states = guild
            .voice_states
            .iter()
            .filter_map(|(user_id, state)| {
                state
                    .channel_id
                    .map(|channel_id| (user_id.get().to_string(), channel_id.get().to_string()))
            })
            .collect::<BTreeMap<_, _>>();
        let bot_flags = guild
            .voice_states
            .iter()
            .filter_map(|(user_id, state)| {
                state.channel_id.map(|_| {
                    (
                        user_id.get().to_string(),
                        state.member.as_ref().is_some_and(|member| member.user.bot),
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        if let Ok(mut guilds) = self.voice_channels.write() {
            guilds.insert(guild.id.get().to_string(), voice_states);
        }
        if let Ok(mut guilds) = self.voice_bots.write() {
            guilds.insert(guild.id.get().to_string(), bot_flags);
        }
    }

    fn update_voice_state(&self, guild_id: &str, user_id: &str, channel_id: Option<String>) {
        self.update_voice_state_with_bot(guild_id, user_id, channel_id, false);
    }

    fn update_voice_state_with_bot(
        &self,
        guild_id: &str,
        user_id: &str,
        channel_id: Option<String>,
        member_is_bot: bool,
    ) {
        let is_bot = member_is_bot || self.bot_user_id().as_deref() == Some(user_id);
        if let Ok(mut guilds) = self.voice_channels.write() {
            let users = guilds.entry(guild_id.to_owned()).or_default();
            match channel_id {
                Some(channel_id) => {
                    if is_bot
                        && !users.contains_key(user_id)
                        && self
                            .voice_drops_pending_reconnect
                            .write()
                            .is_ok_and(|mut pending| pending.remove(guild_id))
                    {
                        self.metrics.record_voice_reconnect();
                    }
                    users.insert(user_id.to_owned(), channel_id);
                    if let Ok(mut bot_guilds) = self.voice_bots.write() {
                        bot_guilds
                            .entry(guild_id.to_owned())
                            .or_default()
                            .insert(user_id.to_owned(), is_bot);
                    }
                }
                None => {
                    let was_present = users.remove(user_id).is_some();
                    if is_bot && was_present {
                        if let Ok(mut pending) = self.voice_drops_pending_reconnect.write() {
                            pending.insert(guild_id.to_owned());
                        }
                        self.metrics.record_voice_drop();
                    }
                    if users.is_empty() {
                        guilds.remove(guild_id);
                    }
                    if let Ok(mut bot_guilds) = self.voice_bots.write()
                        && let Some(users) = bot_guilds.get_mut(guild_id)
                    {
                        users.remove(user_id);
                        if users.is_empty() {
                            bot_guilds.remove(guild_id);
                        }
                    }
                }
            }
        }
    }

    fn forget_guild(&self, guild_id: &str) {
        if let Ok(mut current) = self.guild_ids.write() {
            current.remove(guild_id);
        }
        if let Ok(mut guild_names) = self.guild_names.write() {
            guild_names.remove(guild_id);
        }
        if let Ok(mut snapshots) = self.guild_snapshots.write() {
            snapshots.remove(guild_id);
        }
        if let Ok(mut voice_channels) = self.voice_channels.write() {
            voice_channels.remove(guild_id);
        }
        if let Ok(mut voice_bots) = self.voice_bots.write() {
            voice_bots.remove(guild_id);
        }
        if let Ok(mut pending) = self.voice_drops_pending_reconnect.write() {
            pending.remove(guild_id);
        }
    }
}

const DISCORD_COMMANDS: &str = include_str!("../../../contracts/discord-commands.json");

static COMMAND_CATALOG: LazyLock<DiscordCommandCatalog> = LazyLock::new(|| {
    DiscordCommandCatalog::from_json(DISCORD_COMMANDS).expect("valid command contract")
});

/// Exact gateway permissions requested by the current Node bot. `MESSAGE_CONTENT` is the only
/// privileged intent. Member and presence intents must not be added without a new requirement.
pub fn vozen_gateway_intents() -> GatewayIntents {
    GatewayIntents::GUILDS
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::GUILD_MESSAGE_REACTIONS
        | GatewayIntents::MESSAGE_CONTENT
}

/// Matches the Node runtime's subtle brand presence and real onboarding CTA.
pub const DEFAULT_PRESENCE_TEXT: &str = "type it, hear it. • /setup";

/// Extracts only the subcommand/group chain from a Serenity interaction option tree.
/// Leaf argument values are deliberately excluded; their validation belongs to the handler.
pub fn command_path_from_options(
    options: &[serenity::model::application::CommandDataOption],
) -> Vec<&str> {
    use serenity::model::application::CommandDataOptionValue;

    let mut path = Vec::new();
    let mut current = options;
    loop {
        let Some(option) = current.iter().find(|option| {
            matches!(
                option.value,
                CommandDataOptionValue::SubCommand(_) | CommandDataOptionValue::SubCommandGroup(_)
            )
        }) else {
            return path;
        };
        path.push(option.name.as_str());
        current = match &option.value {
            CommandDataOptionValue::SubCommand(options)
            | CommandDataOptionValue::SubCommandGroup(options) => options,
            _ => unreachable!("subcommand selection was matched above"),
        };
    }
}

/// Validates an incoming Discord command against the versioned catalog before dispatch.
/// This has no side effects and is intentionally separate from response/handler code.
pub fn validate_command_interaction(
    command: &serenity::model::application::CommandData,
) -> Result<(), ContractError> {
    let path = command_path_from_options(&command.options);
    COMMAND_CATALOG
        .resolve_command(&command.name, command.kind.into(), &path)
        .map(|_| ())
}

/// Runtime configuration. The token is intentionally private and the type does not implement
/// `Debug`, preventing accidental log exposure.
pub struct DiscordRuntimeConfig {
    token: String,
    presence_text: String,
}

impl DiscordRuntimeConfig {
    pub fn from_environment() -> Result<Self, DiscordRuntimeError> {
        let token = env::var("DISCORD_TOKEN").map_err(|_| DiscordRuntimeError::MissingToken)?;
        Self::from_token(token)
    }

    pub fn from_token(token: String) -> Result<Self, DiscordRuntimeError> {
        if token.trim().is_empty() {
            return Err(DiscordRuntimeError::MissingToken);
        }
        let presence_text = env::var("PRESENCE_TEXT")
            .ok()
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| DEFAULT_PRESENCE_TEXT.to_owned());
        Ok(Self {
            token,
            presence_text,
        })
    }
}

#[derive(Debug, Error)]
pub enum DiscordRuntimeError {
    #[error("DISCORD_TOKEN is required to start the Discord gateway")]
    MissingToken,
    #[error("Discord gateway error: {0}")]
    Serenity(Box<serenity::Error>),
    #[error("Discord gateway remained unavailable beyond the watchdog limit")]
    GatewayWatchdogRestart,
}

/// Starts the Discord gateway using Discord's recommended shard count. Command registration is
/// intentionally a separate future operation: doing REST registration on every gateway restart
/// would consume Discord's global command quota and invalidate client caches.
pub async fn run_discord_gateway(config: DiscordRuntimeConfig) -> Result<(), DiscordRuntimeError> {
    run_discord_gateway_with_state(config, GatewayState::default()).await
}

/// Starts the gateway while keeping only current bot-guild membership synchronized for API
/// authorization and planned call restoration. The caller owns the state handle, so no global
/// cache can outlive the gateway process.
pub async fn run_discord_gateway_with_state(
    config: DiscordRuntimeConfig,
    gateway_state: GatewayState,
) -> Result<(), DiscordRuntimeError> {
    run_discord_gateway_with_state_and_sink(config, gateway_state, None).await
}

/// Starts the gateway with an explicitly promoted event sink. Passing `None` keeps the current
/// shadow-runtime behaviour: state is synchronized, but no message or interaction is consumed.
/// A caller must construct and pass the sink deliberately, so merely compiling a Rust handler
/// cannot race the still-authoritative Node bot on the same Discord application.
pub async fn run_discord_gateway_with_state_and_sink(
    config: DiscordRuntimeConfig,
    gateway_state: GatewayState,
    event_sink: Option<Arc<dyn GatewayEventSink>>,
) -> Result<(), DiscordRuntimeError> {
    let watch_state = gateway_state.clone();
    let mut client = Client::builder(config.token, vozen_gateway_intents())
        // Registers the voice gateway/driver but never joins a call by itself. Join/rejoin
        // policy remains behind a tested command handler in a later migration step.
        .register_songbird()
        .event_handler(VozenGatewayHandler {
            gateway_state,
            event_sink,
            presence_text: config.presence_text,
        })
        .await
        .map_err(|error| DiscordRuntimeError::Serenity(Box::new(error)))?;
    if gateway_watch_enabled() {
        let shard_manager = client.shard_manager.clone();
        tokio::select! {
            result = client.start_autosharded() => {
                result.map_err(|error| DiscordRuntimeError::Serenity(Box::new(error)))?;
            }
            _ = run_gateway_watch(watch_state, shard_manager) => {
                return Err(DiscordRuntimeError::GatewayWatchdogRestart);
            }
        }
    } else {
        client
            .start_autosharded()
            .await
            .map_err(|error| DiscordRuntimeError::Serenity(Box::new(error)))?;
    }
    Ok(())
}

fn gateway_watch_enabled() -> bool {
    env::var("RUST_RUNTIME_MODE")
        .ok()
        .is_some_and(|mode| mode.trim().eq_ignore_ascii_case("full"))
}

fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(i64::MAX))
        .unwrap_or_default()
}

async fn run_gateway_watch(gateway_state: GatewayState, shard_manager: Arc<ShardManager>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(
        gateway_watch::CHECK_INTERVAL_MS as u64,
    ));
    let mut unhealthy_since_ms = None;
    let mut healthy_ticks = 0_u32;
    loop {
        interval.tick().await;
        let live_stages = {
            let runners = shard_manager.runners.lock().await;
            runners
                .iter()
                .map(|(id, runner)| (id.get() as u64, runner.stage))
                .collect()
        };
        gateway_state.reconcile_live_shards(live_stages);
        let decision = gateway_watch::evaluate_gateway(
            gateway_state.is_ready(),
            unhealthy_since_ms,
            system_now_ms(),
            gateway_watch::MAX_DOWN_MS,
        );
        unhealthy_since_ms = decision.unhealthy_since_ms;
        if decision.healthy {
            if healthy_ticks.is_multiple_of(5) {
                eprintln!(
                    "[gateway] healthy: Ready, {} guild(s)",
                    gateway_state.guild_count()
                );
            }
            healthy_ticks = healthy_ticks.saturating_add(1);
            continue;
        }
        healthy_ticks = 0;
        eprintln!(
            "[gateway] NOT-Ready for {}s",
            decision.down_ms.saturating_div(1_000)
        );
        if decision.should_restart {
            eprintln!("[gateway] unavailable beyond watchdog limit; requesting supervisor restart");
            return;
        }
    }
}

struct VozenGatewayHandler {
    gateway_state: GatewayState,
    event_sink: Option<Arc<dyn GatewayEventSink>>,
    presence_text: String,
}

fn log_event_sink_failure(event: &str) {
    // GatewayEventDispatchError is intentionally content-free. Keep the log equally narrow:
    // event type only, never message text, user IDs, guild IDs, or tokens.
    eprintln!("[gateway] promoted event sink failed during {event}; event ignored");
}

fn should_deliver_guild_create_details(is_new: Option<bool>) -> bool {
    is_new == Some(true)
}

#[async_trait]
impl EventHandler for VozenGatewayHandler {
    async fn ready(&self, context: Context, ready: Ready) {
        self.gateway_state.remember_http(context.http.clone());
        self.gateway_state.mark_ready();
        self.gateway_state
            .remember_bot_user(ready.user.id.get().to_string());
        self.gateway_state
            .replace_guilds(ready.guilds.iter().map(|guild| guild.id.get().to_string()));
        context.set_presence(
            Some(ActivityData::listening(self.presence_text.clone())),
            OnlineStatus::Online,
        );
        if let Some(event_sink) = &self.event_sink
            && event_sink.on_ready(context).await.is_err()
        {
            log_event_sink_failure("ready");
        }
    }

    async fn shard_stage_update(&self, _context: Context, event: ShardStageUpdateEvent) {
        self.gateway_state
            .mark_shard_stage(event.shard_id.get() as u64, event.new);
    }

    async fn entitlement_create(
        &self,
        context: Context,
        _entitlement: serenity::model::monetization::Entitlement,
    ) {
        if let Some(event_sink) = &self.event_sink
            && event_sink.on_entitlement_change(context).await.is_err()
        {
            log_event_sink_failure("entitlement_create");
        }
    }

    async fn entitlement_update(
        &self,
        context: Context,
        _entitlement: serenity::model::monetization::Entitlement,
    ) {
        if let Some(event_sink) = &self.event_sink
            && event_sink.on_entitlement_change(context).await.is_err()
        {
            log_event_sink_failure("entitlement_update");
        }
    }

    async fn entitlement_delete(
        &self,
        context: Context,
        _entitlement: serenity::model::monetization::Entitlement,
    ) {
        if let Some(event_sink) = &self.event_sink
            && event_sink.on_entitlement_change(context).await.is_err()
        {
            log_event_sink_failure("entitlement_delete");
        }
    }

    async fn guild_create(
        &self,
        context: Context,
        guild: serenity::model::guild::Guild,
        is_new: Option<bool>,
    ) {
        let guild_id = guild.id.get().to_string();
        self.gateway_state
            .remember_guild_snapshot(GatewayGuildSnapshot {
                id: guild_id.clone(),
                name: guild.name.clone(),
                icon: guild.icon_url(),
                member_count: guild.member_count,
                joined_timestamp: guild.joined_at.unix_timestamp(),
            });
        self.gateway_state.replace_guild_voice_states(&guild);
        if let Some(event_sink) = &self.event_sink {
            let result = if should_deliver_guild_create_details(is_new) {
                event_sink.on_guild_create_details(context, guild).await
            } else {
                event_sink.on_guild_create(&guild_id).await
            };
            if result.is_err() {
                log_event_sink_failure("guild_create");
            }
        }
    }

    async fn guild_delete(
        &self,
        _context: Context,
        incomplete: serenity::model::guild::UnavailableGuild,
        _full: Option<serenity::model::guild::Guild>,
    ) {
        // Discord also emits Guild Delete when a guild is temporarily unavailable. Treat only a
        // real leave/kick as a departure; otherwise a transient outage could schedule a 30-day
        // data purge and clear the Rust voice recovery hint.
        if !should_mark_guild_departed(incomplete.unavailable) {
            return;
        }
        let guild_id = incomplete.id.get().to_string();
        self.gateway_state.forget_guild(&guild_id);
        if let Some(event_sink) = &self.event_sink
            && event_sink.on_guild_delete(&guild_id).await.is_err()
        {
            log_event_sink_failure("guild_delete");
        }
    }

    async fn voice_state_update(
        &self,
        context: Context,
        old: Option<serenity::model::voice::VoiceState>,
        new: serenity::model::voice::VoiceState,
    ) {
        let Some(guild_id) = new.guild_id else {
            return;
        };
        // Serenity's cache can yield `old = None` when it missed the preceding state update.
        // Keep the gateway-owned channel snapshot as a fallback so mute/deaf/stream changes do
        // not look like a fresh join to the greeting handler.
        let remembered_channel_id = self
            .gateway_state
            .voice_channel_id(&guild_id.get().to_string(), &new.user_id.get().to_string());
        self.gateway_state.update_voice_state_with_bot(
            &guild_id.get().to_string(),
            &new.user_id.get().to_string(),
            new.channel_id
                .map(|channel_id| channel_id.get().to_string()),
            new.member.as_ref().is_some_and(|member| member.user.bot),
        );
        let effective_old_channel_id =
            effective_old_voice_channel_id(old.as_ref(), remembered_channel_id.as_deref());
        let old = match old {
            Some(mut state) => {
                if state.channel_id.is_none() {
                    state.channel_id = effective_old_channel_id
                        .as_deref()
                        .and_then(|channel_id| channel_id.parse::<u64>().ok())
                        .map(ChannelId::new);
                }
                Some(state)
            }
            None => effective_old_channel_id
                .as_deref()
                .and_then(|channel_id| channel_id.parse::<u64>().ok())
                .map(|channel_id| {
                    let mut recovered = new.clone();
                    recovered.channel_id = Some(ChannelId::new(channel_id));
                    recovered
                }),
        };
        if let Some(event_sink) = &self.event_sink
            && event_sink
                .on_voice_state_update(context, old, new)
                .await
                .is_err()
        {
            log_event_sink_failure("voice_state_update");
        }
    }

    async fn message(&self, context: Context, message: serenity::model::channel::Message) {
        if let Some(event_sink) = &self.event_sink
            && event_sink.on_message(context, message).await.is_err()
        {
            log_event_sink_failure("message");
        }
    }

    async fn reaction_add(&self, context: Context, reaction: serenity::model::channel::Reaction) {
        if let Some(event_sink) = &self.event_sink
            && event_sink.on_reaction_add(context, reaction).await.is_err()
        {
            log_event_sink_failure("reaction_add");
        }
    }

    async fn interaction_create(
        &self,
        context: Context,
        interaction: serenity::model::application::Interaction,
    ) {
        if let Some(event_sink) = &self.event_sink
            && event_sink
                .on_interaction(context, interaction)
                .await
                .is_err()
        {
            log_event_sink_failure("interaction_create");
        }
    }
}

fn should_mark_guild_departed(unavailable: bool) -> bool {
    !unavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_guild_unavailability_never_starts_departure_retention() {
        assert!(!should_mark_guild_departed(true));
        assert!(should_mark_guild_departed(false));
    }

    #[test]
    fn asks_for_exactly_the_existing_intent_set() {
        let expected = GatewayIntents::GUILDS
            | GatewayIntents::GUILD_VOICE_STATES
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::GUILD_MESSAGE_REACTIONS
            | GatewayIntents::MESSAGE_CONTENT;
        assert_eq!(vozen_gateway_intents(), expected);
        assert!(!vozen_gateway_intents().contains(GatewayIntents::GUILD_MEMBERS));
        assert!(!vozen_gateway_intents().contains(GatewayIntents::GUILD_PRESENCES));
    }

    #[test]
    fn rejects_missing_or_blank_tokens_without_exposing_them() {
        assert!(matches!(
            DiscordRuntimeConfig::from_token("  ".into()),
            Err(DiscordRuntimeError::MissingToken)
        ));
        assert!(DiscordRuntimeConfig::from_token("not-a-real-token".into()).is_ok());
    }

    #[test]
    fn gateway_state_exposes_only_current_bot_guild_membership() {
        let state = GatewayState::default();
        state.remember_bot_user("bot".into());
        state.replace_guilds(["guild-b".into(), "guild-a".into()]);
        assert!(state.bot_has_guild("guild-a"));
        state.remember_guild("guild-c".into(), "Guild C".into());
        state.forget_guild("guild-b");
        assert_eq!(state.guild_ids(), vec!["guild-a", "guild-c"]);
        assert_eq!(state.guild_count(), 2);
        assert_eq!(state.guild_name("guild-c").as_deref(), Some("Guild C"));
        state.remember_guild_snapshot(GatewayGuildSnapshot {
            id: "guild-c".into(),
            name: "Guild C".into(),
            icon: Some("https://cdn.example/icon.webp".into()),
            member_count: 42,
            joined_timestamp: 1_700_000_000,
        });
        assert_eq!(
            state.guild_snapshots(),
            vec![GatewayGuildSnapshot {
                id: "guild-c".into(),
                name: "Guild C".into(),
                icon: Some("https://cdn.example/icon.webp".into()),
                member_count: 42,
                joined_timestamp: 1_700_000_000,
            }]
        );
        assert_eq!(state.guild_name("guild-b"), None);
        assert!(!state.bot_has_guild("guild-b"));
        state.set_bot_voice_channel("guild-c", Some("voice".into()));
        assert_eq!(
            state.bot_voice_channel_id("guild-c").as_deref(),
            Some("voice")
        );
        assert_eq!(
            state.bot_voice_sessions(),
            vec![("guild-c".into(), "voice".into())]
        );
    }

    #[test]
    fn gateway_state_metrics_are_process_local_and_monotonic() {
        let state = GatewayState::default();
        assert_eq!(state.messages_spoken(), 0);
        state.record_message_spoken();
        state.record_message_spoken();
        assert_eq!(state.messages_spoken(), 2);
    }

    #[test]
    fn gateway_readiness_tracks_known_shard_stage_changes() {
        let state = GatewayState::default();
        state.mark_ready();
        assert!(state.is_ready());

        state.mark_shard_stage(0, ConnectionStage::Disconnected);
        assert!(!state.is_ready());
        state.mark_shard_stage(0, ConnectionStage::Connected);
        assert!(state.is_ready());

        state.mark_shard_stage(1, ConnectionStage::Connected);
        state.mark_shard_stage(0, ConnectionStage::Resuming);
        assert!(!state.is_ready());
        state.mark_shard_stage(0, ConnectionStage::Connected);
        assert!(state.is_ready());
    }

    #[test]
    fn live_shard_snapshot_clears_ready_after_a_runner_disappears() {
        let state = GatewayState::default();
        state.mark_shard_stage(0, ConnectionStage::Connected);
        state.reconcile_live_shards(BTreeMap::new());
        assert!(!state.is_ready(), "a cached stage is not a live connection");
    }

    #[test]
    fn live_shard_snapshot_does_not_hide_a_missing_known_shard() {
        let state = GatewayState::default();
        state.mark_shard_stage(0, ConnectionStage::Connected);
        state.mark_shard_stage(1, ConnectionStage::Connected);
        state.reconcile_live_shards(BTreeMap::from([(0, ConnectionStage::Connected)]));
        assert!(
            !state.is_ready(),
            "one connected shard cannot hide another missing runner"
        );

        state.reconcile_live_shards(BTreeMap::from([
            (0, ConnectionStage::Connected),
            (1, ConnectionStage::Connected),
        ]));
        assert!(state.is_ready(), "all known live shards can recover");
    }

    #[test]
    fn live_shard_snapshot_rejects_connecting_and_empty_startup() {
        let state = GatewayState::default();
        state.reconcile_live_shards(BTreeMap::new());
        assert!(!state.is_ready());
        state.reconcile_live_shards(BTreeMap::from([(0, ConnectionStage::Connecting)]));
        assert!(!state.is_ready());
    }

    #[test]
    fn live_shard_snapshot_fails_closed_if_gateway_state_is_poisoned() {
        let state = GatewayState::default();
        state.mark_shard_stage(0, ConnectionStage::Connected);
        let poisoned = state.clone();
        std::thread::spawn(move || {
            let _guard = poisoned.shard_stages.write().expect("state lock");
            panic!("synthetic state failure");
        })
        .join()
        .expect_err("fixture must poison the state lock");
        state.reconcile_live_shards(BTreeMap::from([(0, ConnectionStage::Connected)]));
        assert!(!state.is_ready());
    }

    #[test]
    fn live_shard_snapshot_allows_the_existing_watchdog_to_count_the_outage() {
        let state = GatewayState::default();
        state.mark_shard_stage(0, ConnectionStage::Connected);
        state.reconcile_live_shards(BTreeMap::new());
        let first = gateway_watch::evaluate_gateway(
            state.is_ready(),
            None,
            1_000,
            gateway_watch::MAX_DOWN_MS,
        );
        let expired = gateway_watch::evaluate_gateway(
            state.is_ready(),
            first.unhealthy_since_ms,
            121_001,
            gateway_watch::MAX_DOWN_MS,
        );
        assert!(expired.should_restart);
    }

    #[test]
    fn gateway_state_counts_only_bot_voice_drops_and_reconnects() {
        let state = GatewayState::default();
        state.remember_bot_user("bot".into());
        state.update_voice_state("guild", "human", Some("voice".into()));
        state.update_voice_state("guild", "bot", Some("voice".into()));
        state.update_voice_state("guild", "bot", None);
        assert_eq!(state.metrics().snapshot().voice_drops, 1);
        assert_eq!(state.metrics().snapshot().voice_reconnects, 0);
        state.update_voice_state("guild", "bot", Some("voice".into()));
        let snapshot = state.metrics().snapshot();
        assert_eq!(snapshot.voice_drops, 1);
        assert_eq!(snapshot.voice_reconnects, 1);
    }

    #[test]
    fn gateway_state_removes_transient_voice_state_on_leave_or_guild_delete() {
        let state = GatewayState::default();
        state.update_voice_state("guild", "user", Some("voice".into()));
        assert_eq!(
            state.voice_channel_id("guild", "user"),
            Some("voice".into())
        );
        state.update_voice_state("guild", "user", None);
        assert_eq!(state.voice_channel_id("guild", "user"), None);
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.forget_guild("guild");
        assert_eq!(state.voice_channel_id("guild", "user"), None);
    }

    #[test]
    fn voice_greeting_falls_back_to_the_gateway_channel_when_serenity_old_is_missing() {
        assert_eq!(
            effective_old_voice_channel_id(None, Some("voice")),
            Some("voice".to_owned())
        );
        assert_eq!(effective_old_voice_channel_id(None, None), None);
    }

    #[test]
    fn gateway_state_lists_only_users_in_the_requested_voice_channel() {
        let state = GatewayState::default();
        state.update_voice_state("guild", "alpha", Some("voice-a".into()));
        state.update_voice_state("guild", "beta", Some("voice-b".into()));
        state.update_voice_state("guild", "gamma", Some("voice-a".into()));
        assert_eq!(
            state.voice_member_ids("guild", "voice-a"),
            vec!["alpha".to_owned(), "gamma".to_owned()]
        );
        assert!(state.voice_member_ids("guild", "missing").is_empty());
    }

    #[test]
    fn gateway_state_counts_humans_without_counting_vozen_or_known_bots() {
        let state = GatewayState::default();
        state.remember_bot_user("vozen".into());
        state.update_voice_state("guild", "human", Some("voice".into()));
        state.update_voice_state_with_bot("guild", "other-bot", Some("voice".into()), true);
        state.update_voice_state("guild", "vozen", Some("voice".into()));
        assert_eq!(state.human_voice_member_count("guild", "voice"), 1);
    }

    #[test]
    fn guild_welcome_runs_only_for_a_confirmed_new_guild() {
        assert!(should_deliver_guild_create_details(Some(true)));
        assert!(!should_deliver_guild_create_details(Some(false)));
        assert!(!should_deliver_guild_create_details(None));
    }

    #[test]
    fn extracts_only_the_subcommand_path_from_discord_options() {
        use serenity::model::application::CommandDataOption;

        let options: Vec<CommandDataOption> = serde_json::from_str(
            r#"[{"name":"set","type":1,"options":[{"name":"model","type":3,"value":"en_US-amy-medium"}]}]"#,
        )
        .expect("Discord subcommand payload");
        assert_eq!(command_path_from_options(&options), vec!["set"]);

        let grouped: Vec<CommandDataOption> = serde_json::from_str(
            r#"[{"name":"block-word","type":2,"options":[{"name":"add","type":1,"options":[]}]}]"#,
        )
        .expect("Discord subcommand group payload");
        assert_eq!(
            command_path_from_options(&grouped),
            vec!["block-word", "add"]
        );
    }
}
