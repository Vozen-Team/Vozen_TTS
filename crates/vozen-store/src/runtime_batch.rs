//! In-memory aggregation for asynchronous durable-store delivery.
//!
//! Recording is a short mutex operation with no I/O. A runtime worker drains the aggregate every
//! few seconds into the local SQLite outbox, which is then delivered to Supabase independently
//! from Discord voice handling.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use serde::Serialize;

use crate::{OperationalMetric, OperationalProvider, UserEngine};

#[derive(Clone)]
pub struct RuntimeBatchBuffer {
    enabled: bool,
    pending: Arc<Mutex<PendingBatch>>,
}

impl Default for RuntimeBatchBuffer {
    fn default() -> Self {
        Self::disabled()
    }
}

#[derive(Debug, Default)]
struct PendingBatch {
    metrics: BTreeMap<(String, String, String), i64>,
    speech: BTreeMap<(String, String, String, String, String), i64>,
}

#[derive(Debug)]
pub struct RuntimeBatchEvent {
    payload: String,
    pending: PendingBatch,
}

#[derive(Serialize)]
struct Payload<'a> {
    version: u8,
    metrics: Vec<Metric<'a>>,
    speech: Vec<Speech<'a>>,
}

#[derive(Serialize)]
struct Metric<'a> {
    day: &'a str,
    metric: &'a str,
    provider: &'a str,
    value: i64,
}

#[derive(Serialize)]
struct Speech<'a> {
    day: &'a str,
    guild_id: &'a str,
    user_id: &'a str,
    model: &'a str,
    engine: &'a str,
    value: i64,
}

impl RuntimeBatchBuffer {
    #[must_use]
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            pending: Arc::new(Mutex::new(PendingBatch::default())),
        }
    }

    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            pending: Arc::new(Mutex::new(PendingBatch::default())),
        }
    }

    /// Records a fixed, identity-free operational measurement without touching SQLite or network.
    pub fn record_metric(
        &self,
        day: &str,
        metric: OperationalMetric,
        provider: OperationalProvider,
        value: i64,
    ) {
        if !self.enabled || value <= 0 {
            return;
        }
        if let Ok(mut pending) = self.pending.lock() {
            *pending
                .metrics
                .entry((
                    day.to_owned(),
                    metric.as_database().to_owned(),
                    provider.as_database().to_owned(),
                ))
                .or_default() += value;
        }
    }

    /// Records an accepted speech aggregate after queue admission. No message content is kept.
    pub fn record_accepted_speech(
        &self,
        day: &str,
        guild_id: &str,
        user_id: &str,
        model: &str,
        engine: UserEngine,
    ) {
        if !self.enabled
            || [day, guild_id, user_id, model]
                .iter()
                .any(|value| value.trim().is_empty())
        {
            return;
        }
        if let Ok(mut pending) = self.pending.lock() {
            *pending
                .speech
                .entry((
                    day.to_owned(),
                    guild_id.to_owned(),
                    user_id.to_owned(),
                    model.to_owned(),
                    engine_name(engine).to_owned(),
                ))
                .or_default() += 1;
        }
    }

    /// Takes the current batch. If persisting it to SQLite fails, pass it back to `restore`.
    pub fn drain(&self) -> Option<RuntimeBatchEvent> {
        if !self.enabled {
            return None;
        }
        let mut pending = self.pending.lock().ok()?;
        if pending.metrics.is_empty() && pending.speech.is_empty() {
            return None;
        }
        let taken = std::mem::take(&mut *pending);
        let payload = encode(&taken)?;
        Some(RuntimeBatchEvent {
            payload,
            pending: taken,
        })
    }

    pub fn restore(&self, event: RuntimeBatchEvent) {
        if let Ok(mut pending) = self.pending.lock() {
            for (key, value) in event.pending.metrics {
                *pending.metrics.entry(key).or_default() += value;
            }
            for (key, value) in event.pending.speech {
                *pending.speech.entry(key).or_default() += value;
            }
        }
    }
}

impl RuntimeBatchEvent {
    #[must_use]
    pub fn payload(&self) -> &str {
        &self.payload
    }

