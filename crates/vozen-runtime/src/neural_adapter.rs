//! Discord-facing adapter for the legacy OpenAI `tts-1` default provider.

use std::{path::PathBuf, sync::Arc, time::Instant};

use async_trait::async_trait;
use vozen_core::{RuntimeMetrics, SynthRequest, SynthesisEngine};
use vozen_discord::{CommandSpeechSynthesizer, CommandSynthesisError};
use vozen_tts::{NeuralEngine, NeuralOptions};

pub struct NeuralCommandSynthesizer {
    engine: Arc<NeuralEngine>,
    metrics: Arc<RuntimeMetrics>,
}

impl NeuralCommandSynthesizer {
    pub fn production(
        api_key: String,
        cache_dir: PathBuf,
        metrics: Arc<RuntimeMetrics>,
    ) -> Result<Self, CommandSynthesisError> {
        let engine = NeuralEngine::new(NeuralOptions::production(api_key, cache_dir))
            .map_err(|_| CommandSynthesisError)?;
        Ok(Self {
            engine: Arc::new(engine),
            metrics,
        })
    }
}

#[async_trait]
impl CommandSpeechSynthesizer for NeuralCommandSynthesizer {
    fn provider(&self) -> vozen_store::OperationalProvider {
        vozen_store::OperationalProvider::Neural
    }

    async fn synthesize(&self, request: &SynthRequest) -> Result<PathBuf, CommandSynthesisError> {
        // Default is used by the per-user router for whichever provider the operator selected;
        // Neural is accepted as an explicit internal marker for tests and future routing.
        if !matches!(
            request.engine,
            SynthesisEngine::Default | SynthesisEngine::Neural
        ) {
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
