//! Async message-to-voice service for the promoted auto-read path.
//!
//! The Discord gateway supplies already-resolved facts, while this service owns the strict
//! ordering: admission (including same-call) -> clean/redact/rate limit -> reserve -> synthesize
//! -> enqueue. No request reaches Piper when capacity or authorization has already failed.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Instant,
};

use vozen_core::{
    CountGate, DuplicateTracker, MediaAnnouncement, MessageSpeechDecision, MessageSpeechDenial,
    QueueEnqueueOptions, QueueSource, RuntimeMetrics, SynthesisEngine, is_repetition_spam,
};
use vozen_store::{
    OperationalMetric, OperationalProvider, ProviderHealth, RuntimeBatchBuffer, SqliteStore,
    TalkBump, UserEngine, utc_day_key_from_unix_millis,
};

use crate::speech_telemetry::SpeechTelemetryWriter;
use crate::{
    CommandSpeechSynthesizer, CommandVoicePlayback, CoreVoiceSettings, DiscordMessageFacts,
    GuildSynthesisCoordinator, MessagePipelineOutcome, MessagePreparationInput,
    MessageSpeechPipeline, VoiceDataCache, admit_discord_message_with_data,
};

/// Per-message values which are never persisted. `facts` must come from the same Discord event
/// as `raw`; in particular, role and voice membership are not permitted to be reconstructed from
/// stale database values.
pub struct MessageVoiceInvocation<'a> {
    pub facts: DiscordMessageFacts<'a>,
    pub raw: &'a str,
    pub media: &'a [MediaAnnouncement],
    pub detected_language: Option<&'a str>,
    pub announce_speaker: Option<&'a str>,
    pub resolve_user: &'a (dyn Fn(&str) -> String + Send + Sync),
    pub resolve_channel: &'a (dyn Fn(&str) -> String + Send + Sync),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageVoiceOutcome {
    Denied(MessageSpeechDenial),
    Empty,
    RateLimited,
    FullyBlocked,
    SpamSuppressed,
    Busy,
    SynthesisFailed,
    PlaybackFailed,
    Queued { talk: Option<TalkBump> },
    StoreUnavailable,
}

pub struct MessageVoiceService<S, P> {
    /// Local compatibility writer: counters and fallback state remain available if Postgres is
    /// temporarily unavailable.
    store: Arc<Mutex<SqliteStore>>,
    /// Snapshot reader. During staging primary-read validation this is a refreshed in-memory
    /// replica sourced from Postgres, never a network call from the message handler.
    read_store: Arc<Mutex<SqliteStore>>,
    pipeline: Mutex<MessageSpeechPipeline>,
    duplicate_tracker: Mutex<DuplicateTracker>,
    count_gate: Mutex<CountGate>,
    voice_data: VoiceDataCache,
    runtime_batch: RuntimeBatchBuffer,
    telemetry: SpeechTelemetryWriter,
    synthesizer: S,
    playback: P,
    synthesis: GuildSynthesisCoordinator,
    settings: CoreVoiceSettings,
    now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    metrics: Arc<RuntimeMetrics>,
}

