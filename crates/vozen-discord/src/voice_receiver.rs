//! Consent-gated Songbird receive adapter for live transcription.
//!
//! The router is deliberately independent from Songbird's event loop. It owns only the
//! short-lived PCM buffers needed to segment one speaker at a time; a caller decides what to do
//! with the resulting utterance (for example, convert it to WAV and send it to Whisper). Audio
//! is never retained for a user who has not consented, and revoking consent drops any pending
//! buffer on the next received frame.

use std::{collections::BTreeMap, sync::Arc};

use crate::{Utterance, UtteranceCollector};

/// An utterance emitted by a consented speaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedUtterance {
    pub user_id: u64,
    pub utterance: Utterance,
}

/// Small, deterministic receive state machine. It maps Songbird SSRCs to Discord users and
/// segments decoded 48 kHz stereo PCM into bounded turns.
pub struct VoiceReceiver {
    frame_samples: usize,
    consented: Arc<dyn Fn(u64) -> bool + Send + Sync>,
    ssrc_users: BTreeMap<u32, u64>,
    collectors: BTreeMap<u64, UtteranceCollector>,
}

impl VoiceReceiver {
    /// `frame_samples` is normally 1,920 (20 ms at 48 kHz stereo).
    pub fn new(frame_samples: usize, consented: Arc<dyn Fn(u64) -> bool + Send + Sync>) -> Self {
        Self {
            frame_samples: frame_samples.max(1),
            consented,
            ssrc_users: BTreeMap::new(),
            collectors: BTreeMap::new(),
        }
    }

    /// Associates an RTP SSRC with a Discord user. Discord may send this update more than once
    /// when a client changes speaking capabilities, so replacing the mapping is intentional.
    pub fn map_ssrc(&mut self, ssrc: u32, user_id: u64) {
        self.ssrc_users.insert(ssrc, user_id);
    }

    /// Handles one decoded frame. The frame is dropped immediately when the speaker is unknown
    /// or no longer consented.
    pub fn push_pcm(&mut self, ssrc: u32, pcm: Vec<i16>) -> Option<ReceivedUtterance> {
        let user_id = self.ssrc_users.get(&ssrc).copied()?;
        if !(self.consented)(user_id) {
            self.collectors.remove(&user_id);
            return None;
        }
        let collector = self.collectors.entry(user_id).or_default();
        collector
            .push(pcm)
            .map(|utterance| ReceivedUtterance { user_id, utterance })
    }

    /// Handles a missing/silent 20 ms tick so an utterance can close after the configured gap.
    pub fn push_silence(&mut self, ssrc: u32) -> Option<ReceivedUtterance> {
        self.push_pcm(ssrc, vec![0; self.frame_samples])
    }

    /// Flushes one speaker, for example when Songbird reports a disconnect or when a session is
    /// stopped. Pending short noise is discarded by `UtteranceCollector`.
    pub fn disconnect_user(&mut self, user_id: u64) -> Option<ReceivedUtterance> {
        self.ssrc_users.retain(|_, mapped| *mapped != user_id);
        let mut collector = self.collectors.remove(&user_id)?;
        if !(self.consented)(user_id) {
            return None;
        }
        collector
            .flush()
            .map(|utterance| ReceivedUtterance { user_id, utterance })
    }

    /// Flushes all pending speakers and clears SSRC mappings. Used on an explicit stop or call
    /// teardown; it makes the no-audio-after-stop boundary deterministic.
    pub fn stop(&mut self) -> Vec<ReceivedUtterance> {
        let mut output = Vec::new();
        for (user_id, mut collector) in std::mem::take(&mut self.collectors) {
            if !(self.consented)(user_id) {
                continue;
            }
            if let Some(utterance) = collector.flush() {
                output.push(ReceivedUtterance { user_id, utterance });
            }
        }
        self.ssrc_users.clear();
        output
    }

    #[must_use]
    pub fn mapped_user(&self, ssrc: u32) -> Option<u64> {
        self.ssrc_users.get(&ssrc).copied()
    }

    #[must_use]
    pub fn pending_speakers(&self) -> usize {
        self.collectors.len()
    }
}

#[cfg(feature = "voice-driver")]
mod songbird_adapter {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use songbird::events::{CoreEvent, Event, EventContext, EventHandler};
    use tokio::sync::mpsc::Sender;

    use super::{ReceivedUtterance, VoiceReceiver};

    /// Event handler installed on a Songbird call when the live STT canary is enabled.
    #[derive(Clone)]
    pub struct SongbirdVoiceReceiver {
        state: Arc<Mutex<Option<ActiveReceiver>>>,
        frame_samples: usize,
    }

    struct ActiveReceiver {
        receiver: VoiceReceiver,
        output: Sender<ReceivedUtterance>,
    }

    impl SongbirdVoiceReceiver {
        pub fn new(
            receiver: VoiceReceiver,
            output: Sender<ReceivedUtterance>,
            frame_samples: usize,
        ) -> Self {
            Self {
                state: Arc::new(Mutex::new(Some(ActiveReceiver { receiver, output }))),
                frame_samples: frame_samples.max(1),
            }
        }

