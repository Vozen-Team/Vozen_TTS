//! Opt-in Rust owner for `/transcribe start|stop`.
//!
//! The sink keeps all sensitive work off Songbird's audio callback: that callback only applies
//! the in-memory consent gate and forwards bounded PCM utterances to a Tokio channel. This sink
//! owns the Whisper/ffmpeg pipeline, Discord messages, consent button and teardown.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serenity::{
    builder::{
        CreateActionRow, CreateAllowedMentions, CreateButton, CreateInteractionResponse,
        CreateInteractionResponseMessage, CreateMessage, EditInteractionResponse, EditMessage,
    },
    client::Context,
    model::{
        Permissions,
        application::Interaction,
        channel::Message,
        id::{ChannelId, GuildId, MessageId},
        voice::VoiceState,
    },
};
use tokio::sync::mpsc::{self, Receiver};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use vozen_discord::{
    GatewayEventDispatchError, GatewayEventSink, ReceivedUtterance, SongbirdVoiceReceiver,
    TranscriptionSessionCommand, VoiceReceiver, VoiceResponseLocalizer,
    parse_transcription_session_command,
};
use vozen_store::SqliteStore;

use crate::{
    system_now_ms, transcription_adapter::AttachmentTranscriber,
    transcription_control_sink::SttConsentRegistry,
};

const FRAME_SAMPLES: usize = 1_920;
const MAX_SESSION_SECONDS: u64 = 20;
// Four 20-second PCM turns cap queued audio at roughly 15 MiB per active session.
const MAX_QUEUED_UTTERANCES: usize = 4;

struct LiveSession {
    receiver: SongbirdVoiceReceiver,
    voice_channel_id: String,
    announcement_channel_id: ChannelId,
    announcement_id: MessageId,
    announcement_stop: String,
    ever_consented: Arc<std::sync::atomic::AtomicBool>,
    _slot: OwnedSemaphorePermit,
}

pub struct LiveTranscriptionGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    gateway_state: vozen_discord::GatewayState,
    transcriber: Arc<AttachmentTranscriber>,
    sessions: Arc<Mutex<BTreeMap<String, LiveSession>>>,
    starting: Arc<Mutex<BTreeSet<String>>>,
    slots: Arc<Semaphore>,
    consent_registry: SttConsentRegistry,
    localizer: VoiceResponseLocalizer,
}

