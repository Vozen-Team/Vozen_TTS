//! Private `/tts-file` preparation and synthesis, independent of a voice call.
//!
//! This service deliberately has no Discord response/upload code. The gateway adapter owns the
//! ephemeral attachment lifecycle, while this layer proves that export never joins a call, obeys
//! the existing per-scope rate limit, and only synthesizes a fully validated request.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use vozen_core::{GuildRateLimiters, detect_language, has_readable_text};
use vozen_store::SqliteStore;

use crate::{
    CommandSpeechSynthesizer, CoreVoiceSettings, MessagePreparationInput,
    MessagePreparationOutcome, prepare_message_speech,
};

pub const MAX_TTS_FILE_CHARS: usize = 500;

/// The preference scope is the guild id in a server or the stable `@user-app` scope in a DM/user
/// install. It is not a Discord call and must never be used to infer voice membership.
pub struct TtsFileExportInvocation<'a> {
    /// `Some` only for a guild invocation. It controls shared server pronunciations and never
    /// represents voice membership.
    pub guild_id: Option<&'a str>,
    pub preference_scope: &'a str,
    pub user_id: &'a str,
    pub raw: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsFileExportOutcome {
    TooLong,
    Empty,
    RateLimited,
    FullyBlocked,
    VoiceUnavailable,
    SynthesisFailed,
    Ready(PathBuf),
    StoreUnavailable,
}

pub struct TtsFileExportService<S> {
    store: Arc<Mutex<SqliteStore>>,
    rate_limiters: Mutex<GuildRateLimiters>,
    synthesizer: S,
    settings: CoreVoiceSettings,
    now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl<S> TtsFileExportService<S> {
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        synthesizer: S,
        settings: CoreVoiceSettings,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            store,
            rate_limiters: Mutex::new(GuildRateLimiters::default()),
            synthesizer,
            settings,
            now_ms,
        }
    }
}

impl<S> TtsFileExportService<S>
where
    S: CommandSpeechSynthesizer,
{
    pub async fn execute(&self, invocation: TtsFileExportInvocation<'_>) -> TtsFileExportOutcome {
        self.execute_with_synthesizer(invocation, &self.synthesizer)
            .await
    }

    /// Reuses the same preparation policy and rate limit while the caller owns an isolated
    /// synthesis workspace. Private exports must not share a persistent voice cache.
    pub async fn execute_with_synthesizer(
        &self,
        invocation: TtsFileExportInvocation<'_>,
        synthesizer: &impl CommandSpeechSynthesizer,
    ) -> TtsFileExportOutcome {
        let raw = invocation.raw.trim();
        // Mirror the command handler: reject non-readable input before spending a token.
        if !has_readable_text(raw) {
            return TtsFileExportOutcome::Empty;
        }
        // Discord validates `max_length`, but the application must retain the bound for old/stale
        // commands and forged payloads. JavaScript's existing contract counts UTF-16 code units.
        if raw.encode_utf16().count() > MAX_TTS_FILE_CHARS {
            return TtsFileExportOutcome::TooLong;
        }
        let now_ms = (self.now_ms)();
        let prepared = {
            let store = match self.store.lock() {
                Ok(store) => store,
                Err(_) => return TtsFileExportOutcome::StoreUnavailable,
            };
            let mut limiters = match self.rate_limiters.lock() {
                Ok(limiters) => limiters,
                Err(_) => return TtsFileExportOutcome::StoreUnavailable,
            };
            let config = match store.guild_config(invocation.preference_scope) {
                Ok(config) => config,
                Err(_) => return TtsFileExportOutcome::StoreUnavailable,
            };
            if !limiters.allow(
                invocation.preference_scope,
                invocation.user_id,
                config.rate_per_min,
                now_ms,
            ) {
                return TtsFileExportOutcome::RateLimited;
            }
            let detected_language = store
                .is_detection_on(invocation.preference_scope, invocation.user_id)
                .ok()
                .filter(|enabled| *enabled)
                .and_then(|_| detect_language(raw));
            prepare_message_speech(
                &store,
                MessagePreparationInput {
                    guild_id: invocation.preference_scope,
                    channel_id: "@tts-file",
                    use_channel_profile: false,
                    include_server_pronunciations: invocation.guild_id.is_some(),
                    user_id: invocation.user_id,
                    raw,
                    max_chars_override: Some(MAX_TTS_FILE_CHARS),
                    available_models: &self.settings.available_models,
                    runtime_default_voice: &self.settings.default_voice,
                    runtime_default_speed: self.settings.default_speed,
                    runtime_default_engine: self.settings.default_engine,
                    detected_language,
                    announce_speaker: None,
                    media: &[],
                    resolve_user: &|_| "someone".to_owned(),
                    resolve_channel: &|_| "channel".to_owned(),
                },
            )
        };

        let request = match prepared {
            Ok(MessagePreparationOutcome::Ready(speech)) => speech.request,
            Ok(MessagePreparationOutcome::Empty) => return TtsFileExportOutcome::Empty,
            Ok(MessagePreparationOutcome::FullyBlocked) => {
                return TtsFileExportOutcome::FullyBlocked;
            }
            Err(_) => return TtsFileExportOutcome::StoreUnavailable,
        };
        if !request_models_available(&request, &self.settings.available_models) {
            return TtsFileExportOutcome::VoiceUnavailable;
        }
        synthesizer
            .synthesize(&request)
            .await
            .map(TtsFileExportOutcome::Ready)
            .unwrap_or(TtsFileExportOutcome::SynthesisFailed)
    }

    pub fn forget_scope(&self, scope: &str) {
        if let Ok(mut limiters) = self.rate_limiters.lock() {
            limiters.forget_guild(scope);
        }
    }
}