        /// Registers only the receive events needed by the router. The handler itself performs no
        /// network, filesystem, Whisper, or SQLite work on Songbird's audio thread.
        pub fn install_on_call(&self, call: &mut songbird::Call) {
            let handler = self.clone();
            call.add_global_event(Event::Core(CoreEvent::SpeakingStateUpdate), handler.clone());
            call.add_global_event(Event::Core(CoreEvent::VoiceTick), handler.clone());
            call.add_global_event(Event::Core(CoreEvent::ClientDisconnect), handler);
        }

        /// Seals every installed clone and closes the consumer after flushing pre-stop audio.
        pub fn stop(&self) {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => {
                    // Discard uncertain audio and close the sender even after a callback panic.
                    drop(poisoned.into_inner().take());
                    return;
                }
            };
            if let Some(mut active) = state.take() {
                Self::emit(&active.output, active.receiver.stop());
            }
        }

        fn emit(
            output: &Sender<ReceivedUtterance>,
            utterances: impl IntoIterator<Item = ReceivedUtterance>,
        ) {
            for utterance in utterances {
                // Never block Songbird's audio thread; overload drops this turn immediately.
                let _ = output.try_send(utterance);
            }
        }
    }

    #[async_trait]
    impl EventHandler for SongbirdVoiceReceiver {
        async fn act(&self, context: &EventContext<'_>) -> Option<Event> {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => {
                    drop(poisoned.into_inner().take());
                    return Some(Event::Cancel);
                }
            };
            let Some(active) = state.as_mut() else {
                return Some(Event::Cancel);
            };
            let state = &mut active.receiver;
            let output = match context {
                EventContext::SpeakingStateUpdate(update) => update
                    .user_id
                    .map(|user| {
                        state.map_ssrc(update.ssrc, user.0);
                        Vec::new()
                    })
                    .unwrap_or_default(),
                EventContext::VoiceTick(tick) => {
                    let mut output = Vec::new();
                    for (ssrc, data) in &tick.speaking {
                        let pcm = data
                            .decoded_voice
                            .clone()
                            .unwrap_or_else(|| vec![0; self.frame_samples]);
                        if let Some(utterance) = state.push_pcm(*ssrc, pcm) {
                            output.push(utterance);
                        }
                    }
                    for ssrc in &tick.silent {
                        if let Some(utterance) = state.push_silence(*ssrc) {
                            output.push(utterance);
                        }
                    }
                    output
                }
                EventContext::ClientDisconnect(disconnect) => state
                    .disconnect_user(disconnect.user_id.0)
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            };
            Self::emit(&active.output, output);
            None
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tokio::sync::mpsc;

        fn receiver() -> VoiceReceiver {
            let mut receiver = VoiceReceiver::new(1_920, Arc::new(|_| true));
            receiver.map_ssrc(9, 7);
            for _ in 0..15 {
                assert!(receiver.push_pcm(9, vec![500; 1_920]).is_none());
            }
            receiver
        }

        #[tokio::test]
        async fn stopped_clones_cannot_resume_or_keep_the_old_consumer_open() {
            let (tx_a, mut rx_a) = mpsc::channel(4);
            let old = SongbirdVoiceReceiver::new(receiver(), tx_a, 1_920);
            let installed_clone = old.clone();
            old.stop();
            assert_eq!(rx_a.recv().await.unwrap().user_id, 7);
            assert!(rx_a.recv().await.is_none());
            old.stop();
            let speaking = EventContext::SpeakingStateUpdate(
                serde_json::from_value(
                    serde_json::json!({"speaking": 1, "ssrc": 9, "user_id": "7", "delay": 0}),
                )
                .unwrap(),
            );
            assert!(matches!(
                installed_clone.act(&speaking).await,
                Some(Event::Cancel)
            ));
            assert!(installed_clone.state.lock().unwrap().is_none());

            let (tx_b, mut rx_b) = mpsc::channel(4);
            let new = SongbirdVoiceReceiver::new(receiver(), tx_b, 1_920);
            let disconnect = EventContext::ClientDisconnect(
                serde_json::from_value(serde_json::json!({"user_id": "7"})).unwrap(),
            );
            assert!(new.act(&disconnect).await.is_none());
            assert_eq!(rx_b.recv().await.unwrap().user_id, 7);
            assert!(rx_a.recv().await.is_none());
            new.stop();
            assert!(rx_b.recv().await.is_none());
        }

        #[tokio::test]
        async fn poisoned_callbacks_release_the_sender_without_flushing_uncertain_audio() {
            for stop_first in [false, true] {
                let (tx, mut rx) = mpsc::channel(4);
                let receiver =
                    VoiceReceiver::new(1_920, Arc::new(|_| panic!("synthetic consent failure")));
                let adapter = SongbirdVoiceReceiver::new(receiver, tx, 1_920);
                let clone = adapter.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut state = adapter.state.lock().unwrap();
                    let router = &mut state.as_mut().unwrap().receiver;
                    router.map_ssrc(9, 7);
                    router.push_pcm(9, vec![500; 1_920]);
                }));
                assert!(result.is_err());
                if stop_first {
                    adapter.stop();
                } else {
                    assert!(matches!(
                        clone.act(&EventContext::Track(&[])).await,
                        Some(Event::Cancel)
                    ));
                }
                assert!(rx.recv().await.is_none());
            }
        }

        #[test]
        fn overload_drops_turns_without_blocking_the_audio_callback() {
            let (tx, mut rx) = mpsc::channel(1);
            let utterance = receiver().stop().pop().unwrap();
            SongbirdVoiceReceiver::emit(&tx, [utterance.clone(), utterance.clone()]);
            assert_eq!(rx.try_recv().unwrap(), utterance);
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
    }

    pub use SongbirdVoiceReceiver as PublicSongbirdVoiceReceiver;
}

