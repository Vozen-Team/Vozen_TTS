//! Opt-in gateway sink for the first fully migrated voice slash commands.
//!
//! Construction is lazy because Serenity only exposes a valid [`Context`] from a gateway event.
//! Until the runtime explicitly installs this sink, Node remains the interaction authority.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use uuid::Uuid;

use async_trait::async_trait;
use serenity::{
    builder::{
        CreateActionRow, CreateAllowedMentions, CreateAttachment, CreateButton, CreateEmbed,
        CreateInputText, CreateInteractionResponse, CreateInteractionResponseFollowup,
        CreateInteractionResponseMessage, CreateMessage, CreateModal, CreateSelectMenu,
        CreateSelectMenuKind, CreateSelectMenuOption, CreateThread, EditInteractionResponse,
    },
    client::Context,
    http::{LightMethod, Request, Route},
    model::{
        Permissions,
        application::{
            ButtonStyle, CommandType, ComponentInteractionDataKind, InputTextStyle, Interaction,
        },
        channel::ChannelType,
        id::{ChannelId, GuildId, UserId},
    },
};
use vozen_core::{PublicQueueItem, QueueLane, QueueSource, SynthesisEngine, detect_language};
use vozen_discord::{
    CAST_LANGUAGE_CHOICES, CAST_MAX_MEMBERS, CAST_THEMES, CastAction, CastMember, CastSession,
    CoreTtsOutcome, CoreVoiceInteractionExecution, CoreVoiceInteractionExecutor,
    CoreVoiceInteractionFacts, CoreVoiceOutcome, DiscordDashboardOptionsProvider,
    DiscordMessageFactsOwned, GAME_CATALOG, GameCoordinator, GameDriverFactory, GameManagerEvent,
    GamePlayAdmission, GamePlayRequest, GameStanding, GameStartOutcome, GatewayEventDispatchError,
    GatewayEventSink, GatewayState, GuildSynthesisCoordinator, JoinVoiceOutcome,
    MessageVoiceInvocation, MessageVoiceOutcome, MessageVoiceService, PlannedRejoinService,
    QueueControlInvocation, QueueControlOutcome, QueueControlService, RandomizerCommand,
    RandomizerSession, RejoinChannelState, RenderedGameAction, SongbirdCommandPlayback,
    SongbirdVoiceSessionTransport, VoiceResponseLocalizer, build_greeting, collect_message_media,
    consume_planned_rejoin_marker, is_join_into_channel, parse_amount_component_id,
    parse_cast_component_id, parse_fill_component_id, parse_game_play_command,
    parse_game_stop_command, parse_modal_options, parse_queue_command, parse_randomizer_command,
    parse_setup_command, parse_speak_message_command, pick_option, render_game_action,
    render_game_finish,
};
use vozen_store::CommunityPromoKind;
use vozen_store::{GuildConfigPatch, RuntimeBatchBuffer, SqliteStore, utc_day_key};

use crate::ui::{color_for_content, danger_embed, message_embed};
use crate::{
    CoreVoiceRuntimeOptions,
    activity_poster::{LeaderboardPoster, VotePromoPoster, random_unit},
    engine_router::PerUserCommandSynthesizer,
    piper_adapter::PiperCommandSynthesizer,
    streak_card::{build_streak_card, card_avatar_url, fetch_avatar_data_url},
    streak_style::style_streak_message,
    system_now_ms,
};

type Executor = CoreVoiceInteractionExecutor<
    SongbirdVoiceSessionTransport,
    PerUserCommandSynthesizer,
    SongbirdCommandPlayback,
>;
type MessageService = MessageVoiceService<PerUserCommandSynthesizer, SongbirdCommandPlayback>;

fn no_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
}

struct VoiceDependencies {
    synthesizer: PerUserCommandSynthesizer,
    playback: SongbirdCommandPlayback,
    synthesis: GuildSynthesisCoordinator,
}

const GAME_PICK_TTL_MS: i64 = 60_000;
const MAX_PENDING_GAME_PICKS: usize = 1024;
const GAME_THREAD_DELETE_DELAY: Duration = Duration::from_secs(5);
const GAME_LANGUAGE_CHOICES: [(&str, &str); 4] = [
    ("en", "English"),
    ("pt", "Portuguese"),
    ("es", "Spanish"),
    ("fr", "French"),
];
const GAME_ENGINE_CHOICES: [(&str, &str); 3] = [
    ("google", "Google (gTTS)"),
    ("piper", "Piper"),
    ("kokoro", "Kokoro"),
];
const SETUP_TIMEOUT: Duration = Duration::from_secs(20);
const GREET_COOLDOWN_MS: i64 = 5 * 60 * 1_000;
const GREET_COOLDOWN_MAX_ENTRIES: usize = 10_000;
const CAST_NOT_IN_VOICE: &str =
    "Ainda nao estou num canal de voz — entra num e corre /join, depois tenta outra vez.";

#[derive(Clone)]
struct PendingGamePick {
    guild_id: String,
    parent_channel_id: String,
    user_id: String,
    selected_game: Option<String>,
    language: Option<String>,
    engine: String,
    locale: String,
    guild_locale: Option<String>,
    issued_at_ms: i64,
}

/// Runtime state for the live `/game play|stop` surface. It lives beside the core voice sink so
/// games reuse the already-authorized Piper/Songbird executor instead of creating a second
/// synthesis queue. The coordinator itself remains transport-free in `vozen-discord`.
struct GameRuntime {
    store: Arc<Mutex<SqliteStore>>,
    gateway_state: GatewayState,
    coordinator: Arc<Mutex<GameCoordinator>>,
    pending_picks: Mutex<BTreeMap<String, PendingGamePick>>,
    session_engines: Mutex<BTreeMap<String, SynthesisEngine>>,
    localizer: VoiceResponseLocalizer,
    available_models: Vec<String>,
    default_voice: String,
    default_speed: f64,
    default_engine: SynthesisEngine,
    executor: Mutex<Option<Arc<Executor>>>,
    tick_started: AtomicBool,
}

pub struct CoreVoiceGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    voice_read_store: Arc<Mutex<SqliteStore>>,
    gateway_state: GatewayState,
    options: CoreVoiceRuntimeOptions,
    localizer: Option<VoiceResponseLocalizer>,
    leaderboard_poster: Mutex<LeaderboardPoster>,
    vote_promo_poster: Mutex<VotePromoPoster>,
    dependencies: Mutex<Option<Arc<VoiceDependencies>>>,
    executor: Mutex<Option<Arc<Executor>>>,
    message_service: Mutex<Option<Arc<MessageService>>>,
    last_speakers: Mutex<BTreeMap<String, String>>,
    randomizer_sessions: Mutex<BTreeMap<String, RandomizerSession>>,
    cast_sessions: Mutex<BTreeMap<String, CastSession>>,
    greet_cooldown: Mutex<BTreeMap<String, i64>>,
    rejoin_attempted: AtomicBool,
    game_runtime: Option<Arc<GameRuntime>>,
    runtime_batch: RuntimeBatchBuffer,
}

impl GameRuntime {
    fn render(
        &self,
        key: &str,
        locale: Option<&str>,
        guild_locale: Option<&str>,
        parameters: &BTreeMap<&str, String>,
    ) -> Result<String, GatewayEventDispatchError> {
        self.localizer
            .render_key(key, locale, guild_locale, parameters)
            .ok_or(GatewayEventDispatchError)
    }

    fn prune_picks(&self, now_ms: i64) {
        if let Ok(mut picks) = self.pending_picks.lock() {
            Self::prune_picks_locked(&mut picks, now_ms);
        }
    }

    fn prune_picks_locked(picks: &mut BTreeMap<String, PendingGamePick>, now_ms: i64) {
        picks.retain(|_, pick| now_ms.saturating_sub(pick.issued_at_ms) <= GAME_PICK_TTL_MS);
        if picks.len() > MAX_PENDING_GAME_PICKS {
            let mut old = picks
                .iter()
                .map(|(id, pick)| (id.clone(), pick.issued_at_ms))
                .collect::<Vec<_>>();
            old.sort_by_key(|(_, issued_at)| *issued_at);
            for (id, _) in old.into_iter().take(picks.len() - MAX_PENDING_GAME_PICKS) {
                picks.remove(&id);
            }
        }
    }

    fn bot_speech_facts(
        &self,
        guild_id: &str,
        channel_id: &str,
    ) -> Option<CoreVoiceInteractionFacts> {
        let bot_id = self.gateway_state.bot_user_id()?;
        self.gateway_state
            .bot_voice_channel_id(guild_id)
            .map(|_| CoreVoiceInteractionFacts {
                guild_id: guild_id.to_owned(),
                channel_id: channel_id.to_owned(),
                user_id: bot_id,
                // Game narration is trusted bot-generated speech. `None` means the Discord
                // member cache is missing and deliberately fails closed when queue-role policy
                // is configured; a present empty list keeps the standard lane without applying
                // a human member's blocked/priority roles to the bot itself.
                member_role_ids: Some(Vec::new()),
            })
    }

    async fn send_content(
        &self,
        context: &Context,
        channel_id: &str,
        content: String,
    ) -> Result<(), GatewayEventDispatchError> {
        let channel_id = channel_id
            .parse::<u64>()
            .map(ChannelId::new)
            .map_err(|_| GatewayEventDispatchError)?;
        let body = game_card_body(&content).map_err(|_| GatewayEventDispatchError)?;
        context
            .http
            .request(
                Request::new(Route::ChannelMessages { channel_id }, LightMethod::Post)
                    .body(Some(body)),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_rendered_actions(
        &self,
        context: &Context,
        guild_id: &str,
        channel_id: &str,
        locale: &str,
        guild_locale: Option<&str>,
        actions: &[RenderedGameAction],
        speech_allowed: bool,
    ) -> Result<(), GatewayEventDispatchError> {
        let executor = self
            .executor
            .lock()
            .ok()
            .and_then(|current| current.clone());
        let engine = self
            .session_engines
            .lock()
            .ok()
            .and_then(|engines| engines.get(guild_id).copied())
            .unwrap_or(self.default_engine);
        for rendered in actions {
            if let Some(content) = rendered.content(&self.localizer, Some(locale), guild_locale) {
                self.send_content(context, channel_id, content).await?;
            }
            let Some(speech) = rendered.speech.as_ref() else {
                continue;
            };
            if !speech_allowed {
                continue;
            }
            let Some(executor) = executor.clone() else {
                continue;
            };
            let Some(facts) = self.bot_speech_facts(guild_id, channel_id) else {
                continue;
            };
            let Some(speech_text) = speech.render(&self.localizer, Some(locale), guild_locale)
            else {
                continue;
            };
            let model = speech
                .model
                .as_deref()
                .filter(|model| {
                    self.available_models
                        .iter()
                        .any(|candidate| candidate == model)
                })
                .unwrap_or_else(|| self.model_for_locale(locale));
            let speed = speech.speed.unwrap_or(self.default_speed);
            let _ = executor
                .speak_text_with_voice(&facts, &speech_text, model, speed, engine, false)
                .await;
        }
        Ok(())
    }

    fn model_for_locale(&self, locale: &str) -> &str {
        let base = locale
            .split(['-', '_'])
            .next()
            .unwrap_or(locale)
            .to_ascii_lowercase();
        self.available_models
            .iter()
            .find(|model| {
                model
                    .split(['-', '_'])
                    .next()
                    .unwrap_or(model)
                    .eq_ignore_ascii_case(&base)
            })
            .map(String::as_str)
            .unwrap_or(self.default_voice.as_str())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_actions(
        &self,
        context: &Context,
        guild_id: &str,
        channel_id: &str,
        locale: &str,
        guild_locale: Option<&str>,
        actions: &[vozen_discord::GameDriverAction],
        speech_allowed: bool,
    ) -> Result<(), GatewayEventDispatchError> {
        let rendered = actions
            .iter()
            .filter_map(render_game_action)
            .collect::<Vec<_>>();
        self.send_rendered_actions(
            context,
            guild_id,
            channel_id,
            locale,
            guild_locale,
            &rendered,
            speech_allowed,
        )
        .await
    }

    async fn schedule_thread_delete(&self, context: &Context, channel_id: String) {
        let http = context.http.clone();
        tokio::spawn(async move {
            tokio::time::sleep(GAME_THREAD_DELETE_DELAY).await;
            let _ = ChannelId::new(channel_id.parse::<u64>().unwrap_or_default())
                .delete(&http)
                .await;
        });
    }

    async fn cleanup_forced(
        &self,
        context: &Context,
        session: Option<vozen_discord::GameSession>,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some(session) = session else {
            return Ok(());
        };
        if let Ok(mut engines) = self.session_engines.lock() {
            engines.remove(&session.guild_id);
        }
        let Some(parent) = session.parent_channel_id else {
            return Ok(());
        };
        let content = self.render(
            "game.thread.ended",
            Some(&session.locale),
            None,
            &BTreeMap::new(),
        )?;
        self.send_content(context, &parent, content).await?;
        self.schedule_thread_delete(context, session.channel_id)
            .await;
        Ok(())
    }

    async fn dispatch_event(
        &self,
        context: &Context,
        event: GameManagerEvent,
        speech_allowed: bool,
        guild_hint: Option<&str>,
    ) -> Result<(), GatewayEventDispatchError> {
        match event {
            GameManagerEvent::Consumed { actions } => {
                let (guild_id, channel_id, locale) = {
                    let coordinator = self
                        .coordinator
                        .lock()
                        .map_err(|_| GatewayEventDispatchError)?;
                    let Some(guild_id) = guild_hint else {
                        return Ok(());
                    };
                    let Some(session) = coordinator.session(guild_id) else {
                        return Ok(());
                    };
                    (session.guild_id, session.channel_id, session.locale)
                };
                self.send_actions(
                    context,
                    &guild_id,
                    &channel_id,
                    &locale,
                    None,
                    &actions,
                    speech_allowed,
                )
                .await?;
            }
            GameManagerEvent::Finished { session, actions } => {
                self.send_actions(
                    context,
                    &session.guild_id,
                    &session.channel_id,
                    &session.locale,
                    None,
                    &actions,
                    speech_allowed,
                )
                .await?;
                let points = session
                    .scores
                    .iter()
                    .map(|score| (score.user_id.clone(), score.points))
                    .collect::<Vec<_>>();
                if let Ok(store) = self.store.lock() {
                    let _ = store.persist_game_scores(&session.guild_id, &points);
                }
                if game_uses_shared_standings(&session.game_id) {
                    let mut standings = Vec::with_capacity(session.scores.len());
                    for score in &session.scores {
                        let name = match score.user_id.parse::<u64>() {
                            Ok(user_id) => UserId::new(user_id)
                                .to_user(&context.http)
                                .await
                                .map(|user| user.global_name.unwrap_or(user.name))
                                .unwrap_or_else(|_| format!("<@{}>", score.user_id)),
                            Err(_) => format!("<@{}>", score.user_id),
                        };
                        standings.push(GameStanding {
                            name,
                            points: score.points,
                        });
                    }
                    let finish = render_game_finish(&standings);
                    self.send_rendered_actions(
                        context,
                        &session.guild_id,
                        &session.channel_id,
                        &session.locale,
                        None,
                        &finish,
                        speech_allowed,
                    )
                    .await?;
                }
                if let Some(parent) = session.parent_channel_id {
                    let winner = session
                        .scores
                        .iter()
                        .filter(|score| score.points > 0)
                        .max_by_key(|score| score.points)
                        .map(|score| format!("<@{}>", score.user_id));
                    let content = if let Some(winner) = winner {
                        let mut parameters = BTreeMap::new();
                        parameters.insert("winner", winner);
                        self.render(
                            "game.thread.winner",
                            Some(&session.locale),
                            None,
                            &parameters,
                        )?
                    } else {
                        self.render(
                            "game.thread.ended",
                            Some(&session.locale),
                            None,
                            &BTreeMap::new(),
                        )?
                    };
                    self.send_content(context, &parent, content).await?;
                    self.schedule_thread_delete(context, session.channel_id)
                        .await;
                }
                if let Ok(mut engines) = self.session_engines.lock() {
                    engines.remove(&session.guild_id);
                }
            }
            GameManagerEvent::VoiceLeft => {
                self.cleanup_forced(context, None).await?;
            }
            GameManagerEvent::Stopped | GameManagerEvent::NoActiveGame => {}
        }
        Ok(())
    }

    fn start_tick(self: &Arc<Self>, context: Context, executor: Arc<Executor>) {
        if self.tick_started.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut current) = self.executor.lock() {
            *current = Some(executor);
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(250));
            loop {
                ticker.tick().await;
                let events = runtime
                    .coordinator
                    .lock()
                    .map(|mut coordinator| coordinator.advance_with_guild(system_now_ms()))
                    .unwrap_or_default();
                for (guild_id, event) in events {
                    let speech_allowed = runtime
                        .gateway_state
                        .bot_voice_channel_id(&guild_id)
                        .is_some();
                    let _ = runtime
                        .dispatch_event(&context, event, speech_allowed, Some(&guild_id))
                        .await;
                }
            }
        });
    }