impl<S, P> MessageVoiceService<S, P> {
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        synthesizer: S,
        playback: P,
        settings: CoreVoiceSettings,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self::new_with_synthesis_coordinator(
            store,
            synthesizer,
            playback,
            GuildSynthesisCoordinator::default(),
            settings,
            now_ms,
        )
    }

    pub fn new_with_synthesis_coordinator(
        store: Arc<Mutex<SqliteStore>>,
        synthesizer: S,
        playback: P,
        synthesis: GuildSynthesisCoordinator,
        settings: CoreVoiceSettings,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self::new_with_synthesis_coordinator_and_runtime_batch(
            store,
            synthesizer,
            playback,
            synthesis,
            settings,
            now_ms,
            RuntimeBatchBuffer::default(),
        )
    }

    pub fn new_with_synthesis_coordinator_and_runtime_batch(
        store: Arc<Mutex<SqliteStore>>,
        synthesizer: S,
        playback: P,
        synthesis: GuildSynthesisCoordinator,
        settings: CoreVoiceSettings,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
        runtime_batch: RuntimeBatchBuffer,
    ) -> Self {
        Self::new_with_synthesis_coordinator_runtime_batch_and_read_store(
            store.clone(),
            store,
            synthesizer,
            playback,
            synthesis,
            settings,
            now_ms,
            runtime_batch,
        )
    }

    /// Uses a separate, local-only state snapshot for automatic voice reads. The write store is
    /// retained for compatibility counters and for fallback durability; the supplied read store
    /// must already be populated by a background task before this service is exposed.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_synthesis_coordinator_runtime_batch_and_read_store(
        store: Arc<Mutex<SqliteStore>>,
        read_store: Arc<Mutex<SqliteStore>>,
        synthesizer: S,
        playback: P,
        synthesis: GuildSynthesisCoordinator,
        settings: CoreVoiceSettings,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
        runtime_batch: RuntimeBatchBuffer,
    ) -> Self {
        Self::new_with_synthesis_coordinator_runtime_batch_read_store_and_metrics(
            store,
            read_store,
            synthesizer,
            playback,
            synthesis,
            settings,
            now_ms,
            runtime_batch,
            Arc::new(RuntimeMetrics::default()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_synthesis_coordinator_runtime_batch_read_store_and_metrics(
        store: Arc<Mutex<SqliteStore>>,
        read_store: Arc<Mutex<SqliteStore>>,
        synthesizer: S,
        playback: P,
        synthesis: GuildSynthesisCoordinator,
        settings: CoreVoiceSettings,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
        runtime_batch: RuntimeBatchBuffer,
        metrics: Arc<RuntimeMetrics>,
    ) -> Self {
        Self {
            store: store.clone(),
            read_store,
            pipeline: Mutex::new(MessageSpeechPipeline::default()),
            duplicate_tracker: Mutex::new(DuplicateTracker::default()),
            count_gate: Mutex::new(CountGate::default()),
            voice_data: VoiceDataCache::default(),
            runtime_batch,
            telemetry: SpeechTelemetryWriter::new(store.clone()),
            synthesizer,
            playback,
            synthesis,
            settings,
            now_ms,
            metrics,
        }
    }
}

impl<S, P> MessageVoiceService<S, P>
where
    S: CommandSpeechSynthesizer,
    P: CommandVoicePlayback,
{
    pub async fn execute(&self, invocation: MessageVoiceInvocation<'_>) -> MessageVoiceOutcome {
        let outcome = self.execute_inner(invocation).await;
        use vozen_core::MessageSpeechDenial as Denial;
        let metric = match &outcome {
            MessageVoiceOutcome::Denied(reason) => match reason {
                Denial::Disabled => Some(OperationalMetric::ReadDisabled),
                Denial::NotInSameVoice => Some(OperationalMetric::ReadWrongCall),
                Denial::RequiredRoleMissing => Some(OperationalMetric::ReadRoleMissing),
                Denial::PassiveOptOut => Some(OperationalMetric::ReadOptedOut),
                Denial::Blocked => Some(OperationalMetric::ReadBlocked),
                Denial::BoundVoiceMismatch => Some(OperationalMetric::ReadProfileMismatch),
                Denial::NotTriggered | Denial::BotsDisabled => None,
            },
            MessageVoiceOutcome::RateLimited => Some(OperationalMetric::ReadRateLimited),
            MessageVoiceOutcome::Busy => Some(OperationalMetric::ReadQueueBusy),
            MessageVoiceOutcome::SpamSuppressed => Some(OperationalMetric::ReadSpamSuppressed),
            MessageVoiceOutcome::FullyBlocked => Some(OperationalMetric::ReadContentBlocked),
            MessageVoiceOutcome::SynthesisFailed => Some(OperationalMetric::ReadSynthesisFailed),
            MessageVoiceOutcome::PlaybackFailed => Some(OperationalMetric::ReadPlaybackFailed),
            MessageVoiceOutcome::StoreUnavailable => Some(OperationalMetric::ReadStoreUnavailable),
            MessageVoiceOutcome::Queued { .. } => Some(OperationalMetric::TtsQueued),
            MessageVoiceOutcome::Empty => None,
        };
        if let Some(metric) = metric {
            self.record_metric(metric);
        }
        outcome
    }

    async fn execute_inner(&self, invocation: MessageVoiceInvocation<'_>) -> MessageVoiceOutcome {
        let received_at = Instant::now();
        let now_ms = (self.now_ms)();
        let mut voice_data = match self.voice_data.snapshot(
            &self.read_store,
            invocation.facts.guild_id,
            invocation.facts.channel_id,
            invocation.facts.author_id,
            now_ms,
        ) {
            Ok(data) => data,
            Err(_) => return MessageVoiceOutcome::StoreUnavailable,
        };
        {
            let Ok(store) = self.store.lock() else {
                return MessageVoiceOutcome::StoreUnavailable;
            };
            if voice_data
                .refresh_live_settings(
                    &store,
                    invocation.facts.guild_id,
                    invocation.facts.channel_id,
                    invocation.facts.author_id,
                )
                .is_err()
            {
                return MessageVoiceOutcome::StoreUnavailable;
            }
        }
        let lane = {
            match admit_discord_message_with_data(&voice_data.admission, invocation.facts) {
                MessageSpeechDecision::Allowed { lane } => lane,
                MessageSpeechDecision::Denied { reason } => {
                    return MessageVoiceOutcome::Denied(reason);
                }
            }
        };

        let prepared = {
            let mut pipeline = match self.pipeline.lock() {
                Ok(pipeline) => pipeline,
                Err(_) => return MessageVoiceOutcome::StoreUnavailable,
            };
            pipeline.prepare_after_admission_with_data(
                &voice_data.preparation,
                lane,
                MessagePreparationInput {
                    guild_id: invocation.facts.guild_id,
                    channel_id: invocation.facts.channel_id,
                    use_channel_profile: true,
                    include_server_pronunciations: true,
                    user_id: invocation.facts.author_id,
                    raw: invocation.raw,
                    max_chars_override: None,
                    available_models: &self.settings.available_models,
                    runtime_default_voice: &self.settings.default_voice,
                    runtime_default_speed: self.settings.default_speed,
                    runtime_default_engine: self.settings.default_engine,
                    detected_language: invocation.detected_language,
                    announce_speaker: invocation.announce_speaker,
                    media: invocation.media,
                    resolve_user: invocation.resolve_user,
                    resolve_channel: invocation.resolve_channel,
                },
                now_ms,
            )
        };

        let (request, cleaned_text, antispam) = match prepared {
            MessagePipelineOutcome::Ready {
                lane: prepared_lane,
                speech,
                cleaned_text,
                antispam,
            } if prepared_lane == lane => (speech.request, cleaned_text, antispam),
            // The lane was determined by the same admission above. Treat a discrepancy as a
            // store/process fault rather than accidentally granting a different priority.
            MessagePipelineOutcome::Ready { .. } | MessagePipelineOutcome::Denied(_) => {
                return MessageVoiceOutcome::StoreUnavailable;
            }
            MessagePipelineOutcome::Empty => return MessageVoiceOutcome::Empty,
            MessagePipelineOutcome::RateLimited => return MessageVoiceOutcome::RateLimited,
            MessagePipelineOutcome::FullyBlocked => return MessageVoiceOutcome::FullyBlocked,
        };

        if antispam
            && self.is_spam(
                invocation.facts.guild_id,
                invocation.facts.author_id,
                &cleaned_text,
                now_ms,
            )
        {
            return MessageVoiceOutcome::SpamSuppressed;
        }

        let admitted_generation = self
            .synthesis
            .admission_generation(invocation.facts.guild_id);
        let gate_started = Instant::now();
        let mut synthesis = self
            .synthesis
            .acquire(invocation.facts.guild_id, admitted_generation)
            .await;
        let gate_wait_ms = elapsed_ms(gate_started);
        self.metrics.record_gate_wait_ms(gate_wait_ms);
        if synthesis.was_cleared() {
            return MessageVoiceOutcome::PlaybackFailed;
        }
        synthesis.activate();

        let reserve_started = Instant::now();
        let reserve_result = self.playback.reserve(invocation.facts.guild_id, lane).await;
        self.metrics
            .record_reserve_latency_ms(elapsed_ms(reserve_started));
        match reserve_result {
            Ok(true) => {}
            Ok(false) => {
                self.record_metric(OperationalMetric::QueueDrop);
                return MessageVoiceOutcome::Busy;
            }
            Err(_) => return MessageVoiceOutcome::PlaybackFailed,
        }
        if synthesis.cancelled() {
            let _ = self
                .playback
                .cancel_reservation(invocation.facts.guild_id, lane)
                .await;
            return MessageVoiceOutcome::PlaybackFailed;
        }
        let synth_started = Instant::now();
        let wav = match self.synthesizer.synthesize_measured(&request).await {
            Ok((wav, provider)) => {
                self.record_synthesis_health(true, provider);
                let day = utc_day_key_from_unix_millis((self.now_ms)());
                let duration = elapsed_ms(synth_started).min(i64::MAX as u64) as i64;
                self.telemetry.record_metric(
                    &day,
                    OperationalMetric::SynthLatencyMs,
                    provider,
                    duration,
                );
                self.runtime_batch.record_metric(
                    &day,
                    OperationalMetric::SynthLatencyMs,
                    provider,
                    duration,
                );
                wav
            }
            Err((_, provider)) => {
                self.record_synthesis_health(false, provider);
                let _ = self
                    .playback
                    .cancel_reservation(invocation.facts.guild_id, lane)
                    .await;
                return MessageVoiceOutcome::SynthesisFailed;
            }
        };
        let synth_ms = elapsed_ms(synth_started);
        if synthesis.cancelled() {
            let _ = self
                .playback
                .cancel_reservation(invocation.facts.guild_id, lane)
                .await;
            return MessageVoiceOutcome::PlaybackFailed;
        }
        let enqueue_started = Instant::now();
        let enqueue_result = self
            .playback
            .enqueue_reserved(
                invocation.facts.guild_id,
                Path::new(&wav),
                QueueEnqueueOptions {
                    author_id: Some(invocation.facts.author_id),
                    source: QueueSource::Message,
                    lane,
                    created_at_ms: now_ms.max(0) as u64,
                },
            )
            .await;
        let enqueue_ms = elapsed_ms(enqueue_started);
        self.metrics.record_enqueue_latency_ms(enqueue_ms);
        let total_ms = elapsed_ms(received_at);
        if total_ms >= 1_000 {
            eprintln!(
                "[voice:latency] slow request total_ms={total_ms} gate_wait_ms={gate_wait_ms} synth_ms={synth_ms} enqueue_ms={enqueue_ms}"
            );
        }
        match enqueue_result {
            Ok(()) => {
                let talk = if self.should_count(
                    invocation.facts.guild_id,
                    invocation.facts.author_id,
                    &cleaned_text,
                    now_ms,
                ) {
                    self.record_accepted_speech(
                        invocation.facts.guild_id,
                        invocation.facts.author_id,
                        &request.model,
                        request.engine,
                        now_ms,
                    )
                } else {
                    None
                };
                MessageVoiceOutcome::Queued { talk }
            }
            Err(_) => {
                let _ = self
                    .playback
                    .cancel_reservation(invocation.facts.guild_id, lane)
                    .await;
                MessageVoiceOutcome::PlaybackFailed
            }
        }
    }

    /// Flushes best-effort speech telemetry for graceful shutdown and deterministic tests.
    pub fn flush_telemetry(&self) {
        self.telemetry.flush();
    }

    /// Stops the telemetry worker after flushing queued events.
    pub fn shutdown_telemetry(&self) {
        self.telemetry.shutdown();
    }

    #[must_use]
    pub fn telemetry_dropped_events(&self) -> u64 {
        self.telemetry.dropped_events()
    }

    pub fn forget_guild(&self, guild_id: &str) {
        if let Ok(mut pipeline) = self.pipeline.lock() {
            pipeline.forget_guild(guild_id);
        }
        self.synthesis.forget_guild(guild_id);
        self.voice_data.forget_guild(guild_id);
    }

    /// Writes only fixed, identity-free operational counters. Metric persistence is strictly
    /// best-effort: telemetry can never make an otherwise valid speech request fail.
    fn record_metric(&self, metric: OperationalMetric) {
        let now = (self.now_ms)();
        let day = utc_day_key_from_unix_millis(now);
        self.runtime_batch
            .record_metric(&day, metric, OperationalProvider::Internal, 1);
        self.telemetry
            .record_metric(&day, metric, OperationalProvider::Internal, 1);
    }

    fn is_spam(&self, guild_id: &str, user_id: &str, cleaned: &str, now_ms: i64) -> bool {
        // Preserve Node ordering: even a repetition-spam message updates the fixed duplicate
        // window, while a suppressed duplicate itself does not renew that window.
        let duplicate = self
            .duplicate_tracker
            .lock()
            .ok()
            .is_some_and(|mut tracker| {
                tracker.is_duplicate_spam(guild_id, user_id, cleaned, now_ms)
            });
        is_repetition_spam(cleaned) || duplicate
    }

    fn should_count(&self, guild_id: &str, user_id: &str, cleaned: &str, now_ms: i64) -> bool {
        self.count_gate
            .lock()
            .ok()
            .is_some_and(|mut gate| gate.should_count(guild_id, user_id, cleaned, now_ms))
    }

    fn record_synthesis_health(&self, success: bool, provider: OperationalProvider) {
        let now = (self.now_ms)();
        let day = utc_day_key_from_unix_millis(now);
        let metric = if success {
            OperationalMetric::SynthSuccess
        } else {
            OperationalMetric::SynthFailure
        };
        self.runtime_batch.record_metric(&day, metric, provider, 1);
        let health = if success {
            ProviderHealth::Healthy
        } else {
            ProviderHealth::Degraded
        };
        self.telemetry.record_metric(&day, metric, provider, 1);
        self.telemetry.record_provider_health(provider, health, now);
    }

    /// Mirrors the Node rule: usage changes only after playback accepted the rendered request.
    /// The established `talk_usage` aggregate is best-effort and contains neither message text
    /// nor audio; failure to update the dashboard counter cannot undo valid playback.
    fn record_accepted_speech(
        &self,
        guild_id: &str,
        user_id: &str,
        model: &str,
        engine: SynthesisEngine,
        now_ms: i64,
    ) -> Option<TalkBump> {
        // `talk_stats` feeds the user-visible streak announcement and remains synchronous. The
        // other post-queue aggregates are best-effort telemetry and are persisted by the bounded
        // worker so an SQLite lock cannot delay an accepted speech acknowledgement.
        let day = utc_day_key_from_unix_millis(now_ms);
        self.runtime_batch.record_accepted_speech(
            &day,
            guild_id,
            user_id,
            model,
            user_engine(engine),
        );
        let talk = self
            .store
            .lock()
            .ok()
            .and_then(|store| store.bump_talk(guild_id, user_id, &day).ok());
        self.telemetry.record_guild_talk(guild_id, &day);
        self.telemetry
            .record_talk_usage(guild_id, user_id, model, user_engine(engine));
        talk
    }
}

fn user_engine(engine: SynthesisEngine) -> UserEngine {
    match engine {
        // `Default` and `Neural` are runtime routes, not persisted UserEngine values. Node
        // records those legacy/default routes as `google` in talk_usage.
        SynthesisEngine::Piper => UserEngine::Piper,
        SynthesisEngine::Kokoro => UserEngine::Kokoro,
        SynthesisEngine::Gcloud => UserEngine::Gcloud,
        SynthesisEngine::Default | SynthesisEngine::Neural => UserEngine::Google,
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use vozen_core::{QueueLane, SynthRequest, SynthesisEngine};
    use vozen_store::{
        DominantTalkUsageOptions, GuildConfigPatch, OperationalMetric, OperationalProvider,
        SqliteStore, TalkUsageSource, UserVoice,
    };

    use super::*;
    use crate::{CommandPlaybackError, CommandPlaybackState, CommandSynthesisError};

    #[derive(Default)]
    struct FakeSynthesizer(AtomicUsize);

    #[async_trait]
    impl CommandSpeechSynthesizer for FakeSynthesizer {
        fn provider(&self) -> OperationalProvider {
            OperationalProvider::Piper
        }
        async fn synthesize(
            &self,
            _request: &SynthRequest,
        ) -> Result<PathBuf, CommandSynthesisError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(PathBuf::from("voice.wav"))
        }
    }

    #[derive(Default)]
    struct CapturingSynthesizer(Mutex<Vec<SynthRequest>>);

    #[async_trait]
    impl CommandSpeechSynthesizer for CapturingSynthesizer {
        async fn synthesize(
            &self,
            request: &SynthRequest,
        ) -> Result<PathBuf, CommandSynthesisError> {
            self.0
                .lock()
                .expect("captured requests")
                .push(request.clone());
            Ok(PathBuf::from("voice.wav"))
        }
    }

    struct FakePlayback {
        reserve: bool,
        enqueued: AtomicUsize,
    }

    #[async_trait]
    impl CommandVoicePlayback for FakePlayback {
        async fn state(
            &self,
            _guild_id: &str,
        ) -> Result<CommandPlaybackState, CommandPlaybackError> {
            Ok(CommandPlaybackState::Idle)
        }

        async fn reserve(
            &self,
            _guild_id: &str,
            _lane: QueueLane,
        ) -> Result<bool, CommandPlaybackError> {
            Ok(self.reserve)
        }

        async fn enqueue_reserved(
            &self,
            _guild_id: &str,
            _wav: &Path,
            _options: QueueEnqueueOptions<'_>,
        ) -> Result<(), CommandPlaybackError> {
            self.enqueued.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn cancel_reservation(
            &self,
            _guild_id: &str,
            _lane: QueueLane,
        ) -> Result<(), CommandPlaybackError> {
            Ok(())
        }

        async fn skip(&self, _guild_id: &str) -> Result<(), CommandPlaybackError> {
            Ok(())
        }

        async fn silence(&self, _guild_id: &str) -> Result<(), CommandPlaybackError> {
            Ok(())
        }
    }

    fn settings() -> CoreVoiceSettings {
        CoreVoiceSettings {
            available_models: vec!["en_US-amy-medium".into()],
            default_voice: "en_US-amy-medium".into(),
            default_speed: 1.0,
            default_engine: vozen_core::SynthesisEngine::Piper,
        }
    }

    fn invocation<'a>(author_voice: Option<&'a str>) -> MessageVoiceInvocation<'a> {
        invocation_with_raw(author_voice, "hello")
    }

    fn invocation_with_raw<'a>(
        author_voice: Option<&'a str>,
        raw: &'a str,
    ) -> MessageVoiceInvocation<'a> {
        MessageVoiceInvocation {
            facts: DiscordMessageFacts {
                guild_id: "guild",
                channel_id: "text",
                author_id: "user",
                author_is_bot: false,
                mentioned_bot: false,
                replied_to_bot: false,
                author_voice_channel_id: author_voice,
                bot_voice_channel_id: Some("voice"),
                member_role_ids: Some(&[]),
                autojoined_for_author: false,
            },
            raw,
            media: &[],
            detected_language: None,
            announce_speaker: None,
            resolve_user: &|_| "user".into(),
            resolve_channel: &|_| "channel".into(),
        }
    }

    fn configured_store() -> Arc<Mutex<SqliteStore>> {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    autoread: Some(true),
                    tts_channel_id: Some(Some("text".into())),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("config");
        Arc::new(Mutex::new(store))
    }

    #[tokio::test]
    async fn same_call_denial_happens_before_synthesis_or_queue_reservation() {
        let synthesizer = FakeSynthesizer::default();
        let service = MessageVoiceService::new(
            configured_store(),
            synthesizer,
            FakePlayback {
                reserve: true,
                enqueued: AtomicUsize::new(0),
            },
            settings(),
            Arc::new(|| 0),
        );

        assert_eq!(
            service.execute(invocation(Some("other"))).await,
            MessageVoiceOutcome::Denied(MessageSpeechDenial::NotInSameVoice)
        );
        assert_eq!(service.synthesizer.0.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_full_queue_does_not_spend_a_piper_synthesis() {
        let synthesizer = FakeSynthesizer::default();
        let service = MessageVoiceService::new(
            configured_store(),
            synthesizer,
            FakePlayback {
                reserve: false,
                enqueued: AtomicUsize::new(0),
            },
            settings(),
            Arc::new(|| 0),
        );

        assert_eq!(
            service.execute(invocation(Some("voice"))).await,
            MessageVoiceOutcome::Busy
        );
        assert_eq!(service.synthesizer.0.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn queue_and_synthesis_metrics_stay_identity_free_and_best_effort() {
        let store = configured_store();
        let service = MessageVoiceService::new(
            store.clone(),
            FakeSynthesizer::default(),
            FakePlayback {
                reserve: false,
                enqueued: AtomicUsize::new(0),
            },
            settings(),
            Arc::new(|| 0),
        );

        assert_eq!(
            service.execute(invocation(Some("voice"))).await,
            MessageVoiceOutcome::Busy
        );
        service.flush_telemetry();
        assert!(
            store
                .lock()
                .expect("store")
                .list_daily_operational_metrics(Some("1970-01-01"))
                .expect("metrics")
                .iter()
                .any(|row| {
                    row.metric == OperationalMetric::QueueDrop
                        && row.provider == OperationalProvider::Internal
                        && row.value == 1
                })
        );
    }

    #[tokio::test]
    async fn successful_piper_synthesis_records_health_without_message_content() {
        let store = configured_store();
        let service = MessageVoiceService::new(
            store.clone(),
            FakeSynthesizer::default(),
            FakePlayback {
                reserve: true,
                enqueued: AtomicUsize::new(0),
            },
            settings(),
            Arc::new(|| 0),
        );

        assert!(matches!(
            service.execute(invocation(Some("voice"))).await,
            MessageVoiceOutcome::Queued { talk: Some(_) }
        ));
        service.flush_telemetry();
        let store = store.lock().expect("store");
        assert!(
            store
                .list_daily_operational_metrics(Some("1970-01-01"))
                .expect("metrics")
                .iter()
                .any(|row| {
                    row.metric == OperationalMetric::SynthSuccess
                        && row.provider == OperationalProvider::Piper
                        && row.value == 1
                })
        );
        assert!(
            store
                .list_provider_health()
                .expect("health")
                .iter()
                .any(|row| row.provider == OperationalProvider::Piper
                    && row.health == ProviderHealth::Healthy)
        );
        let usage = store
            .dominant_talk_usage(&["user".into()], DominantTalkUsageOptions::default())
            .expect("usage");
        assert_eq!(
            usage.get("user"),
            Some(&vozen_store::DominantTalkUsage {
                language: Some("en_US".into()),
                engine: Some(UserEngine::Piper),
                samples: 1,
                source: TalkUsageSource::Measured,
            })
        );
        let top = store
            .top_speakers("guild", "1970-01-01", 10)
            .expect("top speakers");
        assert_eq!(
            top.first().map(|row| (&row.user_id, row.count, row.streak)),
            Some((&"user".to_string(), 1, 1))
        );
        assert_eq!(
            store
                .guild_talk_streak("guild", "1970-01-01")
                .expect("guild streak")
                .streak,
            1
        );
    }

    #[test]
    fn accepted_usage_maps_runtime_engines_to_persisted_engines() {
        assert_eq!(user_engine(SynthesisEngine::Default), UserEngine::Google);
        assert_eq!(user_engine(SynthesisEngine::Neural), UserEngine::Google);
        assert_eq!(user_engine(SynthesisEngine::Piper), UserEngine::Piper);
        assert_eq!(user_engine(SynthesisEngine::Kokoro), UserEngine::Kokoro);
        assert_eq!(user_engine(SynthesisEngine::Gcloud), UserEngine::Gcloud);
    }

    #[tokio::test]
    async fn opt_in_antispam_suppresses_repetition_before_synthesis() {
        let store = configured_store();
        store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    antispam: Some(true),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("antispam");
        let synthesizer = FakeSynthesizer::default();
        let service = MessageVoiceService::new(
            store,
            synthesizer,
            FakePlayback {
                reserve: true,
                enqueued: AtomicUsize::new(0),
            },
            settings(),
            Arc::new(|| 0),
        );

        assert_eq!(
            service
                .execute(invocation_with_raw(
                    Some("voice"),
                    "poke poke poke poke poke poke poke poke poke poke"
                ))
                .await,
            MessageVoiceOutcome::SpamSuppressed
        );
        assert_eq!(service.synthesizer.0.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn count_gate_keeps_repeated_queue_entries_out_of_usage_aggregates() {
        let store = configured_store();
        let service = MessageVoiceService::new(
            store.clone(),
            FakeSynthesizer::default(),
            FakePlayback {
                reserve: true,
                enqueued: AtomicUsize::new(0),
            },
            settings(),
            Arc::new(|| 0),
        );

        assert!(matches!(
            service.execute(invocation(Some("voice"))).await,
            MessageVoiceOutcome::Queued { talk: Some(_) }
        ));
        assert!(matches!(
            service.execute(invocation(Some("voice"))).await,
            MessageVoiceOutcome::Queued { talk: None }
        ));
        service.flush_telemetry();
        let usage = store
            .lock()
            .expect("store")
            .dominant_talk_usage(&["user".into()], DominantTalkUsageOptions::default())
            .expect("usage");
        assert_eq!(usage.get("user").map(|usage| usage.samples), Some(1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn accepts_twenty_five_concurrent_voice_requests_across_independent_servers() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        for index in 0..25 {
            store
                .lock()
                .expect("store")
                .update_guild_config(
                    &format!("guild-{index}"),
                    GuildConfigPatch {
                        autoread: Some(true),
                        tts_channel_id: Some(Some("text".into())),
                        ..GuildConfigPatch::default()
                    },
                )
                .expect("configure guild");
        }
        let service = Arc::new(MessageVoiceService::new(
            store,
            FakeSynthesizer::default(),
            FakePlayback {
                reserve: true,
                enqueued: AtomicUsize::new(0),
            },
            settings(),
            Arc::new(|| 0),
        ));
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..25 {
            let service = service.clone();
            tasks.spawn(async move {
                let guild_id = format!("guild-{index}");
                let user_id = format!("user-{index}");
                let resolve_user = |_: &str| "user".to_owned();
                let resolve_channel = |_: &str| "channel".to_owned();
                service
                    .execute(MessageVoiceInvocation {
                        facts: DiscordMessageFacts {
                            guild_id: &guild_id,
                            channel_id: "text",
                            author_id: &user_id,
                            author_is_bot: false,
                            mentioned_bot: false,
                            replied_to_bot: false,
                            author_voice_channel_id: Some("voice"),
                            bot_voice_channel_id: Some("voice"),
                            member_role_ids: Some(&[]),
                            autojoined_for_author: false,
                        },
                        raw: "parallel hello",
                        media: &[],
                        detected_language: None,
                        announce_speaker: None,
                        resolve_user: &resolve_user,
                        resolve_channel: &resolve_channel,
                    })
                    .await
            });
        }
        let mut accepted = 0;
        while let Some(result) = tasks.join_next().await {
            assert!(matches!(
                result.expect("voice task"),
                MessageVoiceOutcome::Queued { .. }
            ));
            accepted += 1;
        }
        assert_eq!(accepted, 25);
        assert_eq!(service.synthesizer.0.load(Ordering::Relaxed), 25);
        assert_eq!(service.playback.enqueued.load(Ordering::Relaxed), 25);
    }

    #[tokio::test]
    async fn stale_replica_cannot_enable_reading_without_authoritative_setup() {
        let write_store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("writer")));
        let read_store = configured_store();
        let service =
            MessageVoiceService::new_with_synthesis_coordinator_runtime_batch_and_read_store(
                write_store.clone(),
                read_store,
                FakeSynthesizer::default(),
                FakePlayback {
                    reserve: true,
                    enqueued: AtomicUsize::new(0),
                },
                GuildSynthesisCoordinator::default(),
                settings(),
                Arc::new(|| 0),
                RuntimeBatchBuffer::default(),
            );
        assert_eq!(
            service.execute(invocation(Some("voice"))).await,
            MessageVoiceOutcome::Denied(MessageSpeechDenial::NotTriggered)
        );
        assert!(
            write_store
                .lock()
                .expect("writer")
                .guild_config("guild")
                .expect("writer config")
                .tts_channel_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn voice_channel_reading_uses_live_settings_even_with_a_warm_stale_replica() {
        let write_store = configured_store();
        let read_store = configured_store();
        for store in [&write_store, &read_store] {
            store
                .lock()
                .expect("store")
                .update_guild_config(
                    "guild",
                    GuildConfigPatch {
                        text_in_voice: Some(false),
                        ..GuildConfigPatch::default()
                    },
                )
                .expect("disabled initially");
        }
        let service =
            MessageVoiceService::new_with_synthesis_coordinator_runtime_batch_and_read_store(
                write_store.clone(),
                read_store,
                FakeSynthesizer::default(),
                FakePlayback {
                    reserve: true,
                    enqueued: AtomicUsize::new(0),
                },
                GuildSynthesisCoordinator::default(),
                settings(),
                Arc::new(|| 0),
                RuntimeBatchBuffer::default(),
            );
        let voice_message = || {
            let mut message = invocation(Some("voice"));
            message.facts.channel_id = "voice";
            message
        };
        assert_eq!(
            service.execute(voice_message()).await,
            MessageVoiceOutcome::Denied(MessageSpeechDenial::NotTriggered)
        );
        write_store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    text_in_voice: Some(true),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("enable live");
        assert!(matches!(
            service.execute(voice_message()).await,
            MessageVoiceOutcome::Queued { .. }
        ));

        let mut other_call = voice_message();
        other_call.facts.author_voice_channel_id = Some("another-call");
        assert_eq!(
            service.execute(other_call).await,
            MessageVoiceOutcome::Denied(MessageSpeechDenial::NotInSameVoice)
        );
        write_store
            .lock()
            .expect("store")
            .set_opt_out("guild", "user")
            .expect("opt out");
        assert_eq!(
            service.execute(voice_message()).await,
            MessageVoiceOutcome::Denied(MessageSpeechDenial::PassiveOptOut)
        );
        write_store
            .lock()
            .expect("store")
            .set_opt_in("guild", "user")
            .expect("opt in");

        write_store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    tts_role_id: Some(Some("required".into())),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("require role");
        assert_eq!(
            service.execute(voice_message()).await,
            MessageVoiceOutcome::Denied(MessageSpeechDenial::RequiredRoleMissing)
        );
        write_store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    tts_role_id: Some(None),
                    text_in_voice: Some(false),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("disable live");
        assert_eq!(
            service.execute(voice_message()).await,
            MessageVoiceOutcome::Denied(MessageSpeechDenial::NotTriggered)
        );
        assert!(matches!(
            service.execute(invocation(Some("voice"))).await,
            MessageVoiceOutcome::Queued { .. }
        ));
        assert_eq!(service.synthesizer.0.load(Ordering::Relaxed), 2);
        assert_eq!(service.playback.enqueued.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn personal_voice_change_is_visible_before_the_postgres_snapshot_refreshes() {
        let write_store = configured_store();
        let read_store = configured_store();
        let old_voice = UserVoice {
            model: "en_US-amy-medium".into(),
            speed: 1.0,
            engine: UserEngine::Google,
        };
        for store in [&write_store, &read_store] {
            store
                .lock()
                .expect("store")
                .set_user_voice("guild", "user", &old_voice)
                .expect("old voice");
        }
        let service =
            MessageVoiceService::new_with_synthesis_coordinator_runtime_batch_and_read_store(
                write_store.clone(),
                read_store,
                CapturingSynthesizer::default(),
                FakePlayback {
                    reserve: true,
                    enqueued: AtomicUsize::new(0),
                },
                GuildSynthesisCoordinator::default(),
                CoreVoiceSettings {
                    available_models: vec!["en_US-amy-medium".into(), "es_ES-davefx-medium".into()],
                    ..settings()
                },
                Arc::new(|| 0),
                RuntimeBatchBuffer::default(),
            );

        assert!(matches!(
            service.execute(invocation(Some("voice"))).await,
            MessageVoiceOutcome::Queued { .. }
        ));
        write_store
            .lock()
            .expect("writer")
            .set_user_voice(
                "guild",
                "user",
                &UserVoice {
                    model: "es_ES-davefx-medium".into(),
                    speed: 1.0,
                    engine: UserEngine::Piper,
                },
            )
            .expect("new voice");
        assert!(matches!(
            service.execute(invocation(Some("voice"))).await,
            MessageVoiceOutcome::Queued { .. }
        ));

        let requests = service
            .synthesizer
            .0
            .lock()
            .expect("captured requests")
            .iter()
            .map(|request| (request.model.clone(), request.engine))
            .collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec![
                ("en_US-amy-medium".to_owned(), SynthesisEngine::Default),
                ("es_ES-davefx-medium".to_owned(), SynthesisEngine::Piper),
            ]
        );
    }
}
