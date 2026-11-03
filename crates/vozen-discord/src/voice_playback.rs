//! Songbird playback adapter.
//!
//! Kept behind `voice-driver` so storage/API contract checks run on developer machines without a
//! local Opus toolchain. The production image must compile this feature before it can replace the
//! Node voice path.

use std::path::Path;
use std::sync::{Arc, atomic::AtomicU64};
#[cfg(feature = "voice-driver")]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(any(feature = "voice-driver", test))]
use std::collections::{BTreeMap, BTreeSet};

#[cfg(feature = "voice-driver")]
use std::sync::Mutex;

use async_trait::async_trait;
use thiserror::Error;

use crate::{CommandPlaybackError, CommandPlaybackState, CommandVoicePlayback, VoiceDestination};

#[cfg(feature = "voice-driver")]
use crate::QueueControlPlayback;

#[cfg(feature = "voice-driver")]
use vozen_core::QueueEnqueueOptions;
use vozen_core::RuntimeMetrics;
#[cfg(any(feature = "voice-driver", test))]
use vozen_core::{PublicQueueItem, QueueLane, QueueSource};

#[cfg(any(feature = "voice-driver", test))]
use uuid::Uuid;

#[cfg(feature = "voice-driver")]
use songbird::events::{Event, EventContext, EventData, EventHandler, TrackEvent};

#[cfg(feature = "voice-driver")]
fn songbird_driver(call: &songbird::Call) -> &songbird::driver::Driver {
    <songbird::Call as std::ops::Deref>::deref(call)
}

#[cfg(feature = "voice-driver")]
fn songbird_driver_mut(call: &mut songbird::Call) -> &mut songbird::driver::Driver {
    <songbird::Call as std::ops::DerefMut>::deref_mut(call)
}

/// Private track bookkeeping. It deliberately excludes synthesized text, voice model and WAV
/// path; only the opaque values required for the existing `/queue` controls are retained.
#[cfg(any(feature = "voice-driver", test))]
#[derive(Clone)]
struct QueueTrackMetadata {
    author_id: Option<String>,
    source: QueueSource,
    lane: QueueLane,
    created_at_ms: u64,
}

#[cfg(feature = "voice-driver")]
struct SpokenTrackCounter(Arc<AtomicU64>);

#[cfg(feature = "voice-driver")]
#[async_trait]
impl EventHandler for SpokenTrackCounter {
    async fn act(&self, _context: &EventContext<'_>) -> Option<Event> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(Event::Cancel)
    }
}

#[cfg(feature = "voice-driver")]
struct PlaybackLifecycleLogger {
    stage: &'static str,
    metrics: Arc<RuntimeMetrics>,
    created_at_ms: u64,
    telemetry: Option<(
        crate::speech_telemetry::SpeechTelemetryWriter,
        vozen_store::RuntimeBatchBuffer,
    )>,
    started: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(feature = "voice-driver")]
#[async_trait]
impl EventHandler for PlaybackLifecycleLogger {
    async fn act(&self, context: &EventContext<'_>) -> Option<Event> {
        let observed = if let EventContext::Track(tracks) = context {
            tracks.first()
        } else {
            None
        };
        if let Some((state, _)) = observed {
            use vozen_store::OperationalMetric as Metric;
            let (metric, progress) =
                playback_observation(self.stage, &state.playing, state.position);
            if progress
                && !self
                    .started
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                self.record(Metric::PlaybackStarted);
                if self.created_at_ms > 0 {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
                        .unwrap_or(self.created_at_ms);
                    let ttfa_ms = now_ms.saturating_sub(self.created_at_ms);
                    self.metrics.record_ttfa_ms(ttfa_ms);
                    self.record_value(Metric::TtfaMs, ttfa_ms.min(i64::MAX as u64) as i64);
                }
            }
            if let Some(metric) = metric {
                self.record(metric);
            }
            if self.stage == "progress" {
                return (progress || state.playing.is_done()).then_some(Event::Cancel);
            }
        }
        if let EventContext::Track(tracks) = context
            && let Some((state, _)) = tracks.first()
        {
            eprintln!(
                "[voice:playback] track {}: playing={:?} ready={:?} position_ms={}",
                self.stage,
                state.playing,
                state.ready,
                state.position.as_millis()
            );
        } else {
            eprintln!("[voice:playback] track {}", self.stage);
        }
        Some(Event::Cancel)
    }
}