    async fn create_game_thread(
        &self,
        context: &Context,
        parent_channel_id: &str,
        game_name: &str,
    ) -> Option<String> {
        let parent = parent_channel_id.parse::<u64>().ok().map(ChannelId::new)?;
        let name = format!("🎮 {}", game_name)
            .chars()
            .take(100)
            .collect::<String>();
        parent
            .create_thread(
                &context.http,
                CreateThread::new(name)
                    .kind(ChannelType::PublicThread)
                    .audit_log_reason("Vozen game session"),
            )
            .await
            .ok()
            .map(|channel| channel.id.get().to_string())
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_game(
        &self,
        context: &Context,
        guild_id: String,
        parent_channel_id: String,
        user_id: String,
        locale: String,
        guild_locale: Option<String>,
        game_id: Option<String>,
        language: Option<String>,
        engine: Option<String>,
    ) -> Result<String, GatewayEventDispatchError> {
        let Some(game_id) = game_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return self.render(
                "game.pickPrompt",
                Some(&locale),
                guild_locale.as_deref(),
                &BTreeMap::new(),
            );
        };
        let bot_voice_channel_id = self.gateway_state.bot_voice_channel_id(&guild_id);
        let now = system_now_ms();
        let (user_premium, guild_premium) = self
            .store
            .lock()
            .ok()
            .map(|store| {
                (
                    store.is_user_premium(&user_id, now).unwrap_or(false),
                    store.is_guild_premium(&guild_id, now).unwrap_or(false),
                )
            })
            .unwrap_or((false, false));
        let selected_engine = engine
            .as_deref()
            .and_then(Self::game_engine)
            .unwrap_or(self.default_engine);
        if selected_engine == SynthesisEngine::Kokoro && !user_premium && !guild_premium {
            return self.render(
                "voice.engine.kokoroLocked",
                Some(&locale),
                guild_locale.as_deref(),
                &BTreeMap::new(),
            );
        }
        let active_channel_id = self
            .coordinator
            .lock()
            .ok()
            .and_then(|coordinator| coordinator.channel_of(&guild_id).map(str::to_owned));
        let admission = vozen_discord::admit_game_play(vozen_discord::GamePlayAdmissionFacts {
            guild_id: Some(&guild_id),
            game_id: Some(game_id),
            bot_voice_channel_id: bot_voice_channel_id.as_deref(),
            active_channel_id: active_channel_id.as_deref(),
            user_premium,
            guild_premium,
        });
        if !matches!(admission, GamePlayAdmission::Allowed { .. }) {
            return self.render_admission(
                admission,
                &locale,
                guild_locale.as_deref(),
                game_id,
                active_channel_id.as_deref(),
            );
        }
        let server_locale = self
            .store
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .guild_config(&guild_id)
            .map_err(|_| GatewayEventDispatchError)?
            .locale;
        let game_locale = language.as_deref().unwrap_or(&server_locale).to_owned();
        let game_name = vozen_discord::game_definition(game_id)
            .and_then(|definition| {
                self.render(
                    definition.name_key,
                    Some(&game_locale),
                    guild_locale.as_deref(),
                    &BTreeMap::new(),
                )
                .ok()
            })
            .unwrap_or_else(|| game_id.to_owned());
        let thread_id = self
            .create_game_thread(context, &parent_channel_id, &game_name)
            .await;
        let game_channel_id = thread_id
            .clone()
            .unwrap_or_else(|| parent_channel_id.clone());
        let outcome = self
            .coordinator
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .start(GamePlayRequest {
                guild_id: Some(guild_id.clone()),
                parent_channel_id: parent_channel_id.clone(),
                game_channel_id: game_channel_id.clone(),
                starter_id: user_id,
                game_id: Some(game_id.to_owned()),
                language,
                locale: server_locale.clone(),
                bot_voice_channel_id,
                user_premium,
                guild_premium,
                seed: now,
                now_ms: now,
            })
            .map_err(|_| GatewayEventDispatchError)?;
        let actions = match &outcome {
            GameStartOutcome::Started { actions, .. } => actions.clone(),
            GameStartOutcome::PickRequired | GameStartOutcome::Rejected(_) => {
                if let Some(thread_id) = thread_id {
                    let _ = ChannelId::new(thread_id.parse::<u64>().unwrap_or_default())
                        .delete(&context.http)
                        .await;
                }
                return self.render_admission(
                    match &outcome {
                        GameStartOutcome::PickRequired => GamePlayAdmission::PickRequired,
                        GameStartOutcome::Rejected(reason) => *reason,
                        GameStartOutcome::Started { .. } => unreachable!(),
                    },
                    &locale,
                    guild_locale.as_deref(),
                    game_id,
                    active_channel_id.as_deref(),
                );
            }
        };
        self.session_engines
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .insert(guild_id.clone(), selected_engine);
        let speech_allowed = self.gateway_state.bot_voice_channel_id(&guild_id).is_some();
        self.send_actions(
            context,
            &guild_id,
            &game_channel_id,
            &game_locale,
            guild_locale.as_deref(),
            &actions,
            speech_allowed,
        )
        .await?;
        if let Some(thread_id) = thread_id {
            let mut parameters = BTreeMap::new();
            parameters.insert("game", game_name);
            parameters.insert("channel", thread_id);
            self.render(
                "game.start.startedThread",
                None,
                Some(&server_locale),
                &parameters,
            )
        } else {
            let mut parameters = BTreeMap::new();
            parameters.insert("game", game_name);
            self.render(
                "game.start.started",
                None,
                Some(&server_locale),
                &parameters,
            )
        }
    }

    fn render_admission(
        &self,
        admission: GamePlayAdmission,
        locale: &str,
        guild_locale: Option<&str>,
        game_id: &str,
        active_channel_id: Option<&str>,
    ) -> Result<String, GatewayEventDispatchError> {
        match admission {
            GamePlayAdmission::UnknownGame => {
                let mut parameters = BTreeMap::new();
                parameters.insert("game", game_id.to_owned());
                self.render("game.unknownGame", Some(locale), guild_locale, &parameters)
            }
            GamePlayAdmission::AlreadyActive => {
                let mut parameters = BTreeMap::new();
                parameters.insert(
                    "channel",
                    active_channel_id
                        .map(|channel| format!("<#{}>", channel))
                        .unwrap_or_else(|| "the current game".to_owned()),
                );
                self.render(
                    "game.start.alreadyActive",
                    Some(locale),
                    guild_locale,
                    &parameters,
                )
            }
            GamePlayAdmission::VoiceUnavailable => self.render(
                "game.start.needVoice",
                Some(locale),
                guild_locale,
                &BTreeMap::new(),
            ),
            GamePlayAdmission::PremiumRequired => {
                let mut parameters = BTreeMap::new();
                parameters.insert("game", game_id.to_owned());
                self.render(
                    "game.start.premiumLocked",
                    Some(locale),
                    guild_locale,
                    &parameters,
                )
            }
            GamePlayAdmission::PickRequired => self.render(
                "game.pickPrompt",
                Some(locale),
                guild_locale,
                &BTreeMap::new(),
            ),
            GamePlayAdmission::GuildOnly => self.render(
                "error.generic",
                Some(locale),
                guild_locale,
                &BTreeMap::new(),
            ),
            GamePlayAdmission::Allowed { .. } => self.render(
                "error.generic",
                Some(locale),
                guild_locale,
                &BTreeMap::new(),
            ),
        }
    }