#[cfg(feature = "voice-driver")]
pub use songbird_adapter::PublicSongbirdVoiceReceiver as SongbirdVoiceReceiver;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn frame(value: i16) -> Vec<i16> {
        vec![value; 1_920]
    }

    #[test]
    fn unknown_and_non_consented_audio_is_never_buffered() {
        let consent = Arc::new(AtomicBool::new(false));
        let gate = {
            let consent = consent.clone();
            Arc::new(move |user_id| user_id == 7 && consent.load(Ordering::Relaxed))
        };
        let mut receiver = VoiceReceiver::new(1_920, gate);
        assert!(receiver.push_pcm(10, frame(600)).is_none());
        receiver.map_ssrc(10, 7);
        assert!(receiver.push_pcm(10, frame(600)).is_none());
        assert_eq!(receiver.pending_speakers(), 0);
        consent.store(true, Ordering::Relaxed);
        assert!(receiver.push_pcm(10, frame(600)).is_none());
        assert_eq!(receiver.pending_speakers(), 1);
    }

    #[test]
    fn consent_revoke_drops_pending_audio_and_ssrc_survives_reconsent() {
        let consent = Arc::new(AtomicBool::new(true));
        let gate = {
            let consent = consent.clone();
            Arc::new(move |_| consent.load(Ordering::Relaxed))
        };
        let mut receiver = VoiceReceiver::new(1_920, gate);
        receiver.map_ssrc(4, 99);
        for _ in 0..15 {
            receiver.push_pcm(4, frame(700));
        }
        assert_eq!(receiver.pending_speakers(), 1);
        consent.store(false, Ordering::Relaxed);
        assert!(receiver.push_pcm(4, frame(700)).is_none());
        assert_eq!(receiver.pending_speakers(), 0);
        consent.store(true, Ordering::Relaxed);
        assert!(receiver.push_pcm(4, frame(700)).is_none());
        assert_eq!(receiver.pending_speakers(), 1);
    }

    #[test]
    fn revoke_before_disconnect_discards_audio_without_another_frame() {
        let consent = Arc::new(AtomicBool::new(true));
        let gate = {
            let consent = consent.clone();
            Arc::new(move |_| consent.load(Ordering::Relaxed))
        };
        let mut receiver = VoiceReceiver::new(1_920, gate);
        receiver.map_ssrc(2, 42);
        for _ in 0..15 {
            receiver.push_pcm(2, frame(500));
        }
        consent.store(false, Ordering::Relaxed);
        assert!(receiver.disconnect_user(42).is_none());
        assert_eq!(receiver.pending_speakers(), 0);
        assert_eq!(receiver.mapped_user(2), None);
    }

    #[test]
    fn stop_flushes_only_speakers_whose_consent_is_still_active() {
        let consent = Arc::new(AtomicBool::new(true));
        let gate = {
            let consent = consent.clone();
            Arc::new(move |user_id| user_id == 7 || consent.load(Ordering::Relaxed))
        };
        let mut receiver = VoiceReceiver::new(1_920, gate);
        receiver.map_ssrc(2, 42);
        receiver.map_ssrc(3, 7);
        for _ in 0..15 {
            receiver.push_pcm(2, frame(500));
            receiver.push_pcm(3, frame(500));
        }
        consent.store(false, Ordering::Relaxed);
        let output = receiver.stop();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].user_id, 7);
        assert_eq!(receiver.pending_speakers(), 0);
        assert_eq!(receiver.mapped_user(2), None);
        assert_eq!(receiver.mapped_user(3), None);
    }

    #[test]
    fn disconnect_flushes_only_a_complete_turn() {
        let mut receiver = VoiceReceiver::new(1_920, Arc::new(|_| true));
        receiver.map_ssrc(2, 42);
        for _ in 0..15 {
            assert!(receiver.push_pcm(2, frame(500)).is_none());
        }
        let output = receiver.disconnect_user(42).expect("flushes complete turn");
        assert_eq!(output.user_id, 42);
        assert_eq!(output.utterance.voiced_ms, 300);
        assert_eq!(receiver.mapped_user(2), None);
    }
}
