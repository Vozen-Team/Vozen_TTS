//! Testable orchestration for promoted slash commands, without owning Serenity responses.
//!
//! The eventual gateway sink must defer `/tts` before invoking this executor. Keeping Discord's
//! response token outside the service means the same authorization/synthesis path is covered by
//! unit tests and cannot be accidentally reused for an unvalidated interaction.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use serenity::model::application::CommandData;
use thiserror::Error;
use vozen_store::SqliteStore;

use crate::{
    CommandSpeechSynthesizer, CommandVoicePlayback, CoreVoiceInteractionFacts, CoreVoiceOutcome,
    CoreVoiceResponse, CoreVoiceService, CoreVoiceSettings, GatewayState,
    GuildSynthesisCoordinator, LeaveVoiceOutcome, VoiceResponseLocalizer,
    VoiceResponseLocalizerError, VoiceSessionTransport, core_voice_response,
    parse_promoted_core_voice,
};

#[derive(Debug, Error)]
pub enum CoreVoiceExecutionError {
    #[error("promoted voice command is invalid")]
    Command,
    #[error("voice response localisation failed: {0}")]
    Localizer(#[from] VoiceResponseLocalizerError),
    #[error("voice response could not be rendered")]
    MissingResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreVoiceInteractionExecution {
    /// The caller must leave this interaction to the still-authoritative Node process.
    NotPromoted,
    /// The caller owns the interaction and must send this exact localized response.
    Reply {
        content: String,
        /// `/tts` can wait for Piper; all other promoted commands complete immediately.
        defer_ephemeral: bool,
        /// Micro-fun replies are public in Node, but may still wait for optional speech.
        defer_public: bool,
    },
}

pub struct CoreVoiceInteractionExecutor<T, S, P> {
    store: Arc<Mutex<SqliteStore>>,
    gateway_state: GatewayState,
    service: CoreVoiceService<T, S, P>,
    localizer: VoiceResponseLocalizer,
}

impl<T, S, P> CoreVoiceInteractionExecutor<T, S, P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        gateway_state: GatewayState,
        transport: T,
        synthesizer: S,
        playback: P,
        settings: CoreVoiceSettings,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Result<Self, CoreVoiceExecutionError> {
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
    ) -> Result<Self, CoreVoiceExecutionError> {
        Ok(Self {
            service: CoreVoiceService::new_with_synthesis_coordinator(
                store.clone(),
                gateway_state.clone(),
                transport,
                synthesizer,
                playback,
                synthesis,
                settings,
                now_ms,
            ),
            store,
            gateway_state,
            localizer: VoiceResponseLocalizer::from_generated_contract()?,
        })
    }

    /// Checks the versioned command payload before the gateway spends an interaction response.
    /// A malformed payload is not deferred: responding to an unknown/forged command would make
    /// Rust claim traffic it cannot safely own.
    pub fn requires_ephemeral_defer(
        command: &CommandData,
    ) -> Result<bool, CoreVoiceExecutionError> {
        Ok(matches!(
            parse_promoted_core_voice(command).map_err(|_| CoreVoiceExecutionError::Command)?,
            Some(
                crate::CoreVoiceCommand::Tts { .. }
                    | crate::CoreVoiceCommand::Laugh
                    | crate::CoreVoiceCommand::Joke { .. }
                    | crate::CoreVoiceCommand::Rizz { .. }
                    | crate::CoreVoiceCommand::Sound { .. }
                    | crate::CoreVoiceCommand::VoicePreview { .. },
            )
        ))
    }

    pub fn requires_public_defer(command: &CommandData) -> Result<bool, CoreVoiceExecutionError> {
        Ok(matches!(
            parse_promoted_core_voice(command).map_err(|_| CoreVoiceExecutionError::Command)?,
            Some(crate::CoreVoiceCommand::MicroFun { .. })
        ))
    }
}

impl<T, S, P> CoreVoiceInteractionExecutor<T, S, P>
where
    T: VoiceSessionTransport,
    S: CommandSpeechSynthesizer,
    P: CommandVoicePlayback,
{
    pub async fn execute(
        &self,
        command: &CommandData,
        facts: &CoreVoiceInteractionFacts,
        interaction_locale: Option<&str>,
        resolve_user: &(dyn Fn(&str) -> String + Send + Sync),
        resolve_channel: &(dyn Fn(&str) -> String + Send + Sync),
    ) -> Result<CoreVoiceInteractionExecution, CoreVoiceExecutionError> {
        let Some(command) =
            parse_promoted_core_voice(command).map_err(|_| CoreVoiceExecutionError::Command)?
        else {
            return Ok(CoreVoiceInteractionExecution::NotPromoted);
        };
        let defer_ephemeral = matches!(
            command,
            crate::CoreVoiceCommand::Tts { .. }
                | crate::CoreVoiceCommand::Laugh
                | crate::CoreVoiceCommand::Joke { .. }
                | crate::CoreVoiceCommand::Rizz { .. }
                | crate::CoreVoiceCommand::Sound { .. }
                | crate::CoreVoiceCommand::VoicePreview { .. }
        );
        let defer_public = matches!(command, crate::CoreVoiceCommand::MicroFun { .. });
        let outcome = if let crate::CoreVoiceCommand::VoicePreview { model, engine } = &command {
            let guild_locale = self.guild_locale(facts);
            let configured_model = model
                .is_none()
                .then(|| self.service.preview_model(&facts.guild_id, &facts.user_id))
                .flatten();
            let selected_model = model.as_deref().or(configured_model.as_deref());
            let sample_locale = selected_model.and_then(preview_locale_for_model);
            let sample = self
                .localizer
                .render_key(
                    "preview.sample",
                    sample_locale.as_deref().or(interaction_locale),
                    guild_locale.as_deref(),
                    &BTreeMap::new(),
                )
                .ok_or(CoreVoiceExecutionError::MissingResponse)?;
            CoreVoiceOutcome::Preview(
                self.service
                    .execute_preview(
                        facts.invocation(resolve_user, resolve_channel),
                        model.as_deref(),
                        engine.as_deref(),
                        &sample,
                    )
                    .await,
            )
        } else {
            self.service
                .execute(facts.invocation(resolve_user, resolve_channel), &command)
                .await
        };
        let (response, parameters, guild_locale) = self.response_context(outcome, facts);
        let content = self
            .localizer
            .render(
                response,
                if defer_ephemeral {
                    interaction_locale
                } else {
                    None
                },
                if defer_ephemeral {
                    None
                } else {
                    guild_locale.as_deref()
                },
                &parameters,
            )
            .ok_or(CoreVoiceExecutionError::MissingResponse)?;
        Ok(CoreVoiceInteractionExecution::Reply {
            content,
            defer_ephemeral,
            defer_public,
        })
    }

    /// Speaks a short runtime-generated line through the exact same admission, rate-limit and
    /// queue path as `/tts`. This is deliberately not a Discord response helper: callers remain
    /// responsible for choosing whether the originating interaction is public or ephemeral.
    pub async fn speak_text(
        &self,
        facts: &CoreVoiceInteractionFacts,
        text: &str,
    ) -> CoreVoiceOutcome {
        self.service
            .execute(
                facts.invocation(&|_| "someone".to_owned(), &|_| "a channel".to_owned()),
                &crate::CoreVoiceCommand::Tts {
                    text: text.to_owned(),
                },
            )
            .await
    }

    /// Localizes the outcome of a message context-menu Speak invocation using the same response
    /// mapping and guild-locale fallback as the promoted slash-command path.
    pub fn render_speak_outcome(
        &self,
        outcome: CoreVoiceOutcome,
        facts: &CoreVoiceInteractionFacts,
        interaction_locale: Option<&str>,
    ) -> Result<String, CoreVoiceExecutionError> {
        let (response, parameters, _) = self.response_context(outcome, facts);
        self.localizer
            .render(response, interaction_locale, None, &parameters)
            .ok_or(CoreVoiceExecutionError::MissingResponse)
    }

    /// Renders a fixed response-catalog key for a voice-adjacent interaction outcome.
    pub fn render_key(
        &self,
        key: &str,
        facts: &CoreVoiceInteractionFacts,
        interaction_locale: Option<&str>,
    ) -> Result<String, CoreVoiceExecutionError> {
        let guild_locale = self.guild_locale(facts);
        self.localizer
            .render_key(
                key,
                interaction_locale,
                guild_locale.as_deref(),
                &BTreeMap::new(),
            )
            .ok_or(CoreVoiceExecutionError::MissingResponse)
    }

    /// Joins the caller's current voice channel without requiring a synthetic Discord command
    /// payload. Used by the atomic `/setup` onboarding flow after its permission checklist.
    pub async fn join_for_setup(&self, facts: &CoreVoiceInteractionFacts) -> CoreVoiceOutcome {
        self.service
            .execute(
                facts.invocation(&|_| "someone".to_owned(), &|_| "a channel".to_owned()),
                &crate::CoreVoiceCommand::Join,
            )
            .await
    }

    /// Joins the author's current voice channel for the one message that triggered an enabled
    /// auto-join policy. It shares the exact `/join` transport and durable-presence ordering.
    pub async fn join_for_message(&self, facts: &CoreVoiceInteractionFacts) -> CoreVoiceOutcome {
        self.join_for_setup(facts).await
    }

    /// Leaves a call from a gateway lifecycle rule using the same durable cleanup as `/leave`.
    pub async fn leave_for_lifecycle(&self, guild_id: &str) -> LeaveVoiceOutcome {
        self.service.leave_for_lifecycle(guild_id).await
    }

    /// Speaks a generated line with an explicitly validated model/engine, while retaining the
    /// service's ordinary admission and queue safeguards.
    pub async fn speak_text_with_voice(
        &self,
        facts: &CoreVoiceInteractionFacts,
        text: &str,
        model: &str,
        speed: f64,
        engine: vozen_core::SynthesisEngine,
        enforce_rate_limit: bool,
    ) -> CoreVoiceOutcome {
        CoreVoiceOutcome::Tts(
            self.service
                .execute_custom_speech(
                    facts.invocation(&|_| "someone".to_owned(), &|_| "a channel".to_owned()),
                    text,
                    model,
                    speed,
                    engine,
                    enforce_rate_limit,
                )
                .await,
        )
    }

    fn response_context(
        &self,
        outcome: CoreVoiceOutcome,
        facts: &CoreVoiceInteractionFacts,
    ) -> (
        CoreVoiceResponse,
        BTreeMap<&'static str, String>,
        Option<String>,
    ) {
        let joke_text = match &outcome {
            CoreVoiceOutcome::Joke(result) => result.joke.clone(),
            _ => None,
        };
        let rizz_line = match &outcome {
            CoreVoiceOutcome::Rizz(result) => result.line.clone(),
            _ => None,
        };
        let sound_details = match &outcome {
            CoreVoiceOutcome::Sound(result) => Some((result.name.clone(), result.sounds.clone())),
            _ => None,
        };
        let microfun = match &outcome {
            CoreVoiceOutcome::MicroFun(result) => {
                Some((result.kind, result.question.clone(), result.text.clone()))
            }
            _ => None,
        };
        let mut response = core_voice_response(outcome);
        let guild = self
            .store
            .lock()
            .ok()
            .and_then(|store| store.guild_config(&facts.guild_id).ok());
        let guild_locale = guild.as_ref().map(|config| config.locale.clone());
        let mut parameters = BTreeMap::new();

        if response == CoreVoiceResponse::Joined {
            let Some(voice_channel_id) = self
                .gateway_state
                .voice_channel_id(&facts.guild_id, &facts.user_id)
            else {
                return (
                    CoreVoiceResponse::StoreUnavailable,
                    parameters,
                    guild_locale,
                );
            };
            parameters.insert("channel", channel_mention(&voice_channel_id));
            if let Some(read_channel_id) = guild.as_ref().and_then(|config| {
                (config.autoread)
                    .then_some(config.tts_channel_id.as_deref())
                    .flatten()
            }) {
                parameters.insert("readChannel", channel_mention(read_channel_id));
                response = CoreVoiceResponse::JoinedAutoread;
            }
        } else if response == CoreVoiceResponse::JoinPermissionDenied {
            let Some(voice_channel_id) = self
                .gateway_state
                .voice_channel_id(&facts.guild_id, &facts.user_id)
            else {
                return (
                    CoreVoiceResponse::StoreUnavailable,
                    parameters,
                    guild_locale,
                );
            };
            parameters.insert("channel", channel_mention(&voice_channel_id));
        }
        if let Some(joke) = joke_text {
            parameters.insert("joke", joke);
        }
        if let Some(line) = rizz_line {
            parameters.insert("line", line);
        }
        if let Some((name, sounds)) = sound_details {
            if let Some(name) = name {
                parameters.insert("name", name);
            }
            if let Some(sounds) = sounds {
                parameters.insert("sounds", sounds);
            }
        }
        if let Some((kind, question, text)) = microfun {
            if let Some(question) = question {
                parameters.insert("question", question);
            }
            parameters.insert(
                match kind {
                    crate::MicroFunKind::EightBall => "answer",
                    _ => "text",
                },
                text,
            );
        }
        (response, parameters, guild_locale)
    }

    fn guild_locale(&self, facts: &CoreVoiceInteractionFacts) -> Option<String> {
        self.store
            .lock()
            .ok()
            .and_then(|store| store.guild_config(&facts.guild_id).ok())
            .map(|config| config.locale)
    }

    pub fn forget_guild(&self, guild_id: &str) {
        self.service.forget_guild(guild_id);
    }
}

fn channel_mention(channel_id: &str) -> String {
    format!("<#{channel_id}>")
}

/// Piper/Gcloud model identifiers start with a BCP-47-ish locale, for example
/// `en_US-amy-medium` or `pt_PT-google-medium`. Preview samples must use that locale;
/// the Discord interaction locale is only a fallback when the model has no locale prefix.
fn preview_locale_for_model(model: &str) -> Option<String> {
    let locale = model.split_once('-')?.0.trim();
    (!locale.is_empty()).then(|| locale.replace('_', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CommandPlaybackError, CommandPlaybackState, CommandSynthesisError,
        VoiceSessionTransportError,
    };
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use vozen_core::{QueueEnqueueOptions, QueueLane, SynthRequest, SynthesisEngine};

    struct NoAudio;
    #[async_trait]
    impl VoiceSessionTransport for NoAudio {
        async fn join(&self, _: &str, _: &str) -> Result<(), VoiceSessionTransportError> {
            panic!("voice must not be joined")
        }
        async fn leave(&self, _: &str) -> Result<(), VoiceSessionTransportError> {
            panic!("voice must not be left")
        }
    }
    #[async_trait]
    impl CommandSpeechSynthesizer for NoAudio {
        async fn synthesize(&self, _: &SynthRequest) -> Result<PathBuf, CommandSynthesisError> {
            panic!("no authorized voice session")
        }
    }
    #[async_trait]
    impl CommandVoicePlayback for NoAudio {
        async fn state(&self, _: &str) -> Result<CommandPlaybackState, CommandPlaybackError> {
            Ok(CommandPlaybackState::NoSession)
        }
        async fn reserve(&self, _: &str, _: QueueLane) -> Result<bool, CommandPlaybackError> {
            panic!("no authorized voice session")
        }
        async fn enqueue_reserved(
            &self,
            _: &str,
            _: &Path,
            _: QueueEnqueueOptions<'_>,
        ) -> Result<(), CommandPlaybackError> {
            panic!("no authorized voice session")
        }
        async fn cancel_reservation(
            &self,
            _: &str,
            _: QueueLane,
        ) -> Result<(), CommandPlaybackError> {
            panic!("no authorized voice session")
        }
        async fn skip(&self, _: &str) -> Result<(), CommandPlaybackError> {
            panic!("no authorized voice session")
        }
        async fn silence(&self, _: &str) -> Result<(), CommandPlaybackError> {
            panic!("no authorized voice session")
        }
    }

    #[tokio::test]
    async fn private_responses_follow_the_user_and_public_responses_follow_saved_server_language() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap()));
        let executor = CoreVoiceInteractionExecutor::new(
            store.clone(),
            GatewayState::default(),
            NoAudio,
            NoAudio,
            NoAudio,
            CoreVoiceSettings {
                available_models: vec!["en_US-amy-medium".into()],
                default_voice: "en_US-amy-medium".into(),
                default_speed: 1.0,
                default_engine: SynthesisEngine::Piper,
            },
            Arc::new(|| 100),
        )
        .unwrap();
        let facts = CoreVoiceInteractionFacts {
            guild_id: "7".into(),
            channel_id: "77".into(),
            user_id: "42".into(),
            member_role_ids: Some(vec![]),
        };
        let localizer = VoiceResponseLocalizer::from_generated_contract().unwrap();
        for (saved, user, private) in [
            ("en", "pt-BR", false),
            ("pt", "en-US", false),
            ("en", "pt-PT", true),
            ("pt", "fr-CA", true),
        ] {
            store
                .lock()
                .unwrap()
                .update_guild_config(
                    "7",
                    vozen_store::GuildConfigPatch {
                        locale: Some(saved.into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            let data = if private {
                command(
                    r#"{"id":"1","name":"tts","type":1,"options":[{"name":"text","type":3,"value":"hello"}]}"#,
                )
            } else {
                command(r#"{"id":"1","name":"join","type":1,"options":[]}"#)
            };
            let result = executor
                .execute(&data, &facts, Some(user), &|_| "someone".into(), &|_| {
                    "channel".into()
                })
                .await
                .unwrap();
            let CoreVoiceInteractionExecution::Reply {
                content,
                defer_ephemeral,
                ..
            } = result
            else {
                panic!("claimed command")
            };
            assert_eq!(defer_ephemeral, private);
            let expected = localizer
                .render_key(
                    if private {
                        "tts.notInVoice"
                    } else {
                        "join.needVoiceChannel"
                    },
                    Some(if private { user } else { saved }),
                    None,
                    &BTreeMap::new(),
                )
                .unwrap();
            assert_eq!(
                content, expected,
                "saved={saved}, user={user}, private={private}"
            );
        }
    }

    fn command(payload: &str) -> CommandData {
        serde_json::from_str(payload).expect("valid command data")
    }

    #[test]
    fn speech_commands_require_an_ephemeral_defer_and_unpromoted_commands_stay_unclaimed() {
        assert!(CoreVoiceInteractionExecutor::<(), (), ()>::requires_ephemeral_defer(&command(
            r#"{"id":"1","name":"tts","type":1,"options":[{"name":"text","type":3,"value":"hello"}]}"#
        ))
        .expect("tts"));
        assert!(
            CoreVoiceInteractionExecutor::<(), (), ()>::requires_ephemeral_defer(&command(
                r#"{"id":"1","name":"laugh","type":1,"options":[]}"#
            ))
            .expect("laugh")
        );
        let preview = command(
            r#"{"id":"1","name":"voice","type":1,"options":[{"name":"preview","type":1,"options":[]}] }"#,
        );
        assert_eq!(
            crate::parse_promoted_core_voice(&preview).expect("preview parse"),
            Some(crate::CoreVoiceCommand::VoicePreview {
                model: None,
                engine: None,
            })
        );
        assert!(
            CoreVoiceInteractionExecutor::<(), (), ()>::requires_ephemeral_defer(&preview)
                .expect("voice preview")
        );
        assert!(
            !CoreVoiceInteractionExecutor::<(), (), ()>::requires_ephemeral_defer(&command(
                r#"{"id":"1","name":"join","type":1,"options":[]}"#
            ))
            .expect("join")
        );
        assert!(!CoreVoiceInteractionExecutor::<(), (), ()>::requires_ephemeral_defer(&command(
            r#"{"id":"1","name":"queue","type":1,"options":[{"name":"show","type":1,"options":[]}]}"#
        ))
        .expect("unpromoted"));
    }

    #[test]
    fn public_micro_fun_commands_defer_publicly_but_other_commands_do_not() {
        let eight_ball = command(
            r#"{"id":"1","name":"8-ball","type":1,"options":[{"name":"question","type":3,"value":"Will it work?"},{"name":"language","type":3,"value":"en"},{"name":"engine","type":3,"value":"piper"}]}"#,
        );
        assert!(
            CoreVoiceInteractionExecutor::<(), (), ()>::requires_public_defer(&eight_ball)
                .expect("8-ball")
        );
        assert!(
            !CoreVoiceInteractionExecutor::<(), (), ()>::requires_ephemeral_defer(&eight_ball)
                .expect("8-ball")
        );
        assert!(!CoreVoiceInteractionExecutor::<(), (), ()>::requires_public_defer(&command(
            r#"{"id":"1","name":"joke","type":1,"options":[{"name":"language","type":3,"value":"en"},{"name":"laughter","type":5,"value":false}]}"#,
        ))
        .expect("joke"));
    }

    #[test]
    fn channel_mentions_are_safe_discord_references_not_untrusted_names() {
        assert_eq!(channel_mention("123"), "<#123>");
    }

    #[test]
    fn preview_sample_locale_follows_voice_model() {
        assert_eq!(
            preview_locale_for_model("en_US-amy-medium").as_deref(),
            Some("en-US")
        );
        assert_eq!(
            preview_locale_for_model("pt_PT-google-medium").as_deref(),
            Some("pt-PT")
        );
        assert_eq!(preview_locale_for_model("model"), None);
    }
}