    fn default_game_language(locale: &str) -> &'static str {
        let locale = locale.to_ascii_lowercase();
        if locale.starts_with("pt") {
            "pt"
        } else if locale.starts_with("es") {
            "es"
        } else if locale.starts_with("fr") {
            "fr"
        } else {
            "en"
        }
    }

    fn game_language_label(value: &str) -> &'static str {
        GAME_LANGUAGE_CHOICES
            .iter()
            .find(|(key, _)| *key == value)
            .map(|(_, label)| *label)
            .unwrap_or("English")
    }

    fn game_engine(value: &str) -> Option<SynthesisEngine> {
        match value {
            "google" => Some(SynthesisEngine::Default),
            "piper" => Some(SynthesisEngine::Piper),
            "kokoro" => Some(SynthesisEngine::Kokoro),
            _ => None,
        }
    }

    fn game_engine_label(value: &str) -> &'static str {
        GAME_ENGINE_CHOICES
            .iter()
            .find(|(key, _)| *key == value)
            .map(|(_, label)| *label)
            .unwrap_or("Google (gTTS)")
    }

    fn game_pick_components(
        &self,
        interaction_id: &str,
        locale: &str,
        guild_locale: Option<&str>,
        selected_game: Option<&str>,
        selected_language: &str,
        selected_engine: &str,
    ) -> Result<(String, Vec<CreateActionRow>), GatewayEventDispatchError> {
        let prompt = self.render(
            "game.pickPrompt",
            Some(locale),
            guild_locale,
            &BTreeMap::new(),
        )?;
        let game_placeholder = self.render(
            "game.pickPlaceholder",
            Some(locale),
            guild_locale,
            &BTreeMap::new(),
        )?;
        let language_label = if locale.to_ascii_lowercase().starts_with("pt") {
            "Idioma (Word Chain)"
        } else {
            "Language (Word Chain)"
        };
        let language_placeholder = if locale.to_ascii_lowercase().starts_with("pt") {
            "Escolhe uma língua (Word Chain)"
        } else {
            "Choose a language (Word Chain)"
        };
        let engine_label = if locale.to_ascii_lowercase().starts_with("pt") {
            "Motor"
        } else {
            "Engine"
        };
        let engine_placeholder = if locale.to_ascii_lowercase().starts_with("pt") {
            "Escolhe um motor de voz"
        } else {
            "Choose a voice engine"
        };
        let options = GAME_CATALOG
            .iter()
            .map(|game| {
                let label = self
                    .render(game.name_key, Some(locale), guild_locale, &BTreeMap::new())
                    .unwrap_or_else(|_| game.id.to_owned());
                let description = self
                    .render(game.desc_key, Some(locale), guild_locale, &BTreeMap::new())
                    .unwrap_or_default();
                CreateSelectMenuOption::new(label, game.id)
                    .default_selection(selected_game == Some(game.id))
                    .description(description.chars().take(100).collect::<String>())
            })
            .collect::<Vec<_>>();
        let languages = GAME_LANGUAGE_CHOICES
            .iter()
            .map(|(value, label)| {
                CreateSelectMenuOption::new(*label, *value)
                    .default_selection(*value == selected_language)
            })
            .collect::<Vec<_>>();
        let engines = GAME_ENGINE_CHOICES
            .iter()
            .map(|(value, label)| {
                CreateSelectMenuOption::new(*label, *value)
                    .default_selection(*value == selected_engine)
            })
            .collect::<Vec<_>>();
        let game_menu = CreateSelectMenu::new(
            format!("gamePick:{interaction_id}"),
            CreateSelectMenuKind::String { options },
        )
        .placeholder(game_placeholder)
        .min_values(1)
        .max_values(1);
        let language_menu = CreateSelectMenu::new(
            format!("gameLanguage:{interaction_id}"),
            CreateSelectMenuKind::String { options: languages },
        )
        .placeholder(language_placeholder)
        .min_values(1)
        .max_values(1);
        let engine_menu = CreateSelectMenu::new(
            format!("gameEngine:{interaction_id}"),
            CreateSelectMenuKind::String { options: engines },
        )
        .placeholder(engine_placeholder)
        .min_values(1)
        .max_values(1);
        let start_label = if locale.to_ascii_lowercase().starts_with("pt") {
            "Começar jogo"
        } else {
            "Start game"
        };
        let start_button = CreateButton::new(format!("gameStart:{interaction_id}"))
            .label(start_label)
            .style(ButtonStyle::Primary)
            .disabled(selected_game.is_none());
        let selected_game_label = selected_game
            .and_then(|id| GAME_CATALOG.iter().find(|game| game.id == id))
            .and_then(|game| {
                self.render(game.name_key, Some(locale), guild_locale, &BTreeMap::new())
                    .ok()
            });
        let content = format!(
            "{prompt}\n{language_label}: {}\n{engine_label}: {}{}",
            Self::game_language_label(selected_language),
            Self::game_engine_label(selected_engine),
            selected_game_label
                .map(|game| format!("\nGame: {game}"))
                .unwrap_or_default()
        );
        Ok((
            content,
            vec![
                CreateActionRow::SelectMenu(game_menu),
                CreateActionRow::SelectMenu(language_menu),
                CreateActionRow::SelectMenu(engine_menu),
                CreateActionRow::Buttons(vec![start_button]),
            ],
        ))
    }

    fn game_pick_payload(content: &str, rows: Vec<CreateActionRow>) -> Value {
        let rows = serde_json::to_value(rows).expect("game pick action rows are serializable");
        let mut children = vec![json!({"type": 10, "content": content})];
        if let Value::Array(rows) = rows {
            children.extend(rows);
        }
        json!({
            "type": 4,
            "data": {
                "flags": 32832,
                "components": [{
                    "type": 17,
                    "accent_color": 0x5865F2u32,
                    "components": children,
                }]
            }
        })
    }

    fn game_pick_edit_payload(content: &str, rows: Vec<CreateActionRow>) -> Value {
        let rows = serde_json::to_value(rows).expect("game pick action rows are serializable");
        let mut children = vec![json!({"type": 10, "content": content})];
        if let Value::Array(rows) = rows {
            children.extend(rows);
        }
        json!({
            "flags": 32768,
            "components": [{
                "type": 17,
                "accent_color": 0x5865F2u32,
                "components": children,
            }]
        })
    }

    async fn handle_command(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<bool, GatewayEventDispatchError> {
        let Some(guild_id) = command.guild_id else {
            return Ok(false);
        };
        let guild_id = guild_id.get().to_string();
        if let Some(play) =
            parse_game_play_command(&command.data).map_err(|_| GatewayEventDispatchError)?
        {
            if play.game.is_none() {
                let locale = command.locale.clone();
                let guild_locale = Some(
                    self.store
                        .lock()
                        .map_err(|_| GatewayEventDispatchError)?
                        .guild_config(&guild_id)
                        .map_err(|_| GatewayEventDispatchError)?
                        .locale,
                );
                let selected_language = play
                    .language
                    .as_deref()
                    .filter(|language| {
                        GAME_LANGUAGE_CHOICES
                            .iter()
                            .any(|(value, _)| value == language)
                    })
                    .unwrap_or_else(|| {
                        Self::default_game_language(guild_locale.as_deref().unwrap_or("en"))
                    })
                    .to_owned();
                let (content, rows) = self.game_pick_components(
                    &command.id.get().to_string(),
                    &locale,
                    guild_locale.as_deref(),
                    None,
                    &selected_language,
                    "google",
                )?;
                if let Ok(mut picks) = self.pending_picks.lock() {
                    let now_ms = system_now_ms();
                    picks.insert(
                        format!("gamePick:{}", command.id.get()),
                        PendingGamePick {
                            guild_id: guild_id.clone(),
                            parent_channel_id: command.channel_id.get().to_string(),
                            user_id: command.user.id.get().to_string(),
                            selected_game: None,
                            language: Some(selected_language),
                            engine: "google".to_owned(),
                            locale,
                            guild_locale,
                            issued_at_ms: now_ms,
                        },
                    );
                    Self::prune_picks_locked(&mut picks, now_ms);
                }
                context
                    .http
                    .create_interaction_response(
                        command.id,
                        &command.token,
                        &Self::game_pick_payload(&content, rows),
                        Vec::new(),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                return Ok(true);
            }
            command
                .defer_ephemeral(context)
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            let content = self
                .start_game(
                    context,
                    guild_id,
                    command.channel_id.get().to_string(),
                    command.user.id.get().to_string(),
                    command.locale.clone(),
                    command.guild_locale.clone(),
                    play.game,
                    play.language,
                    None,
                )
                .await?;
            context
                .http
                .edit_original_interaction_response(
                    &command.token,
                    &Self::game_pick_edit_payload(&content, Vec::new()),
                    Vec::new(),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(true);
        }
        let Some(_stop) =
            parse_game_stop_command(&command.data).map_err(|_| GatewayEventDispatchError)?
        else {
            return Ok(false);
        };
        let user_id = command.user.id.get().to_string();
        let can_manage = command
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .is_some_and(|permissions| permissions.contains(Permissions::MANAGE_GUILD));
        let can_stop = can_manage
            || self
                .coordinator
                .lock()
                .map(|coordinator| coordinator.is_starter(&guild_id, &user_id))
                .unwrap_or(false);
        let session = self
            .coordinator
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .session(&guild_id);
        let result = self
            .coordinator
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .stop(&guild_id, &user_id, can_stop);
        match result {
            Err(_) => {
                let content = self.render(
                    "error.needManageGuild",
                    Some(&command.locale),
                    command.guild_locale.as_deref(),
                    &BTreeMap::new(),
                )?;
                command
                    .create_response(
                        context,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(content)
                                .ephemeral(true),
                        ),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
            }
            Ok(GameManagerEvent::Stopped) => {
                let content = self.render(
                    "game.stop.ok",
                    Some(&command.locale),
                    command.guild_locale.as_deref(),
                    &BTreeMap::new(),
                )?;
                command
                    .create_response(
                        context,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(content)
                                .ephemeral(true),
                        ),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                self.cleanup_forced(context, session).await?;
            }
            Ok(GameManagerEvent::NoActiveGame) => {
                let content = self.render(
                    "game.stop.none",
                    Some(&command.locale),
                    command.guild_locale.as_deref(),
                    &BTreeMap::new(),
                )?;
                command
                    .create_response(
                        context,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(content)
                                .ephemeral(true),
                        ),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
            }
            Ok(GameManagerEvent::VoiceLeft)
            | Ok(GameManagerEvent::Consumed { .. })
            | Ok(GameManagerEvent::Finished { .. }) => {}
        }
        Ok(true)
    }

    async fn handle_component(
        &self,
        context: &Context,
        component: serenity::model::application::ComponentInteraction,
    ) -> Result<bool, GatewayEventDispatchError> {
        if let Some(id) = component.data.custom_id.strip_prefix("gameLanguage:") {
            component
                .defer(context)
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            self.prune_picks(system_now_ms());
            let key = format!("gamePick:{id}");
            let selected_language = match &component.data.kind {
                ComponentInteractionDataKind::StringSelect { values } if values.len() == 1 => {
                    values[0].as_str()
                }
                _ => return Ok(true),
            };
            if !GAME_LANGUAGE_CHOICES
                .iter()
                .any(|(value, _)| *value == selected_language)
            {
                return Ok(true);
            }
            let pick_state = {
                let mut picks = self
                    .pending_picks
                    .lock()
                    .map_err(|_| GatewayEventDispatchError)?;
                picks
                    .get_mut(&key)
                    .filter(|pick| {
                        component
                            .guild_id
                            .is_some_and(|guild| guild.get().to_string() == pick.guild_id)
                            && component.user.id.get().to_string() == pick.user_id
                    })
                    .map(|pick| {
                        pick.language = Some(selected_language.to_owned());
                        (
                            pick.locale.clone(),
                            pick.guild_locale.clone(),
                            pick.selected_game.clone(),
                            pick.language.clone().unwrap_or_else(|| "en".to_owned()),
                            pick.engine.clone(),
                        )
                    })
            };
            let Some((locale, guild_locale, selected_game, selected_language, selected_engine)) =
                pick_state
            else {
                let content = self.render(
                    "game.pickTimeout",
                    Some(&component.locale),
                    component.guild_locale.as_deref(),
                    &BTreeMap::new(),
                )?;
                context
                    .http
                    .edit_original_interaction_response(
                        &component.token,
                        &Self::game_pick_edit_payload(&content, Vec::new()),
                        Vec::new(),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                return Ok(true);
            };
            let (content, rows) = self.game_pick_components(
                id,
                &locale,
                guild_locale.as_deref(),
                selected_game.as_deref(),
                &selected_language,
                &selected_engine,
            )?;
            context
                .http
                .edit_original_interaction_response(
                    &component.token,
                    &Self::game_pick_edit_payload(&content, rows),
                    Vec::new(),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(true);
        }
        if let Some(id) = component.data.custom_id.strip_prefix("gameEngine:") {
            component
                .defer(context)
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            self.prune_picks(system_now_ms());
            let key = format!("gamePick:{id}");
            let selected_engine = match &component.data.kind {
                ComponentInteractionDataKind::StringSelect { values } if values.len() == 1 => {
                    values[0].as_str()
                }
                _ => return Ok(true),
            };
            if Self::game_engine(selected_engine).is_none() {
                return Ok(true);
            }
            let pick_state = {
                let mut picks = self
                    .pending_picks
                    .lock()
                    .map_err(|_| GatewayEventDispatchError)?;
                picks
                    .get_mut(&key)
                    .filter(|pick| {
                        component
                            .guild_id
                            .is_some_and(|guild| guild.get().to_string() == pick.guild_id)
                            && component.user.id.get().to_string() == pick.user_id
                    })
                    .map(|pick| {
                        pick.engine = selected_engine.to_owned();
                        (
                            pick.locale.clone(),
                            pick.guild_locale.clone(),
                            pick.selected_game.clone(),
                            pick.language.clone().unwrap_or_else(|| "en".to_owned()),
                            pick.engine.clone(),
                        )
                    })
            };
            let Some((locale, guild_locale, selected_game, selected_language, selected_engine)) =
                pick_state
            else {
                let content = self.render(
                    "game.pickTimeout",
                    Some(&component.locale),
                    component.guild_locale.as_deref(),
                    &BTreeMap::new(),
                )?;
                context
                    .http
                    .edit_original_interaction_response(
                        &component.token,
                        &Self::game_pick_edit_payload(&content, Vec::new()),
                        Vec::new(),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                return Ok(true);
            };
            let (content, rows) = self.game_pick_components(
                id,
                &locale,
                guild_locale.as_deref(),
                selected_game.as_deref(),
                &selected_language,
                &selected_engine,
            )?;
            context
                .http
                .edit_original_interaction_response(
                    &component.token,
                    &Self::game_pick_edit_payload(&content, rows),
                    Vec::new(),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(true);
        }
        if let Some(id) = component.data.custom_id.strip_prefix("gamePick:") {
            component
                .defer(context)
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            self.prune_picks(system_now_ms());
            let key = format!("gamePick:{id}");
            let selected_game = match &component.data.kind {
                ComponentInteractionDataKind::StringSelect { values } if values.len() == 1 => {
                    values[0].clone()
                }
                _ => return Ok(true),
            };
            if !GAME_CATALOG.iter().any(|game| game.id == selected_game) {
                return Ok(true);
            }
            let pick_state = {
                let mut picks = self
                    .pending_picks
                    .lock()
                    .map_err(|_| GatewayEventDispatchError)?;
                picks
                    .get_mut(&key)
                    .filter(|pick| {
                        component
                            .guild_id
                            .is_some_and(|guild| guild.get().to_string() == pick.guild_id)
                            && component.user.id.get().to_string() == pick.user_id
                    })
                    .map(|pick| {
                        pick.selected_game = Some(selected_game);
                        (
                            pick.locale.clone(),
                            pick.guild_locale.clone(),
                            pick.selected_game.clone(),
                            pick.language.clone().unwrap_or_else(|| "en".to_owned()),
                            pick.engine.clone(),
                        )
                    })
            };
            let Some((locale, guild_locale, selected_game, selected_language, selected_engine)) =
                pick_state
            else {
                let content = self.render(
                    "game.pickTimeout",
                    Some(&component.locale),
                    component.guild_locale.as_deref(),
                    &BTreeMap::new(),
                )?;
                context
                    .http
                    .edit_original_interaction_response(
                        &component.token,
                        &Self::game_pick_edit_payload(&content, Vec::new()),
                        Vec::new(),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                return Ok(true);
            };
            let (content, rows) = self.game_pick_components(
                id,
                &locale,
                guild_locale.as_deref(),
                selected_game.as_deref(),
                &selected_language,
                &selected_engine,
            )?;
            context
                .http
                .edit_original_interaction_response(
                    &component.token,
                    &Self::game_pick_edit_payload(&content, rows),
                    Vec::new(),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(true);
        }

        let Some(id) = component.data.custom_id.strip_prefix("gameStart:") else {
            return Ok(false);
        };
        component
            .defer(context)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        self.prune_picks(system_now_ms());
        let key = format!("gamePick:{id}");
        let pick = self
            .pending_picks
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .remove(&key);
        let valid = pick.as_ref().is_some_and(|pick| {
            component
                .guild_id
                .is_some_and(|guild| guild.get().to_string() == pick.guild_id)
                && component.user.id.get().to_string() == pick.user_id
        });
        let Some(pick) = pick.filter(|_| valid) else {
            let content = self.render(
                "game.pickTimeout",
                Some(&component.locale),
                component.guild_locale.as_deref(),
                &BTreeMap::new(),
            )?;
            context
                .http
                .edit_original_interaction_response(
                    &component.token,
                    &Self::game_pick_edit_payload(&content, Vec::new()),
                    Vec::new(),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(true);
        };
        let Some(game_id) = pick.selected_game else {
            return Ok(true);
        };
        let content = self
            .start_game(
                context,
                pick.guild_id,
                pick.parent_channel_id,
                pick.user_id,
                pick.locale,
                pick.guild_locale,
                Some(game_id),
                pick.language,
                Some(pick.engine),
            )
            .await?;
        context
            .http
            .edit_original_interaction_response(
                &component.token,
                &Self::game_pick_edit_payload(&content, Vec::new()),
                Vec::new(),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(true)
    }

    async fn handle_message(
        &self,
        context: &Context,
        message: &serenity::model::channel::Message,
    ) -> Result<bool, GatewayEventDispatchError> {
        if message.author.bot {
            return Ok(false);
        }
        let Some(guild_id) = message.guild_id else {
            return Ok(false);
        };
        let guild_id = guild_id.get().to_string();
        let user_id = message.author.id.get().to_string();
        let bot_channel = self.gateway_state.bot_voice_channel_id(&guild_id);
        let caller_channel = self.gateway_state.voice_channel_id(&guild_id, &user_id);
        let speech_allowed = bot_channel.is_some() && caller_channel == bot_channel;
        let event = self
            .coordinator
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .handle_message_at(
                &vozen_discord::GameMessage {
                    guild_id: guild_id.clone(),
                    channel_id: message.channel_id.get().to_string(),
                    author_id: user_id,
                    author_name: message.author.name.clone(),
                    content: message.content.clone(),
                    can_trigger_speech: speech_allowed,
                },
                system_now_ms(),
            );
        let Some(event) = event else {
            return Ok(false);
        };
        let _ = self
            .dispatch_event(context, event, speech_allowed, Some(&guild_id))
            .await;
        Ok(true)
    }

    async fn on_voice_state_update(
        &self,
        context: &Context,
        new: &serenity::model::voice::VoiceState,
    ) -> Result<(), GatewayEventDispatchError> {
        let new_user_id = new.user_id.get().to_string();
        if new.channel_id.is_some()
            || self.gateway_state.bot_user_id().as_deref() != Some(new_user_id.as_str())
        {
            return Ok(());
        }
        let Some(guild_id) = new.guild_id else {
            return Ok(());
        };
        let guild_id = guild_id.get().to_string();
        let session = self
            .coordinator
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .session(&guild_id);
        let event = self
            .coordinator
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .on_voice_left(&guild_id);
        if matches!(event, GameManagerEvent::VoiceLeft) {
            self.cleanup_forced(context, session).await?;
        }
        Ok(())
    }

    fn on_guild_delete(&self, guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        if let Ok(mut coordinator) = self.coordinator.lock() {
            coordinator.end_guild(guild_id);
        }
        if let Ok(mut picks) = self.pending_picks.lock() {
            picks.retain(|_, pick| pick.guild_id != guild_id);
        }
        if let Ok(mut engines) = self.session_engines.lock() {
            engines.remove(guild_id);
        }
        Ok(())
    }
}

fn game_uses_shared_standings(game_id: &str) -> bool {
    matches!(
        game_id,
        "guess-language"
            | "math"
            | "skip-count"
            | "spelling"
            | "spell-out"
            | "fast-speech"
            | "accent-swap"
            | "reflexes"
            | "vozen-says"
            | "headsOrTails"
    )
}

fn game_card_body(content: &str) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&json!({
        "flags": 1_u64 << 15,
        "components": [{
            "type": 17,
            "accent_color": color_for_content(content),
            "components": [{
                "type": 10,
                "content": content
            }]
        }],
        "allowed_mentions": { "parse": [] }
    }))
}

impl CoreVoiceGatewaySink {
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        gateway_state: GatewayState,
        options: CoreVoiceRuntimeOptions,
    ) -> Self {
        Self::new_with_runtime_batch(store, gateway_state, options, RuntimeBatchBuffer::default())
    }

    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new_with_runtime_batch(
        store: Arc<Mutex<SqliteStore>>,
        gateway_state: GatewayState,
        options: CoreVoiceRuntimeOptions,
        runtime_batch: RuntimeBatchBuffer,
    ) -> Self {
        Self::new_with_runtime_batch_and_voice_read_store(
            store.clone(),
            store,
            gateway_state,
            options,
            runtime_batch,
        )
    }

    #[must_use]
    pub fn new_with_runtime_batch_and_voice_read_store(
        store: Arc<Mutex<SqliteStore>>,
        voice_read_store: Arc<Mutex<SqliteStore>>,
        gateway_state: GatewayState,
        options: CoreVoiceRuntimeOptions,
        runtime_batch: RuntimeBatchBuffer,
    ) -> Self {
        let game_runtime = options
            .game_play_enabled
            .then(|| {
                VoiceResponseLocalizer::from_generated_contract()
                    .ok()
                    .map(|localizer| {
                        Arc::new(GameRuntime {
                            store: store.clone(),
                            gateway_state: gateway_state.clone(),
                            coordinator: Arc::new(Mutex::new(GameCoordinator::new(
                                GameDriverFactory::new(
                                    options.settings.available_models.clone(),
                                    options.settings.default_voice.clone(),
                                    "en",
                                ),
                            ))),
                            pending_picks: Mutex::new(BTreeMap::new()),
                            session_engines: Mutex::new(BTreeMap::new()),
                            localizer,
                            available_models: options.settings.available_models.clone(),
                            default_voice: options.settings.default_voice.clone(),
                            default_speed: options.settings.default_speed,
                            default_engine: options.settings.default_engine,
                            executor: Mutex::new(None),
                            tick_started: AtomicBool::new(false),
                        })
                    })
            })
            .flatten();
        Self {
            store,
            voice_read_store,
            gateway_state,
            options,
            localizer: VoiceResponseLocalizer::from_generated_contract().ok(),
            leaderboard_poster: Mutex::new(LeaderboardPoster::default()),
            vote_promo_poster: Mutex::new(VotePromoPoster::default()),
            dependencies: Mutex::new(None),
            executor: Mutex::new(None),
            message_service: Mutex::new(None),
            last_speakers: Mutex::new(BTreeMap::new()),
            randomizer_sessions: Mutex::new(BTreeMap::new()),
            cast_sessions: Mutex::new(BTreeMap::new()),
            greet_cooldown: Mutex::new(BTreeMap::new()),
            rejoin_attempted: AtomicBool::new(false),
            game_runtime,
            runtime_batch,
        }
    }

    fn dependencies(
        &self,
        context: &Context,
    ) -> Result<Arc<VoiceDependencies>, GatewayEventDispatchError> {
        let mut current = self
            .dependencies
            .lock()
            .map_err(|_| GatewayEventDispatchError)?;
        if let Some(dependencies) = &*current {
            return Ok(dependencies.clone());
        }
        let options = &self.options;
        let piper = Arc::new(PiperCommandSynthesizer::production_with_metrics(
            options.piper_path.clone(),
            options.models_dir.clone(),
            options.cache_dir.clone(),
            options.piper_concurrency,
            self.gateway_state.metrics(),
        ));
        let piper: Arc<dyn vozen_discord::CommandSpeechSynthesizer> = piper;
        let neural = options
            .openai_api_key
            .as_ref()
            .map(|api_key| {
                crate::neural_adapter::NeuralCommandSynthesizer::production(
                    api_key.clone(),
                    options.neural_cache_dir.clone(),
                    self.gateway_state.metrics(),
                )
            })
            .transpose()
            .map_err(|_| GatewayEventDispatchError)?
            .map(|provider| Arc::new(provider) as Arc<dyn vozen_discord::CommandSpeechSynthesizer>);
        let default: Arc<dyn vozen_discord::CommandSpeechSynthesizer> =
            match options.settings.default_engine {
                SynthesisEngine::Default => {
                    let gtts = Arc::new(
                        crate::gtts_adapter::GttsCommandSynthesizer::production(
                            options.ffmpeg.clone(),
                            options.gtts_cache_dir.clone(),
                            self.gateway_state.metrics(),
                        )
                        .map_err(|_| GatewayEventDispatchError)?,
                    )
                        as Arc<dyn vozen_discord::CommandSpeechSynthesizer>;
                    Arc::new(
                        crate::gtts_adapter::GttsWithPiperFallback::new_with_metrics(
                            gtts,
                            piper.clone(),
                            self.gateway_state.metrics(),
                        ),
                    )
                }
                SynthesisEngine::Neural => neural.clone().ok_or(GatewayEventDispatchError)?,
                _ => piper.clone(),
            };
        let gcloud = options
            .gcloud_api_key
            .as_ref()
            .map(|api_key| {
                crate::gcloud_adapter::GcloudCommandSynthesizer::production(
                    api_key.clone(),
                    options.gcloud_cache_dir.clone(),
                    self.store.clone(),
                    options.gcloud_limits,
                    self.gateway_state.metrics(),
                )
            })
            .transpose()
            .map_err(|_| GatewayEventDispatchError)?
            .map(|provider| Arc::new(provider) as Arc<dyn vozen_discord::CommandSpeechSynthesizer>);
        let kokoro = options
            .kokoro_command
            .clone()
            .map(|command| {
                crate::kokoro_adapter::KokoroCommandSynthesizer::production(
                    command,
                    options.kokoro_cache_dir.clone(),
                    options.kokoro_languages.clone(),
                    self.gateway_state.metrics(),
                )
            })
            .transpose()
            .map_err(|_| GatewayEventDispatchError)?
            .map(|provider| Arc::new(provider) as Arc<dyn vozen_discord::CommandSpeechSynthesizer>);
        let dependencies = Arc::new(VoiceDependencies {
            synthesizer: PerUserCommandSynthesizer::new_with_metrics(
                default,
                piper,
                kokoro,
                gcloud,
                neural,
                self.gateway_state.metrics(),
            ),
            playback: SongbirdCommandPlayback::new_with_metrics(
                context.clone(),
                options.queue_cap,
                self.gateway_state.message_counter(),
                self.gateway_state.metrics(),
            ),
            synthesis: GuildSynthesisCoordinator::default(),
        });
        *current = Some(dependencies.clone());
        Ok(dependencies)
    }

    fn executor(&self, context: &Context) -> Result<Arc<Executor>, GatewayEventDispatchError> {
        let mut current = self
            .executor
            .lock()
            .map_err(|_| GatewayEventDispatchError)?;
        if let Some(executor) = &*current {
            return Ok(executor.clone());
        }
        let options = &self.options;
        let dependencies = self.dependencies(context)?;
        let executor = CoreVoiceInteractionExecutor::new_with_synthesis_coordinator(
            self.store.clone(),
            self.gateway_state.clone(),
            SongbirdVoiceSessionTransport::new(context.clone()),
            dependencies.synthesizer.clone(),
            dependencies.playback.clone(),
            dependencies.synthesis.clone(),
            options.settings.clone(),
            Arc::new(system_now_ms),
        )
        .map_err(|_| GatewayEventDispatchError)?;
        let executor = Arc::new(executor);
        *current = Some(executor.clone());
        Ok(executor)
    }

    fn message_service(
        &self,
        context: &Context,
    ) -> Result<Arc<MessageService>, GatewayEventDispatchError> {
        let mut current = self
            .message_service
            .lock()
            .map_err(|_| GatewayEventDispatchError)?;
        if let Some(service) = &*current {
            return Ok(service.clone());
        }
        let dependencies = self.dependencies(context)?;
        let service = Arc::new(
            MessageVoiceService::new_with_synthesis_coordinator_runtime_batch_read_store_and_metrics(
                self.store.clone(),
                self.voice_read_store.clone(),
                dependencies.synthesizer.clone(),
                dependencies.playback.clone(),
                dependencies.synthesis.clone(),
                self.options.settings.clone(),
                Arc::new(system_now_ms),
                self.runtime_batch.clone(),
                self.gateway_state.metrics(),
            ),
        );
        *current = Some(service.clone());
        Ok(service)
    }

    /// Mirrors Node's `maybeAutojoin`: only a message that already passed the trigger and policy
    /// gates may cause a join, and the current channel is checked against live Discord permissions
    /// immediately before the transport call. The result is an ephemeral exception for this
    /// message; the normal message admission still receives an explicit autojoin marker.
    async fn maybe_autojoin_for_message(
        &self,
        context: &Context,
        facts: &DiscordMessageFactsOwned,
    ) -> bool {
        let eligible = self
            .store
            .lock()
            .ok()
            .and_then(|store| {
                vozen_discord::should_attempt_autojoin(&store, facts.as_borrowed()).ok()
            })
            .unwrap_or(false);
        if !eligible {
            return false;
        }
        let Some(author_voice_channel_id) = facts.author_voice_channel_id.as_deref() else {
            return false;
        };
        let Ok(guild_number) = facts.guild_id.parse::<u64>() else {
            return false;
        };
        let Ok(channel_number) = author_voice_channel_id.parse::<u64>() else {
            return false;
        };
        let guild_id = GuildId::new(guild_number);
        let channel_id = ChannelId::new(channel_number);
        let Ok(channels) = guild_id.channels(&context.http).await else {
            return false;
        };
        let Some(voice) = channels.get(&channel_id) else {
            return false;
        };
        if !matches!(voice.kind, ChannelType::Voice | ChannelType::Stage) {
            return false;
        }
        let Ok(guild) = guild_id.to_partial_guild(&context.http).await else {
            return false;
        };
        let Ok(bot) = context.http.get_current_user().await else {
            return false;
        };
        let Ok(bot_member) = guild_id.member(&context.http, bot.id).await else {
            return false;
        };
        let permissions = guild.user_permissions_in(voice, &bot_member);
        if !permissions.contains(Permissions::CONNECT | Permissions::SPEAK) {
            return false;
        }
        let core_facts = CoreVoiceInteractionFacts {
            guild_id: facts.guild_id.clone(),
            channel_id: facts.channel_id.clone(),
            user_id: facts.author_id.clone(),
            member_role_ids: None,
        };
        let Ok(executor) = self.executor(context) else {
            return false;
        };
        matches!(
            executor.join_for_message(&core_facts).await,
            CoreVoiceOutcome::Joined(JoinVoiceOutcome::Joined)
        )
    }

    fn greet_cooldown_allows(&self, guild_id: &str, user_id: &str, now_ms: i64) -> bool {
        let key = format!("{guild_id}:{user_id}");
        let Ok(mut cooldown) = self.greet_cooldown.lock() else {
            return false;
        };
        if cooldown
            .get(&key)
            .is_some_and(|previous| now_ms.saturating_sub(*previous) < GREET_COOLDOWN_MS)
        {
            return false;
        }
        cooldown.remove(&key);
        cooldown.insert(key, now_ms);
        while cooldown.len() > GREET_COOLDOWN_MAX_ENTRIES {
            let Some(oldest) = cooldown.keys().next().cloned() else {
                break;
            };
            cooldown.remove(&oldest);
        }
        true
    }

    async fn greet_joining_member(
        &self,
        context: &Context,
        old: Option<&serenity::model::voice::VoiceState>,
        new: &serenity::model::voice::VoiceState,
        bot_channel_id: &str,
    ) {
        let Some(member) = new.member.as_ref() else {
            return;
        };
        if member.user.bot
            || !is_join_into_channel(
                old.and_then(|state| state.channel_id.map(|id| id.get().to_string()))
                    .as_deref(),
                new.channel_id.map(|id| id.get().to_string()).as_deref(),
                Some(bot_channel_id),
            )
        {
            return;
        }
        let Some(guild_id) = new.guild_id.map(|id| id.get().to_string()) else {
            return;
        };
        let user_id = new.user_id.get().to_string();
        let now_ms = system_now_ms();
        let (config, birthday, nickname) = {
            let Ok(store) = self.store.lock() else {
                return;
            };
            let Ok(config) = store.guild_config(&guild_id) else {
                return;
            };
            let birthday = store.birthday(&guild_id, &user_id).ok().flatten();
            let nickname = store.nickname(&guild_id, &user_id).ok().flatten();
            (config, birthday, nickname)
        };
        let today = time::OffsetDateTime::now_utc().date();
        let is_birthday = birthday.is_some_and(|birthday| {
            u8::from(today.month()) == birthday.month && today.day() == birthday.day
        });
        if !config.enabled || (!is_birthday && !config.greet_on_join) {
            return;
        }
        if !self.greet_cooldown_allows(&guild_id, &user_id, now_ms) {
            return;
        }
        let raw_name = nickname
            .or_else(|| Some(member.display_name().to_owned()))
            .unwrap_or_else(|| member.user.name.clone());
        let safe_name = sanitize_speaker_name(&raw_name).unwrap_or_else(|| "someone".to_owned());
        let greeting = build_greeting(
            &config.greet_locale,
            &safe_name,
            &self.options.settings.available_models,
            if config.default_voice.trim().is_empty() {
                &self.options.settings.default_voice
            } else {
                &config.default_voice
            },
            self.options.settings.default_speed,
            is_birthday,
        );
        let Some(bot_id) = self.gateway_state.bot_user_id() else {
            return;
        };
        let facts = CoreVoiceInteractionFacts {
            guild_id,
            channel_id: bot_channel_id.to_owned(),
            user_id: bot_id,
            member_role_ids: None,
        };
        let Ok(executor) = self.executor(context) else {
            return;
        };
        let _ = executor
            .speak_text_with_voice(
                &facts,
                &greeting.text,
                &greeting.model,
                greeting.speed,
                self.options.settings.default_engine,
                false,
            )
            .await;
    }

    async fn leave_if_alone(&self, context: &Context, guild_id: &str, bot_channel_id: &str) {
        let humans = self
            .gateway_state
            .human_voice_member_count(guild_id, bot_channel_id);
        if humans > 0 {
            return;
        }
        let Ok(executor) = self.executor(context) else {
            return;
        };
        let _ = executor.leave_for_lifecycle(guild_id).await;
        // A new voice session must announce its first speaker again.  Keep this state
        // independent of the selected TTS engine: switching between Google and Piper must
        // never make xsaid appear to work in one engine only because the previous session's
        // speaker marker was still cached.
        if let Ok(mut speakers) = self.last_speakers.lock() {
            speakers.remove(guild_id);
        }
    }

    /// Restores calls only once per process and only after checking every persisted channel
    /// against Discord's live REST state. This is intentionally separate from the gateway's
    /// small transient state: no stale voice presence can authorize a join by itself.
    async fn recover_planned_sessions(
        &self,
        context: &Context,
    ) -> Result<(), GatewayEventDispatchError> {
        if self.rejoin_attempted.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let marker_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let scope = consume_planned_rejoin_marker(&marker_directory, std::time::SystemTime::now());
        let presences = self
            .store
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .voice_presences()
            .map_err(|_| GatewayEventDispatchError)?;
        if presences.is_empty() {
            return Ok(());
        }

        // Persisted voice rows are active-call hints, not guild-scale data. Bound the startup
        // lookups anyway so a damaged database cannot burst Discord's REST rate limit.
        const REJOIN_LOOKUP_CONCURRENCY: usize = 4;
        let provider = DiscordDashboardOptionsProvider::new(self.gateway_state.clone());
        let mut states = BTreeMap::new();
        for batch in presences.chunks(REJOIN_LOOKUP_CONCURRENCY) {
            let mut tasks = tokio::task::JoinSet::new();
            for presence in batch {
                let provider = provider.clone();
                let guild_id = presence.guild_id.clone();
                let channel_id = presence.channel_id.clone();
                tasks.spawn(async move {
                    let state = provider.rejoin_channel_state(&guild_id, &channel_id).await;
                    (guild_id, channel_id, state)
                });
            }
            while let Some(result) = tasks.join_next().await {
                if let Ok((guild_id, channel_id, state)) = result {
                    states.insert((guild_id, channel_id), state);
                }
            }
        }

        PlannedRejoinService::new(
            self.store.clone(),
            self.gateway_state.clone(),
            SongbirdVoiceSessionTransport::new(context.clone()),
        )
        .recover(scope.as_ref(), system_now_ms(), |guild_id, channel_id| {
            states
                .get(&(guild_id.to_owned(), channel_id.to_owned()))
                .copied()
                .unwrap_or(RejoinChannelState::NoPermissions)
        })
        .await
        .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    fn prune_randomizer_sessions(&self, now_ms: i64) {
        if let Ok(mut sessions) = self.randomizer_sessions.lock() {
            sessions.retain(|_, session| session.valid_at(now_ms));
            // A Discord interaction can be replayed or abandoned. Keep the transient map bounded
            // even if a guild sends a large number of unfinished modal flows.
            if sessions.len() > 1024 {
                let mut entries = sessions
                    .iter()
                    .map(|(id, session)| (id.clone(), session.issued_at_ms))
                    .collect::<Vec<_>>();
                entries.sort_by_key(|(_, issued_at)| *issued_at);
                for (id, _) in entries.into_iter().take(sessions.len() - 1024) {
                    sessions.remove(&id);
                }
            }
        }
    }

    fn randomizer_localizer(&self) -> Result<VoiceResponseLocalizer, GatewayEventDispatchError> {
        VoiceResponseLocalizer::from_generated_contract().map_err(|_| GatewayEventDispatchError)
    }

    fn randomizer_session_matches(
        session: &RandomizerSession,
        user_id: &str,
        guild_id: &str,
        now_ms: i64,
    ) -> bool {
        session.user_id == user_id && session.guild_id == guild_id && session.valid_at(now_ms)
    }

    fn randomizer_facts(
        guild_id: &str,
        channel_id: &str,
        user_id: &str,
        member: Option<&serenity::model::guild::Member>,
    ) -> CoreVoiceInteractionFacts {
        CoreVoiceInteractionFacts {
            guild_id: guild_id.to_owned(),
            channel_id: channel_id.to_owned(),
            user_id: user_id.to_owned(),
            member_role_ids: member.map(|member| {
                member
                    .roles
                    .iter()
                    .map(|role_id| role_id.get().to_string())
                    .collect()
            }),
        }
    }

    async fn randomizer_result(
        &self,
        context: &Context,
        facts: &CoreVoiceInteractionFacts,
        locale: Option<&str>,
        guild_locale: Option<&str>,
        options: &[String],
        voice: (&str, &str),
    ) -> Result<String, GatewayEventDispatchError> {
        let winner = pick_option(options).ok_or(GatewayEventDispatchError)?;
        let localizer = self.randomizer_localizer()?;
        let mut parameters = BTreeMap::new();
        parameters.insert("winner", winner.to_owned());
        parameters.insert("count", options.len().to_string());
        let line = localizer
            .render_key("rand.result", locale, guild_locale, &parameters)
            .ok_or(GatewayEventDispatchError)?;
        parameters.clear();
        parameters.insert("winner", winner.to_owned());
        let speak_text = localizer
            .render_key("rand.speak", Some(voice.0), None, &parameters)
            .ok_or(GatewayEventDispatchError)?;
        let engine = GameRuntime::game_engine(voice.1).ok_or(GatewayEventDispatchError)?;
        let (stored, premium) = {
            let store = self.store.lock().map_err(|_| GatewayEventDispatchError)?;
            let stored = store
                .get_user_voice(&facts.guild_id, &facts.user_id)
                .map_err(|_| GatewayEventDispatchError)?;
            let premium = store
                .is_user_premium(&facts.user_id, system_now_ms())
                .and_then(|user| {
                    store
                        .is_guild_premium(&facts.guild_id, system_now_ms())
                        .map(|guild| user || guild)
                })
                .map_err(|_| GatewayEventDispatchError)?;
            (stored, premium)
        };
        if engine == SynthesisEngine::Kokoro && !premium {
            return localizer
                .render_key(
                    "voice.engine.kokoroLocked",
                    locale,
                    guild_locale,
                    &BTreeMap::new(),
                )
                .ok_or(GatewayEventDispatchError);
        }
        let model = vozen_discord::randomizer_model(
            &self.options.settings.available_models,
            stored.as_ref().map(|value| value.model.as_str()),
            voice.0,
            voice.1,
        );
        let Some(model) = model else {
            return localizer
                .render_key("voice.unknownModel", locale, guild_locale, &BTreeMap::new())
                .ok_or(GatewayEventDispatchError);
        };
        let speed = stored
            .as_ref()
            .map(|value| value.speed)
            .filter(|value| value.is_finite())
            .unwrap_or(self.options.settings.default_speed);
        let executor = self.executor(context)?;
        let outcome = executor
            .speak_text_with_voice(facts, &speak_text, model, speed, engine, true)
            .await;
        let mut content = format!("🎲 {line}");
        if outcome != CoreVoiceOutcome::Tts(CoreTtsOutcome::Queued) {
            let not_in_voice = executor
                .render_speak_outcome(outcome, facts, locale)
                .map_err(|_| GatewayEventDispatchError)?;
            content.push('\n');
            content.push_str(&not_in_voice);
        }
        Ok(content)
    }

    fn randomizer_modal(
        &self,
        interaction_id: &str,
        amount: usize,
        locale: Option<&str>,
        guild_locale: Option<&str>,
    ) -> Result<CreateInteractionResponse, GatewayEventDispatchError> {
        let localizer = self.randomizer_localizer()?;
        let mut title_parameters = BTreeMap::new();
        title_parameters.insert("amount", amount.to_string());
        let title = localizer
            .render_key("rand.modalTitle", locale, guild_locale, &title_parameters)
            .ok_or(GatewayEventDispatchError)?;
        let mut rows = Vec::with_capacity(amount);
        for index in 1..=amount {
            let mut parameters = BTreeMap::new();
            parameters.insert("n", index.to_string());
            let label = localizer
                .render_key("rand.modalOption", locale, guild_locale, &parameters)
                .ok_or(GatewayEventDispatchError)?;
            rows.push(CreateActionRow::InputText(
                CreateInputText::new(InputTextStyle::Short, label, format!("opt{index}"))
                    .max_length(vozen_discord::MAX_OPTION_CHARS as u16)
                    .required(true),
            ));
        }
        Ok(CreateInteractionResponse::Modal(
            CreateModal::new(format!("randFill:{interaction_id}"), title).components(rows),
        ))
    }

    async fn handle_randomizer_command(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some(guild_id) = command.guild_id else {
            return Ok(());
        };
        let Some(facts) = CoreVoiceInteractionFacts::from_command(command) else {
            return Ok(());
        };
        let parsed = match parse_randomizer_command(&command.data) {
            Ok(parsed) => parsed,
            Err(_) => {
                command.create_response(context, CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("Choose a language and voice engine in `/randomizer`, and provide at least two options.")
                        .ephemeral(true),
                )).await.map_err(|_| GatewayEventDispatchError)?;
                return Ok(());
            }
        };
        let Some(parsed) = parsed else {
            return Ok(());
        };
        let now_ms = system_now_ms();
        self.prune_randomizer_sessions(now_ms);
        let guild_id = guild_id.get().to_string();
        let user_id = command.user.id.get().to_string();
        let (language, engine) = parsed.voice();
        let issued = RandomizerSession {
            user_id,
            guild_id,
            amount: None,
            locale: command.locale.clone(),
            language: language.to_owned(),
            engine: engine.to_owned(),
            issued_at_ms: now_ms,
        };
        match parsed {
            RandomizerCommand::Direct {
                options,
                language,
                engine,
            } => {
                command
                    .defer(context)
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                let content = self
                    .randomizer_result(
                        context,
                        &facts,
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                        &options,
                        (&language, &engine),
                    )
                    .await?;
                command
                    .edit_response(
                        context,
                        EditInteractionResponse::new()
                            .content(content)
                            .allowed_mentions(
                                CreateAllowedMentions::new()
                                    .all_users(false)
                                    .all_roles(false)
                                    .everyone(false),
                            ),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
            }
            RandomizerCommand::ChooseAmount { .. } => {
                if let Ok(mut sessions) = self.randomizer_sessions.lock() {
                    sessions.insert(command.id.get().to_string(), issued);
                }
                let localizer = self.randomizer_localizer()?;
                let prompt = localizer
                    .render_key(
                        "rand.selectPrompt",
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                        &BTreeMap::new(),
                    )
                    .ok_or(GatewayEventDispatchError)?;
                let placeholder = localizer
                    .render_key(
                        "rand.selectPlaceholder",
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                        &BTreeMap::new(),
                    )
                    .ok_or(GatewayEventDispatchError)?;
                let options = (vozen_discord::MIN_OPTIONS..=vozen_discord::MAX_MODAL_OPTIONS)
                    .map(|amount| {
                        let mut parameters = BTreeMap::new();
                        parameters.insert("n", amount.to_string());
                        let label = localizer
                            .render_key(
                                "rand.selectOption",
                                Some(&command.locale),
                                command.guild_locale.as_deref(),
                                &parameters,
                            )
                            .unwrap_or_else(|| amount.to_string());
                        CreateSelectMenuOption::new(label, amount.to_string())
                    })
                    .collect();
                let select = CreateSelectMenu::new(
                    format!("randAmount:{}", command.id.get()),
                    CreateSelectMenuKind::String { options },
                )
                .placeholder(placeholder)
                .min_values(1)
                .max_values(1);
                command
                    .create_response(
                        context,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(prompt)
                                .ephemeral(true)
                                .components(vec![CreateActionRow::SelectMenu(select)]),
                        ),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
            }
            RandomizerCommand::Modal { amount, .. } => {
                if let Ok(mut sessions) = self.randomizer_sessions.lock() {
                    let mut session = issued;
                    session.amount = Some(amount);
                    sessions.insert(command.id.get().to_string(), session);
                }
                command
                    .create_response(
                        context,
                        self.randomizer_modal(
                            &command.id.get().to_string(),
                            amount,
                            Some(&command.locale),
                            command.guild_locale.as_deref(),
                        )?,
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
            }
        }
        Ok(())
    }

    async fn handle_randomizer_component(
        &self,
        context: &Context,
        component: serenity::model::application::ComponentInteraction,
    ) -> Result<bool, GatewayEventDispatchError> {
        let Some(session_id) = parse_amount_component_id(&component.data.custom_id) else {
            return Ok(false);
        };
        let Some(guild_id) = component.guild_id else {
            return Ok(true);
        };
        let now_ms = system_now_ms();
        self.prune_randomizer_sessions(now_ms);
        let amount = match &component.data.kind {
            ComponentInteractionDataKind::StringSelect { values } if values.len() == 1 => values
                .first()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|value| {
                    (vozen_discord::MIN_OPTIONS..=vozen_discord::MAX_MODAL_OPTIONS).contains(value)
                }),
            _ => None,
        };
        let Some(amount) = amount else {
            return Ok(true);
        };
        let guild_id = guild_id.get().to_string();
        let user_id = component.user.id.get().to_string();
        let response = {
            let mut sessions = self
                .randomizer_sessions
                .lock()
                .map_err(|_| GatewayEventDispatchError)?;
            let Some(session) = sessions.get_mut(&session_id) else {
                return Ok(true);
            };
            if !Self::randomizer_session_matches(session, &user_id, &guild_id, now_ms) {
                sessions.remove(&session_id);
                return Ok(true);
            }
            session.amount = Some(amount);
            self.randomizer_modal(
                &session_id,
                amount,
                Some(&component.locale),
                component.guild_locale.as_deref(),
            )?
        };
        component
            .create_response(context, response)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(true)
    }

    async fn handle_randomizer_modal(
        &self,
        context: &Context,
        modal: serenity::model::application::ModalInteraction,
    ) -> Result<bool, GatewayEventDispatchError> {
        let Some(session_id) = parse_fill_component_id(&modal.data.custom_id) else {
            return Ok(false);
        };
        let Some(guild_id) = modal.guild_id else {
            return Ok(true);
        };
        let now_ms = system_now_ms();
        self.prune_randomizer_sessions(now_ms);
        let guild_id = guild_id.get().to_string();
        let user_id = modal.user.id.get().to_string();
        let session = self
            .randomizer_sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .remove(&session_id);
        let Some(session) = session else {
            return Ok(true);
        };
        let Some(amount) = session.amount else {
            return Ok(true);
        };
        if !Self::randomizer_session_matches(&session, &user_id, &guild_id, now_ms) {
            return Ok(true);
        }
        let options = parse_modal_options(&modal, amount).map_err(|_| GatewayEventDispatchError)?;
        let facts = Self::randomizer_facts(
            &guild_id,
            &modal.channel_id.get().to_string(),
            &user_id,
            modal.member.as_ref(),
        );
        modal
            .defer(context)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let content = self
            .randomizer_result(
                context,
                &facts,
                Some(&modal.locale),
                modal.guild_locale.as_deref(),
                &options,
                (&session.language, &session.engine),
            )
            .await?;
        modal
            .edit_response(
                context,
                EditInteractionResponse::new()
                    .content(content)
                    .allowed_mentions(
                        CreateAllowedMentions::new()
                            .all_users(false)
                            .all_roles(false)
                            .everyone(false),
                    ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(true)
    }

    fn prune_cast_sessions(&self, now_ms: i64) {
        if let Ok(mut sessions) = self.cast_sessions.lock() {
            sessions.retain(|_, session| session.valid_at(now_ms));
            if sessions.len() > 256 {
                let mut entries = sessions
                    .iter()
                    .map(|(id, session)| (id.clone(), session.issued_at_ms))
                    .collect::<Vec<_>>();
                entries.sort_by_key(|(_, issued_at)| *issued_at);
                for (id, _) in entries.into_iter().take(sessions.len() - 256) {
                    sessions.remove(&id);
                }
            }
        }
    }

    fn cast_panel_content(session: &CastSession) -> String {
        let theme = session
            .theme_key
            .as_deref()
            .and_then(vozen_discord::cast_theme_by_key)
            .map(|theme| theme.label)
            .unwrap_or("choose a theme");
        let language = CAST_LANGUAGE_CHOICES
            .iter()
            .find(|choice| choice.value == session.language)
            .map(|choice| choice.name)
            .unwrap_or("English");
        let engine = match session.engine.as_str() {
            "piper" => "Piper",
            "kokoro" => "Kokoro",
            "gcloud" => "Google HD",
            _ => "Google (gTTS)",
        };
        let mut content = format!(
            "🎭 **Create a cast for your voice call**\nTheme: {theme}\nLanguage: {language}\nEngine: {engine}"
        );
        if session.theme_key.as_deref() == Some("pokemon") {
            content.push_str(
                "\n-# Unofficial fan reference; not affiliated with Nintendo, Game Freak, or The Pokémon Company.",
            );
        }
        content
    }

    fn cast_panel_payload(interaction_id: &str, session: &CastSession) -> Value {
        json!({
            "type": 4,
            "data": {
                "flags": 32832,
                "components": [Self::cast_panel_container(interaction_id, session)]
            }
        })
    }

    /// Components V2 is the only Discord layout that visually nests action rows inside the
    /// rounded card. Serenity 0.12 does not expose type 17 yet, so we serialize this small
    /// payload directly while continuing to use Serenity's builders for every child control.
    fn cast_panel_container(interaction_id: &str, session: &CastSession) -> Value {
        let rows = serde_json::to_value(Self::cast_panel_components(interaction_id, session))
            .expect("cast action rows are serializable");
        let mut children = vec![json!({
            "type": 10,
            "content": Self::cast_panel_content(session),
        })];
        if let Value::Array(rows) = rows {
            children.extend(rows);
        }
        json!({
            "type": 17,
            "accent_color": 0x5865F2u32,
            "components": children,
        })
    }

    fn cast_panel_edit_payload(interaction_id: &str, session: &CastSession) -> Value {
        json!({
            "flags": 32768,
            "components": [Self::cast_panel_container(interaction_id, session)]
        })
    }

    fn cast_terminal_edit_payload(content: &str, accent_color: u32) -> Value {
        json!({
            "flags": 32768,
            "components": [{
                "type": 17,
                "accent_color": accent_color,
                "components": [{"type": 10, "content": content}]
            }]
        })
    }

    #[allow(dead_code)]
    fn cast_panel_response_legacy(
        interaction_id: &str,
        session: &CastSession,
    ) -> CreateInteractionResponse {
        let themes = CAST_THEMES
            .iter()
            .map(|theme| {
                CreateSelectMenuOption::new(theme.label, theme.key)
                    .default_selection(session.theme_key.as_deref() == Some(theme.key))
            })
            .collect();
        let languages = CAST_LANGUAGE_CHOICES
            .iter()
            .map(|choice| {
                CreateSelectMenuOption::new(choice.name, choice.value)
                    .default_selection(choice.value == session.language)
            })
            .collect();
        let engines = [
            ("Google (gTTS)", "google"),
            ("Piper", "piper"),
            ("Kokoro", "kokoro"),
        ]
        .into_iter()
        .map(|(label, value)| {
            CreateSelectMenuOption::new(label, value).default_selection(value == session.engine)
        })
        .collect();
        let theme_menu = CreateSelectMenu::new(
            format!("cast:theme:{interaction_id}"),
            CreateSelectMenuKind::String { options: themes },
        )
        .placeholder("Choose a theme")
        .min_values(1)
        .max_values(1);
        let language_menu = CreateSelectMenu::new(
            format!("cast:language:{interaction_id}"),
            CreateSelectMenuKind::String { options: languages },
        )
        .placeholder("Choose a language")
        .min_values(1)
        .max_values(1);
        let engine_menu = CreateSelectMenu::new(
            format!("cast:engine:{interaction_id}"),
            CreateSelectMenuKind::String { options: engines },
        )
        .placeholder("Choose a voice engine")
        .min_values(1)
        .max_values(1);
        let reveal = CreateButton::new(format!("cast:reveal:{interaction_id}"))
            .label("Reveal cast")
            .style(ButtonStyle::Primary)
            .disabled(session.theme_key.is_none());
        let cancel = CreateButton::new(format!("cast:cancel:{interaction_id}"))
            .label("Cancel")
            .style(ButtonStyle::Secondary);
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .embed(message_embed(Self::cast_panel_content(session)))
                .ephemeral(true)
                .components(vec![
                    CreateActionRow::SelectMenu(theme_menu),
                    CreateActionRow::SelectMenu(language_menu),
                    CreateActionRow::SelectMenu(engine_menu),
                    CreateActionRow::Buttons(vec![reveal, cancel]),
                ]),
        )
    }

    async fn setup_reply(
        command: &serenity::model::application::CommandInteraction,
        context: &Context,
        content: String,
    ) -> Result<(), GatewayEventDispatchError> {
        let accent_color = if content.trim_start().starts_with('❌') {
            0xED4245
        } else if content.trim_start().starts_with('⚠') {
            0xFEE75C
        } else {
            0x5865F2
        };
        context
            .http
            .edit_original_interaction_response(
                &command.token,
                &Self::cast_terminal_edit_payload(&content, accent_color),
                Vec::new(),
            )
            .await
            .map(|_| ())
            .map_err(|_| GatewayEventDispatchError)
    }

    async fn setup_embed_reply(
        command: &serenity::model::application::CommandInteraction,
        context: &Context,
        embed: CreateEmbed,
    ) -> Result<(), GatewayEventDispatchError> {
        command
            .edit_response(
                context,
                EditInteractionResponse::new()
                    .embeds(vec![embed])
                    .allowed_mentions(no_mentions()),
            )
            .await
            .map(|_| ())
            .map_err(|_| GatewayEventDispatchError)
    }

    async fn handle_setup_command(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some(parsed) =
            parse_setup_command(&command.data).map_err(|_| GatewayEventDispatchError)?
        else {
            return Ok(());
        };
        let Some(guild_id) = command.guild_id else {
            return Ok(());
        };
        // `/setup` performs several REST lookups and may join Songbird before it can render the
        // permission report. A direct response would race Discord's three-second interaction
        // deadline, producing the misleading "application did not respond" message. Claim the
        // interaction immediately, then edit the deferred ephemeral response at every exit.
        command
            .defer_ephemeral(context)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let can_manage = command
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .is_some_and(|permissions| permissions.contains(Permissions::MANAGE_GUILD));
        let localizer = VoiceResponseLocalizer::from_generated_contract()
            .map_err(|_| GatewayEventDispatchError)?;
        let render = |key: &str, parameters: &BTreeMap<&str, String>| {
            localizer
                .render_key(
                    key,
                    Some(&command.locale),
                    command.guild_locale.as_deref(),
                    parameters,
                )
                .ok_or(GatewayEventDispatchError)
        };
        if !can_manage {
            return Self::setup_reply(
                command,
                context,
                render("error.needManageGuild", &BTreeMap::new())?,
            )
            .await;
        }

        let target_id = parsed
            .channel_id
            .unwrap_or_else(|| command.channel_id.get());
        let channels = guild_id
            .channels(&context.http)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let Some(target) = channels.get(&ChannelId::new(target_id)) else {
            return Self::setup_reply(
                command,
                context,
                render("setup.noChannel", &BTreeMap::new())?,
            )
            .await;
        };
        if target.kind != ChannelType::Text {
            return Self::setup_reply(
                command,
                context,
                render("setup.channelWrongType", &BTreeMap::new())?,
            )
            .await;
        }

        let guild = guild_id
            .to_partial_guild(&context.http)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let bot = context
            .http
            .get_current_user()
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let bot_member = guild_id
            .member(&context.http, bot.id)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let text_permissions = guild.user_permissions_in(target, &bot_member);
        let can_view = text_permissions.contains(Permissions::VIEW_CHANNEL);
        let can_send = text_permissions.contains(Permissions::SEND_MESSAGES);

        let voice_id = self.gateway_state.voice_channel_id(
            &guild_id.get().to_string(),
            &command.user.id.get().to_string(),
        );
        let (can_connect, can_speak) = if let Some(voice_id) = voice_id.as_deref() {
            match channels.get(&ChannelId::new(voice_id.parse().unwrap_or_default())) {
                Some(voice) => {
                    let permissions = guild.user_permissions_in(voice, &bot_member);
                    (
                        permissions.contains(Permissions::CONNECT),
                        permissions.contains(Permissions::SPEAK),
                    )
                }
                None => (false, false),
            }
        } else {
            (false, false)
        };

        self.store
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .update_guild_config(
                &guild_id.get().to_string(),
                GuildConfigPatch {
                    tts_channel_id: Some(Some(target_id.to_string())),
                    autoread: Some(true),
                    ..Default::default()
                },
            )
            .map_err(|_| GatewayEventDispatchError)?;
        self.store
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .record_guild_setup_completed(&guild_id.get().to_string(), system_now_ms())
            .map_err(|_| GatewayEventDispatchError)?;

        let facts =
            CoreVoiceInteractionFacts::from_command(command).ok_or(GatewayEventDispatchError)?;
        let joined = if can_connect && can_speak {
            matches!(
                self.executor(context)?.join_for_setup(&facts).await,
                CoreVoiceOutcome::Joined(JoinVoiceOutcome::Joined)
            )
        } else {
            false
        };
        let voice_test = if parsed.test_voice && joined {
            matches!(
                self.executor(context)?
                    .speak_text_with_voice(
                        &facts,
                        "Vozen is ready.",
                        &self.options.settings.default_voice,
                        self.options.settings.default_speed,
                        SynthesisEngine::Piper,
                        false,
                    )
                    .await,
                CoreVoiceOutcome::Tts(CoreTtsOutcome::Queued)
            )
        } else {
            false
        };

        let mut lines = Vec::new();
        lines.push(render("setup.done", &BTreeMap::new())?);
        let mut parameters = BTreeMap::new();
        parameters.insert("channel", format!("<#{}>", target_id));
        lines.push(render("setup.channelLine", &parameters)?);
        lines.push(render("setup.autoreadOn", &BTreeMap::new())?);
        lines.push(String::new());
        lines.push(render("setup.permsHeader", &BTreeMap::new())?);
        for (label_key, state) in [
            ("setup.permView", Some(can_view)),
            ("setup.permSend", Some(can_send)),
            ("setup.permConnect", voice_id.as_ref().map(|_| can_connect)),
            ("setup.permSpeak", voice_id.as_ref().map(|_| can_speak)),
        ] {
            let label = render(label_key, &BTreeMap::new())?;
            let mut params = BTreeMap::new();
            params.insert("label", label);
            let key = if state == Some(true) {
                "setup.permOk"
            } else if state == Some(false) {
                "setup.permMissing"
            } else {
                "setup.permUnchecked"
            };
            lines.push(render(key, &params)?);
        }
        if joined {
            let mut params = BTreeMap::new();
            params.insert(
                "channel",
                format!("<#{}>", voice_id.as_deref().unwrap_or_default()),
            );
            lines.push(String::new());
            lines.push(render("setup.joinedVoice", &params)?);
        }
        if voice_test {
            lines.push("🔊 Voice test queued with the local Piper engine.".to_owned());
        }
        if !can_view || !can_send || (voice_id.is_some() && (!can_connect || !can_speak)) {
            lines.push(String::new());
            lines.push(render("setup.fixHint", &BTreeMap::new())?);
        }
        if voice_id.is_none() {
            lines.push(String::new());
            lines.push(render("setup.voiceUncheckedNote", &BTreeMap::new())?);
        }
        if can_view && can_send && voice_id.is_some() && can_connect && can_speak {
            lines.push(String::new());
            lines.push(render(
                if joined {
                    "setup.readyTalk"
                } else {
                    "setup.allGood"
                },
                &BTreeMap::new(),
            )?);
        }
        lines.push(String::new());
        lines.push(render("setup.membersHeader", &BTreeMap::new())?);
        lines.push(render("setup.membersBody", &BTreeMap::new())?);
        let any_missing =
            !can_view || !can_send || (voice_id.is_some() && (!can_connect || !can_speak));
        let embed = CreateEmbed::new()
            .color(if any_missing { 0xFEE75C } else { 0x57F287 })
            .description(lines.join("\n"));
        Self::setup_embed_reply(command, context, embed).await
    }

    async fn fetch_cast_members(
        &self,
        context: &Context,
        guild_id: &str,
        voice_channel_id: &str,
    ) -> Vec<CastMember> {
        let Ok(guild_number) = guild_id.parse::<u64>() else {
            return Vec::new();
        };
        let ids = self
            .gateway_state
            .voice_member_ids(guild_id, voice_channel_id);
        let mut members = Vec::new();
        // Fetch only one more than the product limit. This keeps a malformed/stale gateway map
        // from turning a reveal into an unbounded REST fan-out.
        for id in ids.into_iter().take(CAST_MAX_MEMBERS + 1) {
            let Ok(user_number) = id.parse::<u64>() else {
                continue;
            };
            let Ok(member) = context
                .http
                .get_member(GuildId::new(guild_number), UserId::new(user_number))
                .await
            else {
                continue;
            };
            members.push(CastMember {
                id,
                display_name: member.display_name().to_owned(),
                bot: member.user.bot,
            });
        }
        members
    }

    fn cast_voice_settings(
        &self,
        guild_id: &str,
        user_id: &str,
        session: &CastSession,
    ) -> Option<(String, f64, SynthesisEngine)> {
        let language = session.language.to_ascii_lowercase();
        let compatible = self
            .options
            .settings
            .available_models
            .iter()
            .filter(|model| {
                let model = model.to_ascii_lowercase();
                model.starts_with(&format!("{language}_"))
                    || model.starts_with(&format!("{language}-"))
            })
            .cloned()
            .collect::<Vec<_>>();
        let store = self.store.lock().ok()?;
        let config = store.guild_config(guild_id).ok()?;
        let stored = store.get_user_voice(guild_id, user_id).ok().flatten();
        let preferred = stored
            .as_ref()
            .map(|voice| voice.model.clone())
            .filter(|model| compatible.iter().any(|candidate| candidate == model));
        let model = preferred
            .or_else(|| compatible.first().cloned())
            .or_else(|| {
                (!config.default_voice.trim().is_empty())
                    .then(|| config.default_voice.clone())
                    .filter(|model| {
                        self.options
                            .settings
                            .available_models
                            .iter()
                            .any(|candidate| candidate == model)
                    })
            })
            .unwrap_or_else(|| self.options.settings.default_voice.clone());
        if session.engine == "piper" && !compatible.iter().any(|candidate| candidate == &model) {
            return None;
        }
        let speed = stored
            .as_ref()
            .map(|voice| voice.speed)
            .filter(|speed| speed.is_finite())
            .unwrap_or(self.options.settings.default_speed);
        let engine = match session.engine.as_str() {
            "piper" => SynthesisEngine::Piper,
            "kokoro" => SynthesisEngine::Kokoro,
            "gcloud" => SynthesisEngine::Gcloud,
            _ => SynthesisEngine::Default,
        };
        Some((model, speed, engine))
    }

    async fn handle_cast_command(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some(guild_id) = command.guild_id else {
            return Ok(());
        };
        if !command.data.options.is_empty() {
            return Ok(());
        }
        let Some(facts) = CoreVoiceInteractionFacts::from_command(command) else {
            return Ok(());
        };
        let guild_id = guild_id.get().to_string();
        let user_id = command.user.id.get().to_string();
        let Some(user_voice) = self.gateway_state.voice_channel_id(&guild_id, &user_id) else {
            command
                .create_response(
                    context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .embed(danger_embed(CAST_NOT_IN_VOICE))
                            .ephemeral(true),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        };
        if self
            .gateway_state
            .bot_voice_channel_id(&guild_id)
            .as_deref()
            != Some(&user_voice)
        {
            command
                .create_response(
                    context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .embed(danger_embed(CAST_NOT_IN_VOICE))
                            .ephemeral(true),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        }
        let engine = self
            .store
            .lock()
            .ok()
            .and_then(|store| store.get_user_voice(&guild_id, &user_id).ok().flatten())
            .map(|voice| match voice.engine {
                vozen_store::UserEngine::Piper => "piper",
                vozen_store::UserEngine::Kokoro => "kokoro",
                _ => "google",
            })
            .unwrap_or("piper")
            .to_owned();
        let session = CastSession {
            user_id,
            guild_id: guild_id.clone(),
            channel_id: command.channel_id.get().to_string(),
            voice_channel_id: user_voice,
            theme_key: None,
            language: "en".to_owned(),
            engine,
            issued_at_ms: system_now_ms(),
        };
        self.prune_cast_sessions(system_now_ms());
        if let Ok(mut sessions) = self.cast_sessions.lock() {
            sessions.insert(command.id.get().to_string(), session.clone());
        }
        let _ = facts;
        context
            .http
            .create_interaction_response(
                command.id,
                &command.token,
                &Self::cast_panel_payload(&command.id.get().to_string(), &session),
                Vec::new(),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    async fn handle_cast_component(
        &self,
        context: &Context,
        component: serenity::model::application::ComponentInteraction,
    ) -> Result<bool, GatewayEventDispatchError> {
        let Some((action, session_id)) = parse_cast_component_id(&component.data.custom_id) else {
            return Ok(false);
        };
        let Some(guild_id) = component.guild_id else {
            return Ok(true);
        };
        self.prune_cast_sessions(system_now_ms());
        let guild_id = guild_id.get().to_string();
        let user_id = component.user.id.get().to_string();
        let channel_id = component.channel_id.get().to_string();
        let mut session = {
            let mut sessions = self
                .cast_sessions
                .lock()
                .map_err(|_| GatewayEventDispatchError)?;
            let Some(session) = sessions.get(&session_id) else {
                return Ok(true);
            };
            if session.user_id != user_id
                || session.guild_id != guild_id
                || session.channel_id != channel_id
                || !session.valid_at(system_now_ms())
            {
                sessions.remove(&session_id);
                return Ok(true);
            }
            session.clone()
        };

        let selected = match &component.data.kind {
            ComponentInteractionDataKind::StringSelect { values } if values.len() == 1 => {
                values.first().cloned()
            }
            _ => None,
        };
        match action {
            CastAction::Theme => {
                if selected
                    .as_deref()
                    .and_then(vozen_discord::cast_theme_by_key)
                    .is_none()
                {
                    return Ok(true);
                }
                session.theme_key = selected;
            }
            CastAction::Language => {
                if selected.as_deref().is_none_or(|value| {
                    !CAST_LANGUAGE_CHOICES
                        .iter()
                        .any(|choice| choice.value == value)
                }) {
                    return Ok(true);
                }
                session.language = selected.expect("validated language");
            }
            CastAction::Engine => {
                if selected
                    .as_deref()
                    .is_none_or(|value| !matches!(value, "google" | "piper" | "kokoro"))
                {
                    return Ok(true);
                }
                session.engine = selected.expect("validated engine");
            }
            CastAction::Cancel => {
                if let Ok(mut sessions) = self.cast_sessions.lock() {
                    sessions.remove(&session_id);
                }
                component
                    .defer(context)
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                context
                    .http
                    .edit_original_interaction_response(
                        &component.token,
                        &Self::cast_terminal_edit_payload("Cast cancelled.", 0x5865F2),
                        Vec::new(),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                return Ok(true);
            }
            CastAction::Reveal => {
                if session.theme_key.is_none() {
                    return Ok(true);
                }
                if let Ok(mut sessions) = self.cast_sessions.lock() {
                    sessions.remove(&session_id);
                }
                component
                    .defer(context)
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                return self
                    .finish_cast_reveal(context, &component, &session)
                    .await
                    .map(|()| true);
            }
        }

        if let Ok(mut sessions) = self.cast_sessions.lock() {
            sessions.insert(session_id.clone(), session.clone());
        }
        component
            .defer(context)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        context
            .http
            .edit_original_interaction_response(
                &component.token,
                &Self::cast_panel_edit_payload(&session_id, &session),
                Vec::new(),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(true)
    }

    fn cast_panel_components(interaction_id: &str, session: &CastSession) -> Vec<CreateActionRow> {
        let themes = CAST_THEMES
            .iter()
            .map(|theme| {
                CreateSelectMenuOption::new(theme.label, theme.key)
                    .default_selection(session.theme_key.as_deref() == Some(theme.key))
            })
            .collect();
        let languages = CAST_LANGUAGE_CHOICES
            .iter()
            .map(|choice| {
                CreateSelectMenuOption::new(choice.name, choice.value)
                    .default_selection(choice.value == session.language)
            })
            .collect();
        let engines = [
            ("Google (gTTS)", "google"),
            ("Piper", "piper"),
            ("Kokoro", "kokoro"),
        ]
        .into_iter()
        .map(|(label, value)| {
            CreateSelectMenuOption::new(label, value).default_selection(value == session.engine)
        })
        .collect();
        let theme_menu = CreateSelectMenu::new(
            format!("cast:theme:{interaction_id}"),
            CreateSelectMenuKind::String { options: themes },
        )
        .placeholder("Choose a theme")
        .min_values(1)
        .max_values(1);
        let language_menu = CreateSelectMenu::new(
            format!("cast:language:{interaction_id}"),
            CreateSelectMenuKind::String { options: languages },
        )
        .placeholder("Choose a language")
        .min_values(1)
        .max_values(1);
        let engine_menu = CreateSelectMenu::new(
            format!("cast:engine:{interaction_id}"),
            CreateSelectMenuKind::String { options: engines },
        )
        .placeholder("Choose a voice engine")
        .min_values(1)
        .max_values(1);
        let reveal = CreateButton::new(format!("cast:reveal:{interaction_id}"))
            .label("Reveal cast")
            .style(ButtonStyle::Primary)
            .disabled(session.theme_key.is_none());
        let cancel = CreateButton::new(format!("cast:cancel:{interaction_id}"))
            .label("Cancel")
            .style(ButtonStyle::Secondary);
        vec![
            CreateActionRow::SelectMenu(theme_menu),
            CreateActionRow::SelectMenu(language_menu),
            CreateActionRow::SelectMenu(engine_menu),
            CreateActionRow::Buttons(vec![reveal, cancel]),
        ]
    }

    async fn finish_cast_reveal(
        &self,
        context: &Context,
        component: &serenity::model::application::ComponentInteraction,
        session: &CastSession,
    ) -> Result<(), GatewayEventDispatchError> {
        if self
            .gateway_state
            .voice_channel_id(&session.guild_id, &session.user_id)
            .as_deref()
            != Some(session.voice_channel_id.as_str())
            || self
                .gateway_state
                .bot_voice_channel_id(&session.guild_id)
                .as_deref()
                != Some(session.voice_channel_id.as_str())
        {
            context
                .http
                .edit_original_interaction_response(
                    &component.token,
                    &Self::cast_terminal_edit_payload(CAST_NOT_IN_VOICE, 0xED4245),
                    Vec::new(),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        }
        let members = self
            .fetch_cast_members(context, &session.guild_id, &session.voice_channel_id)
            .await;
        let humans = members
            .into_iter()
            .filter(|member| !member.bot)
            .collect::<Vec<_>>();
        if humans.is_empty() || humans.len() > CAST_MAX_MEMBERS {
            let message = if humans.len() > CAST_MAX_MEMBERS {
                "There are too many people in this call. `/cast` supports up to 25 humans."
            } else {
                "Ninguem esta disponivel no canal de voz."
            };
            context
                .http
                .edit_original_interaction_response(
                    &component.token,
                    &Self::cast_terminal_edit_payload(message, 0xED4245),
                    Vec::new(),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        }
        let mut seed = Uuid::new_v4().as_u128();
        let assignments = vozen_discord::assign_cast(
            &humans,
            session.theme_key.as_deref().unwrap_or_default(),
            || {
                seed = seed.rotate_left(17);
                (seed as f64) / (u128::MAX as f64)
            },
        )
        .ok_or(GatewayEventDispatchError)?;
        let Some((model, speed, engine)) =
            self.cast_voice_settings(&session.guild_id, &session.user_id, session)
        else {
            context
                .http
                .edit_original_interaction_response(
                    &component.token,
                    &Self::cast_terminal_edit_payload(
                        "A lingua selecionada nao tem uma voz instalada. Escolhe outra lingua e corre `/cast` novamente.",
                        0xED4245,
                    ),
                    Vec::new(),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        };
        let speech_assignments = assignments
            .iter()
            .map(|assignment| vozen_discord::CastAssignment {
                user_id: assignment.user_id.clone(),
                display_name: assignment.display_name.clone(),
                entry: assignment.entry,
            })
            .collect::<Vec<_>>();
        let speech = vozen_discord::build_cast_speech(&speech_assignments, &session.language);
        let chunks = vozen_discord::chunk_cast_speech(&speech, 260);
        let facts = CoreVoiceInteractionFacts {
            guild_id: session.guild_id.clone(),
            channel_id: session.channel_id.clone(),
            user_id: session.user_id.clone(),
            member_role_ids: component.member.as_ref().map(|member| {
                member
                    .roles
                    .iter()
                    .map(|role_id| role_id.get().to_string())
                    .collect()
            }),
        };
        let executor = self.executor(context)?;
        let first_outcome = executor
            .speak_text_with_voice(&facts, &chunks[0], &model, speed, engine, true)
            .await;
        let CoreVoiceOutcome::Tts(first_tts) = first_outcome else {
            return Ok(());
        };
        if matches!(
            first_tts,
            CoreTtsOutcome::NotInSameVoice
                | CoreTtsOutcome::Blocked
                | CoreTtsOutcome::RateLimited
                | CoreTtsOutcome::Empty
                | CoreTtsOutcome::FullyBlocked
                | CoreTtsOutcome::StoreUnavailable
        ) {
            let message = match first_tts {
                CoreTtsOutcome::NotInSameVoice => CAST_NOT_IN_VOICE,
                CoreTtsOutcome::Blocked => "You cannot use voice commands in this server.",
                CoreTtsOutcome::RateLimited => {
                    "You're doing that too quickly. Try again in a moment."
                }
                _ => "This cast could not be spoken right now. Run `/cast` again.",
            };
            context
                .http
                .edit_original_interaction_response(
                    &component.token,
                    &Self::cast_terminal_edit_payload(message, 0xED4245),
                    Vec::new(),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        }
        let theme_label = session
            .theme_key
            .as_deref()
            .and_then(vozen_discord::cast_theme_by_key)
            .map(|theme| theme.label)
            .unwrap_or("Cast");
        let language_label = CAST_LANGUAGE_CHOICES
            .iter()
            .find(|choice| choice.value == session.language)
            .map(|choice| choice.name)
            .unwrap_or("English");
        let mut public = format!("🎭 **Cast revealed — {theme_label} · {language_label}**");
        for assignment in &assignments {
            public.push_str(&format!(
                "\n• <@{}> → {}",
                assignment.user_id, assignment.entry.label
            ));
        }
        if session.theme_key.as_deref() == Some("pokemon") {
            public.push_str(
                "\n-# Unofficial fan reference; not affiliated with Nintendo, Game Freak, or The Pokémon Company.",
            );
        }
        component
            .create_followup(
                context,
                CreateInteractionResponseFollowup::new()
                    .content(public)
                    .allowed_mentions(
                        CreateAllowedMentions::new()
                            .all_users(false)
                            .all_roles(false)
                            .everyone(false),
                    ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;

        let spoken = first_tts == CoreTtsOutcome::Queued;
        if spoken {
            for chunk in chunks.into_iter().skip(1) {
                let outcome = executor
                    .speak_text_with_voice(&facts, &chunk, &model, speed, engine, false)
                    .await;
                if outcome != CoreVoiceOutcome::Tts(CoreTtsOutcome::Queued) {
                    break;
                }
            }
        }
        let terminal_message = if spoken {
            "Cast revelado e falado no canal de voz."
        } else {
            "Cast revelado no chat; a voz esta ocupada."
        };
        context
            .http
            .edit_original_interaction_response(
                &component.token,
                &Self::cast_terminal_edit_payload(
                    terminal_message,
                    if spoken { 0x57F287 } else { 0xED4245 },
                ),
                Vec::new(),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }
}

#[async_trait]
impl GatewayEventSink for CoreVoiceGatewaySink {
    async fn on_ready(&self, context: Context) -> Result<(), GatewayEventDispatchError> {
        self.recover_planned_sessions(&context).await?;
        if let Some(game) = &self.game_runtime {
            let executor = self.executor(&context)?;
            game.start_tick(context.clone(), executor);
        }
        Ok(())
    }

    async fn on_message(
        &self,
        context: Context,
        message: serenity::model::channel::Message,
    ) -> Result<(), GatewayEventDispatchError> {
        if let Some(game) = &self.game_runtime
            && game.handle_message(&context, &message).await?
        {
            return Ok(());
        }
        if !self.options.message_autoread
            || self
                .gateway_state
                .bot_user_id()
                .is_some_and(|bot_id| bot_id == message.author.id.get().to_string())
        {
            return Ok(());
        }
        let Some(facts) = DiscordMessageFactsOwned::from_message(&self.gateway_state, &message)
        else {
            return Ok(());
        };
        let media = collect_message_media(&message);
        if message.content.is_empty() && media.is_empty() {
            return Ok(());
        }
        let autojoined_for_author = self.maybe_autojoin_for_message(&context, &facts).await;
        let service = self.message_service(&context)?;
        let announce_speaker = self.announce_speaker(&facts, &message);
        let detected_language = self.detected_language(&facts, &message.content);
        let mentioned_users = message
            .mentions
            .iter()
            .map(|user| (user.id.get().to_string(), user.name.clone()))
            .collect::<BTreeMap<_, _>>();
        let mentioned_channels = message
            .mention_channels
            .iter()
            .map(|channel| (channel.id.get().to_string(), channel.name.clone()))
            .collect::<BTreeMap<_, _>>();
        // These maps are derived only from this gateway payload. They avoid a guild-wide member
        // cache and are discarded after the message is prepared.
        let resolve_user = |id: &str| {
            mentioned_users
                .get(id)
                .cloned()
                .unwrap_or_else(|| "someone".to_owned())
        };
        let resolve_channel = |id: &str| {
            mentioned_channels
                .get(id)
                .cloned()
                .unwrap_or_else(|| "a channel".to_owned())
        };
        let outcome = service
            .execute(MessageVoiceInvocation {
                facts: facts.as_borrowed_with_autojoined(autojoined_for_author),
                raw: &message.content,
                media: &media,
                detected_language,
                announce_speaker: announce_speaker.as_deref(),
                resolve_user: &resolve_user,
                resolve_channel: &resolve_channel,
            })
            .await;
        if !facts.author_is_bot
            && facts.bot_voice_channel_id.as_deref() == Some(facts.channel_id.as_str())
        {
            // Diagnose the integrated voice-chat path without logging message content or users.
            let result = match &outcome {
                MessageVoiceOutcome::Denied(reason) => format!("denied:{reason:?}"),
                MessageVoiceOutcome::Queued { .. } => "queued".to_owned(),
                other => format!("{other:?}"),
            };
            eprintln!(
                "[voice:channel-chat] guild={} outcome={result}",
                facts.guild_id
            );
        }
        if let MessageVoiceOutcome::Queued { talk } = outcome {
            if let Ok(mut speakers) = self.last_speakers.lock() {
                speakers.insert(facts.guild_id.clone(), facts.author_id.clone());
            }
            let (streak_announce, locale, tts_channel_id, vote_promos) = self
                .store
                .lock()
                .ok()
                .and_then(|store| store.guild_config(&facts.guild_id).ok())
                .map(|config| {
                    (
                        config.streak_announce,
                        config.locale,
                        config.tts_channel_id,
                        config.vote_promos,
                    )
                })
                .unwrap_or((false, "en".to_owned(), None, false));
            if let Some(talk) = talk
                && talk.first_of_day
                && talk.streak >= 2
                && let Some(localizer) = self.localizer.as_ref()
                && streak_announce
            {
                let mut parameters = BTreeMap::new();
                parameters.insert("user", facts.author_id.clone());
                parameters.insert("n", talk.streak.to_string());
                let content = localizer
                    .render_key("streak.day", Some(locale.as_str()), None, &parameters)
                    .map(|content| style_streak_message(content, talk.streak));
                let avatar_url = card_avatar_url(&message.author.static_face());
                let avatar_data_url = fetch_avatar_data_url(Some(&avatar_url)).await;
                let mut outgoing = CreateMessage::new().allowed_mentions(no_mentions());
                if let Some(card_bytes) = build_streak_card(
                    &message.author.name,
                    talk.streak,
                    avatar_data_url.as_deref(),
                ) {
                    outgoing =
                        outgoing.add_file(CreateAttachment::bytes(card_bytes, "vozen-streak.png"));
                } else {
                    eprintln!(
                        "[streak] card rendering failed for user {}; sending text fallback",
                        facts.author_id
                    );
                }
                if let Some(content) = content {
                    outgoing = outgoing.content(content);
                }
                if let Err(error) = message
                    .channel_id
                    .send_message(&context.http, outgoing)
                    .await
                {
                    eprintln!(
                        "[streak] announcement send failed for user {}: {error}",
                        facts.author_id
                    );
                }
            }

            // Activity is counted only after the queue accepted the message. The poster itself
            // is process-local and bounded; durable rows are read only when a draw succeeds.
            let mut automatic_post_sent = false;
            if tts_channel_id.is_some()
                && self
                    .leaderboard_poster
                    .lock()
                    .ok()
                    .is_some_and(|mut poster| {
                        poster.record(&facts.guild_id, system_now_ms(), random_unit())
                    })
            {
                let rows = self
                    .store
                    .lock()
                    .ok()
                    .and_then(|store| store.top_speakers(&facts.guild_id, &utc_day_key(), 10).ok())
                    .unwrap_or_default();
                if !rows.is_empty()
                    && let Some(localizer) = self.localizer.as_ref()
                    && let Some(channel_id) = tts_channel_id.as_deref()
                    && let Ok(channel_id) = channel_id.parse::<u64>()
                {
                    let mut lines = Vec::with_capacity(rows.len() + 1);
                    let empty = BTreeMap::new();
                    if let Some(title) =
                        localizer.render_key("leaderboard.autoTitle", Some(&locale), None, &empty)
                    {
                        lines.push(title);
                    }
                    for (index, row) in rows.into_iter().enumerate() {
                        let mut parameters = BTreeMap::new();
                        parameters.insert("rank", (index + 1).to_string());
                        parameters.insert("user", row.user_id);
                        parameters.insert("count", row.count.to_string());
                        parameters.insert("streak", row.streak.to_string());
                        if let Some(line) = localizer.render_key(
                            "topspeakers.line",
                            Some(&locale),
                            None,
                            &parameters,
                        ) {
                            lines.push(line);
                        }
                    }
                    if !lines.is_empty()
                        && ChannelId::new(channel_id)
                            .send_message(
                                &context.http,
                                CreateMessage::new()
                                    .content(lines.join("\n"))
                                    .allowed_mentions(no_mentions()),
                            )
                            .await
                            .is_ok()
                    {
                        automatic_post_sent = true;
                    }
                }
            }

            // The promotional slot shares activity with the leaderboard but is suppressed when
            // that leaderboard already posted. Reservation happens only after the target channel
            // is parseable, and the SQLite update remains atomic across restarts/workers.
            if !automatic_post_sent
                && vote_promos
                && self.options.client_id.is_some()
                && let Some(channel_id) = tts_channel_id.as_deref().and_then(|id| id.parse().ok())
            {
                let promo = self.store.lock().ok().and_then(|store| {
                    self.vote_promo_poster.lock().ok().and_then(|mut poster| {
                        poster
                            .record(&store, &facts.guild_id, system_now_ms(), random_unit())
                            .ok()
                            .flatten()
                    })
                });
                if let Some(kind) = promo {
                    let mut parameters = BTreeMap::new();
                    let content_key = match kind {
                        CommunityPromoKind::Vote => "vote.upsell",
                        CommunityPromoKind::Support => "help.support",
                    };
                    if let Some(client_id) = self.options.client_id.as_deref()
                        && let Some(url) = match kind {
                            CommunityPromoKind::Vote => {
                                Some(format!("https://top.gg/bot/{client_id}/vote"))
                            }
                            CommunityPromoKind::Support => Some(self.options.support_url.clone()),
                        }
                    {
                        parameters.insert("url", url.clone());
                        let opt_out_label = self
                            .localizer
                            .as_ref()
                            .and_then(|localizer| {
                                localizer.render_key(
                                    "config.votePromosLabel",
                                    Some(&locale),
                                    None,
                                    &BTreeMap::new(),
                                )
                            })
                            .unwrap_or_else(|| "Vozen notices".to_owned());
                        if let Some(localizer) = self.localizer.as_ref()
                            && let Some(content) =
                                localizer.render_key(content_key, Some(&locale), None, &parameters)
                        {
                            let button = match kind {
                                CommunityPromoKind::Vote => {
                                    let label = localizer
                                        .render_key(
                                            "vote.button",
                                            Some(&locale),
                                            None,
                                            &BTreeMap::new(),
                                        )
                                        .unwrap_or_else(|| "Vote on top.gg".to_owned());
                                    CreateButton::new_link(url).label(label)
                                }
                                CommunityPromoKind::Support => {
                                    CreateButton::new_link(url).label("Vozen Support")
                                }
                            };
                            let _ = ChannelId::new(channel_id)
                                .send_message(
                                    &context.http,
                                    CreateMessage::new()
                                        .content(format!(
                                            "{content}\n\n{opt_out_label}: `/config vote-reminders active:false`"
                                        ))
                                        .components(vec![CreateActionRow::Buttons(vec![button])])
                                        .allowed_mentions(no_mentions()),
                                )
                                .await;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn on_interaction(
        &self,
        context: Context,
        interaction: Interaction,
    ) -> Result<(), GatewayEventDispatchError> {
        if let Some(game) = &self.game_runtime {
            // The Rust sink can be promoted after the gateway READY event.  Lazily bind the
            // shared voice executor here as well so game rounds can speak even in that case.
            let executor = self.executor(&context)?;
            game.start_tick(context.clone(), executor);
            match &interaction {
                Interaction::Component(component) => {
                    if game.handle_component(&context, component.clone()).await? {
                        return Ok(());
                    }
                }
                Interaction::Command(command)
                    if command.data.name == "game"
                        && game.handle_command(&context, command).await? =>
                {
                    return Ok(());
                }
                _ => {}
            }
        }
        if self.options.setup_enabled
            && let Interaction::Command(command) = &interaction
            && command.data.name == "setup"
            && !command.data.options.is_empty()
        {
            return match tokio::time::timeout(
                SETUP_TIMEOUT,
                self.handle_setup_command(&context, command),
            )
            .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => {
                    eprintln!("[setup] setup flow failed after claiming the interaction");
                    command
                        .edit_response(
                            &context,
                            EditInteractionResponse::new()
                                .content(
                                    "Setup could not finish. Check the bot's channel permissions and try again.",
                                )
                                .allowed_mentions(no_mentions()),
                        )
                        .await
                        .map(|_| ())
                        .map_err(|_| GatewayEventDispatchError)
                }
                Err(_) => {
                    eprintln!("[setup] setup flow timed out after claiming the interaction");
                    command
                        .edit_response(
                            &context,
                            EditInteractionResponse::new()
                                .content(
                                    "Setup timed out while checking Discord or voice state. Try again after checking the bot's voice permissions.",
                                )
                                .allowed_mentions(no_mentions()),
                        )
                        .await
                        .map(|_| ())
                        .map_err(|_| GatewayEventDispatchError)
                }
            };
        }
        if self.options.randomizer_enabled || self.options.cast_enabled {
            match &interaction {
                Interaction::Component(component) => {
                    if self.options.randomizer_enabled
                        && self
                            .handle_randomizer_component(&context, component.clone())
                            .await?
                    {
                        return Ok(());
                    }
                    if self.options.cast_enabled
                        && self
                            .handle_cast_component(&context, component.clone())
                            .await?
                    {
                        return Ok(());
                    }
                    return Ok(());
                }
                Interaction::Modal(modal) => {
                    if self.options.randomizer_enabled
                        && self
                            .handle_randomizer_modal(&context, modal.clone())
                            .await?
                    {
                        return Ok(());
                    }
                    return Ok(());
                }
                Interaction::Command(command) if command.data.name == "randomizer" => {
                    return self.handle_randomizer_command(&context, command).await;
                }
                Interaction::Command(command) if command.data.name == "cast" => {
                    if self.options.cast_enabled {
                        return self.handle_cast_command(&context, command).await;
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
        let Interaction::Command(command) = interaction else {
            return Ok(());
        };
        if self.options.speak_context_enabled
            && command.data.kind == CommandType::Message
            && command.data.name == vozen_discord::SPEAK_MESSAGE_COMMAND
        {
            return self.handle_speak_context(&context, &command).await;
        }
        let Some(facts) = CoreVoiceInteractionFacts::from_command(&command) else {
            return Ok(());
        };
        if self.options.queue_enabled
            && let Some(queue) =
                parse_queue_command(&command.data).map_err(|_| GatewayEventDispatchError)?
        {
            return self
                .handle_queue_interaction(&context, &command, &facts, queue)
                .await;
        }
        let executor = self.executor(&context)?;
        let defer_ephemeral = Executor::requires_ephemeral_defer(&command.data)
            .map_err(|_| GatewayEventDispatchError)?;
        let defer_public = Executor::requires_public_defer(&command.data)
            .map_err(|_| GatewayEventDispatchError)?;
        if defer_ephemeral {
            command
                .defer_ephemeral(&context)
                .await
                .map_err(|_| GatewayEventDispatchError)?;
        } else if defer_public {
            command
                .defer(&context)
                .await
                .map_err(|_| GatewayEventDispatchError)?;
        }
        let response = executor
            .execute(
                &command.data,
                &facts,
                Some(&command.locale),
                &|_| "someone".into(),
                &|_| "channel".into(),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let CoreVoiceInteractionExecution::Reply { content, .. } = response else {
            return Ok(());
        };
        if defer_ephemeral || defer_public {
            command
                .edit_response(
                    &context,
                    EditInteractionResponse::new().embeds(vec![message_embed(content)]),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
        } else {
            command
                .create_response(
                    &context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .embeds(vec![message_embed(content)]),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
        }
        Ok(())
    }

    async fn on_guild_delete(&self, guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        if let Some(game) = &self.game_runtime {
            game.on_guild_delete(guild_id)?;
        }
        if let Ok(executor) = self.executor.lock()
            && let Some(executor) = executor.as_ref()
        {
            executor.forget_guild(guild_id);
        }
        if let Ok(service) = self.message_service.lock()
            && let Some(service) = service.as_ref()
        {
            service.forget_guild(guild_id);
        }
        if let Ok(mut speakers) = self.last_speakers.lock() {
            speakers.remove(guild_id);
        }
        if let Ok(mut poster) = self.leaderboard_poster.lock() {
            poster.forget_guild(guild_id);
        }
        if let Ok(mut poster) = self.vote_promo_poster.lock() {
            poster.forget_guild(guild_id);
        }
        if let Ok(mut sessions) = self.cast_sessions.lock() {
            sessions.retain(|_, session| session.guild_id != guild_id);
        }
        Ok(())
    }

    async fn on_voice_state_update(
        &self,
        context: Context,
        old: Option<serenity::model::voice::VoiceState>,
        new: serenity::model::voice::VoiceState,
    ) -> Result<(), GatewayEventDispatchError> {
        if let Some(game) = &self.game_runtime {
            game.on_voice_state_update(&context, &new).await?;
        }
        let Some(guild_id) = new.guild_id.map(|id| id.get().to_string()) else {
            return Ok(());
        };
        if let Some(bot_channel_id) = self.gateway_state.bot_voice_channel_id(&guild_id) {
            self.greet_joining_member(&context, old.as_ref(), &new, &bot_channel_id)
                .await;
            self.leave_if_alone(&context, &guild_id, &bot_channel_id)
                .await;
        } else if let Ok(mut speakers) = self.last_speakers.lock() {
            // The bot left the call (manual leave, disconnect, or permission loss).  The next
            // session starts fresh so xsaid behaves identically for Google and Piper.
            speakers.remove(&guild_id);
        }
        Ok(())
    }
}

impl CoreVoiceGatewaySink {
    async fn handle_speak_context(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some(parsed) =
            parse_speak_message_command(&command.data).map_err(|_| GatewayEventDispatchError)?
        else {
            return Ok(());
        };
        let Some(facts) = CoreVoiceInteractionFacts::from_command(command) else {
            return Ok(());
        };
        let executor = self.executor(context)?;
        command
            .defer_ephemeral(context)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let raw = parsed.message.content.trim();
        let content = if raw.is_empty() {
            executor
                .render_key("speak.emptyMessage", &facts, Some(&command.locale))
                .map_err(|_| GatewayEventDispatchError)?
        } else {
            let outcome = executor.speak_text(&facts, raw).await;
            executor
                .render_speak_outcome(outcome, &facts, Some(&command.locale))
                .map_err(|_| GatewayEventDispatchError)?
        };
        command
            .edit_response(
                context,
                EditInteractionResponse::new()
                    .content(content)
                    .allowed_mentions(no_mentions()),
            )
            .await
            .map(|_| ())
            .map_err(|_| GatewayEventDispatchError)
    }

    async fn handle_queue_interaction(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
        facts: &CoreVoiceInteractionFacts,
        queue: vozen_discord::QueueCommand,
    ) -> Result<(), GatewayEventDispatchError> {
        let dependencies = self.dependencies(context)?;
        let can_manage_guild = command
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .is_some_and(|permissions| permissions.contains(Permissions::MANAGE_GUILD));
        let caller_voice_channel = self
            .gateway_state
            .voice_channel_id(&facts.guild_id, &facts.user_id);
        let bot_voice_channel = self.gateway_state.bot_voice_channel_id(&facts.guild_id);
        let outcome = QueueControlService::new(dependencies.playback.clone())
            .execute(
                QueueControlInvocation {
                    guild_id: &facts.guild_id,
                    user_id: &facts.user_id,
                    can_manage_guild,
                    caller_voice_channel_id: caller_voice_channel.as_deref(),
                    bot_voice_channel_id: bot_voice_channel.as_deref(),
                    now_ms: system_now_ms().try_into().unwrap_or_default(),
                },
                queue,
            )
            .await;
        command
            .create_response(
                context,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(queue_response(outcome))
                        .ephemeral(true),
                ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    /// Returns a detection only for members who explicitly enabled automatic language detection.
    /// Store faults and uncertain text deliberately fall back to the configured voice.
    fn detected_language(
        &self,
        facts: &DiscordMessageFactsOwned,
        message: &str,
    ) -> Option<&'static str> {
        let enabled = self
            .store
            .lock()
            .ok()
            .and_then(|store| {
                store
                    .is_detection_on(&facts.guild_id, &facts.author_id)
                    .ok()
            })
            .unwrap_or(false);
        enabled.then(|| detect_language(message)).flatten()
    }

    fn announce_speaker(
        &self,
        facts: &DiscordMessageFactsOwned,
        message: &serenity::model::channel::Message,
    ) -> Option<String> {
        let (xsaid, nickname) = self.store.lock().ok().and_then(|store| {
            let config = store.guild_config(&facts.guild_id).ok()?;
            let nickname = store.nickname(&facts.guild_id, &facts.author_id).ok()?;
            Some((config.xsaid, nickname))
        })?;
        if !xsaid
            || self
                .last_speakers
                .lock()
                .ok()
                .and_then(|speakers| speakers.get(&facts.guild_id).cloned())
                .is_some_and(|last| last == facts.author_id)
        {
            return None;
        }
        let raw = nickname
            .or_else(|| {
                message
                    .member
                    .as_ref()
                    .and_then(|member| member.nick.clone())
            })
            .unwrap_or_else(|| message.author.name.clone());
        sanitize_speaker_name(&raw)
    }
}

fn sanitize_speaker_name(raw: &str) -> Option<String> {
    let mut output = String::with_capacity(raw.len().min(40));
    let mut last_was_space = true;
    for character in raw.chars() {
        let allowed = character.is_alphanumeric() || matches!(character, '-' | '\'' | '\u{2019}');
        if allowed {
            output.push(character);
            last_was_space = false;
        } else if (character.is_whitespace() || character == '_')
            && !last_was_space
            && !output.is_empty()
        {
            output.push(' ');
            last_was_space = true;
        }
        if output.chars().count() >= 40 {
            break;
        }
    }
    let value = output.trim();
    value
        .chars()
        .any(char::is_alphanumeric)
        .then(|| value.to_owned())
}

fn queue_response(outcome: QueueControlOutcome) -> String {
    match outcome {
        QueueControlOutcome::Empty => "The queue is empty.".to_owned(),
        QueueControlOutcome::Snapshot(items) => {
            let lines = items.iter().map(queue_item_line).collect::<Vec<_>>();
            if lines.is_empty() {
                "The queue is empty.".to_owned()
            } else {
                format!("Pending queue ({}):\n{}", lines.len(), lines.join("\n"))
            }
        }
        QueueControlOutcome::Removed => "Removed that queued item.".to_owned(),
        QueueControlOutcome::Unavailable => "That queue item is unavailable.".to_owned(),
        QueueControlOutcome::RequiresManageGuild => {
            "You need Manage Server to control the queue.".to_owned()
        }
        QueueControlOutcome::NotInSameVoice => {
            "Join Vozen's voice channel to control audio.".to_owned()
        }
        QueueControlOutcome::Cleared => "Cleared the queue.".to_owned(),
        QueueControlOutcome::Paused => "Audio paused.".to_owned(),
        QueueControlOutcome::NothingToPause => "There is no audio to pause.".to_owned(),
        QueueControlOutcome::Resumed => "Audio resumed.".to_owned(),
        QueueControlOutcome::NotPaused => "Audio is not paused.".to_owned(),
        QueueControlOutcome::Skipped => "Skipped the current audio.".to_owned(),
        QueueControlOutcome::NothingPlaying => "There is no audio to skip.".to_owned(),
        QueueControlOutcome::PlaybackFailed => "The queue is unavailable right now.".to_owned(),
    }
}

fn queue_item_line(item: &PublicQueueItem) -> String {
    format!(
        "- `{}` - {}, {}, {}s waiting",
        item.id,
        queue_source_label(item.source),
        queue_lane_label(item.lane),
        item.age_ms / 1_000
    )
}

fn queue_source_label(source: QueueSource) -> &'static str {
    match source {
        QueueSource::Message => "message",
        QueueSource::Command => "command",
        QueueSource::Game => "game",
        QueueSource::Sound => "sound",
        QueueSource::System => "system",
    }
}

fn queue_lane_label(lane: QueueLane) -> &'static str {
    match lane {
        QueueLane::Standard => "standard",
        QueueLane::Accessibility => "accessibility",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_picker_insertions_obey_capacity_and_ttl_without_component_activity() {
        let pick = |issued_at_ms| PendingGamePick {
            guild_id: "guild".into(),
            parent_channel_id: "text".into(),
            user_id: "user".into(),
            selected_game: None,
            language: None,
            engine: "google".into(),
            locale: "en".into(),
            guild_locale: None,
            issued_at_ms,
        };
        let mut picks = BTreeMap::new();
        for index in 0..(MAX_PENDING_GAME_PICKS + 50) {
            picks.insert(format!("pick-{index}"), pick(index as i64));
            GameRuntime::prune_picks_locked(&mut picks, index as i64);
            assert!(picks.len() <= MAX_PENDING_GAME_PICKS);
        }
        assert!(!picks.contains_key("pick-0"));
        let now = GAME_PICK_TTL_MS + MAX_PENDING_GAME_PICKS as i64 + 51;
        picks.insert("new".into(), pick(now));
        GameRuntime::prune_picks_locked(&mut picks, now);
        assert_eq!(
            picks.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["new"]
        );
    }

    use vozen_discord::CoreVoiceSettings;

    #[test]
    fn queue_responses_keep_items_opaque_and_match_node_wording() {
        assert_eq!(
            queue_response(QueueControlOutcome::Snapshot(vec![PublicQueueItem {
                id: "opaque".into(),
                source: QueueSource::Message,
                lane: QueueLane::Standard,
                age_ms: 3_200,
            }])),
            "Pending queue (1):\n- `opaque` - message, standard, 3s waiting"
        );
        assert_eq!(
            queue_response(QueueControlOutcome::NotInSameVoice),
            "Join Vozen's voice channel to control audio."
        );
    }

    #[test]
    fn promotion_options_preserve_distinct_queue_and_synthesis_limits() {
        let options = CoreVoiceRuntimeOptions {
            piper_path: "piper".into(),
            models_dir: "models".into(),
            cache_dir: "cache".into(),
            gtts_cache_dir: "gtts-cache".into(),
            ffmpeg: "ffmpeg".into(),
            openai_api_key: None,
            neural_cache_dir: "neural-cache".into(),
            gcloud_api_key: None,
            gcloud_cache_dir: "gcloud-cache".into(),
            gcloud_limits: vozen_tts::GcloudLimits {
                max_chars: 500,
                plus_monthly: 100_000,
                pass3_monthly: 400_000,
                pass8_monthly: 1_000_000,
                daily_budget: 300_000,
            },
            kokoro_command: None,
            kokoro_cache_dir: "kokoro-cache".into(),
            kokoro_languages: None,
            piper_concurrency: 2,
            queue_cap: 20,
            queue_enabled: true,
            message_autoread: false,
            randomizer_enabled: false,
            cast_enabled: false,
            setup_enabled: false,
            speak_context_enabled: false,
            game_play_enabled: false,
            client_id: None,
            support_url: "https://discord.gg/4kYw2WUbNN".into(),
            settings: CoreVoiceSettings {
                available_models: vec!["en_US-amy-medium".into()],
                default_voice: "en_US-amy-medium".into(),
                default_speed: 1.0,
                default_engine: SynthesisEngine::Piper,
            },
        };
        assert_eq!(options.piper_concurrency, 2);
        assert_eq!(options.queue_cap, 20);
    }

    #[test]
    fn speaker_names_keep_only_pronounceable_characters() {
        assert_eq!(
            sanitize_speaker_name("🔥xX_Pro_Xx🔥").as_deref(),
            Some("xX Pro Xx")
        );
        assert_eq!(sanitize_speaker_name("---").as_deref(), None);
        assert_eq!(
            sanitize_speaker_name("Rexy’s test").as_deref(),
            Some("Rexy’s test")
        );
    }

    #[test]
    fn language_detection_requires_the_members_opt_in() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let sink = CoreVoiceGatewaySink::new(
            store.clone(),
            GatewayState::default(),
            CoreVoiceRuntimeOptions {
                piper_path: "piper".into(),
                models_dir: "models".into(),
                cache_dir: "cache".into(),
                gtts_cache_dir: "gtts-cache".into(),
                ffmpeg: "ffmpeg".into(),
                openai_api_key: None,
                neural_cache_dir: "neural-cache".into(),
                gcloud_api_key: None,
                gcloud_cache_dir: "gcloud-cache".into(),
                gcloud_limits: vozen_tts::GcloudLimits {
                    max_chars: 500,
                    plus_monthly: 100_000,
                    pass3_monthly: 400_000,
                    pass8_monthly: 1_000_000,
                    daily_budget: 300_000,
                },
                kokoro_command: None,
                kokoro_cache_dir: "kokoro-cache".into(),
                kokoro_languages: None,
                piper_concurrency: 1,
                queue_cap: 1,
                queue_enabled: true,
                message_autoread: true,
                randomizer_enabled: false,
                cast_enabled: false,
                setup_enabled: false,
                speak_context_enabled: false,
                game_play_enabled: false,
                client_id: None,
                support_url: "https://discord.gg/4kYw2WUbNN".into(),
                settings: CoreVoiceSettings {
                    available_models: vec!["en_US-amy-medium".into()],
                    default_voice: "en_US-amy-medium".into(),
                    default_speed: 1.0,
                    default_engine: SynthesisEngine::Piper,
                },
            },
        );
        let facts = DiscordMessageFactsOwned {
            guild_id: "guild".into(),
            channel_id: "text".into(),
            author_id: "user".into(),
            author_is_bot: false,
            mentioned_bot: false,
            replied_to_bot: false,
            author_voice_channel_id: Some("voice".into()),
            bot_voice_channel_id: Some("voice".into()),
            member_role_ids: Some(Vec::new()),
        };
        assert_eq!(sink.detected_language(&facts, "Olá!"), None);
        store
            .lock()
            .expect("store lock")
            .set_detection_on("guild", "user", true)
            .expect("enable");
        assert_eq!(sink.detected_language(&facts, "Olá!"), Some("por"));
    }

    #[test]
    fn game_cards_use_the_same_components_v2_container_as_node() {
        let body: Value =
            serde_json::from_slice(&game_card_body("Round 1/5").expect("serialize game card"))
                .expect("valid payload");
        assert_eq!(body["flags"], 1_u64 << 15);
        assert_eq!(body["components"][0]["type"], 17);
        assert_eq!(body["components"][0]["components"][0]["type"], 10);
        assert_eq!(
            body["components"][0]["components"][0]["content"],
            "Round 1/5"
        );
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    }

    #[test]
    fn game_engine_picker_routes_every_visible_choice() {
        assert_eq!(
            GameRuntime::game_engine("google"),
            Some(SynthesisEngine::Default)
        );
        assert_eq!(
            GameRuntime::game_engine("piper"),
            Some(SynthesisEngine::Piper)
        );
        assert_eq!(
            GameRuntime::game_engine("kokoro"),
            Some(SynthesisEngine::Kokoro)
        );
        assert_eq!(GameRuntime::game_engine("gcloud"), None);
    }

    #[test]
    fn only_node_scoreboard_games_receive_the_shared_final_card() {
        for game in [
            "guess-language",
            "math",
            "skip-count",
            "spelling",
            "spell-out",
            "fast-speech",
            "accent-swap",
            "reflexes",
            "vozen-says",
            "headsOrTails",
        ] {
            assert!(game_uses_shared_standings(game), "{game}");
        }
        for game in [
            "roulette",
            "hangman",
            "wordle",
            "tictactoe",
            "chess",
            "word-chain",
        ] {
            assert!(!game_uses_shared_standings(game), "{game}");
        }
    }
}
