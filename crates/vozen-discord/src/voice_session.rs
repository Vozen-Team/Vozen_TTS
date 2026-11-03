//! Durable `/join` and `/leave` session lifecycle.
//!
//! The Discord adapter supplies the transport implementation. This service owns the critical
//! ordering: a failed join never overwrites a working recovery hint, while an explicit leave
//! always removes that hint so an administrator's intent cannot be undone on restart.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use thiserror::Error;
use vozen_store::SqliteStore;

use crate::GatewayState;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum VoiceSessionTransportError {
    #[error("Vozen cannot connect or speak in that voice channel")]
    PermissionDenied,
    #[error("Discord voice transport is unavailable")]
    Unavailable,
    #[error("Discord voice transport failed")]
    Failed,
}

/// Narrow voice lifecycle boundary. It receives IDs only after the command adapter has bound
/// them to the invoking Discord user; it never reads configuration or user text.
#[async_trait]
pub trait VoiceSessionTransport: Send + Sync {
    async fn join(
        &self,
        guild_id: &str,
        channel_id: &str,
    ) -> Result<(), VoiceSessionTransportError>;
    async fn join_if_idle(
        &self,
        guild_id: &str,
        channel_id: &str,
        still_authorized: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), VoiceSessionTransportError> {
        if !still_authorized() {
            return Err(VoiceSessionTransportError::PermissionDenied);
        }
        self.join(guild_id, channel_id).await
    }
    async fn leave(&self, guild_id: &str) -> Result<(), VoiceSessionTransportError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinVoiceOutcome {
    NoUserVoiceChannel,
    Joined,
    PermissionDenied,
    Unavailable,
    Failed,
    StoreUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveVoiceOutcome {
    Left,
    TransportFailed,
    StoreUnavailable,
}

pub struct VoiceSessionService<T> {
    store: Arc<Mutex<SqliteStore>>,
    gateway_state: GatewayState,
    transport: T,
}

impl<T> VoiceSessionService<T> {
    pub fn new(store: Arc<Mutex<SqliteStore>>, gateway_state: GatewayState, transport: T) -> Self {
        Self {
            store,
            gateway_state,
            transport,
        }
    }
}

impl<T: VoiceSessionTransport> VoiceSessionService<T> {
    /// Joins the caller's current voice channel. No stored presence is changed until the
    /// transport reports success, preserving a healthy existing call after a forbidden join.
    pub async fn join_for_user(
        &self,
        guild_id: &str,
        user_id: &str,
        now_ms: i64,
    ) -> JoinVoiceOutcome {
        let Some(channel_id) = self.gateway_state.voice_channel_id(guild_id, user_id) else {
            return JoinVoiceOutcome::NoUserVoiceChannel;
        };
        let joined = self.transport.join(guild_id, &channel_id).await;
        self.finish_join(guild_id, &channel_id, now_ms, joined)
    }

    /// Joins only the channel the message's autojoin policy and permission check approved.
    pub async fn join_for_message(
        &self,
        guild_id: &str,
        user_id: &str,
        expected_channel: &str,
        now_ms: i64,
    ) -> JoinVoiceOutcome {
        let still_authorized = || {
            self.gateway_state.bot_voice_channel_id(guild_id).is_none()
                && self
                    .gateway_state
                    .voice_channel_id(guild_id, user_id)
                    .as_deref()
                    == Some(expected_channel)
        };
        let joined = self
            .transport
            .join_if_idle(guild_id, expected_channel, &still_authorized)
            .await;
        self.finish_join(guild_id, expected_channel, now_ms, joined)
    }

    fn finish_join(
        &self,
        guild_id: &str,
        channel_id: &str,
        now_ms: i64,
        joined: Result<(), VoiceSessionTransportError>,
    ) -> JoinVoiceOutcome {
        match joined {
            Ok(()) => {
                // The voice gateway update is asynchronous. Publish the local fact now so an
                // immediately following message cannot bypass the same-call policy while the
                // durable rejoin marker is being written.
                self.gateway_state
                    .set_bot_voice_channel(guild_id, Some(channel_id.to_owned()));
                match self.store.lock() {
                    Ok(store) => {
                        match store.remember_voice_presence(guild_id, channel_id, now_ms) {
                            Ok(()) => JoinVoiceOutcome::Joined,
                            Err(_) => JoinVoiceOutcome::StoreUnavailable,
                        }
                    }
                    Err(_) => JoinVoiceOutcome::StoreUnavailable,
                }
            }
            Err(VoiceSessionTransportError::PermissionDenied) => JoinVoiceOutcome::PermissionDenied,
            Err(VoiceSessionTransportError::Unavailable) => JoinVoiceOutcome::Unavailable,
            Err(VoiceSessionTransportError::Failed) => JoinVoiceOutcome::Failed,
        }
    }

    /// An explicit leave is authoritative. The recovery hint is deleted even if the driver has
    /// already disappeared; otherwise a later clean restart could silently rejoin the channel.
    pub async fn leave_explicitly(&self, guild_id: &str) -> LeaveVoiceOutcome {
        let transport = self.transport.leave(guild_id).await;
        // An explicit leave must immediately stop message admission even if the driver is
        // already gone. The later gateway update merely confirms this fail-closed state.
        self.gateway_state.set_bot_voice_channel(guild_id, None);
        let erased = self
            .store
            .lock()
            .ok()
            .and_then(|store| store.forget_voice_presence(guild_id).ok())
            .is_some();
        if !erased {
            return LeaveVoiceOutcome::StoreUnavailable;
        }
        match transport {
            Ok(()) => LeaveVoiceOutcome::Left,
            Err(_) => LeaveVoiceOutcome::TransportFailed,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use tokio::sync::Notify;

    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        joins: Mutex<Vec<(String, String)>>,
        leaves: Mutex<Vec<String>>,
        join_error: Option<VoiceSessionTransportError>,
        leave_error: Option<VoiceSessionTransportError>,
        authorization_gate: Option<(Notify, Notify)>,
    }

    #[async_trait]
    impl VoiceSessionTransport for FakeTransport {
        async fn join(
            &self,
            guild_id: &str,
            channel_id: &str,
        ) -> Result<(), VoiceSessionTransportError> {
            self.joins
                .lock()
                .expect("joins")
                .push((guild_id.into(), channel_id.into()));
            self.join_error.map_or(Ok(()), Err)
        }

        async fn join_if_idle(
            &self,
            guild_id: &str,
            channel_id: &str,
            still_authorized: &(dyn Fn() -> bool + Sync),
        ) -> Result<(), VoiceSessionTransportError> {
            if let Some((started, release)) = &self.authorization_gate {
                started.notify_one();
                release.notified().await;
            }
            if !still_authorized() {
                return Err(VoiceSessionTransportError::PermissionDenied);
            }
            self.join(guild_id, channel_id).await
        }

        async fn leave(&self, guild_id: &str) -> Result<(), VoiceSessionTransportError> {
            self.leaves.lock().expect("leaves").push(guild_id.into());
            self.leave_error.map_or(Ok(()), Err)
        }
    }

    fn make_service(
        transport: FakeTransport,
    ) -> (
        VoiceSessionService<FakeTransport>,
        Arc<Mutex<SqliteStore>>,
        GatewayState,
    ) {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let state = GatewayState::default();
        (
            VoiceSessionService::new(store.clone(), state.clone(), transport),
            store,
            state,
        )
    }

    #[tokio::test]
    async fn autojoin_rejects_a_channel_changed_while_waiting_for_transport() {
        let (service, store, state) = make_service(FakeTransport {
            authorization_gate: Some((Notify::new(), Notify::new())),
            ..FakeTransport::default()
        });
        state.remember_bot_user("bot".into());
        state.update_voice_state("guild", "user", Some("voice".into()));
        let (started, release) = service.transport.authorization_gate.as_ref().expect("gate");
        let join = service.join_for_message("guild", "user", "voice", 42);
        tokio::pin!(join);
        tokio::select! {
            _ = started.notified() => {}
            result = &mut join => panic!("autojoin finished before the state change: {result:?}"),
        }
        state.update_voice_state("guild", "user", Some("other".into()));
        release.notify_one();
        assert_eq!(join.await, JoinVoiceOutcome::PermissionDenied);
        assert!(service.transport.joins.lock().expect("joins").is_empty());
        assert!(
            store
                .lock()
                .expect("store")
                .voice_presences()
                .expect("presence")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn autojoin_only_joins_the_approved_channel_when_the_bot_is_idle() {
        let (service, _, state) = make_service(FakeTransport::default());
        state.remember_bot_user("bot".into());
        state.update_voice_state("guild", "user", Some("voice".into()));
        state.set_bot_voice_channel("guild", Some("already-connected".into()));
        assert_eq!(
            service.join_for_message("guild", "user", "voice", 42).await,
            JoinVoiceOutcome::PermissionDenied
        );
        assert!(service.transport.joins.lock().expect("joins").is_empty());

        state.set_bot_voice_channel("guild", None);
        assert_eq!(
            service.join_for_message("guild", "user", "voice", 43).await,
            JoinVoiceOutcome::Joined
        );
        assert_eq!(
            *service.transport.joins.lock().expect("joins"),
            vec![("guild".to_owned(), "voice".to_owned())]
        );
    }

    #[tokio::test]
    async fn join_persists_only_after_a_successful_transport_join() {
        let (service, store, state) = make_service(FakeTransport::default());
        state.remember_bot_user("bot".into());
        state.update_voice_state("guild", "user", Some("voice".into()));
        assert_eq!(
            service.join_for_user("guild", "user", 42).await,
            JoinVoiceOutcome::Joined
        );
        assert_eq!(
            store
                .lock()
                .expect("store")
                .voice_presences()
                .expect("presence")[0]
                .channel_id,
            "voice"
        );
        assert_eq!(
            state.bot_voice_channel_id("guild").as_deref(),
            Some("voice")
        );

        let (failed, store, state) = make_service(FakeTransport {
            join_error: Some(VoiceSessionTransportError::PermissionDenied),
            ..FakeTransport::default()
        });
        state.remember_bot_user("bot".into());
        store
            .lock()
            .expect("store")
            .remember_voice_presence("guild", "existing", 1)
            .expect("existing presence");
        state.update_voice_state("guild", "user", Some("forbidden".into()));
        assert_eq!(
            failed.join_for_user("guild", "user", 2).await,
            JoinVoiceOutcome::PermissionDenied
        );
        assert_eq!(
            store
                .lock()
                .expect("store")
                .voice_presences()
                .expect("presence")[0]
                .channel_id,
            "existing"
        );
    }

    #[tokio::test]
    async fn explicit_leave_clears_rejoin_hint_even_when_driver_is_already_gone() {
        let (service, store, state) = make_service(FakeTransport {
            leave_error: Some(VoiceSessionTransportError::Unavailable),
            ..FakeTransport::default()
        });
        state.remember_bot_user("bot".into());
        state.set_bot_voice_channel("guild", Some("voice".into()));
        store
            .lock()
            .expect("store")
            .remember_voice_presence("guild", "voice", 1)
            .expect("presence");
        assert_eq!(
            service.leave_explicitly("guild").await,
            LeaveVoiceOutcome::TransportFailed
        );
        assert!(
            store
                .lock()
                .expect("store")
                .voice_presences()
                .expect("presence")
                .is_empty()
        );
        assert_eq!(state.bot_voice_channel_id("guild"), None);
    }
}
