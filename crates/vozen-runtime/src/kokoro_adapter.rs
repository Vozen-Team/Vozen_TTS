//! Discord-facing adapter for the opt-in Rust Kokoro sidecar.

use std::{path::PathBuf, sync::Arc, time::Instant};

use async_trait::async_trait;
use vozen_core::{RuntimeMetrics, SynthRequest, SynthesisEngine};
use vozen_discord::{CommandSpeechSynthesizer, CommandSynthesisError};
use vozen_tts::{KokoroCommand, KokoroEngine, KokoroOptions};

#[derive(Clone)]
pub struct KokoroCommandSynthesizer {
    engine: Arc<KokoroEngine>,
    metrics: Arc<RuntimeMetrics>,
}

impl KokoroCommandSynthesizer {
    pub fn production(
        command: KokoroCommand,
        cache_dir: impl Into<PathBuf>,
        allowed_languages: Option<Vec<String>>,
        metrics: Arc<RuntimeMetrics>,
    ) -> Result<Self, CommandSynthesisError> {
        let mut options = KokoroOptions::production(command, cache_dir);
        options.allowed_languages = allowed_languages;
        let engine = KokoroEngine::new(options).map_err(|_| CommandSynthesisError)?;
        Ok(Self {
            engine: Arc::new(engine),
            metrics,
        })
    }
}

#[async_trait]
impl CommandSpeechSynthesizer for KokoroCommandSynthesizer {
    fn provider(&self) -> vozen_store::OperationalProvider {
        vozen_store::OperationalProvider::Kokoro
    }

    async fn synthesize(&self, request: &SynthRequest) -> Result<PathBuf, CommandSynthesisError> {
        if !matches!(request.engine, SynthesisEngine::Kokoro) {
            return Err(CommandSynthesisError);
        }
        let started = Instant::now();
        let result = self.engine.synth(request).await;
        self.metrics
            .record_synth_latency_ms(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        if let Err(error) = &result {
            self.metrics.record_synth_error();
            eprintln!("[tts:kokoro] synthesis failed: {error:?}");
        }
        result.map_err(|_| CommandSynthesisError)
    }
}