#[cfg(feature = "voice-driver")]
impl PlaybackLifecycleLogger {
    fn record(&self, metric: vozen_store::OperationalMetric) {
        self.record_value(metric, 1);
    }

    fn record_value(&self, metric: vozen_store::OperationalMetric, value: i64) {
        if let Some((writer, batch)) = &self.telemetry {
            let day = vozen_store::utc_day_key();
            writer.record_metric(
                &day,
                metric,
                vozen_store::OperationalProvider::Internal,
                value,
            );
            batch.record_metric(
                &day,
                metric,
                vozen_store::OperationalProvider::Internal,
                value,
            );
        }
    }
}

#[cfg(feature = "voice-driver")]
fn playback_observation(
    stage: &str,
    mode: &songbird::tracks::PlayMode,
    position: std::time::Duration,
) -> (Option<vozen_store::OperationalMetric>, bool) {
    use songbird::tracks::PlayMode;
    use vozen_store::OperationalMetric as Metric;
    let progressed = !position.is_zero() && matches!(mode, PlayMode::Play | PlayMode::End);
    let metric = match stage {
        "playable" => Some(Metric::PlaybackReady),
        "error" => Some(Metric::PlaybackFailed),
        "ended" if matches!(mode, PlayMode::End) => Some(Metric::PlaybackCompleted),
        "ended" if matches!(mode, PlayMode::Stop) => Some(Metric::PlaybackStopped),
        _ => None,
    };
    (metric, progressed)
}

/// Bounded metadata mirror of Songbird's queue. Songbird is the playback authority; this only
/// allows the public queue command to find opaque IDs and their authors without reading request
/// text. Every read receives the live Songbird order and prunes tracks which have ended.
#[cfg(any(feature = "voice-driver", test))]
#[derive(Default)]
struct QueueTrackLedger {
    by_guild: BTreeMap<String, BTreeMap<Uuid, QueueTrackMetadata>>,
    paused_guilds: BTreeSet<String>,
}

#[cfg(any(feature = "voice-driver", test))]
impl QueueTrackLedger {
    fn remember(&mut self, guild_id: &str, id: Uuid, metadata: QueueTrackMetadata) {
        self.by_guild
            .entry(guild_id.to_owned())
            .or_default()
            .insert(id, metadata);
    }

    fn snapshot(
        &mut self,
        guild_id: &str,
        active_order: &[Uuid],
        now_ms: u64,
    ) -> Vec<PublicQueueItem> {
        self.active_metadata(guild_id, active_order)
            .into_iter()
            // The first Songbird entry is currently audible. Node's `/queue show` renders only
            // pending work, so it must never appear in this response.
            .skip(1)
            .map(|(id, metadata)| PublicQueueItem {
                id: id.to_string(),
                source: metadata.source,
                lane: metadata.lane,
                age_ms: now_ms.saturating_sub(metadata.created_at_ms),
            })
            .collect()
    }

    /// Returns the pending Songbird UUID eligible for removal. The current item is deliberately
    /// excluded, and a non-manager can only select an item authored by that same Discord user.
    fn removable_track(
        &mut self,
        guild_id: &str,
        active_order: &[Uuid],
        public_id: &str,
        author_id: Option<&str>,
    ) -> Option<Uuid> {
        self.active_metadata(guild_id, active_order)
            .into_iter()
            .skip(1)
            .find(|(id, metadata)| {
                id.to_string() == public_id
                    && author_id.is_none_or(|author| metadata.author_id.as_deref() == Some(author))
            })
            .map(|(track_id, _)| track_id)
    }

    fn remove(&mut self, guild_id: &str, id: Uuid) {
        let Some(by_track) = self.by_guild.get_mut(guild_id) else {
            return;
        };
        by_track.remove(&id);
        if by_track.is_empty() {
            self.by_guild.remove(guild_id);
        }
    }

    fn clear(&mut self, guild_id: &str) {
        self.by_guild.remove(guild_id);
        self.paused_guilds.remove(guild_id);
    }

    /// Returns `true` only for the transition from playing to paused. It makes concurrent slash
    /// commands deterministic without relying on a stale `TrackState` read.
    fn pause(&mut self, guild_id: &str) -> bool {
        self.paused_guilds.insert(guild_id.to_owned())
    }

