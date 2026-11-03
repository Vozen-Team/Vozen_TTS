//! Application service for the first promoted voice commands.
//!
//! This keeps Discord event parsing, durable policy, synthesis and audio transport on separate
//! boundaries. It is deliberately usable with fake implementations: production must prove this
//! same service before an interaction handler is allowed to own `/join`, `/leave` or `/tts`.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use thiserror::Error;
use vozen_core::{
    GuildRateLimiters, QueueEnqueueOptions, QueueLane, QueueSource, RolePolicy, SynthRequest,
    SynthesisEngine, UserSpeechAdmission, admit_user_speech,
};
use vozen_store::{SqliteStore, UserEngine};

use crate::{
    CommandSpeechInput, CommandSpeechOutcome, CommandSpeechPipeline, CoreVoiceCommand,
    GatewayState, GuildSynthesisCoordinator, JoinVoiceOutcome, LeaveVoiceOutcome, MicroFunKind,
    VoiceDestination, VoiceSessionService, VoiceSessionTransport, gcloud_budget_for,
    joke_lang_by_key, laughter_for_model, laughter_for_prefix, pick_joke,
};

#[derive(Debug, Error)]
#[error("speech synthesis failed")]
pub struct CommandSynthesisError;

#[derive(Debug, Error)]
#[error("voice playback failed")]
pub struct CommandPlaybackError;

/// Command handlers only see whether a call exists and whether it is currently speaking; they
/// never inspect another member's queue entries or text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandPlaybackState {
    NoSession,
    Idle,
    Active,
}

/// Synthesizes an already-authorized private request. Implementations must never log the request
/// text or use it as a shell argument.
#[async_trait]
pub trait CommandSpeechSynthesizer: Send + Sync {
    async fn synthesize(
        &self,
        request: &SynthRequest,
    ) -> Result<std::path::PathBuf, CommandSynthesisError>;

    fn provider(&self) -> vozen_store::OperationalProvider {
        vozen_store::OperationalProvider::Internal
    }

    /// Provider is returned with this result, so concurrent requests and fallbacks cannot
    /// overwrite another request's attribution. Unknown adapters remain explicitly internal.
    async fn synthesize_measured(
        &self,
        request: &SynthRequest,
    ) -> Result<
        (std::path::PathBuf, vozen_store::OperationalProvider),
        (CommandSynthesisError, vozen_store::OperationalProvider),
    > {
        self.synthesize(request)
            .await
            .map(|path| (path, self.provider()))
            .map_err(|error| (error, self.provider()))
    }
}

/// Reserves capacity before synthesis and later accepts the immutable WAV in the guild FIFO.
/// This prevents an already-full queue from spending CPU on Piper output that can never play.
/// Implementations must make a successful reservation visible to every concurrent request and
/// must release it when `cancel_reservation` is called.
#[async_trait]
pub trait CommandVoicePlayback: Send + Sync {
    async fn state(&self, guild_id: &str) -> Result<CommandPlaybackState, CommandPlaybackError>;
    async fn reserve(&self, guild_id: &str, lane: QueueLane) -> Result<bool, CommandPlaybackError>;
    async fn enqueue_reserved(
        &self,
        guild_id: &str,
        destination: &VoiceDestination,
        wav: &Path,
        options: QueueEnqueueOptions<'_>,
    ) -> Result<(), CommandPlaybackError>;
    async fn cancel_reservation(
        &self,
        guild_id: &str,
        lane: QueueLane,
    ) -> Result<(), CommandPlaybackError>;
    async fn skip(&self, guild_id: &str) -> Result<(), CommandPlaybackError>;
    async fn silence(&self, guild_id: &str) -> Result<(), CommandPlaybackError>;
}

