//! Discord-facing adapter for the opt-in Rust gTTS engine.

use std::{path::PathBuf, sync::Arc, time::Instant};

use async_trait::async_trait;
use vozen_core::{RuntimeMetrics, SynthRequest, SynthesisEngine};
use vozen_discord::{CommandSpeechSynthesizer, CommandSynthesisError};
use vozen_tts::{GttsEngine, GttsOptions};

#[derive(Clone)]
pub struct GttsCommandSynthesizer {
    engine: Arc<GttsEngine>,
    metrics: Arc<RuntimeMetrics>,
}

pub struct GttsWithPiperFallback {
    primary: Arc<dyn CommandSpeechSynthesizer>,
    fallback: Arc<dyn CommandSpeechSynthesizer>,
    metrics: Arc<RuntimeMetrics>,
}

impl GttsWithPiperFallback {
    #[must_use]
    pub fn new_with_metrics(
        primary: Arc<dyn CommandSpeechSynthesizer>,
        fallback: Arc<dyn CommandSpeechSynthesizer>,
        metrics: Arc<RuntimeMetrics>,
    ) -> Self {
        Self {
            primary,
            fallback,
            metrics,
        }
    }
}

impl GttsCommandSynthesizer {
    pub fn production(
        ffmpeg: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        metrics: Arc<RuntimeMetrics>,
    ) -> Result<Self, CommandSynthesisError> {
        let engine = GttsEngine::new(GttsOptions::production(ffmpeg, cache_dir))
            .map_err(|_| CommandSynthesisError)?;
        Ok(Self {
            engine: Arc::new(engine),
            metrics,
        })
    }
}

#[async_trait]
impl CommandSpeechSynthesizer for GttsCommandSynthesizer {
    fn provider(&self) -> vozen_store::OperationalProvider {
        vozen_store::OperationalProvider::Gtts
    }

    async fn synthesize(&self, request: &SynthRequest) -> Result<PathBuf, CommandSynthesisError> {
        if !matches!(request.engine, SynthesisEngine::Default) {
            return Err(CommandSynthesisError);
        }
        let started = Instant::now();
        let result = self.engine.synth(request).await;
        self.metrics
            .record_synth_latency_ms(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        if let Err(error) = &result {
            self.metrics.record_synth_error();
            // Keep provider diagnostics in the container log while returning the same
            // content-free error contract as the other adapters.
            eprintln!("[tts:gtts] synthesis failed: {error}");
        }
        result.map_err(|_| CommandSynthesisError)
    }
}

#[async_trait]
impl CommandSpeechSynthesizer for GttsWithPiperFallback {
    async fn synthesize(&self, request: &SynthRequest) -> Result<PathBuf, CommandSynthesisError> {
        self.synthesize_measured(request)
            .await
            .map(|(path, _)| path)
            .map_err(|(error, _)| error)
    }

    async fn synthesize_measured(
        &self,
        request: &SynthRequest,
    ) -> Result<
        (PathBuf, vozen_store::OperationalProvider),
        (CommandSynthesisError, vozen_store::OperationalProvider),
    > {
        match self.primary.synthesize_measured(request).await {
            Ok(path) => Ok(path),
            Err(_) => {
                self.metrics.record_synth_fallback();
                eprintln!("[tts:gtts] using Piper fallback");
                let mut fallback_request = request.clone();
                fallback_request.engine = SynthesisEngine::Piper;
                self.fallback.synthesize_measured(&fallback_request).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(engine: SynthesisEngine) -> SynthRequest {
        SynthRequest {
            text: "hello".into(),
            model: "en_US-amy-medium".into(),
            asset_path: None,
            speed: 1.0,
            engine,
            gcloud_budget: None,
            segments: None,
            single_voice: None,
            emphasis_source: None,
            lead_silence_ms: 0,
        }
    }

    #[test]
    fn provider_adapter_is_only_the_default_route() {
        assert!(matches!(
            request(SynthesisEngine::Default).engine,
            SynthesisEngine::Default
        ));
        assert!(matches!(
            request(SynthesisEngine::Piper).engine,
            SynthesisEngine::Piper
        ));
    }

    #[tokio::test]
    async fn failed_default_provider_uses_piper_fallback() {
        struct Failing;
        #[async_trait]
        impl CommandSpeechSynthesizer for Failing {
            async fn synthesize(
                &self,
                _request: &SynthRequest,
            ) -> Result<PathBuf, CommandSynthesisError> {
                Err(CommandSynthesisError)
            }
        }

        struct Successful;
        #[async_trait]
        impl CommandSpeechSynthesizer for Successful {
            fn provider(&self) -> vozen_store::OperationalProvider {
                vozen_store::OperationalProvider::Piper
            }
            async fn synthesize(
                &self,
                request: &SynthRequest,
            ) -> Result<PathBuf, CommandSynthesisError> {
                assert_eq!(request.engine, SynthesisEngine::Piper);
                Ok("piper.wav".into())
            }
        }

        let metrics = Arc::new(RuntimeMetrics::default());
        let fallback = GttsWithPiperFallback::new_with_metrics(
            Arc::new(Failing),
            Arc::new(Successful),
            metrics.clone(),
        );
        let output = fallback
            .synthesize_measured(&request(SynthesisEngine::Default))
            .await
            .expect("fallback synthesis");
        assert_eq!(
            output,
            (
                PathBuf::from("piper.wav"),
                vozen_store::OperationalProvider::Piper
            )
        );
        assert_eq!(metrics.snapshot().synth_fallbacks, 1);
    }
}
