//! Composition for independently promoted gateway slices.
//!
//! Each slice still validates and claims only its own Discord events. Dispatching all sinks is
//! safe because a handler must leave unpromoted events untouched; this allows private exports to
//! be promoted without coupling them to in-call Songbird playback.
//!
//! Every sink receives every event, in order, even when an earlier sink fails: one feature's
//! transient error (for example a failed game-thread post) must not stop another feature from
//! seeing the same event (for example live transcription releasing its slot when the bot leaves).
//! The composite still reports a failure to its caller when any sink failed.

use std::sync::Arc;

use async_trait::async_trait;
use serenity::{client::Context, model::application::Interaction};

use crate::{GatewayEventDispatchError, GatewayEventSink};

pub struct CompositeGatewayEventSink {
    sinks: Vec<Arc<dyn GatewayEventSink>>,
}

impl CompositeGatewayEventSink {
    #[must_use]
    pub fn new(sinks: Vec<Arc<dyn GatewayEventSink>>) -> Self {
        Self { sinks }
    }
}

/// Records one sink's result without stopping the dispatch. Errors are content-free, so only the
/// hook name and the sink position are logged.
fn note(
    hook: &'static str,
    index: usize,
    result: Result<(), GatewayEventDispatchError>,
    failed: &mut bool,
) {
    if result.is_err() {
        eprintln!("[gateway] sink {index} failed in {hook}");
        *failed = true;
    }
}

fn settle(failed: bool) -> Result<(), GatewayEventDispatchError> {
    if failed {
        Err(GatewayEventDispatchError)
    } else {
        Ok(())
    }
}

#[async_trait]
impl GatewayEventSink for CompositeGatewayEventSink {
    async fn on_ready(&self, context: Context) -> Result<(), GatewayEventDispatchError> {
        let mut failed = false;
        for (index, sink) in self.sinks.iter().enumerate() {
            let result = sink.on_ready(context.clone()).await;
            note("on_ready", index, result, &mut failed);
        }
        settle(failed)
    }

    async fn on_entitlement_change(
        &self,
        context: Context,
    ) -> Result<(), GatewayEventDispatchError> {
        let mut failed = false;
        for (index, sink) in self.sinks.iter().enumerate() {
            let result = sink.on_entitlement_change(context.clone()).await;
            note("on_entitlement_change", index, result, &mut failed);
        }
        settle(failed)
    }

    async fn on_guild_create(&self, guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        let mut failed = false;
        for (index, sink) in self.sinks.iter().enumerate() {
            let result = sink.on_guild_create(guild_id).await;
            note("on_guild_create", index, result, &mut failed);
        }
        settle(failed)
    }

    async fn on_guild_create_details(
        &self,
        context: Context,
        guild: serenity::model::guild::Guild,
    ) -> Result<(), GatewayEventDispatchError> {
        let mut failed = false;
        for (index, sink) in self.sinks.iter().enumerate() {
            let result = sink
                .on_guild_create_details(context.clone(), guild.clone())
                .await;
            note("on_guild_create_details", index, result, &mut failed);
        }
        settle(failed)
    }

    async fn on_message(
        &self,
        context: Context,
        message: serenity::model::channel::Message,
    ) -> Result<(), GatewayEventDispatchError> {
        let mut failed = false;
        for (index, sink) in self.sinks.iter().enumerate() {
            let result = sink.on_message(context.clone(), message.clone()).await;
            note("on_message", index, result, &mut failed);
        }
        settle(failed)
    }

    async fn on_reaction_add(
        &self,
        context: Context,
        reaction: serenity::model::channel::Reaction,
    ) -> Result<(), GatewayEventDispatchError> {
        let mut failed = false;
        for (index, sink) in self.sinks.iter().enumerate() {
            let result = sink
                .on_reaction_add(context.clone(), reaction.clone())
                .await;
            note("on_reaction_add", index, result, &mut failed);
        }
        settle(failed)
    }

    async fn on_interaction(
        &self,
        context: Context,
        interaction: Interaction,
    ) -> Result<(), GatewayEventDispatchError> {
        let mut failed = false;
        for (index, sink) in self.sinks.iter().enumerate() {
            let result = sink
                .on_interaction(context.clone(), interaction.clone())
                .await;
            note("on_interaction", index, result, &mut failed);
        }
        settle(failed)
    }

    async fn on_voice_state_update(
        &self,
        context: Context,
        old: Option<serenity::model::voice::VoiceState>,
        new: serenity::model::voice::VoiceState,
    ) -> Result<(), GatewayEventDispatchError> {
        let mut failed = false;
        for (index, sink) in self.sinks.iter().enumerate() {
            let result = sink
                .on_voice_state_update(context.clone(), old.clone(), new.clone())
                .await;
            note("on_voice_state_update", index, result, &mut failed);
        }
        settle(failed)
    }

    async fn on_guild_delete(&self, guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        let mut failed = false;
        for (index, sink) in self.sinks.iter().enumerate() {
            let result = sink.on_guild_delete(guild_id).await;
            note("on_guild_delete", index, result, &mut failed);
        }
        settle(failed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Recorder {
        fails: bool,
        calls: AtomicUsize,
    }

    impl Recorder {
        fn new(fails: bool) -> Arc<Self> {
            Arc::new(Self {
                fails,
                calls: AtomicUsize::new(0),
            })
        }

        fn result(&self) -> Result<(), GatewayEventDispatchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fails {
                Err(GatewayEventDispatchError)
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl GatewayEventSink for Recorder {
        async fn on_guild_create(&self, _guild_id: &str) -> Result<(), GatewayEventDispatchError> {
            self.result()
        }

        async fn on_message(
            &self,
            _context: Context,
            _message: serenity::model::channel::Message,
        ) -> Result<(), GatewayEventDispatchError> {
            Ok(())
        }

        async fn on_interaction(
            &self,
            _context: Context,
            _interaction: Interaction,
        ) -> Result<(), GatewayEventDispatchError> {
            Ok(())
        }

        async fn on_guild_delete(&self, _guild_id: &str) -> Result<(), GatewayEventDispatchError> {
            self.result()
        }
    }

    #[tokio::test]
    async fn a_failing_sink_does_not_hide_the_event_from_later_sinks() {
        let failing = Recorder::new(true);
        let later = Recorder::new(false);
        let composite = CompositeGatewayEventSink::new(vec![
            failing.clone() as Arc<dyn GatewayEventSink>,
            later.clone(),
        ]);
        assert!(composite.on_guild_delete("1").await.is_err());
        assert!(composite.on_guild_create("1").await.is_err());
        assert_eq!(failing.calls.load(Ordering::SeqCst), 2);
        assert_eq!(later.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_composite_succeeds_when_every_sink_succeeds() {
        let first = Recorder::new(false);
        let second = Recorder::new(false);
        let composite = CompositeGatewayEventSink::new(vec![
            first.clone() as Arc<dyn GatewayEventSink>,
            second.clone(),
        ]);
        assert!(composite.on_guild_delete("1").await.is_ok());
        assert_eq!(second.calls.load(Ordering::SeqCst), 1);
    }
}