    /// Splits the batch until every payload fits `max_bytes`, so a backlog larger than the
    /// outbox limit is delivered in parts instead of being rejected and restored forever. A
    /// single row that still does not fit is returned on its own for the caller to drop.
    #[must_use]
    pub fn split(self, max_bytes: usize) -> Vec<Self> {
        if self.payload.len() <= max_bytes
            || self.pending.metrics.len() + self.pending.speech.len() <= 1
        {
            return vec![self];
        }
        let (left, right) = self.pending.halve();
        [left, right]
            .into_iter()
            .filter_map(|pending| {
                let payload = encode(&pending)?;
                Some(Self { payload, pending })
            })
            .flat_map(|part| part.split(max_bytes))
            .collect()
    }
}

impl PendingBatch {
    /// Moves the last half of the rows into a second batch; counters are never split.
    fn halve(mut self) -> (Self, Self) {
        let total = self.metrics.len() + self.speech.len();
        let mut right = Self::default();
        while right.metrics.len() + right.speech.len() < total - total / 2 {
            if let Some((key, value)) = self.speech.pop_last() {
                right.speech.insert(key, value);
            } else if let Some((key, value)) = self.metrics.pop_last() {
                right.metrics.insert(key, value);
            } else {
                break;
            }
        }
        (self, right)
    }
}

fn encode(pending: &PendingBatch) -> Option<String> {
    let metrics = pending
        .metrics
        .iter()
        .map(|((day, metric, provider), value)| Metric {
            day,
            metric,
            provider,
            value: *value,
        })
        .collect();
    let speech = pending
        .speech
        .iter()
        .map(|((day, guild_id, user_id, model, engine), value)| Speech {
            day,
            guild_id,
            user_id,
            model,
            engine,
            value: *value,
        })
        .collect();
    serde_json::to_string(&Payload {
        version: 1,
        metrics,
        speech,
    })
    .ok()
}

fn engine_name(engine: UserEngine) -> &'static str {
    match engine {
        UserEngine::Google => "google",
        UserEngine::Piper => "piper",
        UserEngine::Kokoro => "kokoro",
        UserEngine::Gcloud => "gcloud",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_drain_splits_into_parts_that_fit_and_keep_every_count() {
        let buffer = RuntimeBatchBuffer::enabled();
        for index in 0..200 {
            buffer.record_accepted_speech(
                "2026-11-25",
                &format!("guild-{index}"),
                &format!("user-{index}"),
                "pt_PT-voice",
                UserEngine::Piper,
            );
        }
        let batch = buffer.drain().expect("batch");
        let limit = batch.payload().len() / 5;
        let parts = batch.split(limit);
        assert!(parts.len() >= 5);
        let mut rows = 0;
        for part in &parts {
            assert!(part.payload().len() <= limit, "{}", part.payload().len());
            let payload: serde_json::Value =
                serde_json::from_str(part.payload()).expect("valid JSON");
            rows += payload["speech"].as_array().expect("speech rows").len();
        }
        assert_eq!(rows, 200);
    }

    #[test]
    fn coalesces_events_and_restores_a_failed_drain() {
        let buffer = RuntimeBatchBuffer::enabled();
        buffer.record_metric(
            "2026-07-29",
            OperationalMetric::SynthSuccess,
            OperationalProvider::Piper,
            1,
        );
        buffer.record_metric(
            "2026-07-29",
            OperationalMetric::SynthSuccess,
            OperationalProvider::Piper,
            2,
        );
        buffer.record_accepted_speech(
            "2026-07-29",
            "guild",
            "user",
            "pt_PT-voice",
            UserEngine::Piper,
        );
        let batch = buffer.drain().expect("batch");
        assert!(batch.payload().contains("\"value\":3"));
        buffer.restore(batch);
        assert!(
            buffer
                .drain()
                .expect("restored")
                .payload()
                .contains("pt_PT-voice")
        );
    }

    #[test]
    fn disabled_buffer_is_a_noop() {
        let buffer = RuntimeBatchBuffer::default();
        buffer.record_accepted_speech("2026-07-29", "guild", "user", "voice", UserEngine::Piper);
        buffer.record_metric(
            "2026-07-29",
            OperationalMetric::SynthSuccess,
            OperationalProvider::Piper,
            1,
        );
        assert!(buffer.drain().is_none());
    }
}