impl LiveTranscriptionGatewaySink {
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        gateway_state: vozen_discord::GatewayState,
        transcriber: AttachmentTranscriber,
        max_concurrency: usize,
        consent_registry: SttConsentRegistry,
    ) -> Result<Self, GatewayEventDispatchError> {
        Ok(Self {
            store,
            gateway_state,
            transcriber: Arc::new(transcriber),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            starting: Arc::new(Mutex::new(BTreeSet::new())),
            slots: Arc::new(Semaphore::new(max_concurrency.max(1))),
            consent_registry,
            localizer: VoiceResponseLocalizer::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
        })
    }

    fn message(
        &self,
        key: &str,
        interaction_locale: Option<&str>,
        guild_locale: Option<&str>,
    ) -> Result<String, GatewayEventDispatchError> {
        self.localizer
            .render_key(key, interaction_locale, guild_locale, &BTreeMap::new())
            .ok_or(GatewayEventDispatchError)
    }

    fn can_manage(command: &serenity::model::application::CommandInteraction) -> bool {
        command
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .is_some_and(|permissions| permissions.contains(Permissions::MANAGE_GUILD))
    }

    async fn send_ephemeral(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
        content: String,
    ) -> Result<(), GatewayEventDispatchError> {
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
            .map(|_| ())
            .map_err(|_| GatewayEventDispatchError)
    }

    async fn edit_response(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
        content: String,
    ) -> Result<(), GatewayEventDispatchError> {
        command
            .edit_response(context, EditInteractionResponse::new().content(content))
            .await
            .map(|_| ())
            .map_err(|_| GatewayEventDispatchError)
    }

    async fn start(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
        language: Option<String>,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some(guild_id) = command.guild_id else {
            return self
                .send_ephemeral(
                    context,
                    command,
                    self.message(
                        "stt.guildOnly",
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                    )?,
                )
                .await;
        };
        let guild_key = guild_id.get().to_string();
        if !Self::can_manage(command) {
            return self
                .send_ephemeral(
                    context,
                    command,
                    self.message(
                        "stt.noManage",
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                    )?,
                )
                .await;
        }
        if !self
            .store
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .is_guild_premium(&guild_key, system_now_ms())
            .map_err(|_| GatewayEventDispatchError)?
        {
            return self
                .send_ephemeral(
                    context,
                    command,
                    self.message(
                        "stt.notPremium",
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                    )?,
                )
                .await;
        }
        let already_running = self
            .sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .contains_key(&guild_key);
        let starting = self
            .starting
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .contains(&guild_key);
        if already_running || starting {
            return self
                .send_ephemeral(
                    context,
                    command,
                    self.message(
                        "stt.alreadyRunning",
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                    )?,
                )
                .await;
        }
        let Some(voice_channel_id) = self.gateway_state.bot_voice_channel_id(&guild_key) else {
            return self
                .send_ephemeral(
                    context,
                    command,
                    self.message(
                        "stt.notInVoice",
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                    )?,
                )
                .await;
        };
        let Some(slot) = self.slots.clone().try_acquire_owned().ok() else {
            return self
                .send_ephemeral(
                    context,
                    command,
                    self.message(
                        "stt.atCapacity",
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                    )?,
                )
                .await;
        };
        if !self
            .starting
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .insert(guild_key.clone())
        {
            drop(slot);
            return self
                .send_ephemeral(
                    context,
                    command,
                    self.message(
                        "stt.alreadyRunning",
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                    )?,
                )
                .await;
        }
        // Reserve the interaction before touching Songbird, Whisper or the announcement. The
        // Discord deadline is three seconds; all responses in start_inner therefore edit this
        // deferred ephemeral response instead of attempting a second initial response.
        if command.defer_ephemeral(context).await.is_err() {
            // The reservation must not survive a failed initial response: otherwise a transient
            // Discord error would leave this guild permanently "starting" and leak one STT slot.
            if let Ok(mut starting) = self.starting.lock() {
                starting.remove(&guild_key);
            }
            drop(slot);
            return Err(GatewayEventDispatchError);
        }
        let result = self
            .start_inner(
                context,
                command,
                guild_id,
                guild_key.clone(),
                voice_channel_id,
                language,
                slot,
            )
            .await;
        self.starting
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .remove(&guild_key);
        if result.is_err() {
            // The normal failure paths below already edit the deferred response. This is a
            // final safety net for Songbird/SQLite/lock failures before the session is inserted;
            // Discord ignores it if the interaction was already edited successfully.
            let _ = self
                .edit_response(
                    context,
                    command,
                    self.message(
                        "stt.startFailed",
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                    )
                    .unwrap_or_else(|_| "Unable to start transcription.".to_owned()),
                )
                .await;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_inner(
        &self,
        context: &Context,
        command: &serenity::model::application::CommandInteraction,
        guild_id: GuildId,
        guild_key: String,
        voice_channel_id: String,
        language: Option<String>,
        slot: OwnedSemaphorePermit,
    ) -> Result<(), GatewayEventDispatchError> {
        // Resolve every localized string before mutating Songbird. A malformed/missing generated
        // contract must fail before the bot is undeafened, never leave a half-started receiver.
        let announcement_content = self.message(
            "stt.announceStart",
            Some(&command.locale),
            command.guild_locale.as_deref(),
        )?;
        let consent_label = self.message(
            "stt.consentBtn",
            Some(&command.locale),
            command.guild_locale.as_deref(),
        )?;
        let announcement_stop = self.message(
            "stt.announceStop",
            Some(&command.locale),
            command.guild_locale.as_deref(),
        )?;
        let started_content = self.message(
            "stt.started",
            Some(&command.locale),
            command.guild_locale.as_deref(),
        )?;
        let failed_content = self.message(
            "stt.startFailed",
            Some(&command.locale),
            command.guild_locale.as_deref(),
        )?;
        let manager = songbird::get(context)
            .await
            .ok_or(GatewayEventDispatchError)?;
        let call = manager.get(guild_id).ok_or(GatewayEventDispatchError)?;

        // Load durable consent before installing the receiver. The callback itself only reads
        // the process-local registry, so SQLite is never queried on Songbird's audio thread.
        for user_id in self
            .gateway_state
            .voice_member_ids(&guild_key, &voice_channel_id)
        {
            if self
                .store
                .lock()
                .ok()
                .and_then(|store| store.has_stt_consent(&user_id, &guild_key).ok())
                .unwrap_or(false)
                && let Ok(user_id) = user_id.parse::<u64>()
            {
                self.consent_registry.grant(&guild_key, user_id);
            }
        }
        let registry = self.consent_registry.clone();
        let guild_for_gate = guild_key.clone();
        let receiver = VoiceReceiver::new(
            FRAME_SAMPLES,
            Arc::new(move |user_id| registry.is_consented(&guild_for_gate, user_id)),
        );
        let (tx, rx) = mpsc::channel(MAX_QUEUED_UTTERANCES);
        let songbird_receiver = SongbirdVoiceReceiver::new(receiver, tx, FRAME_SAMPLES);
        {
            let mut handler = call.lock().await;
            if handler.deafen(false).await.is_err() {
                let _ = handler.deafen(true).await;
                return Err(GatewayEventDispatchError);
            }
            songbird_receiver.install_on_call(&mut handler);
        }
        let announcement = match command
            .channel_id
            .send_message(
                &context.http,
                CreateMessage::new()
                    .content(announcement_content)
                    .components(vec![CreateActionRow::Buttons(vec![
                        CreateButton::new("sttconsent")
                            .label(consent_label)
                            .style(serenity::all::ButtonStyle::Success),
                    ])]),
            )
            .await
        {
            Ok(message) => message,
            Err(_) => {
                songbird_receiver.stop();
                if let Some(call) = manager.get(guild_id) {
                    let mut handler = call.lock().await;
                    let _ = handler.deafen(true).await;
                }
                return self
                    .edit_response(context, command, failed_content.clone())
                    .await;
            }
        };
        let ever_consented = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let has_existing_consent = self
            .gateway_state
            .voice_member_ids(&guild_key, &voice_channel_id)
            .into_iter()
            .filter_map(|user_id| user_id.parse::<u64>().ok())
            .any(|user_id| self.consent_registry.is_consented(&guild_key, user_id));
        ever_consented.store(has_existing_consent, std::sync::atomic::Ordering::Release);
        self.spawn_consumer(command.channel_id, context.http.clone(), rx, language);
        self.sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .insert(
                guild_key,
                LiveSession {
                    receiver: songbird_receiver,
                    voice_channel_id,
                    announcement_channel_id: command.channel_id,
                    announcement_id: announcement.id,
                    announcement_stop,
                    ever_consented,
                    _slot: slot,
                },
            );
        self.edit_response(context, command, started_content).await
    }

    fn spawn_consumer(
        &self,
        channel_id: ChannelId,
        http: Arc<serenity::http::Http>,
        mut rx: Receiver<ReceivedUtterance>,
        language: Option<String>,
    ) {
        let transcriber = self.transcriber.clone();
        tokio::spawn(async move {
            while let Some(received) = rx.recv().await {
                let result = transcriber
                    .transcribe_pcm(
                        &received.utterance.pcm,
                        received
                            .utterance
                            .duration_ms
                            .min(MAX_SESSION_SECONDS * 1_000),
                        language.as_deref(),
                    )
                    .await;
                let Ok(transcript) = result else {
                    continue;
                };
                let text = clean_transcript(&transcript.text);
                if text.is_empty() {
                    continue;
                }
                let speaker = format!("<@{}>", received.user_id);
                let content = format!("**{speaker}:** {}", defuse_mentions(&text));
                let _ = channel_id
                    .send_message(
                        &http,
                        CreateMessage::new().content(content).allowed_mentions(
                            CreateAllowedMentions::new()
                                .all_users(false)
                                .all_roles(false)
                                .everyone(false),
                        ),
                    )
                    .await;
            }
        });
    }

    async fn stop_session(
        &self,
        context: &Context,
        guild_id: &str,
        announce: bool,
    ) -> Result<bool, GatewayEventDispatchError> {
        let Some(session) = self
            .sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .remove(guild_id)
        else {
            return Ok(false);
        };
        session.receiver.stop();
        if let Ok(parsed) = guild_id.parse::<u64>()
            && let Some(call) = songbird::get(context)
                .await
                .and_then(|manager| manager.get(GuildId::new(parsed)))
        {
            let mut handler = call.lock().await;
            let _ = handler.deafen(true).await;
        }
        if announce {
            let _ = session
                .announcement_channel_id
                .edit_message(
                    &context.http,
                    session.announcement_id,
                    EditMessage::new()
                        .content(session.announcement_stop)
                        .components(Vec::new()),
                )
                .await;
        }
        Ok(true)
    }

    async fn handle_component(
        &self,
        context: &Context,
        component: &serenity::model::application::ComponentInteraction,
    ) -> Result<(), GatewayEventDispatchError> {
        if component.data.custom_id != "sttconsent" {
            return Ok(());
        }
        let Some(guild_id) = component.guild_id else {
            return Ok(());
        };
        let guild_key = guild_id.get().to_string();
        let Some(session) = self
            .sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .get(&guild_key)
            .map(|session| session.ever_consented.clone())
        else {
            return Ok(());
        };
        self.store
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .grant_stt_consent(&component.user.id.get().to_string(), &guild_key, now_ms())
            .map_err(|_| GatewayEventDispatchError)?;
        self.consent_registry
            .grant(&guild_key, component.user.id.get());
        session.store(true, std::sync::atomic::Ordering::Release);
        component
            .create_response(
                context,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(self.message(
                            "stt.consentThanks",
                            Some(&component.locale),
                            component.guild_locale.as_deref(),
                        )?)
                        .ephemeral(true),
                ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for LiveTranscriptionGatewaySink {
    async fn on_message(
        &self,
        _context: Context,
        _message: Message,
    ) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    async fn on_interaction(
        &self,
        context: Context,
        interaction: Interaction,
    ) -> Result<(), GatewayEventDispatchError> {
        if let Interaction::Component(component) = &interaction {
            return self.handle_component(&context, component).await;
        }
        let Interaction::Command(command) = interaction else {
            return Ok(());
        };
        let Some(parsed) = parse_transcription_session_command(&command.data)
            .map_err(|_| GatewayEventDispatchError)?
        else {
            return Ok(());
        };
        match parsed {
            TranscriptionSessionCommand::Start { language } => {
                self.start(&context, &command, language).await
            }
            TranscriptionSessionCommand::Stop => {
                let Some(guild_id) = command.guild_id else {
                    return self
                        .send_ephemeral(
                            &context,
                            &command,
                            self.message(
                                "stt.guildOnly",
                                Some(&command.locale),
                                command.guild_locale.as_deref(),
                            )?,
                        )
                        .await;
                };
                if !Self::can_manage(&command) {
                    return self
                        .send_ephemeral(
                            &context,
                            &command,
                            self.message(
                                "stt.noManage",
                                Some(&command.locale),
                                command.guild_locale.as_deref(),
                            )?,
                        )
                        .await;
                }
                let stopped = self
                    .stop_session(&context, &guild_id.get().to_string(), true)
                    .await?;
                self.send_ephemeral(
                    &context,
                    &command,
                    self.message(
                        if stopped {
                            "stt.stopped"
                        } else {
                            "stt.notRunning"
                        },
                        Some(&command.locale),
                        command.guild_locale.as_deref(),
                    )?,
                )
                .await
            }
        }
    }

    async fn on_voice_state_update(
        &self,
        context: Context,
        _old: Option<VoiceState>,
        new: VoiceState,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some(guild_id) = new.guild_id else {
            return Ok(());
        };
        let guild_key = guild_id.get().to_string();
        let Some((voice_channel_id, ever_consented)) = self
            .sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .get(&guild_key)
            .map(|session| {
                (
                    session.voice_channel_id.clone(),
                    session
                        .ever_consented
                        .load(std::sync::atomic::Ordering::Acquire),
                )
            })
        else {
            return Ok(());
        };
        if new
            .channel_id
            .is_some_and(|channel_id| channel_id.get().to_string() == voice_channel_id)
        {
            let user_key = new.user_id.get().to_string();
            if self
                .store
                .lock()
                .ok()
                .and_then(|store| store.has_stt_consent(&user_key, &guild_key).ok())
                .unwrap_or(false)
            {
                self.consent_registry.grant(&guild_key, new.user_id.get());
            }
        }
        let humans = self
            .gateway_state
            .voice_member_ids(&guild_key, &voice_channel_id)
            .into_iter()
            .filter(|user_id| self.gateway_state.bot_user_id().as_deref() != Some(user_id))
            .collect::<Vec<_>>();
        let no_consent = ever_consented
            && humans.iter().all(|user_id| {
                user_id
                    .parse::<u64>()
                    .ok()
                    .is_none_or(|id| !self.consent_registry.is_consented(&guild_key, id))
            });
        let bot_user_id = self.gateway_state.bot_user_id();
        let new_user_id = new.user_id.get().to_string();
        let bot_left = bot_user_id.as_deref() == Some(new_user_id.as_str())
            && new.channel_id.map(|id| id.get().to_string()) != Some(voice_channel_id);
        if bot_left || humans.is_empty() || no_consent {
            let _ = self.stop_session(&context, &guild_key, true).await?;
        }
        Ok(())
    }

    async fn on_guild_delete(&self, guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        if let Some(session) = self
            .sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .remove(guild_id)
        {
            session.receiver.stop();
        }
        self.consent_registry.clear_guild(guild_id);
        Ok(())
    }
}

fn clean_transcript(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn defuse_mentions(text: &str) -> String {
    text.replace("@everyone", "@\u{200b}everyone")
        .replace("@here", "@\u{200b}here")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}