fn request_models_available(request: &vozen_core::SynthRequest, available: &[String]) -> bool {
    available.iter().any(|model| model == &request.model)
        && request.segments.as_deref().is_none_or(|segments| {
            segments
                .iter()
                .all(|segment| available.iter().any(|model| model == &segment.model))
        })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use vozen_store::{GuildConfigPatch, SqliteStore};

    use super::*;
    use crate::CommandSynthesisError;

    #[derive(Default)]
    struct FakeSynthesizer {
        calls: AtomicUsize,
        last_request: Mutex<Option<vozen_core::SynthRequest>>,
    }

    #[async_trait]
    impl CommandSpeechSynthesizer for FakeSynthesizer {
        async fn synthesize(
            &self,
            _request: &vozen_core::SynthRequest,
        ) -> Result<PathBuf, CommandSynthesisError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            *self.last_request.lock().expect("request") = Some(_request.clone());
            Ok(PathBuf::from("export.wav"))
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

    #[tokio::test]
    async fn private_export_request_synthesizers_share_rate_limit_and_never_use_default_cache() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    rate_per_min: Some(1),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("config");
        let service = TtsFileExportService::new(
            store,
            FakeSynthesizer::default(),
            settings(),
            Arc::new(|| 0),
        );
        let first = FakeSynthesizer::default();
        let second = FakeSynthesizer::default();
        let invocation = || TtsFileExportInvocation {
            guild_id: Some("guild"),
            preference_scope: "guild",
            user_id: "user",
            raw: "private text",
        };
        assert!(matches!(
            service.execute_with_synthesizer(invocation(), &first).await,
            TtsFileExportOutcome::Ready(_)
        ));
        assert_eq!(
            service
                .execute_with_synthesizer(invocation(), &second)
                .await,
            TtsFileExportOutcome::RateLimited
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 0);
        assert_eq!(first.calls.load(Ordering::Relaxed), 1);
        assert_eq!(second.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn export_never_needs_voice_state_and_enforces_the_private_cap() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let synthesizer = FakeSynthesizer::default();
        let service = TtsFileExportService::new(store, synthesizer, settings(), Arc::new(|| 0));
        let oversized = "a".repeat(MAX_TTS_FILE_CHARS + 1);
        assert_eq!(
            service
                .execute(TtsFileExportInvocation {
                    guild_id: None,
                    preference_scope: "@user-app",
                    user_id: "user",
                    raw: "hello",
                })
                .await,
            TtsFileExportOutcome::Ready(PathBuf::from("export.wav"))
        );
        assert_eq!(
            service
                .execute(TtsFileExportInvocation {
                    guild_id: None,
                    preference_scope: "@user-app",
                    user_id: "user",
                    raw: &oversized,
                })
                .await,
            TtsFileExportOutcome::TooLong
        );
        assert_eq!(service.synthesizer.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn export_uses_the_existing_scope_rate_limit_before_synthesis() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    rate_per_min: Some(1),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("config");
        let service = TtsFileExportService::new(
            store,
            FakeSynthesizer::default(),
            settings(),
            Arc::new(|| 0),
        );
        let invocation = || TtsFileExportInvocation {
            guild_id: Some("guild"),
            preference_scope: "guild",
            user_id: "user",
            raw: "hello",
        };
        assert!(matches!(
            service.execute(invocation()).await,
            TtsFileExportOutcome::Ready(_)
        ));
        assert_eq!(
            service.execute(invocation()).await,
            TtsFileExportOutcome::RateLimited
        );
    }

    #[tokio::test]
    async fn export_respects_existing_blocklists_without_storing_text() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        store
            .lock()
            .expect("store")
            .add_blockword("guild", "secret")
            .expect("block");
        let service = TtsFileExportService::new(
            store,
            FakeSynthesizer::default(),
            settings(),
            Arc::new(|| 0),
        );
        assert_eq!(
            service
                .execute(TtsFileExportInvocation {
                    guild_id: Some("guild"),
                    preference_scope: "guild",
                    user_id: "user",
                    raw: "secret",
                })
                .await,
            TtsFileExportOutcome::FullyBlocked
        );
    }

    #[tokio::test]
    async fn private_export_rejects_empty_input_before_the_rate_limit() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        store
            .lock()
            .expect("store")
            .update_guild_config(
                "@user-app",
                GuildConfigPatch {
                    rate_per_min: Some(1),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("config");
        let service = TtsFileExportService::new(
            store,
            FakeSynthesizer::default(),
            settings(),
            Arc::new(|| 0),
        );
        assert_eq!(
            service
                .execute(TtsFileExportInvocation {
                    guild_id: None,
                    preference_scope: "@user-app",
                    user_id: "user",
                    raw: "  \u{1f600}  ",
                })
                .await,
            TtsFileExportOutcome::Empty
        );
        assert!(matches!(
            service
                .execute(TtsFileExportInvocation {
                    guild_id: None,
                    preference_scope: "@user-app",
                    user_id: "user",
                    raw: "hello",
                })
                .await,
            TtsFileExportOutcome::Ready(_)
        ));
    }

    #[tokio::test]
    async fn private_export_never_inherits_server_pronunciations() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        store
            .lock()
            .expect("store")
            .add_server_pronunciation("@user-app", "hello", "bonjour", 3)
            .expect("server pronunciation");
        let service = TtsFileExportService::new(
            store,
            FakeSynthesizer::default(),
            settings(),
            Arc::new(|| 0),
        );
        assert!(matches!(
            service
                .execute(TtsFileExportInvocation {
                    guild_id: None,
                    preference_scope: "@user-app",
                    user_id: "user",
                    raw: "hello",
                })
                .await,
            TtsFileExportOutcome::Ready(_)
        ));
        assert_eq!(
            service
                .synthesizer
                .last_request
                .lock()
                .expect("request")
                .as_ref()
                .expect("synthesized")
                .text,
            "hello"
        );
    }
}
