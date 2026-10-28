//! Bounded, best-effort persistence for post-playback speech telemetry.
//!
//! The message handler only performs a non-blocking queue send.  A small worker owns the
//! SQLite writes and coalesces compatible counters before flushing them.  Durable quota and
//! entitlement reservations deliberately do not use this writer.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use vozen_store::{
    OperationalMetric, OperationalProvider, PendingUserData, ProviderHealth, SqliteStore,
    UserEngine,
};

const QUEUE_CAPACITY: usize = 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone)]
pub struct SpeechTelemetryWriter {
    inner: Arc<WriterInner>,
}

struct WriterInner {
    sender: SyncSender<Command>,
    dropped: AtomicU64,
    pending_user_data: PendingUserData,
}

enum Command {
    Event(Event),
    Flush(mpsc::Sender<()>),
    Shutdown(mpsc::Sender<()>),
}

#[derive(Debug)]
enum Event {
    Metric {
        day: String,
        metric: OperationalMetric,
        provider: OperationalProvider,
        value: i64,
    },
    ProviderHealth {
        provider: OperationalProvider,
        health: ProviderHealth,
        changed_at: i64,
    },
    GuildTalk {
        guild_id: String,
        day: String,
    },
    TalkUsage {
        guild_id: String,
        user_id: String,
        model: String,
        engine: UserEngine,
        validity: Arc<AtomicBool>,
    },
}

impl SpeechTelemetryWriter {
    pub fn new(store: Arc<Mutex<SqliteStore>>) -> Self {
        let pending_user_data = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pending_user_data();
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        thread::Builder::new()
            .name("vozen-speech-telemetry".to_owned())
            .spawn(move || worker(receiver, store))
            .expect("spawn speech telemetry worker");
        Self {
            inner: Arc::new(WriterInner {
                sender,
                dropped: AtomicU64::new(0),
                pending_user_data,
            }),
        }
    }

    pub fn record_metric(
        &self,
        day: &str,
        metric: OperationalMetric,
        provider: OperationalProvider,
        value: i64,
    ) {
        self.enqueue(Event::Metric {
            day: day.to_owned(),
            metric,
            provider,
            value,
        });
    }

    pub fn record_provider_health(
        &self,
        provider: OperationalProvider,
        health: ProviderHealth,
        changed_at: i64,
    ) {
        self.enqueue(Event::ProviderHealth {
            provider,
            health,
            changed_at,
        });
    }

    pub fn record_guild_talk(&self, guild_id: &str, day: &str) {
        self.enqueue(Event::GuildTalk {
            guild_id: guild_id.to_owned(),
            day: day.to_owned(),
        });
    }

    pub fn record_talk_usage(
        &self,
        guild_id: &str,
        user_id: &str,
        model: &str,
        engine: UserEngine,
    ) {
        self.enqueue(Event::TalkUsage {
            guild_id: guild_id.to_owned(),
            user_id: user_id.to_owned(),
            model: model.to_owned(),
            engine,
            validity: self.inner.pending_user_data.capture(user_id),
        });
    }

    fn enqueue(&self, event: Event) {
        match self.inner.sender.try_send(Command::Event(event)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Flushes queued telemetry. Intended for graceful shutdown and deterministic tests.
    pub fn flush(&self) {
        let (ack, wait) = mpsc::channel();
        if self.inner.sender.send(Command::Flush(ack)).is_ok() {
            let _ = wait.recv();
        }
    }

    /// Flushes queued telemetry and stops the worker. Safe to call more than once.
    pub fn shutdown(&self) {
        let (ack, wait) = mpsc::channel();
        if self.inner.sender.send(Command::Shutdown(ack)).is_ok() {
            let _ = wait.recv();
        }
    }

    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for SpeechTelemetryWriter {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.shutdown();
        }
    }
}

fn worker(receiver: Receiver<Command>, store: Arc<Mutex<SqliteStore>>) {
    let mut pending = Pending::default();
    let mut next_flush = Instant::now() + FLUSH_INTERVAL;
    loop {
        if Instant::now() >= next_flush {
            pending.flush(&store);
            next_flush = Instant::now() + FLUSH_INTERVAL;
        }
        match receiver.recv_timeout(next_flush.saturating_duration_since(Instant::now())) {
            Ok(Command::Event(event)) => pending.push(event),
            Ok(Command::Flush(ack)) => {
                pending.flush(&store);
                let _ = ack.send(());
            }
            Ok(Command::Shutdown(ack)) => {
                pending.flush(&store);
                let _ = ack.send(());
                return;
            }
            Err(RecvTimeoutError::Timeout) => pending.flush(&store),
            Err(RecvTimeoutError::Disconnected) => {
                pending.flush(&store);
                return;
            }
        }
    }
}

#[derive(Default)]
struct Pending {
    metrics: MetricPending,
    provider_health: HashMap<String, (OperationalProvider, ProviderHealth, i64)>,
    guild_talk: HashMap<(String, String), i64>,
    talk_usage: HashMap<(String, String, String, UserEngine), (Arc<AtomicBool>, i64)>,
}

type MetricPending =
    HashMap<(String, String, String), (String, OperationalMetric, OperationalProvider, i64)>;

impl Pending {
    fn push(&mut self, event: Event) {
        match event {
            Event::Metric {
                day,
                metric,
                provider,
                value,
            } => {
                let key = (day.clone(), format!("{metric:?}"), format!("{provider:?}"));
                let entry = self
                    .metrics
                    .entry(key)
                    .or_insert((day, metric, provider, 0));
                entry.3 += value;
            }
            Event::ProviderHealth {
                provider,
                health,
                changed_at,
            } => {
                self.provider_health
                    .insert(format!("{provider:?}"), (provider, health, changed_at));
            }
            Event::GuildTalk { guild_id, day } => {
                *self.guild_talk.entry((guild_id, day)).or_default() += 1;
            }
            Event::TalkUsage {
                guild_id,
                user_id,
                model,
                engine,
                validity,
            } => {
                if !validity.load(Ordering::Acquire) {
                    return;
                }
                let entry = self
                    .talk_usage
                    .entry((guild_id, user_id, model, engine))
                    .or_insert_with(|| (validity.clone(), 0));
                if !Arc::ptr_eq(&entry.0, &validity) {
                    *entry = (validity, 0);
                }
                entry.1 += 1;
            }
        }
    }

