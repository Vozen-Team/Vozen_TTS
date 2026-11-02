//! Runtime adapter for the official Google Cloud TTS provider.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use async_trait::async_trait;
use vozen_core::{GcloudBudget, GcloudBudgetScope, RuntimeMetrics, SynthRequest, SynthesisEngine};
use vozen_discord::{CommandSpeechSynthesizer, CommandSynthesisError};
use vozen_store::{GcloudUsageScope, SqliteStore, day_key_utc, month_key_utc};
use vozen_tts::{
    GcloudEngine, GcloudLedgerError, GcloudLimits, GcloudOptions, GcloudUsageLedger,
    monthly_limit_for,
};

struct SqliteGcloudLedger {
    store: Arc<Mutex<SqliteStore>>,
}

impl GcloudUsageLedger for SqliteGcloudLedger {
    fn reserve(
        &self,
        budget: &GcloudBudget,
        now_ms: i64,
        limits: GcloudLimits,
        chars: i64,
    ) -> Result<bool, GcloudLedgerError> {
        let scope = scope(budget.scope);
        let month = month_key_utc(now_ms);
        let day = day_key_utc(now_ms);
        let store = self.store.lock().map_err(|_| GcloudLedgerError)?;
        store
            .reserve_gcloud_chars(
                scope,
                &budget.key,
                &month,
                monthly_limit_for(budget, limits),
                &day,
                limits.daily_budget,
                chars,
            )
            .map_err(|_| GcloudLedgerError)
    }

    fn refund(
        &self,
        budget: &GcloudBudget,
        now_ms: i64,
        limits: GcloudLimits,
        chars: i64,
    ) -> Result<(), GcloudLedgerError> {
        let scope = scope(budget.scope);
        let month = month_key_utc(now_ms);
        let day = day_key_utc(now_ms);
        let store = self.store.lock().map_err(|_| GcloudLedgerError)?;
        store
            .refund_gcloud_chars(scope, &budget.key, &month, &day, limits.daily_budget, chars)
            .map_err(|_| GcloudLedgerError)
    }
}

fn scope(value: GcloudBudgetScope) -> GcloudUsageScope {
    match value {
        GcloudBudgetScope::User => GcloudUsageScope::User,
        GcloudBudgetScope::Pass => GcloudUsageScope::Pass,
        GcloudBudgetScope::Guild => GcloudUsageScope::Guild,
    }
}

pub struct GcloudCommandSynthesizer {
    engine: Arc<GcloudEngine>,
    metrics: Arc<RuntimeMetrics>,
}

impl GcloudCommandSynthesizer {
    pub fn production(
        api_key: String,
        cache_dir: PathBuf,
        store: Arc<Mutex<SqliteStore>>,
        limits: GcloudLimits,
        metrics: Arc<RuntimeMetrics>,
    ) -> Result<Self, CommandSynthesisError> {
        let ledger: Arc<dyn GcloudUsageLedger> = Arc::new(SqliteGcloudLedger { store });
        let engine = GcloudEngine::new(GcloudOptions {
            api_key,
            cache_dir,
            concurrency: 3,
            request_timeout: std::time::Duration::from_secs(15),
            limits: Some(limits),
            ledger: Some(ledger),
        })
        .map_err(|_| CommandSynthesisError)?;
        Ok(Self {
            engine: Arc::new(engine),
            metrics,
        })
    }
}

#[async_trait]
impl CommandSpeechSynthesizer for GcloudCommandSynthesizer {
    fn provider(&self) -> vozen_store::OperationalProvider {
        vozen_store::OperationalProvider::Gcloud
    }

    async fn synthesize(&self, request: &SynthRequest) -> Result<PathBuf, CommandSynthesisError> {
        if !matches!(request.engine, SynthesisEngine::Gcloud) {
            return Err(CommandSynthesisError);
        }
        let started = Instant::now();
        let result = self.engine.synth(request).await;
        self.metrics
            .record_synth_latency_ms(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        if result.is_err() {
            self.metrics.record_synth_error();
        }
        result.map_err(|_| CommandSynthesisError)
    }
}