    /// Returns `true` only if this Rust-owned player was paused before the call.
    fn resume(&mut self, guild_id: &str) -> bool {
        self.paused_guilds.remove(guild_id)
    }

    fn active_metadata(
        &mut self,
        guild_id: &str,
        active_order: &[Uuid],
    ) -> Vec<(Uuid, QueueTrackMetadata)> {
        let active = active_order.iter().copied().collect::<BTreeSet<_>>();
        let empty = match self.by_guild.get_mut(guild_id) {
            Some(by_track) => {
                by_track.retain(|id, _| active.contains(id));
                by_track.is_empty()
            }
            None => return Vec::new(),
        };
        if empty {
            self.by_guild.remove(guild_id);
            return Vec::new();
        }
        let by_track = self
            .by_guild
            .get(guild_id)
            .expect("non-empty queue metadata was retained");
        active_order
            .iter()
            .filter_map(|track_id| {
                by_track
                    .get(track_id)
                    .cloned()
                    .map(|metadata| (*track_id, metadata))
            })
            .collect()
    }
}

#[derive(Debug, Error)]
pub enum VoicePlaybackError {
    #[error("Rust voice driver is not compiled into this runtime")]
    DriverUnavailable,
    #[error("Songbird voice manager is unavailable")]
    ManagerUnavailable,
    #[error("failed to join or switch Discord voice channel")]
    JoinFailed,
}

/// Tracks capacity promised to synthesis jobs which have not produced a WAV yet. Songbird can
/// only report tracks it has already received, so these reservations prevent concurrent `/tts`
/// requests from all deciding that the last queue slot is free.
#[cfg(any(feature = "voice-driver", test))]
#[derive(Debug, Default)]
struct QueueReservations {
    by_guild: BTreeMap<String, usize>,
}

#[cfg(any(feature = "voice-driver", test))]
impl QueueReservations {
    fn reserve(&mut self, guild_id: &str, queued: usize, maximum: usize) -> bool {
        let reserved = self.by_guild.get(guild_id).copied().unwrap_or_default();
        if queued.saturating_add(reserved) >= maximum {
            return false;
        }

        self.by_guild.insert(guild_id.to_owned(), reserved + 1);
        true
    }

    /// Returns `true` only when this invocation really owned a reservation. That makes cleanup
    /// idempotent: the core service can safely cancel after a partial playback failure.
    fn release(&mut self, guild_id: &str) -> bool {
        let Some(reserved) = self.by_guild.get_mut(guild_id) else {
            return false;
        };

        if *reserved == 1 {
            self.by_guild.remove(guild_id);
        } else {
            *reserved -= 1;
        }
        true
    }

    fn clear(&mut self, guild_id: &str) {
        self.by_guild.remove(guild_id);
    }
}

/// Production adapter for the command service's bounded guild FIFO.
///
/// It only sees an existing Songbird call: `/join` remains responsible for creating it and the
/// command service remains responsible for authorization. Keeping those two concerns separate
/// avoids a text command silently connecting the bot to a channel.
#[derive(Clone)]
pub struct SongbirdCommandPlayback {
    #[cfg(feature = "voice-driver")]
    telemetry: Option<(
        crate::speech_telemetry::SpeechTelemetryWriter,
        vozen_store::RuntimeBatchBuffer,
    )>,
    #[cfg(feature = "voice-driver")]
    context: serenity::client::Context,
    #[cfg(feature = "voice-driver")]
    maximum_queue_items: usize,
    #[cfg(feature = "voice-driver")]
    reservations: Arc<Mutex<QueueReservations>>,
    #[cfg(feature = "voice-driver")]
    queue_metadata: Arc<Mutex<QueueTrackLedger>>,
    #[cfg(feature = "voice-driver")]
    messages_spoken: Arc<AtomicU64>,
    #[cfg(feature = "voice-driver")]
    metrics: Arc<RuntimeMetrics>,
}

impl SongbirdCommandPlayback {
    #[must_use]
    pub fn new(
        context: serenity::client::Context,
        maximum_queue_items: usize,
        messages_spoken: Arc<AtomicU64>,
    ) -> Self {
        Self::new_with_metrics(
            context,
            maximum_queue_items,
            messages_spoken,
            Arc::new(RuntimeMetrics::default()),
        )
    }