/// Facts that Discord resolved for an interaction. Role IDs are read only from the current guild
/// member object; `None` preserves the fail-closed role policy if the cache could not resolve it.
pub struct CoreVoiceInvocation<'a> {
    pub guild_id: &'a str,
    pub channel_id: &'a str,
    pub user_id: &'a str,
    pub member_role_ids: Option<&'a [String]>,
    pub resolve_user: &'a (dyn Fn(&str) -> String + Send + Sync),
    pub resolve_channel: &'a (dyn Fn(&str) -> String + Send + Sync),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreTtsOutcome {
    NotInSameVoice,
    Blocked,
    Empty,
    RateLimited,
    FullyBlocked,
    Queued,
    Busy,
    SynthesisFailed,
    PlaybackFailed,
    StoreUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorePreviewOutcome {
    NotInPlayer,
    NotInSameVoice,
    RateLimited,
    Busy,
    UnknownModel,
    Queued,
    SynthesisFailed,
    PlaybackFailed,
    StoreUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreJokeOutcome {
    NotInPlayer,
    NotInSameVoice,
    UnknownLanguage,
    PremiumLocked,
    RateLimited,
    Busy,
    Queued,
    SynthesisFailed,
    PlaybackFailed,
    StoreUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreRizzOutcome {
    PremiumLocked,
    NotInPlayer,
    NotInSameVoice,
    UnknownLanguage,
    RateLimited,
    Busy,
    Queued,
    SynthesisFailed,
    PlaybackFailed,
    StoreUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreJokeResult {
    pub outcome: CoreJokeOutcome,
    pub joke: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreRizzResult {
    pub outcome: CoreRizzOutcome,
    pub line: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreSoundOutcome {
    Disabled,
    List,
    Unknown,
    NotInVoice,
    NotInSameVoice,
    RateLimited,
    Busy,
    Queued,
    SynthesisFailed,
    PlaybackFailed,
    StoreUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreSoundResult {
    pub outcome: CoreSoundOutcome,
    pub name: Option<String>,
    pub sounds: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreMicroFunResult {
    pub kind: MicroFunKind,
    pub question: Option<String>,
    pub text: String,
    pub queued: bool,
    pub premium_locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreVoiceOutcome {
    Joined(JoinVoiceOutcome),
    Left(LeaveVoiceOutcome),
    Laugh(CorePreviewOutcome),
    Joke(CoreJokeResult),
    Rizz(CoreRizzResult),
    Sound(CoreSoundResult),
    MicroFun(CoreMicroFunResult),
    Tts(CoreTtsOutcome),
    Preview(CorePreviewOutcome),
    Skipped(CorePlaybackControlOutcome),
    Silenced(CorePlaybackControlOutcome),
    /// A contract-valid but not-yet-promoted command must remain with the Node runtime.
    NotPromoted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorePlaybackControlOutcome {
    NotInVoice,
    NothingPlaying,
    Completed,
    PlaybackFailed,
}

/// Operational configuration selected at Rust process startup. These values are trusted
/// environment/config values, never supplied by Discord users.
#[derive(Debug, Clone)]
pub struct CoreVoiceSettings {
    pub available_models: Vec<String>,
    pub default_voice: String,
    pub default_speed: f64,
    /// Concrete route used for legacy `google` voice preferences.
    pub default_engine: SynthesisEngine,
}

pub struct CoreVoiceService<T, S, P> {
    store: Arc<Mutex<SqliteStore>>,
    gateway_state: GatewayState,
    sessions: VoiceSessionService<T>,
    speech: Mutex<CommandSpeechPipeline>,
    preview_limiters: Mutex<GuildRateLimiters>,
    synthesizer: S,
    playback: P,
    synthesis: GuildSynthesisCoordinator,
    settings: CoreVoiceSettings,
    now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl<T, S, P> CoreVoiceService<T, S, P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        gateway_state: GatewayState,
        transport: T,
        synthesizer: S,
        playback: P,
        settings: CoreVoiceSettings,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self::new_with_synthesis_coordinator(
            store,
            gateway_state,
            transport,
            synthesizer,
            playback,
            GuildSynthesisCoordinator::default(),
            settings,
            now_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_synthesis_coordinator(
        store: Arc<Mutex<SqliteStore>>,
        gateway_state: GatewayState,
        transport: T,
        synthesizer: S,
        playback: P,
        synthesis: GuildSynthesisCoordinator,
        settings: CoreVoiceSettings,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        let sessions = VoiceSessionService::new(store.clone(), gateway_state.clone(), transport);
        Self {
            store,
            gateway_state,
            sessions,
            speech: Mutex::new(CommandSpeechPipeline::default()),
            preview_limiters: Mutex::new(GuildRateLimiters::default()),
            synthesizer,
            playback,
            synthesis,
            settings,
            now_ms,
        }
    }

    fn paid_budget(
        &self,
        guild_id: &str,
        user_id: &str,
        engine: SynthesisEngine,
        now_ms: i64,
    ) -> Option<vozen_core::GcloudBudget> {
        self.store
            .lock()
            .ok()
            .and_then(|store| gcloud_budget_for(&store, guild_id, user_id, engine, now_ms))
    }

    /// Resolves the same saved/guild/runtime model precedence used by `/voice preview`.
    /// The interaction layer uses this before rendering the sample phrase so its locale follows
    /// the voice that will actually be synthesized, rather than the Discord UI locale.
    pub fn preview_model(&self, guild_id: &str, user_id: &str) -> Option<String> {
        let store = self.store.lock().ok()?;
        let config = store.guild_config(guild_id).ok()?;
        let stored = store.get_user_voice(guild_id, user_id).ok().flatten();

        stored
            .as_ref()
            .map(|voice| voice.model.clone())
            .filter(|model| !model.trim().is_empty())
            .or_else(|| {
                (!config.default_voice.trim().is_empty()).then(|| config.default_voice.clone())
            })
            .or_else(|| {
                (!self.settings.default_voice.trim().is_empty())
                    .then(|| self.settings.default_voice.clone())
            })
    }
}

impl<T, S, P> CoreVoiceService<T, S, P>
where
    T: VoiceSessionTransport,
    S: CommandSpeechSynthesizer,
    P: CommandVoicePlayback,
{
    /// Executes only commands that have a complete, safe Rust path. `NotPromoted` is intentional
    /// during shadow migration: the gateway must leave such commands to Node rather than render
    /// an incorrect response or lose an interaction.
    pub async fn execute(
        &self,
        invocation: CoreVoiceInvocation<'_>,
        command: &CoreVoiceCommand,
    ) -> CoreVoiceOutcome {
        match command {
            CoreVoiceCommand::Join => CoreVoiceOutcome::Joined(
                self.sessions
                    .join_for_user(invocation.guild_id, invocation.user_id, (self.now_ms)())
                    .await,
            ),
            CoreVoiceCommand::Leave => {
                CoreVoiceOutcome::Left(self.sessions.leave_explicitly(invocation.guild_id).await)
            }
            CoreVoiceCommand::Laugh => {
                CoreVoiceOutcome::Laugh(self.execute_laugh(invocation).await)
            }
            CoreVoiceCommand::Joke {
                language,
                laughter,
                engine,
            } => CoreVoiceOutcome::Joke(
                self.execute_joke(invocation, language, *laughter, engine.as_deref())
                    .await,
            ),
            CoreVoiceCommand::Rizz {
                language,
                sound,
                engine,
            } => CoreVoiceOutcome::Rizz(
                self.execute_rizz(invocation, language, *sound, engine.as_deref())
                    .await,
            ),
            CoreVoiceCommand::Sound { name } => {
                CoreVoiceOutcome::Sound(self.execute_sound(invocation, name.as_deref()).await)
            }
            CoreVoiceCommand::MicroFun {
                kind,
                question,
                language,
                engine,
            } => CoreVoiceOutcome::MicroFun(
                self.execute_microfun(
                    invocation,
                    *kind,
                    question.clone(),
                    language.as_deref(),
                    engine.as_deref(),
                )
                .await,
            ),
            CoreVoiceCommand::Skip => {
                CoreVoiceOutcome::Skipped(self.skip(invocation.guild_id).await)
            }
            CoreVoiceCommand::ShutUp => {
                CoreVoiceOutcome::Silenced(self.silence(invocation.guild_id).await)
            }
            CoreVoiceCommand::Tts { text } => {
                CoreVoiceOutcome::Tts(self.execute_tts(invocation, text).await)
            }
            CoreVoiceCommand::VoicePreview { model, engine } => CoreVoiceOutcome::Preview(
                self.execute_preview(
                    invocation,
                    model.as_deref(),
                    engine.as_deref(),
                    "Hi, I'm Vozen. type it, hear it.",
                )
                .await,
            ),
        }
    }

    /// Enqueues a runtime-generated line after applying the same cleaning, role, same-call and
    /// rate-limit gates as `/tts`. The caller may select only an already validated model/engine;
    /// this method does not trust Discord option values as a model catalogue.
    pub async fn execute_custom_speech(
        &self,
        invocation: CoreVoiceInvocation<'_>,
        text: &str,
        model: &str,
        speed: f64,
        engine: SynthesisEngine,
        enforce_rate_limit: bool,
    ) -> CoreTtsOutcome {
        let destination = self
            .gateway_state
            .bot_voice_destination(invocation.guild_id);
        let roles = invocation
            .member_role_ids
            .map(|roles| roles.iter().map(String::as_str).collect::<Vec<_>>());
        let prepared = {
            let store = match self.store.lock() {
                Ok(store) => store,
                Err(_) => return CoreTtsOutcome::StoreUnavailable,
            };
            let mut speech = match self.speech.lock() {
                Ok(speech) => speech,
                Err(_) => return CoreTtsOutcome::StoreUnavailable,
            };
            speech.prepare_with_rate_limit(
                &store,
                CommandSpeechInput {
                    guild_id: invocation.guild_id,
                    channel_id: invocation.channel_id,
                    user_id: invocation.user_id,
                    raw: text,
                    caller_voice_channel_id: self
                        .gateway_state
                        .voice_channel_id(invocation.guild_id, invocation.user_id)
                        .as_deref(),
                    bot_voice_channel_id: destination.as_ref().map(VoiceDestination::channel_id),
                    member_role_ids: roles.as_deref(),
                    available_models: &self.settings.available_models,
                    runtime_default_voice: &self.settings.default_voice,
                    runtime_default_speed: self.settings.default_speed,
                    runtime_default_engine: self.settings.default_engine,
                    detected_language: None,
                    resolve_user: invocation.resolve_user,
                    resolve_channel: invocation.resolve_channel,
                },
                (self.now_ms)(),
                enforce_rate_limit,
            )
        };
        let (lane, mut request) = match prepared {
            Ok(CommandSpeechOutcome::Ready { lane, speech }) => (lane, speech.request),
            Ok(CommandSpeechOutcome::NotInSameVoice) => return CoreTtsOutcome::NotInSameVoice,
            Ok(CommandSpeechOutcome::Blocked) => return CoreTtsOutcome::Blocked,
            Ok(CommandSpeechOutcome::Empty) => return CoreTtsOutcome::Empty,
            Ok(CommandSpeechOutcome::RateLimited) => return CoreTtsOutcome::RateLimited,
            Ok(CommandSpeechOutcome::FullyBlocked) => return CoreTtsOutcome::FullyBlocked,
            Err(_) => return CoreTtsOutcome::StoreUnavailable,
        };
        request.model = model.to_owned();
        request.speed = if speed.is_finite() {
            speed
        } else {
            self.settings.default_speed
        };
        request.engine = engine;
        request.gcloud_budget = self.paid_budget(
            invocation.guild_id,
            invocation.user_id,
            engine,
            (self.now_ms)(),
        );
        request.segments = None;
        request.single_voice = Some(true);
        match self
            .enqueue_synth_request(
                invocation.guild_id,
                invocation.user_id,
                destination.as_ref(),
                lane,
                request,
            )
            .await
        {
            CorePreviewOutcome::Queued => CoreTtsOutcome::Queued,
            CorePreviewOutcome::Busy => CoreTtsOutcome::Busy,
            CorePreviewOutcome::SynthesisFailed => CoreTtsOutcome::SynthesisFailed,
            CorePreviewOutcome::PlaybackFailed => CoreTtsOutcome::PlaybackFailed,
            _ => CoreTtsOutcome::PlaybackFailed,
        }
    }

    pub async fn join_for_message(
        &self,
        invocation: CoreVoiceInvocation<'_>,
        expected_channel: &str,
    ) -> JoinVoiceOutcome {
        self.sessions
            .join_for_message(
                invocation.guild_id,
                invocation.user_id,
                expected_channel,
                (self.now_ms)(),
            )
            .await
    }

    /// Leaves a call from a gateway lifecycle rule (for example, when no humans remain).
    /// It uses the same durable-marker cleanup as `/leave`, so an automatic departure cannot
    /// unexpectedly turn into a silent rejoin after restart.
    pub async fn leave_for_lifecycle(&self, guild_id: &str) -> LeaveVoiceOutcome {
        self.sessions.leave_explicitly(guild_id).await
    }

    /// Executes a deliberately selected voice sample. The gateway supplies the localized sample
    /// phrase; this keeps the semantic service independent from Discord's locale catalog while
    /// preserving the Node precedence of explicit model, saved model, guild default and runtime
    /// default.
    pub async fn execute_preview(
        &self,
        invocation: CoreVoiceInvocation<'_>,
        explicit_model: Option<&str>,
        explicit_engine: Option<&str>,
        sample: &str,
    ) -> CorePreviewOutcome {
        let destination = self
            .gateway_state
            .bot_voice_destination(invocation.guild_id);
        if let Some(model) = explicit_model
            && !self
                .settings
                .available_models
                .iter()
                .any(|available| available == model)
        {
            return CorePreviewOutcome::UnknownModel;
        }

        let playback_state = match self.playback.state(invocation.guild_id).await {
            Ok(state) => state,
            Err(_) => return CorePreviewOutcome::NotInPlayer,
        };
        if matches!(playback_state, CommandPlaybackState::NoSession) {
            return CorePreviewOutcome::NotInPlayer;
        }

        let roles = invocation
            .member_role_ids
            .map(|roles| roles.iter().map(String::as_str).collect::<Vec<_>>());
        let (model, speed, engine, lane) = {
            let store = match self.store.lock() {
                Ok(store) => store,
                Err(_) => return CorePreviewOutcome::StoreUnavailable,
            };
            let config = match store.guild_config(invocation.guild_id) {
                Ok(config) => config,
                Err(_) => return CorePreviewOutcome::StoreUnavailable,
            };
            let policy = RolePolicy {
                priority_role_id: config.priority_role_id.as_deref(),
                blocked_role_id: config.blocked_role_id.as_deref(),
            };
            let lane = match admit_user_speech(
                self.gateway_state
                    .voice_channel_id(invocation.guild_id, invocation.user_id)
                    .as_deref(),
                destination.as_ref().map(VoiceDestination::channel_id),
                roles.as_deref(),
                policy,
            ) {
                UserSpeechAdmission::Allowed { lane } => lane,
                // Node intentionally uses the same not-in-voice response for blocked preview
                // requests; do not expose role-policy state through this sample command.
                UserSpeechAdmission::Denied { .. } => {
                    return CorePreviewOutcome::NotInSameVoice;
                }
            };
            let allowed = match self.preview_limiters.lock() {
                Ok(mut limiters) => limiters.allow(
                    invocation.guild_id,
                    invocation.user_id,
                    config.rate_per_min,
                    (self.now_ms)(),
                ),
                Err(_) => return CorePreviewOutcome::StoreUnavailable,
            };
            if !allowed {
                return CorePreviewOutcome::RateLimited;
            }

            let stored = match store.get_user_voice(invocation.guild_id, invocation.user_id) {
                Ok(stored) => stored,
                Err(_) => return CorePreviewOutcome::StoreUnavailable,
            };
            let model = explicit_model
                .map(str::to_owned)
                .or_else(|| {
                    stored
                        .as_ref()
                        .map(|voice| voice.model.clone())
                        .filter(|model| !model.trim().is_empty())
                })
                .or_else(|| {
                    (!config.default_voice.trim().is_empty()).then(|| config.default_voice.clone())
                })
                .unwrap_or_else(|| self.settings.default_voice.clone());
            let speed = stored
                .as_ref()
                .map(|voice| voice.speed)
                .filter(|speed| speed.is_finite())
                .unwrap_or(self.settings.default_speed);
            let engine = resolve_preview_engine(
                &store,
                invocation.guild_id,
                invocation.user_id,
                stored.map(|voice| voice.engine),
                explicit_engine,
                (self.now_ms)(),
            );
            (model, speed, engine, lane)
        };

        let Some(destination) = destination.as_ref() else {
            return CorePreviewOutcome::PlaybackFailed;
        };
        let admitted_generation = self.synthesis.admission_generation(invocation.guild_id);
        let mut synthesis = self
            .synthesis
            .acquire(invocation.guild_id, admitted_generation)
            .await;
        if synthesis.was_cleared() || !destination.is_current() {
            return CorePreviewOutcome::PlaybackFailed;
        }
        synthesis.activate();
        match self.playback.reserve(invocation.guild_id, lane).await {
            Ok(true) => {}
            Ok(false) => return CorePreviewOutcome::Busy,
            Err(_) => return CorePreviewOutcome::PlaybackFailed,
        }
        let request = SynthRequest {
            text: sample.to_owned(),
            model,
            asset_path: None,
            speed,
            engine,
            gcloud_budget: self.paid_budget(
                invocation.guild_id,
                invocation.user_id,
                engine,
                (self.now_ms)(),
            ),
            segments: None,
            single_voice: Some(true),
            emphasis_source: None,
            lead_silence_ms: 0,
        };
        if synthesis.cancelled() || !destination.is_current() {
            let _ = self
                .playback
                .cancel_reservation(invocation.guild_id, lane)
                .await;
            return CorePreviewOutcome::PlaybackFailed;
        }
        let wav = match self.synthesizer.synthesize(&request).await {
            Ok(wav) => wav,
            Err(_) => {
                let _ = self
                    .playback
                    .cancel_reservation(invocation.guild_id, lane)
                    .await;
                return CorePreviewOutcome::SynthesisFailed;
            }
        };
        if synthesis.cancelled() || !destination.is_current() {
            let _ = self
                .playback
                .cancel_reservation(invocation.guild_id, lane)
                .await;
            return CorePreviewOutcome::PlaybackFailed;
        }
        match self
            .playback
            .enqueue_reserved(
                invocation.guild_id,
                destination,
                &wav,
                QueueEnqueueOptions {
                    author_id: Some(invocation.user_id),
                    source: QueueSource::Command,
                    lane,
                    created_at_ms: (self.now_ms)().max(0) as u64,
                },
            )
            .await
        {
            Ok(()) => CorePreviewOutcome::Queued,
            Err(_) => {
                let _ = self
                    .playback
                    .cancel_reservation(invocation.guild_id, lane)
                    .await;
                CorePreviewOutcome::PlaybackFailed
            }
        }
    }

    /// `/laugh` keeps the Node precedence (saved model, guild default, runtime default) but
    /// chooses laughter in the same script as that model before entering the shared preview
    /// admission, synthesis and queue path.
    async fn execute_laugh(&self, invocation: CoreVoiceInvocation<'_>) -> CorePreviewOutcome {
        let model = {
            let store = match self.store.lock() {
                Ok(store) => store,
                Err(_) => return CorePreviewOutcome::StoreUnavailable,
            };
            let config = match store.guild_config(invocation.guild_id) {
                Ok(config) => config,
                Err(_) => return CorePreviewOutcome::StoreUnavailable,
            };
            let stored = match store.get_user_voice(invocation.guild_id, invocation.user_id) {
                Ok(stored) => stored,
                Err(_) => return CorePreviewOutcome::StoreUnavailable,
            };
            stored
                .as_ref()
                .map(|voice| voice.model.clone())
                .filter(|model| !model.trim().is_empty())
                .or_else(|| {
                    (!config.default_voice.trim().is_empty()).then(|| config.default_voice.clone())
                })
                .unwrap_or_else(|| self.settings.default_voice.clone())
        };
        let sample = laughter_for_model(&model);
        self.execute_preview(invocation, Some(&model), None, &sample)
            .await
    }

    /// Micro-fun commands always produce their public text answer. When a player exists, they
    /// additionally use the same same-call, role and rate-limit gates as explicit speech and
    /// queue the answer in the selected language. A missing/unauthorized call therefore never
    /// turns a useful text command into an error.
    async fn execute_microfun(
        &self,
        invocation: CoreVoiceInvocation<'_>,
        kind: MicroFunKind,
        question: Option<String>,
        language: Option<&str>,
        explicit_engine: Option<&str>,
    ) -> CoreMicroFunResult {
        let destination = self
            .gateway_state
            .bot_voice_destination(invocation.guild_id);
        let locale = self
            .store
            .lock()
            .ok()
            .and_then(|store| store.guild_config(invocation.guild_id).ok())
            .map(|config| config.locale)
            .unwrap_or_else(|| "en".to_owned());
        let spoken_language = language.unwrap_or(&locale);
        let text = crate::pick_microfun(kind, spoken_language, (self.now_ms)());
        let mut result = CoreMicroFunResult {
            kind,
            question,
            text,
            queued: false,
            premium_locked: false,
        };

        // An explicit Kokoro choice is an entitlement request, not a silent fallback.
        // Keep the behavior identical to /joke and /rizz: free users get a clear Premium
        // response and no audio is queued.
        if explicit_engine.is_some_and(|value| value.trim().eq_ignore_ascii_case("kokoro")) {
            let premium = self
                .store
                .lock()
                .ok()
                .and_then(|store| {
                    store
                        .is_user_premium(invocation.user_id, (self.now_ms)())
                        .ok()
                        .and_then(|user| {
                            store
                                .is_guild_premium(invocation.guild_id, (self.now_ms)())
                                .ok()
                                .map(|guild| user || guild)
                        })
                })
                .unwrap_or(false);
            if !premium {
                result.premium_locked = true;
                return result;
            }
        }

        let Ok(CommandPlaybackState::Active | CommandPlaybackState::Idle) =
            self.playback.state(invocation.guild_id).await
        else {
            return result;
        };
        let roles = invocation
            .member_role_ids
            .map(|roles| roles.iter().map(String::as_str).collect::<Vec<_>>());
        let (model, speed, engine, lane) = {
            let Ok(store) = self.store.lock() else {
                return result;
            };
            let Ok(config) = store.guild_config(invocation.guild_id) else {
                return result;
            };
            let policy = RolePolicy {
                priority_role_id: config.priority_role_id.as_deref(),
                blocked_role_id: config.blocked_role_id.as_deref(),
            };
            let lane = match admit_user_speech(
                self.gateway_state
                    .voice_channel_id(invocation.guild_id, invocation.user_id)
                    .as_deref(),
                destination.as_ref().map(VoiceDestination::channel_id),
                roles.as_deref(),
                policy,
            ) {
                UserSpeechAdmission::Allowed { lane } => lane,
                UserSpeechAdmission::Denied { .. } => return result,
            };
            let Ok(mut limiters) = self.preview_limiters.lock() else {
                return result;
            };
            if !limiters.allow(
                invocation.guild_id,
                invocation.user_id,
                config.rate_per_min,
                (self.now_ms)(),
            ) {
                return result;
            }
            let stored = store
                .get_user_voice(invocation.guild_id, invocation.user_id)
                .ok()
                .flatten();
            let prefix = joke_lang_by_key(spoken_language)
                .map(|info| info.prefix)
                .unwrap_or_else(|| {
                    if spoken_language.starts_with("pt") {
                        "pt_"
                    } else {
                        "en_"
                    }
                });
            let model = self
                .settings
                .available_models
                .iter()
                .find(|model| model.starts_with(prefix))
                .cloned()
                .or_else(|| {
                    (!config.default_voice.trim().is_empty()).then(|| config.default_voice.clone())
                })
                .unwrap_or_else(|| {
                    if self.settings.default_voice.trim().is_empty() {
                        "en_US-amy-medium".to_owned()
                    } else {
                        self.settings.default_voice.clone()
                    }
                });
            let selected_engine = explicit_engine
                .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
                    "google" => Some(UserEngine::Google),
                    "piper" => Some(UserEngine::Piper),
                    "kokoro" => Some(UserEngine::Kokoro),
                    _ => None,
                })
                .or_else(|| stored.as_ref().map(|voice| voice.engine));
            let engine = resolve_preview_engine(
                &store,
                invocation.guild_id,
                invocation.user_id,
                selected_engine,
                None,
                (self.now_ms)(),
            );
            (model, self.settings.default_speed, engine, lane)
        };
        let request = SynthRequest {
            text: result.text.clone(),
            model,
            asset_path: None,
            speed,
            engine,
            gcloud_budget: self.paid_budget(
                invocation.guild_id,
                invocation.user_id,
                engine,
                (self.now_ms)(),
            ),
            segments: None,
            single_voice: Some(true),
            emphasis_source: None,
            lead_silence_ms: 0,
        };
        result.queued = matches!(
            self.enqueue_synth_request(
                invocation.guild_id,
                invocation.user_id,
                destination.as_ref(),
                lane,
                request
            )
            .await,
            CorePreviewOutcome::Queued
        );
        result
    }

    /// `/rizz` keeps the Node order: Premium gate, live player, same-call admission, language
    /// validation, rate limit, then line synthesis and an optional best-effort WAV effect.
    async fn execute_rizz(
        &self,
        invocation: CoreVoiceInvocation<'_>,
        language: &str,
        sound: bool,
        explicit_engine: Option<&str>,
    ) -> CoreRizzResult {
        let destination = self
            .gateway_state
            .bot_voice_destination(invocation.guild_id);
        let now = (self.now_ms)();
        let premium = {
            let Ok(store) = self.store.lock() else {
                return CoreRizzResult {
                    outcome: CoreRizzOutcome::StoreUnavailable,
                    line: None,
                };
            };
            match store
                .is_user_premium(invocation.user_id, now)
                .and_then(|user| {
                    store
                        .is_guild_premium(invocation.guild_id, now)
                        .map(|guild| user || guild)
                }) {
                Ok(premium) => premium,
                Err(_) => {
                    return CoreRizzResult {
                        outcome: CoreRizzOutcome::StoreUnavailable,
                        line: None,
                    };
                }
            }
        };
        if !premium {
            return CoreRizzResult {
                outcome: CoreRizzOutcome::PremiumLocked,
                line: None,
            };
        }
        let Ok(CommandPlaybackState::Active | CommandPlaybackState::Idle) =
            self.playback.state(invocation.guild_id).await
        else {
            return CoreRizzResult {
                outcome: CoreRizzOutcome::NotInPlayer,
                line: None,
            };
        };
        let Some(language_info) = joke_lang_by_key(language) else {
            return CoreRizzResult {
                outcome: CoreRizzOutcome::UnknownLanguage,
                line: None,
            };
        };
        let roles = invocation
            .member_role_ids
            .map(|roles| roles.iter().map(String::as_str).collect::<Vec<_>>());
        let (model, speed, engine, lane) = {
            let Ok(store) = self.store.lock() else {
                return CoreRizzResult {
                    outcome: CoreRizzOutcome::StoreUnavailable,
                    line: None,
                };
            };
            let Ok(config) = store.guild_config(invocation.guild_id) else {
                return CoreRizzResult {
                    outcome: CoreRizzOutcome::StoreUnavailable,
                    line: None,
                };
            };
            let policy = RolePolicy {
                priority_role_id: config.priority_role_id.as_deref(),
                blocked_role_id: config.blocked_role_id.as_deref(),
            };
            let lane = match admit_user_speech(
                self.gateway_state
                    .voice_channel_id(invocation.guild_id, invocation.user_id)
                    .as_deref(),
                destination.as_ref().map(VoiceDestination::channel_id),
                roles.as_deref(),
                policy,
            ) {
                UserSpeechAdmission::Allowed { lane } => lane,
                UserSpeechAdmission::Denied { .. } => {
                    return CoreRizzResult {
                        outcome: CoreRizzOutcome::NotInSameVoice,
                        line: None,
                    };
                }
            };
            let Ok(mut limiters) = self.preview_limiters.lock() else {
                return CoreRizzResult {
                    outcome: CoreRizzOutcome::StoreUnavailable,
                    line: None,
                };
            };
            if !limiters.allow(
                invocation.guild_id,
                invocation.user_id,
                config.rate_per_min,
                now,
            ) {
                return CoreRizzResult {
                    outcome: CoreRizzOutcome::RateLimited,
                    line: None,
                };
            }
            let stored = store
                .get_user_voice(invocation.guild_id, invocation.user_id)
                .ok()
                .flatten();
            let model = self
                .settings
                .available_models
                .iter()
                .find(|model| model.starts_with(language_info.prefix))
                .cloned()
                .or_else(|| {
                    (!config.default_voice.trim().is_empty()).then(|| config.default_voice.clone())
                })
                .unwrap_or_else(|| self.settings.default_voice.clone());
            let speed = stored
                .as_ref()
                .map(|voice| voice.speed)
                .filter(|speed| speed.is_finite())
                .unwrap_or(self.settings.default_speed);
            let selected_engine = explicit_engine
                .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
                    "google" => Some(UserEngine::Google),
                    "piper" => Some(UserEngine::Piper),
                    "kokoro" => Some(UserEngine::Kokoro),
                    _ => None,
                })
                .or_else(|| stored.as_ref().map(|voice| voice.engine));
            let engine = resolve_preview_engine(
                &store,
                invocation.guild_id,
                invocation.user_id,
                selected_engine,
                None,
                now,
            );
            (model, speed, engine, lane)
        };
        let line = crate::pick_line(language, now);
        let request = SynthRequest {
            text: line.clone(),
            model: model.clone(),
            asset_path: None,
            speed,
            engine,
            gcloud_budget: self.paid_budget(invocation.guild_id, invocation.user_id, engine, now),
            segments: None,
            single_voice: Some(true),
            emphasis_source: None,
            lead_silence_ms: 0,
        };
        let outcome = self
            .enqueue_synth_request(
                invocation.guild_id,
                invocation.user_id,
                destination.as_ref(),
                lane,
                request,
            )
            .await;
        if outcome == CorePreviewOutcome::Queued && sound {
            let effect = SynthRequest {
                text: String::new(),
                model,
                asset_path: Some(std::path::PathBuf::from("assets/sfx/rizz.wav")),
                speed,
                // Curated WAVs bypass the user's TTS provider. Keeping this as `Default` lets the
                // Piper adapter accept the asset even when the pickup line used a paid engine.
                engine: SynthesisEngine::Default,
                gcloud_budget: None,
                segments: None,
                single_voice: Some(true),
                emphasis_source: None,
                lead_silence_ms: 0,
            };
            let _ = self
                .enqueue_synth_request(
                    invocation.guild_id,
                    invocation.user_id,
                    destination.as_ref(),
                    lane,
                    effect,
                )
                .await;
        }
        CoreRizzResult {
            outcome: match outcome {
                CorePreviewOutcome::Queued => CoreRizzOutcome::Queued,
                CorePreviewOutcome::Busy => CoreRizzOutcome::Busy,
                CorePreviewOutcome::SynthesisFailed => CoreRizzOutcome::SynthesisFailed,
                CorePreviewOutcome::PlaybackFailed => CoreRizzOutcome::PlaybackFailed,
                CorePreviewOutcome::NotInPlayer => CoreRizzOutcome::NotInPlayer,
                CorePreviewOutcome::NotInSameVoice => CoreRizzOutcome::NotInSameVoice,
                CorePreviewOutcome::RateLimited => CoreRizzOutcome::RateLimited,
                CorePreviewOutcome::UnknownModel => CoreRizzOutcome::SynthesisFailed,
                CorePreviewOutcome::StoreUnavailable => CoreRizzOutcome::StoreUnavailable,
            },
            line: Some(line),
        }
    }

    /// `/sound` is limited to the fixed asset catalog and keeps Node's discovery behavior: a
    /// missing name returns the list without requiring a call, while playback requires the same
    /// live-call and per-user rate gates as every other audible command.
    async fn execute_sound(
        &self,
        invocation: CoreVoiceInvocation<'_>,
        name: Option<&str>,
    ) -> CoreSoundResult {
        let destination = self
            .gateway_state
            .bot_voice_destination(invocation.guild_id);
        let config = {
            let Ok(store) = self.store.lock() else {
                return CoreSoundResult {
                    outcome: CoreSoundOutcome::StoreUnavailable,
                    name: None,
                    sounds: None,
                };
            };
            let Ok(config) = store.guild_config(invocation.guild_id) else {
                return CoreSoundResult {
                    outcome: CoreSoundOutcome::StoreUnavailable,
                    name: None,
                    sounds: None,
                };
            };
            config
        };
        if !config.soundboard {
            return CoreSoundResult {
                outcome: CoreSoundOutcome::Disabled,
                name: None,
                sounds: None,
            };
        }
        let Some(name) = name else {
            return CoreSoundResult {
                outcome: CoreSoundOutcome::List,
                name: None,
                sounds: Some(crate::sound_list()),
            };
        };
        let Some(clip) = crate::sound_by_key(name) else {
            return CoreSoundResult {
                outcome: CoreSoundOutcome::Unknown,
                name: None,
                sounds: None,
            };
        };
        let Ok(CommandPlaybackState::Active | CommandPlaybackState::Idle) =
            self.playback.state(invocation.guild_id).await
        else {
            return CoreSoundResult {
                outcome: CoreSoundOutcome::NotInVoice,
                name: Some(clip.name.to_owned()),
                sounds: None,
            };
        };
        let roles = invocation
            .member_role_ids
            .map(|roles| roles.iter().map(String::as_str).collect::<Vec<_>>());
        let (lane, model) = {
            let policy = RolePolicy {
                priority_role_id: config.priority_role_id.as_deref(),
                blocked_role_id: config.blocked_role_id.as_deref(),
            };
            let lane = match admit_user_speech(
                self.gateway_state
                    .voice_channel_id(invocation.guild_id, invocation.user_id)
                    .as_deref(),
                destination.as_ref().map(VoiceDestination::channel_id),
                roles.as_deref(),
                policy,
            ) {
                UserSpeechAdmission::Allowed { lane } => lane,
                UserSpeechAdmission::Denied { .. } => {
                    return CoreSoundResult {
                        outcome: CoreSoundOutcome::NotInSameVoice,
                        name: Some(clip.name.to_owned()),
                        sounds: None,
                    };
                }
            };
            let Ok(mut limiters) = self.preview_limiters.lock() else {
                return CoreSoundResult {
                    outcome: CoreSoundOutcome::StoreUnavailable,
                    name: Some(clip.name.to_owned()),
                    sounds: None,
                };
            };
            if !limiters.allow(
                invocation.guild_id,
                invocation.user_id,
                config.rate_per_min,
                (self.now_ms)(),
            ) {
                return CoreSoundResult {
                    outcome: CoreSoundOutcome::RateLimited,
                    name: Some(clip.name.to_owned()),
                    sounds: None,
                };
            }
            let model = if config.default_voice.trim().is_empty() {
                self.settings.default_voice.clone()
            } else {
                config.default_voice.clone()
            };
            (lane, model)
        };
        let request = SynthRequest {
            text: String::new(),
            model,
            asset_path: Some(std::path::PathBuf::from(format!(
                "assets/sfx/{}.wav",
                clip.key
            ))),
            speed: self.settings.default_speed,
            engine: SynthesisEngine::Default,
            gcloud_budget: None,
            segments: None,
            single_voice: Some(true),
            emphasis_source: None,
            lead_silence_ms: 0,
        };
        let outcome = self
            .enqueue_synth_request(
                invocation.guild_id,
                invocation.user_id,
                destination.as_ref(),
                lane,
                request,
            )
            .await;
        CoreSoundResult {
            outcome: match outcome {
                CorePreviewOutcome::Queued => CoreSoundOutcome::Queued,
                CorePreviewOutcome::Busy => CoreSoundOutcome::Busy,
                CorePreviewOutcome::SynthesisFailed => CoreSoundOutcome::SynthesisFailed,
                CorePreviewOutcome::PlaybackFailed => CoreSoundOutcome::PlaybackFailed,
                CorePreviewOutcome::NotInPlayer => CoreSoundOutcome::NotInVoice,
                CorePreviewOutcome::NotInSameVoice => CoreSoundOutcome::NotInSameVoice,
                CorePreviewOutcome::RateLimited => CoreSoundOutcome::RateLimited,
                CorePreviewOutcome::UnknownModel => CoreSoundOutcome::SynthesisFailed,
                CorePreviewOutcome::StoreUnavailable => CoreSoundOutcome::StoreUnavailable,
            },
            name: Some(clip.name.to_owned()),
            sounds: None,
        }
    }

    /// `/joke` keeps Node's language-first model selection and queues the optional laugh as a
    /// separate utterance with one second of lead silence. The second item is best-effort, while
    /// the joke itself determines the public success response.
    async fn execute_joke(
        &self,
        invocation: CoreVoiceInvocation<'_>,
        language: &str,
        laughter: bool,
        explicit_engine: Option<&str>,
    ) -> CoreJokeResult {
        let destination = self
            .gateway_state
            .bot_voice_destination(invocation.guild_id);
        let playback_state = match self.playback.state(invocation.guild_id).await {
            Ok(state) => state,
            Err(_) => {
                return CoreJokeResult {
                    outcome: CoreJokeOutcome::NotInPlayer,
                    joke: None,
                };
            }
        };
        if matches!(playback_state, CommandPlaybackState::NoSession) {
            return CoreJokeResult {
                outcome: CoreJokeOutcome::NotInPlayer,
                joke: None,
            };
        }

        let roles = invocation
            .member_role_ids
            .map(|roles| roles.iter().map(String::as_str).collect::<Vec<_>>());
        let (model, speed, engine, lane, joke) = {
            let store = match self.store.lock() {
                Ok(store) => store,
                Err(_) => {
                    return CoreJokeResult {
                        outcome: CoreJokeOutcome::StoreUnavailable,
                        joke: None,
                    };
                }
            };
            let config = match store.guild_config(invocation.guild_id) {
                Ok(config) => config,
                Err(_) => {
                    return CoreJokeResult {
                        outcome: CoreJokeOutcome::StoreUnavailable,
                        joke: None,
                    };
                }
            };
            let policy = RolePolicy {
                priority_role_id: config.priority_role_id.as_deref(),
                blocked_role_id: config.blocked_role_id.as_deref(),
            };
            let lane = match admit_user_speech(
                self.gateway_state
                    .voice_channel_id(invocation.guild_id, invocation.user_id)
                    .as_deref(),
                destination.as_ref().map(VoiceDestination::channel_id),
                roles.as_deref(),
                policy,
            ) {
                UserSpeechAdmission::Allowed { lane } => lane,
                UserSpeechAdmission::Denied { .. } => {
                    return CoreJokeResult {
                        outcome: CoreJokeOutcome::NotInSameVoice,
                        joke: None,
                    };
                }
            };
            if joke_lang_by_key(language).is_none() {
                return CoreJokeResult {
                    outcome: CoreJokeOutcome::UnknownLanguage,
                    joke: None,
                };
            }
            let allowed = match self.preview_limiters.lock() {
                Ok(mut limiters) => limiters.allow(
                    invocation.guild_id,
                    invocation.user_id,
                    config.rate_per_min,
                    (self.now_ms)(),
                ),
                Err(_) => {
                    return CoreJokeResult {
                        outcome: CoreJokeOutcome::StoreUnavailable,
                        joke: None,
                    };
                }
            };
            if !allowed {
                return CoreJokeResult {
                    outcome: CoreJokeOutcome::RateLimited,
                    joke: None,
                };
            }
            let stored = match store.get_user_voice(invocation.guild_id, invocation.user_id) {
                Ok(stored) => stored,
                Err(_) => {
                    return CoreJokeResult {
                        outcome: CoreJokeOutcome::StoreUnavailable,
                        joke: None,
                    };
                }
            };
            let prefix = joke_lang_by_key(language)
                .expect("validated joke language")
                .prefix;
            let model = self
                .settings
                .available_models
                .iter()
                .find(|model| model.starts_with(prefix))
                .cloned()
                .or_else(|| {
                    (!config.default_voice.trim().is_empty()).then(|| config.default_voice.clone())
                })
                .unwrap_or_else(|| {
                    if self.settings.default_voice.trim().is_empty() {
                        "en_US-amy-medium".to_owned()
                    } else {
                        self.settings.default_voice.clone()
                    }
                });
            // An explicitly selected engine follows the same Premium gate as a saved
            // preference. We pass it as the selected value (rather than the preview-only
            // explicit argument) so Kokoro remains free only for `/voice preview`.
            let selected_engine = explicit_engine
                .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
                    "google" => Some(UserEngine::Google),
                    "piper" => Some(UserEngine::Piper),
                    "kokoro" => Some(UserEngine::Kokoro),
                    _ => None,
                })
                .or_else(|| stored.as_ref().map(|voice| voice.engine));
            if selected_engine == Some(UserEngine::Kokoro) {
                let premium = match store
                    .is_user_premium(invocation.user_id, (self.now_ms)())
                    .and_then(|user| {
                        store
                            .is_guild_premium(invocation.guild_id, (self.now_ms)())
                            .map(|guild| user || guild)
                    }) {
                    Ok(premium) => premium,
                    Err(_) => {
                        return CoreJokeResult {
                            outcome: CoreJokeOutcome::StoreUnavailable,
                            joke: None,
                        };
                    }
                };
                if !premium {
                    return CoreJokeResult {
                        outcome: CoreJokeOutcome::PremiumLocked,
                        joke: None,
                    };
                }
            }
            let engine = resolve_preview_engine(
                &store,
                invocation.guild_id,
                invocation.user_id,
                selected_engine,
                None,
                (self.now_ms)(),
            );
            let joke = pick_joke(language, (self.now_ms)()).to_owned();
            (model, self.settings.default_speed, engine, lane, joke)
        };

        let joke_request = SynthRequest {
            text: joke.clone(),
            model: model.clone(),
            asset_path: None,
            speed,
            engine,
            gcloud_budget: self.paid_budget(
                invocation.guild_id,
                invocation.user_id,
                engine,
                (self.now_ms)(),
            ),
            segments: None,
            single_voice: Some(true),
            emphasis_source: None,
            lead_silence_ms: 0,
        };
        let queued = match self
            .enqueue_synth_request(
                invocation.guild_id,
                invocation.user_id,
                destination.as_ref(),
                lane,
                joke_request,
            )
            .await
        {
            CorePreviewOutcome::Queued => true,
            CorePreviewOutcome::Busy => {
                return CoreJokeResult {
                    outcome: CoreJokeOutcome::Busy,
                    joke: Some(joke),
                };
            }
            CorePreviewOutcome::SynthesisFailed => {
                return CoreJokeResult {
                    outcome: CoreJokeOutcome::SynthesisFailed,
                    joke: Some(joke),
                };
            }
            CorePreviewOutcome::PlaybackFailed => {
                return CoreJokeResult {
                    outcome: CoreJokeOutcome::PlaybackFailed,
                    joke: Some(joke),
                };
            }
            CorePreviewOutcome::NotInPlayer => {
                return CoreJokeResult {
                    outcome: CoreJokeOutcome::NotInPlayer,
                    joke: Some(joke),
                };
            }
            CorePreviewOutcome::NotInSameVoice
            | CorePreviewOutcome::RateLimited
            | CorePreviewOutcome::UnknownModel
            | CorePreviewOutcome::StoreUnavailable => {
                return CoreJokeResult {
                    outcome: CoreJokeOutcome::PlaybackFailed,
                    joke: Some(joke),
                };
            }
        };
        if queued && laughter {
            let laugh_request = SynthRequest {
                text: laughter_for_prefix(
                    joke_lang_by_key(language)
                        .expect("validated joke language")
                        .prefix,
                ),
                model,
                asset_path: None,
                speed,
                engine,
                gcloud_budget: self.paid_budget(
                    invocation.guild_id,
                    invocation.user_id,
                    engine,
                    (self.now_ms)(),
                ),
                segments: None,
                single_voice: Some(true),
                emphasis_source: None,
                lead_silence_ms: 1_000,
            };
            let _ = self
                .enqueue_synth_request(
                    invocation.guild_id,
                    invocation.user_id,
                    destination.as_ref(),
                    lane,
                    laugh_request,
                )
                .await;
        }
        CoreJokeResult {
            outcome: CoreJokeOutcome::Queued,
            joke: Some(joke),
        }
    }

    async fn enqueue_synth_request(
        &self,
        guild_id: &str,
        user_id: &str,
        destination: Option<&VoiceDestination>,
        lane: QueueLane,
        request: SynthRequest,
    ) -> CorePreviewOutcome {
        let Some(destination) = destination else {
            return CorePreviewOutcome::PlaybackFailed;
        };
        let admitted_generation = self.synthesis.admission_generation(guild_id);
        let mut synthesis = self.synthesis.acquire(guild_id, admitted_generation).await;
        if synthesis.was_cleared() || !destination.is_current() {
            return CorePreviewOutcome::PlaybackFailed;
        }
        synthesis.activate();
        match self.playback.reserve(guild_id, lane).await {
            Ok(true) => {}
            Ok(false) => return CorePreviewOutcome::Busy,
            Err(_) => return CorePreviewOutcome::PlaybackFailed,
        }
        if synthesis.cancelled() || !destination.is_current() {
            let _ = self.playback.cancel_reservation(guild_id, lane).await;
            return CorePreviewOutcome::PlaybackFailed;
        }
        let wav = match self.synthesizer.synthesize(&request).await {
            Ok(wav) => wav,
            Err(_) => {
                let _ = self.playback.cancel_reservation(guild_id, lane).await;
                return CorePreviewOutcome::SynthesisFailed;
            }
        };
        if synthesis.cancelled() || !destination.is_current() {
            let _ = self.playback.cancel_reservation(guild_id, lane).await;
            return CorePreviewOutcome::PlaybackFailed;
        }
        match self
            .playback
            .enqueue_reserved(
                guild_id,
                destination,
                &wav,
                QueueEnqueueOptions {
                    author_id: Some(user_id),
                    source: QueueSource::Command,
                    lane,
                    created_at_ms: (self.now_ms)().max(0) as u64,
                },
            )
            .await
        {
            Ok(()) => CorePreviewOutcome::Queued,
            Err(_) => {
                let _ = self.playback.cancel_reservation(guild_id, lane).await;
                CorePreviewOutcome::PlaybackFailed
            }
        }
    }

    async fn skip(&self, guild_id: &str) -> CorePlaybackControlOutcome {
        match self.playback.state(guild_id).await {
            Ok(CommandPlaybackState::NoSession) => CorePlaybackControlOutcome::NotInVoice,
            Ok(CommandPlaybackState::Idle) => {
                if self.synthesis.cancel_active(guild_id) {
                    CorePlaybackControlOutcome::Completed
                } else {
                    CorePlaybackControlOutcome::NothingPlaying
                }
            }
            Ok(CommandPlaybackState::Active) => match self.playback.skip(guild_id).await {
                Ok(()) => CorePlaybackControlOutcome::Completed,
                Err(_) => CorePlaybackControlOutcome::PlaybackFailed,
            },
            Err(_) => CorePlaybackControlOutcome::PlaybackFailed,
        }
    }

    async fn silence(&self, guild_id: &str) -> CorePlaybackControlOutcome {
        let cancelled_synthesis = self.synthesis.clear(guild_id);
        match self.playback.state(guild_id).await {
            Ok(CommandPlaybackState::NoSession) => {
                if cancelled_synthesis {
                    CorePlaybackControlOutcome::Completed
                } else {
                    CorePlaybackControlOutcome::NotInVoice
                }
            }
            Ok(CommandPlaybackState::Idle) => {
                if cancelled_synthesis {
                    CorePlaybackControlOutcome::Completed
                } else {
                    CorePlaybackControlOutcome::NothingPlaying
                }
            }
            Ok(CommandPlaybackState::Active) => match self.playback.silence(guild_id).await {
                Ok(()) => CorePlaybackControlOutcome::Completed,
                Err(_) => CorePlaybackControlOutcome::PlaybackFailed,
            },
            Err(_) => CorePlaybackControlOutcome::PlaybackFailed,
        }
    }

    async fn execute_tts(&self, invocation: CoreVoiceInvocation<'_>, text: &str) -> CoreTtsOutcome {
        let destination = self
            .gateway_state
            .bot_voice_destination(invocation.guild_id);
        let roles = invocation
            .member_role_ids
            .map(|roles| roles.iter().map(String::as_str).collect::<Vec<_>>());
        let prepared = {
            let store = match self.store.lock() {
                Ok(store) => store,
                Err(_) => return CoreTtsOutcome::StoreUnavailable,
            };
            let mut speech = match self.speech.lock() {
                Ok(speech) => speech,
                Err(_) => return CoreTtsOutcome::StoreUnavailable,
            };
            speech.prepare(
                &store,
                CommandSpeechInput {
                    guild_id: invocation.guild_id,
                    channel_id: invocation.channel_id,
                    user_id: invocation.user_id,
                    raw: text,
                    caller_voice_channel_id: self
                        .gateway_state
                        .voice_channel_id(invocation.guild_id, invocation.user_id)
                        .as_deref(),
                    bot_voice_channel_id: destination.as_ref().map(VoiceDestination::channel_id),
                    member_role_ids: roles.as_deref(),
                    available_models: &self.settings.available_models,
                    runtime_default_voice: &self.settings.default_voice,
                    runtime_default_speed: self.settings.default_speed,
                    runtime_default_engine: self.settings.default_engine,
                    detected_language: None,
                    resolve_user: invocation.resolve_user,
                    resolve_channel: invocation.resolve_channel,
                },
                (self.now_ms)(),
            )
        };

        let (lane, request) = match prepared {
            Ok(CommandSpeechOutcome::Ready { lane, speech }) => (lane, speech.request),
            Ok(CommandSpeechOutcome::NotInSameVoice) => return CoreTtsOutcome::NotInSameVoice,
            Ok(CommandSpeechOutcome::Blocked) => return CoreTtsOutcome::Blocked,
            Ok(CommandSpeechOutcome::Empty) => return CoreTtsOutcome::Empty,
            Ok(CommandSpeechOutcome::RateLimited) => return CoreTtsOutcome::RateLimited,
            Ok(CommandSpeechOutcome::FullyBlocked) => return CoreTtsOutcome::FullyBlocked,
            Err(_) => return CoreTtsOutcome::StoreUnavailable,
        };
        let Some(destination) = destination.as_ref() else {
            return CoreTtsOutcome::PlaybackFailed;
        };
        let admitted_generation = self.synthesis.admission_generation(invocation.guild_id);
        let mut synthesis = self
            .synthesis
            .acquire(invocation.guild_id, admitted_generation)
            .await;
        if synthesis.was_cleared() || !destination.is_current() {
            return CoreTtsOutcome::PlaybackFailed;
        }
        synthesis.activate();

        match self.playback.reserve(invocation.guild_id, lane).await {
            Ok(true) => {}
            Ok(false) => return CoreTtsOutcome::Busy,
            Err(_) => return CoreTtsOutcome::PlaybackFailed,
        }
        if synthesis.cancelled() || !destination.is_current() {
            let _ = self
                .playback
                .cancel_reservation(invocation.guild_id, lane)
                .await;
            return CoreTtsOutcome::PlaybackFailed;
        }
        let wav = match self.synthesizer.synthesize(&request).await {
            Ok(wav) => wav,
            Err(_) => {
                let _ = self
                    .playback
                    .cancel_reservation(invocation.guild_id, lane)
                    .await;
                return CoreTtsOutcome::SynthesisFailed;
            }
        };
        if synthesis.cancelled() || !destination.is_current() {
            let _ = self
                .playback
                .cancel_reservation(invocation.guild_id, lane)
                .await;
            return CoreTtsOutcome::PlaybackFailed;
        }
        match self
            .playback
            .enqueue_reserved(
                invocation.guild_id,
                destination,
                &wav,
                QueueEnqueueOptions {
                    author_id: Some(invocation.user_id),
                    source: QueueSource::Command,
                    lane,
                    created_at_ms: (self.now_ms)().max(0) as u64,
                },
            )
            .await
        {
            Ok(()) => CoreTtsOutcome::Queued,
            Err(_) => {
                let _ = self
                    .playback
                    .cancel_reservation(invocation.guild_id, lane)
                    .await;
                CoreTtsOutcome::PlaybackFailed
            }
        }
    }

    /// Releases process-local user buckets on a real guild departure.
    pub fn forget_guild(&self, guild_id: &str) {
        if let Ok(mut speech) = self.speech.lock() {
            speech.forget_guild(guild_id);
        }
        self.synthesis.forget_guild(guild_id);
        if let Ok(mut limiters) = self.preview_limiters.lock() {
            limiters.forget_guild(guild_id);
        }
    }
}

fn resolve_preview_engine(
    store: &SqliteStore,
    guild_id: &str,
    user_id: &str,
    stored: Option<UserEngine>,
    explicit: Option<&str>,
    now_ms: i64,
) -> SynthesisEngine {
    // Preview is the free product sample: an explicit Kokoro choice must be allowed even when
    // the caller has no Premium entitlement. Premium gating still applies to saved preferences
    // and normal message playback; only this one-shot preview bypasses it.
    if explicit.is_some_and(|value| value.trim().eq_ignore_ascii_case("kokoro")) {
        return SynthesisEngine::Kokoro;
    }
    let selected = explicit
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "google" => Some(UserEngine::Google),
            "piper" => Some(UserEngine::Piper),
            "kokoro" => Some(UserEngine::Kokoro),
            _ => None,
        })
        .or(stored)
        .unwrap_or(UserEngine::Google);
    match selected {
        UserEngine::Google => SynthesisEngine::Default,
        UserEngine::Piper => SynthesisEngine::Piper,
        UserEngine::Kokoro => {
            if store
                .is_user_premium(user_id, now_ms)
                .and_then(|user| {
                    store
                        .is_guild_premium(guild_id, now_ms)
                        .map(|guild| user || guild)
                })
                .unwrap_or(false)
            {
                SynthesisEngine::Kokoro
            } else {
                SynthesisEngine::Default
            }
        }
        UserEngine::Gcloud => {
            let unlocked = store.is_user_premium(user_id, now_ms).unwrap_or(false)
                || store
                    .resolve_guild_pass_owner(guild_id, now_ms)
                    .ok()
                    .flatten()
                    .is_some()
                || store.is_guild_premium(guild_id, now_ms).unwrap_or(false);
            if unlocked {
                SynthesisEngine::Gcloud
            } else {
                SynthesisEngine::Default
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use tokio::sync::Notify;
    use vozen_store::{GuildConfigPatch, SqliteStore};

    use super::*;
    use crate::VoiceSessionTransportError;

    #[derive(Default)]
    struct FakeVoiceTransport;

    #[async_trait]
    impl VoiceSessionTransport for FakeVoiceTransport {
        async fn join(
            &self,
            _guild_id: &str,
            _channel_id: &str,
        ) -> Result<(), VoiceSessionTransportError> {
            Ok(())
        }

        async fn leave(&self, _guild_id: &str) -> Result<(), VoiceSessionTransportError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeSynthesizer {
        calls: AtomicUsize,
        fails: bool,
        requests: Mutex<Vec<SynthRequest>>,
    }

    #[async_trait]
    impl CommandSpeechSynthesizer for FakeSynthesizer {
        async fn synthesize(
            &self,
            request: &SynthRequest,
        ) -> Result<PathBuf, CommandSynthesisError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.requests
                .lock()
                .expect("requests")
                .push(request.clone());
            if self.fails {
                return Err(CommandSynthesisError);
            }
            Ok(PathBuf::from("voice.wav"))
        }
    }

    struct BlockingSynthesizer {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl CommandSpeechSynthesizer for BlockingSynthesizer {
        async fn synthesize(
            &self,
            _request: &SynthRequest,
        ) -> Result<PathBuf, CommandSynthesisError> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(PathBuf::from("voice.wav"))
        }
    }

    struct FakePlayback {
        lanes: Mutex<Vec<QueueLane>>,
        accepted: bool,
        reservations: AtomicUsize,
        enqueues: AtomicUsize,
        state: CommandPlaybackState,
        skips: AtomicUsize,
        silences: AtomicUsize,
    }

    impl Default for FakePlayback {
        fn default() -> Self {
            Self {
                lanes: Mutex::new(Vec::new()),
                accepted: false,
                reservations: AtomicUsize::new(0),
                enqueues: AtomicUsize::new(0),
                state: CommandPlaybackState::NoSession,
                skips: AtomicUsize::new(0),
                silences: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl CommandVoicePlayback for FakePlayback {
        async fn state(
            &self,
            _guild_id: &str,
        ) -> Result<CommandPlaybackState, CommandPlaybackError> {
            Ok(self.state)
        }

        async fn reserve(
            &self,
            _guild_id: &str,
            lane: QueueLane,
        ) -> Result<bool, CommandPlaybackError> {
            if self.accepted {
                self.lanes.lock().expect("lanes").push(lane);
                self.reservations.fetch_add(1, Ordering::Relaxed);
            }
            Ok(self.accepted)
        }

        async fn enqueue_reserved(
            &self,
            _guild_id: &str,
            _destination: &VoiceDestination,
            _wav: &Path,
            _options: QueueEnqueueOptions<'_>,
        ) -> Result<(), CommandPlaybackError> {
            self.enqueues.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn cancel_reservation(
            &self,
            _guild_id: &str,
            _lane: QueueLane,
        ) -> Result<(), CommandPlaybackError> {
            self.reservations.fetch_sub(1, Ordering::Relaxed);
            Ok(())
        }

        async fn skip(&self, _guild_id: &str) -> Result<(), CommandPlaybackError> {
            self.skips.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn silence(&self, _guild_id: &str) -> Result<(), CommandPlaybackError> {
            self.silences.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn service(
        accepted: bool,
    ) -> (
        CoreVoiceService<FakeVoiceTransport, FakeSynthesizer, FakePlayback>,
        Arc<Mutex<SqliteStore>>,
        GatewayState,
    ) {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let state = GatewayState::default();
        state.remember_bot_user("bot".into());
        let service = CoreVoiceService::new(
            store.clone(),
            state.clone(),
            FakeVoiceTransport,
            FakeSynthesizer::default(),
            FakePlayback {
                accepted,
                state: CommandPlaybackState::Active,
                ..FakePlayback::default()
            },
            CoreVoiceSettings {
                available_models: vec!["en_US-amy-medium".into()],
                default_voice: "en_US-amy-medium".into(),
                default_speed: 1.0,
                default_engine: SynthesisEngine::Piper,
            },
            Arc::new(|| 0),
        );
        (service, store, state)
    }

    fn invocation() -> CoreVoiceInvocation<'static> {
        CoreVoiceInvocation {
            guild_id: "guild",
            channel_id: "channel",
            user_id: "user",
            member_role_ids: None,
            resolve_user: &|_| "user".into(),
            resolve_channel: &|_| "channel".into(),
        }
    }

    #[tokio::test]
    async fn tts_requires_live_same_call_before_synthesis() {
        let (service, _, state) = service(true);
        state.update_voice_state("guild", "user", Some("other".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::Tts {
                        text: "hello".into()
                    },
                )
                .await,
            CoreVoiceOutcome::Tts(CoreTtsOutcome::NotInSameVoice)
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn preview_requires_live_same_call_before_synthesis() {
        let (service, _, state) = service(true);
        state.update_voice_state("guild", "user", Some("other".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::VoicePreview {
                        model: None,
                        engine: None,
                    },
                )
                .await,
            CoreVoiceOutcome::Preview(CorePreviewOutcome::NotInSameVoice)
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn preview_uses_the_same_bounded_queue_and_rejects_unknown_explicit_models() {
        let (service, _, state) = service(true);
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::VoicePreview {
                        model: Some("missing-model".into()),
                        engine: None,
                    },
                )
                .await,
            CoreVoiceOutcome::Preview(CorePreviewOutcome::UnknownModel)
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 0);

        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::VoicePreview {
                        model: None,
                        engine: None,
                    },
                )
                .await,
            CoreVoiceOutcome::Preview(CorePreviewOutcome::Queued)
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn laugh_uses_the_shared_preview_queue_and_same_call_admission() {
        let (service, _, state) = service(true);
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        assert_eq!(
            service
                .execute(invocation(), &CoreVoiceCommand::Laugh)
                .await,
            CoreVoiceOutcome::Laugh(CorePreviewOutcome::Queued)
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn joke_queues_the_selected_language_and_delayed_language_laughter() {
        let (service, _, state) = service(true);
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        let outcome = service
            .execute(
                invocation(),
                &CoreVoiceCommand::Joke {
                    language: "pt".into(),
                    laughter: true,
                    engine: None,
                },
            )
            .await;
        assert_eq!(
            outcome,
            CoreVoiceOutcome::Joke(CoreJokeResult {
                outcome: CoreJokeOutcome::Queued,
                joke: Some(pick_joke("pt", 0).into()),
            })
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 2);
        let requests = service.synthesizer.requests.lock().expect("requests");
        assert_eq!(requests[0].text, pick_joke("pt", 0));
        assert_eq!(requests[0].lead_silence_ms, 0);
        assert_eq!(requests[1].text, laughter_for_prefix("pt_"));
        assert_eq!(requests[1].lead_silence_ms, 1_000);
    }

    #[tokio::test]
    async fn joke_rejects_unknown_language_without_spending_synthesis_or_queue_capacity() {
        let (service, _, state) = service(true);
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::Joke {
                        language: "made-up".into(),
                        laughter: false,
                        engine: None,
                    },
                )
                .await,
            CoreVoiceOutcome::Joke(CoreJokeResult {
                outcome: CoreJokeOutcome::UnknownLanguage,
                joke: None,
            })
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 0);
        assert_eq!(service.playback.reservations.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn joke_explicit_kokoro_requires_premium_instead_of_falling_back_to_google() {
        let (service, _, state) = service(true);
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::Joke {
                        language: "en".into(),
                        laughter: false,
                        engine: Some("kokoro".into()),
                    },
                )
                .await,
            CoreVoiceOutcome::Joke(CoreJokeResult {
                outcome: CoreJokeOutcome::PremiumLocked,
                joke: None,
            })
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn rizz_requires_premium_and_then_queues_line_and_optional_asset() {
        let (service, store, state) = service(true);
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::Rizz {
                        language: "pt".into(),
                        sound: true,
                        engine: None,
                    },
                )
                .await,
            CoreVoiceOutcome::Rizz(CoreRizzResult {
                outcome: CoreRizzOutcome::PremiumLocked,
                line: None,
            })
        );
        store
            .lock()
            .expect("store")
            .grant_user_premium("user", 30, "test", 0)
            .expect("premium");
        let outcome = service
            .execute(
                invocation(),
                &CoreVoiceCommand::Rizz {
                    language: "pt".into(),
                    sound: true,
                    engine: None,
                },
            )
            .await;
        assert_eq!(
            outcome,
            CoreVoiceOutcome::Rizz(CoreRizzResult {
                outcome: CoreRizzOutcome::Queued,
                line: Some(crate::pick_line("pt", 0)),
            })
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 2);
        let requests = service.synthesizer.requests.lock().expect("requests");
        assert_eq!(requests[0].text, crate::pick_line("pt", 0));
        assert_eq!(requests[0].asset_path, None);
        assert_eq!(
            requests[1].asset_path,
            Some(PathBuf::from("assets/sfx/rizz.wav"))
        );
        assert_eq!(requests[1].engine, SynthesisEngine::Default);
    }

    #[tokio::test]
    async fn rizz_unknown_language_does_not_spend_speech_capacity() {
        let (service, store, state) = service(true);
        store
            .lock()
            .expect("store")
            .grant_user_premium("user", 30, "test", 0)
            .expect("premium");
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        assert!(matches!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::Rizz {
                        language: "missing".into(),
                        sound: false,
                        engine: None,
                    },
                )
                .await,
            CoreVoiceOutcome::Rizz(CoreRizzResult {
                outcome: CoreRizzOutcome::UnknownLanguage,
                line: None,
            })
        ));
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn sound_lists_without_a_call_and_queues_only_curated_assets() {
        let (service, _, state) = service(true);
        let listed = service
            .execute(invocation(), &CoreVoiceCommand::Sound { name: None })
            .await;
        let CoreVoiceOutcome::Sound(listed) = listed else {
            panic!("expected sound list")
        };
        assert_eq!(listed.outcome, CoreSoundOutcome::List);
        assert!(listed.sounds.expect("sound list").contains("airhorn"));
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 0);

        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        let queued = service
            .execute(
                invocation(),
                &CoreVoiceCommand::Sound {
                    name: Some("airhorn".into()),
                },
            )
            .await;
        assert_eq!(
            queued,
            CoreVoiceOutcome::Sound(CoreSoundResult {
                outcome: CoreSoundOutcome::Queued,
                name: Some("Air horn".into()),
                sounds: None,
            })
        );
        let requests = service.synthesizer.requests.lock().expect("requests");
        assert_eq!(requests[0].text, "");
        assert_eq!(
            requests[0].asset_path,
            Some(PathBuf::from("assets/sfx/airhorn.wav"))
        );
    }

    #[tokio::test]
    async fn sound_kill_switch_and_same_call_gate_fail_closed() {
        let (service, store, state) = service(true);
        store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    soundboard: Some(false),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("config");
        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::Sound {
                        name: Some("airhorn".into()),
                    },
                )
                .await,
            CoreVoiceOutcome::Sound(CoreSoundResult {
                outcome: CoreSoundOutcome::Disabled,
                name: None,
                sounds: None,
            })
        );

        store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    soundboard: Some(true),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("config");
        state.update_voice_state("guild", "user", Some("other".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::Sound {
                        name: Some("airhorn".into()),
                    },
                )
                .await,
            CoreVoiceOutcome::Sound(CoreSoundResult {
                outcome: CoreSoundOutcome::NotInSameVoice,
                name: Some("Air horn".into()),
                sounds: None,
            })
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn tts_prepares_then_queues_in_the_effective_priority_lane() {
        let (service, store, state) = service(true);
        store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    priority_role_id: Some(Some("priority".into())),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("config");
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        let roles = vec!["priority".to_owned()];
        let mut invocation = invocation();
        invocation.member_role_ids = Some(&roles);
        assert_eq!(
            service
                .execute(
                    invocation,
                    &CoreVoiceCommand::Tts {
                        text: "hello".into()
                    },
                )
                .await,
            CoreVoiceOutcome::Tts(CoreTtsOutcome::Queued)
        );
        assert_eq!(
            *service.playback.lanes.lock().expect("lanes"),
            vec![QueueLane::Accessibility]
        );
    }

    #[tokio::test]
    async fn internal_game_speech_queues_with_role_policy_configured() {
        let (service, store, state) = service(true);
        store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    priority_role_id: Some(Some("priority".into())),
                    blocked_role_id: Some(Some("blocked".into())),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("config");
        state.set_bot_voice_channel("guild", Some("voice".into()));
        let roles = Vec::new();
        let invocation = CoreVoiceInvocation {
            guild_id: "guild",
            channel_id: "game-thread",
            user_id: "bot",
            member_role_ids: Some(&roles),
            resolve_user: &|_| "Vozen".into(),
            resolve_channel: &|_| "game-thread".into(),
        };

        assert_eq!(
            service
                .execute_custom_speech(
                    invocation,
                    "listen carefully",
                    "en_US-amy-medium",
                    1.0,
                    SynthesisEngine::Piper,
                    false,
                )
                .await,
            CoreTtsOutcome::Queued
        );
        assert_eq!(
            *service.playback.lanes.lock().expect("lanes"),
            vec![QueueLane::Standard]
        );
    }

    #[tokio::test]
    async fn queue_rejection_is_visible_without_claiming_speech_was_queued() {
        let (service, _, state) = service(false);
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::Tts {
                        text: "hello".into()
                    },
                )
                .await,
            CoreVoiceOutcome::Tts(CoreTtsOutcome::Busy)
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn failed_synthesis_releases_the_previously_reserved_capacity() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let state = GatewayState::default();
        state.remember_bot_user("bot".into());
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        let service = CoreVoiceService::new(
            store,
            state,
            FakeVoiceTransport,
            FakeSynthesizer {
                calls: AtomicUsize::new(0),
                fails: true,
                requests: Mutex::new(Vec::new()),
            },
            FakePlayback {
                accepted: true,
                state: CommandPlaybackState::Active,
                ..FakePlayback::default()
            },
            CoreVoiceSettings {
                available_models: vec!["en_US-amy-medium".into()],
                default_voice: "en_US-amy-medium".into(),
                default_speed: 1.0,
                default_engine: SynthesisEngine::Piper,
            },
            Arc::new(|| 0),
        );
        assert_eq!(
            service
                .execute(
                    invocation(),
                    &CoreVoiceCommand::Tts {
                        text: "hello".into()
                    }
                )
                .await,
            CoreVoiceOutcome::Tts(CoreTtsOutcome::SynthesisFailed)
        );
        assert_eq!(service.playback.reservations.load(Ordering::Relaxed), 0);
        assert_eq!(service.playback.enqueues.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn skip_and_silence_only_run_when_audio_is_active() {
        let (service, _, _) = service(true);
        assert_eq!(
            service.execute(invocation(), &CoreVoiceCommand::Skip).await,
            CoreVoiceOutcome::Skipped(CorePlaybackControlOutcome::Completed)
        );
        assert_eq!(
            service
                .execute(invocation(), &CoreVoiceCommand::ShutUp)
                .await,
            CoreVoiceOutcome::Silenced(CorePlaybackControlOutcome::Completed)
        );
        assert_eq!(service.playback.skips.load(Ordering::Relaxed), 1);
        assert_eq!(service.playback.silences.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn idle_playback_does_not_claim_to_have_skipped_any_audio() {
        let (mut service, _, _) = service(true);
        service.playback.state = CommandPlaybackState::Idle;
        assert_eq!(
            service.execute(invocation(), &CoreVoiceCommand::Skip).await,
            CoreVoiceOutcome::Skipped(CorePlaybackControlOutcome::NothingPlaying)
        );
        assert_eq!(service.playback.skips.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn skip_cancels_synthesis_before_it_can_enqueue_audio() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let state = GatewayState::default();
        state.remember_bot_user("bot".into());
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("voice".into()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let service = Arc::new(CoreVoiceService::new(
            store,
            state,
            FakeVoiceTransport,
            BlockingSynthesizer {
                started: started.clone(),
                release: release.clone(),
            },
            FakePlayback {
                accepted: true,
                state: CommandPlaybackState::Idle,
                ..FakePlayback::default()
            },
            CoreVoiceSettings {
                available_models: vec!["en_US-amy-medium".into()],
                default_voice: "en_US-amy-medium".into(),
                default_speed: 1.0,
                default_engine: SynthesisEngine::Piper,
            },
            Arc::new(|| 0),
        ));
        let command = CoreVoiceCommand::Tts {
            text: "hello".into(),
        };
        let queued = service.execute(invocation(), &command);
        tokio::pin!(queued);
        let started_signal = started.notified();
        tokio::pin!(started_signal);
        tokio::select! {
            _ = &mut started_signal => {}
            outcome = &mut queued => panic!("synthesis completed before skip: {outcome:?}"),
        }
        assert_eq!(
            service.execute(invocation(), &CoreVoiceCommand::Skip).await,
            CoreVoiceOutcome::Skipped(CorePlaybackControlOutcome::Completed)
        );
        release.notify_one();

        assert_eq!(
            queued.await,
            CoreVoiceOutcome::Tts(CoreTtsOutcome::PlaybackFailed)
        );
        assert_eq!(service.playback.enqueues.load(Ordering::Relaxed), 0);
        assert_eq!(service.playback.reservations.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn speech_cannot_cross_voice_sessions_during_synthesis() {
        for return_to_original in [false, true] {
            let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
            let state = GatewayState::default();
            state.remember_bot_user("bot".into());
            state.update_voice_state("guild", "user", Some("voice".into()));
            state.set_bot_voice_channel("guild", Some("voice".into()));
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let service = Arc::new(CoreVoiceService::new(
                store,
                state.clone(),
                FakeVoiceTransport,
                BlockingSynthesizer {
                    started: started.clone(),
                    release: release.clone(),
                },
                FakePlayback {
                    accepted: true,
                    state: CommandPlaybackState::Idle,
                    ..FakePlayback::default()
                },
                CoreVoiceSettings {
                    available_models: vec!["en_US-amy-medium".into()],
                    default_voice: "en_US-amy-medium".into(),
                    default_speed: 1.0,
                    default_engine: SynthesisEngine::Piper,
                },
                Arc::new(|| 0),
            ));
            let command = CoreVoiceCommand::Tts {
                text: "hello".into(),
            };
            let queued = service.execute(invocation(), &command);
            tokio::pin!(queued);
            let started_signal = started.notified();
            tokio::pin!(started_signal);
            tokio::select! {
                _ = &mut started_signal => {}
                outcome = &mut queued => panic!("synthesis completed before channel transition: {outcome:?}"),
            }
            state.set_bot_voice_channel("guild", Some("other".into()));
            if return_to_original {
                state.set_bot_voice_channel("guild", None);
                state.set_bot_voice_channel("guild", Some("voice".into()));
            }
            release.notify_one();

            assert_eq!(
                queued.await,
                CoreVoiceOutcome::Tts(CoreTtsOutcome::PlaybackFailed)
            );
            assert_eq!(service.playback.enqueues.load(Ordering::Relaxed), 0);
            assert_eq!(service.playback.reservations.load(Ordering::Relaxed), 0);
        }
    }
}