    fn flush(&mut self, store: &Arc<Mutex<SqliteStore>>) {
        if self.metrics.is_empty()
            && self.provider_health.is_empty()
            && self.guild_talk.is_empty()
            && self.talk_usage.is_empty()
        {
            return;
        }
        let Ok(store) = store.lock() else {
            return;
        };
        for (_, (day, metric, provider, value)) in self.metrics.drain() {
            let _ = store.add_operational_metric(metric, provider, value as f64, Some(&day));
        }
        for (_, (provider, health, changed_at)) in self.provider_health.drain() {
            let _ = store.set_provider_health(provider, health, changed_at);
        }
        for ((guild_id, day), value) in self.guild_talk.drain() {
            for _ in 0..value {
                let _ = store.bump_guild_talk(&guild_id, &day);
            }
        }
        for ((guild_id, user_id, model, engine), (validity, value)) in self.talk_usage.drain() {
            if !validity.load(Ordering::Acquire) {
                continue;
            }
            for _ in 0..value {
                let _ = store.bump_talk_usage(&guild_id, &user_id, &model, engine);
            }
        }
        store.pending_user_data().prune();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(store: &Arc<Mutex<SqliteStore>>, user: &str) -> i64 {
        store
            .lock()
            .unwrap()
            .dominant_talk_usage(&[user.to_owned()], Default::default())
            .unwrap()
            .get(user)
            .map_or(0, |usage| usage.samples)
    }

    #[test]
    fn queued_pre_erasure_usage_cannot_recreate_personal_rows() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap()));
        let writer = SpeechTelemetryWriter::new(store.clone());
        {
            let guard = store.lock().unwrap();
            writer.record_talk_usage("guild", "erased", "en_US-amy-medium", UserEngine::Piper);
            writer.record_talk_usage("guild", "other", "en_US-amy-medium", UserEngine::Piper);
            guard.erase_user_data("erased").unwrap();
        }
        writer.flush();
        assert_eq!(count(&store, "erased"), 0);
        assert_eq!(count(&store, "other"), 1);
        writer.record_talk_usage("guild", "erased", "en_US-amy-medium", UserEngine::Piper);
        writer.flush();
        assert_eq!(count(&store, "erased"), 1);
    }

    #[test]
    fn coalesced_generations_do_not_mix_after_erasure() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap()));
        let registry = store.lock().unwrap().pending_user_data();
        let old = registry.capture("user");
        let event = |validity| Event::TalkUsage {
            guild_id: "guild".into(),
            user_id: "user".into(),
            model: "en_US-amy-medium".into(),
            engine: UserEngine::Piper,
            validity,
        };
        let mut pending = Pending::default();
        pending.push(event(old.clone()));
        pending.push(event(old.clone()));
        store.lock().unwrap().erase_user_data("user").unwrap();
        assert!(!old.load(Ordering::Acquire));
        pending.push(event(registry.capture("user")));
        pending.push(event(old));
        pending.flush(&store);
        assert_eq!(count(&store, "user"), 1);
    }

    #[test]
    fn metrics_from_different_days_keep_separate_buckets() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let writer = SpeechTelemetryWriter::new(store.clone());
        writer.record_metric(
            "2026-07-31",
            OperationalMetric::SynthSuccess,
            OperationalProvider::Piper,
            1,
        );
        writer.record_metric(
            "2026-08-01",
            OperationalMetric::SynthSuccess,
            OperationalProvider::Piper,
            1,
        );
        writer.flush();

        let store = store.lock().expect("store");
        assert_eq!(
            store
                .list_daily_operational_metrics(Some("2026-07-31"))
                .expect("first day")
                .iter()
                .map(|row| row.value)
                .sum::<i64>(),
            1
        );
        assert_eq!(
            store
                .list_daily_operational_metrics(Some("2026-08-01"))
                .expect("second day")
                .iter()
                .map(|row| row.value)
                .sum::<i64>(),
            1
        );
    }
}