    pub fn new_with_metrics(
        context: serenity::client::Context,
        maximum_queue_items: usize,
        messages_spoken: Arc<AtomicU64>,
        metrics: Arc<RuntimeMetrics>,
    ) -> Self {
        #[cfg(not(feature = "voice-driver"))]
        let _ = (context, maximum_queue_items, messages_spoken, metrics);

        Self {
            #[cfg(feature = "voice-driver")]
            telemetry: None,
            #[cfg(feature = "voice-driver")]
            context,
            #[cfg(feature = "voice-driver")]
            // A zero-sized queue would make every request look like a configuration success and
            // then fail forever. Clamp it defensively because this is startup configuration.
            maximum_queue_items: maximum_queue_items.max(1),
            #[cfg(feature = "voice-driver")]
            reservations: Arc::new(Mutex::new(QueueReservations::default())),
            #[cfg(feature = "voice-driver")]
            queue_metadata: Arc::new(Mutex::new(QueueTrackLedger::default())),
            #[cfg(feature = "voice-driver")]
            messages_spoken,
            #[cfg(feature = "voice-driver")]
            metrics,
        }
    }

    #[cfg(feature = "voice-driver")]
    pub fn with_telemetry(
        mut self,
        store: Arc<std::sync::Mutex<vozen_store::SqliteStore>>,
        batch: vozen_store::RuntimeBatchBuffer,
    ) -> Self {
        self.telemetry = Some((
            crate::speech_telemetry::SpeechTelemetryWriter::new(store),
            batch,
        ));
        self
    }

    #[cfg(feature = "voice-driver")]
    fn guild_id(guild_id: &str) -> Result<serenity::model::id::GuildId, CommandPlaybackError> {
        guild_id
            .parse::<u64>()
            .map(serenity::model::id::GuildId::new)
            .map_err(|_| CommandPlaybackError)
    }

    #[cfg(feature = "voice-driver")]
    fn release_reservation(&self, guild_id: &str) -> bool {
        self.reservations
            .lock()
            .expect("voice queue reservation lock poisoned")
            .release(guild_id)
    }

    #[cfg(feature = "voice-driver")]
    fn remember_track(
        &self,
        guild_id: &str,
        id: Uuid,
        metadata: QueueTrackMetadata,
        queue_was_empty: bool,
    ) -> Result<(), CommandPlaybackError> {
        let mut ledger = self
            .queue_metadata
            .lock()
            .map_err(|_| CommandPlaybackError)?;
        if queue_was_empty {
            ledger.clear(guild_id);
        }
        ledger.remember(guild_id, id, metadata);
        Ok(())
    }
}

#[cfg(feature = "voice-driver")]
#[async_trait]
impl CommandVoicePlayback for SongbirdCommandPlayback {
    async fn state(&self, guild_id: &str) -> Result<CommandPlaybackState, CommandPlaybackError> {
        let guild_id = Self::guild_id(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(CommandPlaybackError)?;
        let call = manager.get(guild_id).ok_or(CommandPlaybackError)?;
        let handler = call.lock().await;

        Ok(if songbird_driver(&handler).queue().current().is_some() {
            CommandPlaybackState::Active
        } else {
            CommandPlaybackState::Idle
        })
    }

    async fn reserve(
        &self,
        guild_id: &str,
        _lane: vozen_core::QueueLane,
    ) -> Result<bool, CommandPlaybackError> {
        let discord_guild_id = Self::guild_id(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(CommandPlaybackError)?;
        let call = manager.get(discord_guild_id).ok_or(CommandPlaybackError)?;
        let handler = call.lock().await;
        let queued = songbird_driver(&handler).queue().len();

        Ok(self
            .reservations
            .lock()
            .expect("voice queue reservation lock poisoned")
            .reserve(guild_id, queued, self.maximum_queue_items))
    }

    async fn enqueue_reserved(
        &self,
        guild_id: &str,
        destination: &VoiceDestination,
        wav: &Path,
        options: QueueEnqueueOptions<'_>,
    ) -> Result<(), CommandPlaybackError> {
        // Consume before acquiring Songbird. If the call disappeared during synthesis the core
        // service will invoke the idempotent cancellation path below; no reservation can leak.
        if !self.release_reservation(guild_id) {
            return Err(CommandPlaybackError);
        }

        let discord_guild_id = Self::guild_id(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(CommandPlaybackError)?;
        let call = manager.get(discord_guild_id).ok_or(CommandPlaybackError)?;
        let mut handler = call.lock().await;
        if !destination.is_current()
            || handler.current_channel().map(|channel| channel.0.get())
                != Some(
                    destination
                        .channel_id()
                        .parse::<u64>()
                        .map_err(|_| CommandPlaybackError)?,
                )
            || handler.current_connection().is_none()
            || songbird_driver(&handler).is_mute()
        {
            return Err(CommandPlaybackError);
        }
        handler.set_stop_queue_on_voice_change(true);
        let id = Uuid::new_v4();
        self.remember_track(
            guild_id,
            id,
            QueueTrackMetadata {
                author_id: options.author_id.map(str::to_owned),
                source: options.source,
                lane: options.lane,
                created_at_ms: options.created_at_ms,
            },
            songbird_driver(&handler).queue().is_empty(),
        )?;
        let wav = wav.to_path_buf();
        let mut track =
            songbird::tracks::Track::new_with_uuid(songbird::input::File::new(wav).into(), id);
        track.events.add_event(
            EventData::new(
                Event::Track(TrackEvent::Playable),
                SpokenTrackCounter(self.messages_spoken.clone()),
            ),
            std::time::Duration::ZERO,
        );
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        for (event, stage) in [
            (Event::Track(TrackEvent::Playable), "playable"),
            (Event::Track(TrackEvent::Error), "error"),
            (Event::Track(TrackEvent::End), "ended"),
            (
                Event::Periodic(std::time::Duration::from_millis(100), None),
                "progress",
            ),
        ] {
            track.events.add_event(
                EventData::new(
                    event,
                    PlaybackLifecycleLogger {
                        stage,
                        metrics: self.metrics.clone(),
                        created_at_ms: options.created_at_ms,
                        telemetry: self.telemetry.clone(),
                        started: started.clone(),
                    },
                ),
                std::time::Duration::ZERO,
            );
        }
        let handle = songbird_driver_mut(&mut handler).enqueue(track).await;
        drop(handler);

        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle.make_playable_async(),
        )
        .await
        {
            Ok(Ok(())) => {
                eprintln!("[voice:playback] queued track passed decoder readiness");
            }
            Ok(Err(error)) => {
                eprintln!("[voice:playback] queued track decoder failed: {error:?}");
                let _ = handle.stop();
                self.queue_metadata
                    .lock()
                    .map_err(|_| CommandPlaybackError)?
                    .remove(guild_id, id);
                return Err(CommandPlaybackError);
            }
            Err(_) => {
                eprintln!("[voice:playback] queued track decoder readiness timed out");
                let _ = handle.stop();
                self.queue_metadata
                    .lock()
                    .map_err(|_| CommandPlaybackError)?
                    .remove(guild_id, id);
                return Err(CommandPlaybackError);
            }
        }
        Ok(())
    }

    async fn cancel_reservation(
        &self,
        guild_id: &str,
        _lane: vozen_core::QueueLane,
    ) -> Result<(), CommandPlaybackError> {
        self.release_reservation(guild_id);
        Ok(())
    }

    async fn skip(&self, guild_id: &str) -> Result<(), CommandPlaybackError> {
        let guild_id = Self::guild_id(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(CommandPlaybackError)?;
        let call = manager.get(guild_id).ok_or(CommandPlaybackError)?;
        let handler = call.lock().await;
        songbird_driver(&handler)
            .queue()
            .skip()
            .map_err(|_| CommandPlaybackError)
    }

    async fn silence(&self, guild_id: &str) -> Result<(), CommandPlaybackError> {
        let discord_guild_id = Self::guild_id(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(CommandPlaybackError)?;
        let call = manager.get(discord_guild_id).ok_or(CommandPlaybackError)?;
        let handler = call.lock().await;
        songbird_driver(&handler).queue().stop();
        self.reservations
            .lock()
            .expect("voice queue reservation lock poisoned")
            .clear(guild_id);
        self.queue_metadata
            .lock()
            .map_err(|_| CommandPlaybackError)?
            .clear(guild_id);
        Ok(())
    }
}

/// Adapter for `/queue`. It interrogates Songbird on every operation, so a stale ledger can
/// never authorize a removal or fabricate a queue entry. The ledger merely maps a live track UUID
/// back to the opaque public identifier and author scope.
#[cfg(feature = "voice-driver")]
#[async_trait]
impl QueueControlPlayback for SongbirdCommandPlayback {
    async fn has_queue_player(&self, guild_id: &str) -> Result<bool, CommandPlaybackError> {
        let guild_id = Self::guild_id(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(CommandPlaybackError)?;
        Ok(manager.get(guild_id).is_some())
    }

    async fn queue_snapshot(
        &self,
        guild_id: &str,
        now_ms: u64,
    ) -> Result<Vec<PublicQueueItem>, CommandPlaybackError> {
        let discord_guild_id = Self::guild_id(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(CommandPlaybackError)?;
        let call = manager.get(discord_guild_id).ok_or(CommandPlaybackError)?;
        let handler = call.lock().await;
        let active = songbird_driver(&handler)
            .queue()
            .current_queue()
            .into_iter()
            .map(|track| track.uuid())
            .collect::<Vec<_>>();
        let snapshot = self
            .queue_metadata
            .lock()
            .map_err(|_| CommandPlaybackError)?
            .snapshot(guild_id, &active, now_ms);
        Ok(snapshot)
    }

    async fn remove_queue_item(
        &self,
        guild_id: &str,
        id: &str,
        author_id: Option<&str>,
    ) -> Result<bool, CommandPlaybackError> {
        let discord_guild_id = Self::guild_id(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(CommandPlaybackError)?;
        let call = manager.get(discord_guild_id).ok_or(CommandPlaybackError)?;
        let handler = call.lock().await;
        let active = songbird_driver(&handler).queue().current_queue();
        let active_ids = active
            .iter()
            .map(songbird::tracks::TrackHandle::uuid)
            .collect::<Vec<_>>();
        let target = self
            .queue_metadata
            .lock()
            .map_err(|_| CommandPlaybackError)?
            .removable_track(guild_id, &active_ids, id, author_id);
        let Some(target) = target else {
            return Ok(false);
        };
        let Some(index) = active_ids.iter().position(|candidate| *candidate == target) else {
            return Ok(false);
        };
        // `removable_track` excludes index zero, so this can never interrupt current speech.
        let Some(removed) = songbird_driver(&handler).queue().dequeue(index) else {
            return Ok(false);
        };
        drop(removed.stop());
        self.queue_metadata
            .lock()
            .map_err(|_| CommandPlaybackError)?
            .remove(guild_id, target);
        Ok(true)
    }

    async fn clear_queue(&self, guild_id: &str) -> Result<(), CommandPlaybackError> {
        <Self as CommandVoicePlayback>::silence(self, guild_id).await
    }

    async fn pause_queue(&self, guild_id: &str) -> Result<bool, CommandPlaybackError> {
        let discord_guild_id = Self::guild_id(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(CommandPlaybackError)?;
        let call = manager.get(discord_guild_id).ok_or(CommandPlaybackError)?;
        let handler = call.lock().await;
        let current = songbird_driver(&handler).queue().current();
        let Some(current) = current else {
            return Ok(false);
        };
        let transitioned = self
            .queue_metadata
            .lock()
            .map_err(|_| CommandPlaybackError)?
            .pause(guild_id);
        if !transitioned {
            return Ok(false);
        }
        if current.pause().is_ok() {
            Ok(true)
        } else {
            self.queue_metadata
                .lock()
                .map_err(|_| CommandPlaybackError)?
                .resume(guild_id);
            Err(CommandPlaybackError)
        }
    }

    async fn resume_queue(&self, guild_id: &str) -> Result<bool, CommandPlaybackError> {
        let discord_guild_id = Self::guild_id(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(CommandPlaybackError)?;
        let call = manager.get(discord_guild_id).ok_or(CommandPlaybackError)?;
        let handler = call.lock().await;
        let current = songbird_driver(&handler).queue().current();
        let Some(current) = current else {
            return Ok(false);
        };
        let transitioned = self
            .queue_metadata
            .lock()
            .map_err(|_| CommandPlaybackError)?
            .resume(guild_id);
        if !transitioned {
            return Ok(false);
        }
        if current.play().is_ok() {
            Ok(true)
        } else {
            self.queue_metadata
                .lock()
                .map_err(|_| CommandPlaybackError)?
                .pause(guild_id);
            Err(CommandPlaybackError)
        }
    }

    async fn state(&self, guild_id: &str) -> Result<CommandPlaybackState, CommandPlaybackError> {
        <Self as CommandVoicePlayback>::state(self, guild_id).await
    }

    async fn skip_queue(&self, guild_id: &str) -> Result<(), CommandPlaybackError> {
        <Self as CommandVoicePlayback>::skip(self, guild_id).await
    }
}

#[cfg(not(feature = "voice-driver"))]
#[async_trait]
impl CommandVoicePlayback for SongbirdCommandPlayback {
    async fn state(&self, _guild_id: &str) -> Result<CommandPlaybackState, CommandPlaybackError> {
        Err(CommandPlaybackError)
    }

    async fn reserve(
        &self,
        _guild_id: &str,
        _lane: vozen_core::QueueLane,
    ) -> Result<bool, CommandPlaybackError> {
        Err(CommandPlaybackError)
    }

    async fn enqueue_reserved(
        &self,
        _guild_id: &str,
        _destination: &VoiceDestination,
        _wav: &Path,
        _options: vozen_core::QueueEnqueueOptions<'_>,
    ) -> Result<(), CommandPlaybackError> {
        Err(CommandPlaybackError)
    }

    async fn cancel_reservation(
        &self,
        _guild_id: &str,
        _lane: vozen_core::QueueLane,
    ) -> Result<(), CommandPlaybackError> {
        Err(CommandPlaybackError)
    }

    async fn skip(&self, _guild_id: &str) -> Result<(), CommandPlaybackError> {
        Err(CommandPlaybackError)
    }

    async fn silence(&self, _guild_id: &str) -> Result<(), CommandPlaybackError> {
        Err(CommandPlaybackError)
    }
}

/// Joins (or switches to) the requested call and queues a local WAV after already accepted work.
/// Songbird's native queue starts the first item immediately and advances only when the prior
/// track ends. Switching channels clears speech from the previous call.
/// Callers must already have verified same-call/premium/rejoin policy and Connect+Speak access;
/// this adapter never turns a stored channel id into authorization.
#[cfg(feature = "voice-driver")]
pub async fn join_and_enqueue_wav(
    context: &serenity::client::Context,
    guild_id: serenity::model::id::GuildId,
    channel_id: serenity::model::id::ChannelId,
    wav: impl AsRef<Path> + Send + Sync + 'static,
) -> Result<(), VoicePlaybackError> {
    let manager = songbird::get(context)
        .await
        .ok_or(VoicePlaybackError::ManagerUnavailable)?;
    let call = manager
        .join(guild_id, channel_id)
        .await
        .map_err(|_| VoicePlaybackError::JoinFailed)?;
    let mut handler = call.lock().await;
    if handler.current_channel() != Some(channel_id.into())
        || handler.current_connection().is_none()
        || songbird_driver(&handler).is_mute()
    {
        return Err(VoicePlaybackError::JoinFailed);
    }
    handler.set_stop_queue_on_voice_change(true);
    let handle = songbird_driver_mut(&mut handler)
        .enqueue_input(songbird::input::File::new(wav).into())
        .await;
    drop(handler);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle.make_playable_async(),
    )
    .await
    .map_err(|_| VoicePlaybackError::JoinFailed)?
    .map_err(|_| VoicePlaybackError::JoinFailed)
}

/// Removes the driver call and its tasks after an explicit `/leave`, an alone timeout or a real
/// guild departure. Planned restarts deliberately do not call this path.
#[cfg(feature = "voice-driver")]
pub async fn leave_voice(
    context: &serenity::client::Context,
    guild_id: serenity::model::id::GuildId,
) -> Result<(), VoicePlaybackError> {
    let manager = songbird::get(context)
        .await
        .ok_or(VoicePlaybackError::ManagerUnavailable)?;
    manager
        .remove(guild_id)
        .await
        .map_err(|_| VoicePlaybackError::JoinFailed)
}

#[cfg(not(feature = "voice-driver"))]
pub async fn join_and_enqueue_wav(
    _context: &serenity::client::Context,
    _guild_id: serenity::model::id::GuildId,
    _channel_id: serenity::model::id::ChannelId,
    _wav: impl AsRef<Path> + Send + Sync + 'static,
) -> Result<(), VoicePlaybackError> {
    Err(VoicePlaybackError::DriverUnavailable)
}

#[cfg(not(feature = "voice-driver"))]
pub async fn leave_voice(
    _context: &serenity::client::Context,
    _guild_id: serenity::model::id::GuildId,
) -> Result<(), VoicePlaybackError> {
    Err(VoicePlaybackError::DriverUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "voice-driver")]
    #[test]
    fn ready_audio_is_not_progress_and_stopped_audio_is_not_completion() {
        use songbird::tracks::PlayMode;
        use vozen_store::OperationalMetric as Metric;
        let zero = std::time::Duration::ZERO;
        let heard = std::time::Duration::from_millis(20);
        assert_eq!(
            playback_observation("playable", &PlayMode::Pause, zero),
            (Some(Metric::PlaybackReady), false)
        );
        assert_eq!(
            playback_observation("progress", &PlayMode::Play, heard),
            (None, true)
        );
        assert_eq!(
            playback_observation("ended", &PlayMode::Stop, heard),
            (Some(Metric::PlaybackStopped), false)
        );
        assert_eq!(
            playback_observation("ended", &PlayMode::End, heard),
            (Some(Metric::PlaybackCompleted), true)
        );
    }

    #[test]
    fn portable_builds_never_claim_to_have_a_voice_driver() {
        #[cfg(not(feature = "voice-driver"))]
        assert!(matches!(
            VoicePlaybackError::DriverUnavailable,
            VoicePlaybackError::DriverUnavailable
        ));
    }

    #[test]
    fn reservations_bound_concurrent_synthesis_to_the_songbird_queue_capacity() {
        let mut reservations = QueueReservations::default();

        assert!(reservations.reserve("42", 1, 3));
        assert!(reservations.reserve("42", 1, 3));
        assert!(!reservations.reserve("42", 1, 3));
        assert!(reservations.release("42"));
        assert!(reservations.reserve("42", 1, 3));
    }

    #[test]
    fn reservation_cleanup_is_idempotent_and_isolated_per_guild() {
        let mut reservations = QueueReservations::default();

        assert!(reservations.reserve("42", 0, 1));
        assert!(reservations.reserve("84", 0, 1));
        reservations.clear("42");
        assert!(!reservations.release("42"));
        assert!(reservations.release("84"));
        assert!(!reservations.release("84"));
    }

    #[test]
    fn queue_ledger_never_exposes_current_track_or_allows_it_to_be_removed() {
        let current = Uuid::new_v4();
        let pending = Uuid::new_v4();
        let mut ledger = QueueTrackLedger::default();
        ledger.remember(
            "guild",
            current,
            QueueTrackMetadata {
                author_id: Some("author".into()),
                source: QueueSource::Command,
                lane: QueueLane::Standard,
                created_at_ms: 10,
            },
        );
        ledger.remember(
            "guild",
            pending,
            QueueTrackMetadata {
                author_id: Some("author".into()),
                source: QueueSource::Message,
                lane: QueueLane::Accessibility,
                created_at_ms: 20,
            },
        );

        let order = [current, pending];
        let public = ledger.snapshot("guild", &order, 30);
        assert_eq!(public.len(), 1);
        assert_eq!(public[0].id, pending.to_string());
        assert_eq!(public[0].age_ms, 10);
        assert_eq!(
            ledger.removable_track("guild", &order, &current.to_string(), None),
            None
        );
        assert_eq!(
            ledger.removable_track("guild", &order, &pending.to_string(), Some("other")),
            None
        );
        assert_eq!(
            ledger.removable_track("guild", &order, &pending.to_string(), Some("author")),
            Some(pending)
        );

        ledger.remove("guild", pending);
        assert!(ledger.snapshot("guild", &order, 30).is_empty());
        ledger.clear("guild");
    }

    #[test]
    fn queue_ledger_prunes_ended_tracks_before_rendering() {
        let ended = Uuid::new_v4();
        let mut ledger = QueueTrackLedger::default();
        ledger.remember(
            "guild",
            ended,
            QueueTrackMetadata {
                author_id: None,
                source: QueueSource::System,
                lane: QueueLane::Standard,
                created_at_ms: 0,
            },
        );
        assert!(ledger.snapshot("guild", &[], 0).is_empty());
        assert!(!ledger.by_guild.contains_key("guild"));
    }

    #[test]
    fn queue_ledger_pause_state_has_idempotent_transitions_and_is_cleared_with_audio() {
        let mut ledger = QueueTrackLedger::default();
        assert!(ledger.pause("guild"));
        assert!(!ledger.pause("guild"));
        assert!(ledger.resume("guild"));
        assert!(!ledger.resume("guild"));
        assert!(ledger.pause("guild"));
        ledger.clear("guild");
        assert!(!ledger.resume("guild"));
    }
}
