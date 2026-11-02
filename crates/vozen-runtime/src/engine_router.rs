//! Per-user provider routing for the Rust TTS migration.
//!
//! The legacy `google` value is a configured default route. Provider-specific selections use
//! their provider when installed and fall back to that default on absence or failure, matching
//! Node's no-silence policy. Provider implementations stay behind their explicit adapters and
//! never receive an unvalidated user-controlled command or model path.

use std::sync::Arc;

use async_trait::async_trait;
use vozen_core::{RuntimeMetrics, SynthRequest, SynthesisEngine};
use vozen_discord::{CommandSpeechSynthesizer, CommandSynthesisError};

use crate::piper_adapter::PiperCommandSynthesizer;

#[derive(Clone)]
pub struct PerUserCommandSynthesizer {
    default: Arc<dyn CommandSpeechSynthesizer>,
    piper: Arc<dyn CommandSpeechSynthesizer>,
    kokoro: Option<Arc<dyn CommandSpeechSynthesizer>>,
    gcloud: Option<Arc<dyn CommandSpeechSynthesizer>>,
    neural: Option<Arc<dyn CommandSpeechSynthesizer>>,
    metrics: Arc<RuntimeMetrics>,
}

impl PerUserCommandSynthesizer {
    #[must_use]
    pub fn piper_only(piper: PiperCommandSynthesizer) -> Self {
        let piper: Arc<dyn CommandSpeechSynthesizer> = Arc::new(piper);
        Self::new(piper.clone(), piper, None, None, None)
    }

    #[must_use]
    pub fn new(
        default: Arc<dyn CommandSpeechSynthesizer>,
        piper: Arc<dyn CommandSpeechSynthesizer>,
        kokoro: Option<Arc<dyn CommandSpeechSynthesizer>>,
        gcloud: Option<Arc<dyn CommandSpeechSynthesizer>>,
        neural: Option<Arc<dyn CommandSpeechSynthesizer>>,
    ) -> Self {
        Self {
            default,
            piper,
            kokoro,
            gcloud,
            neural,
            metrics: Arc::new(RuntimeMetrics::default()),
        }
    }

    #[must_use]
    #[allow(dead_code)]
    pub fn new_with_metrics(
        default: Arc<dyn CommandSpeechSynthesizer>,
        piper: Arc<dyn CommandSpeechSynthesizer>,
        kokoro: Option<Arc<dyn CommandSpeechSynthesizer>>,
        gcloud: Option<Arc<dyn CommandSpeechSynthesizer>>,
        neural: Option<Arc<dyn CommandSpeechSynthesizer>>,
        metrics: Arc<RuntimeMetrics>,
    ) -> Self {
        let mut router = Self::new(default, piper, kokoro, gcloud, neural);
        router.metrics = metrics;
        router
    }

    async fn default_synthesis(
        &self,
        request: &SynthRequest,
    ) -> Result<
        (std::path::PathBuf, vozen_store::OperationalProvider),
        (CommandSynthesisError, vozen_store::OperationalProvider),
    > {
        self.default
            .synthesize_measured(&with_engine(request, SynthesisEngine::Default))
            .await
    }

    async fn preferred_or_default(
        &self,
        provider: Option<&Arc<dyn CommandSpeechSynthesizer>>,
        request: &SynthRequest,
        engine: SynthesisEngine,
    ) -> Result<
        (std::path::PathBuf, vozen_store::OperationalProvider),
        (CommandSynthesisError, vozen_store::OperationalProvider),
    > {
        let Some(provider) = provider else {
            self.metrics.record_synth_fallback();
            eprintln!("[tts:router] {engine:?} provider is not configured; using Google fallback");
            return self.default_synthesis(request).await;
        };
        match provider
            .synthesize_measured(&with_engine(request, engine))
            .await
        {
            Ok(path) => Ok(path),
            Err(_) => {
                self.metrics.record_synth_fallback();
                eprintln!("[tts:router] {engine:?} provider failed; using Google fallback");
                self.default_synthesis(request).await
            }
        }
    }
}

#[async_trait]
impl CommandSpeechSynthesizer for PerUserCommandSynthesizer {
    async fn synthesize(
        &self,
        request: &SynthRequest,
    ) -> Result<std::path::PathBuf, CommandSynthesisError> {
        self.synthesize_measured(request)
            .await
            .map(|(path, _)| path)
            .map_err(|(error, _)| error)
    }

    async fn synthesize_measured(
        &self,
        request: &SynthRequest,
    ) -> Result<
        (std::path::PathBuf, vozen_store::OperationalProvider),
        (CommandSynthesisError, vozen_store::OperationalProvider),
    > {
        match request.engine {
            SynthesisEngine::Default => self.default_synthesis(request).await,
            SynthesisEngine::Piper if Arc::ptr_eq(&self.default, &self.piper) => {
                self.piper
                    .synthesize_measured(&with_engine(request, SynthesisEngine::Piper))
                    .await
            }
            SynthesisEngine::Piper => {
                self.preferred_or_default(Some(&self.piper), request, SynthesisEngine::Piper)
                    .await
            }
            SynthesisEngine::Kokoro => {
                self.preferred_or_default(self.kokoro.as_ref(), request, SynthesisEngine::Kokoro)
                    .await
            }
            SynthesisEngine::Gcloud => {
                self.preferred_or_default(self.gcloud.as_ref(), request, SynthesisEngine::Gcloud)
                    .await
            }
            SynthesisEngine::Neural => {
                self.preferred_or_default(self.neural.as_ref(), request, SynthesisEngine::Neural)
                    .await
            }
        }
    }
}

fn with_engine(request: &SynthRequest, engine: SynthesisEngine) -> SynthRequest {
    let mut routed = request.clone();
    routed.engine = engine;
    routed
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    #[derive(Default)]
    struct FakeSynthesizer {
        received: Mutex<Vec<SynthesisEngine>>,
        fails: AtomicBool,
        provider: Option<vozen_store::OperationalProvider>,
    }

    #[async_trait]
    impl CommandSpeechSynthesizer for FakeSynthesizer {
        fn provider(&self) -> vozen_store::OperationalProvider {
            self.provider
                .unwrap_or(vozen_store::OperationalProvider::Internal)
        }
        async fn synthesize(
            &self,
            request: &SynthRequest,
        ) -> Result<std::path::PathBuf, CommandSynthesisError> {
            self.received.lock().expect("received").push(request.engine);
            if self.fails.load(Ordering::Relaxed) {
                Err(CommandSynthesisError)
            } else {
                Ok("audio.wav".into())
            }
        }
    }

    fn request(engine: SynthesisEngine) -> SynthRequest {
        SynthRequest {
            text: "private text".into(),
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

    #[tokio::test]
    async fn unavailable_paid_engine_uses_the_configured_default() {
        let default = Arc::new(FakeSynthesizer::default());
        let router = PerUserCommandSynthesizer::new(
            default.clone(),
            Arc::new(FakeSynthesizer::default()),
            None,
            None,
            None,
        );

        router
            .synthesize(&request(SynthesisEngine::Kokoro))
            .await
            .expect("fallback");
        assert_eq!(
            *default.received.lock().expect("received"),
            [SynthesisEngine::Default]
        );
    }

    #[tokio::test]
    async fn measured_provider_follows_fallback_and_concurrent_requests() {
        use vozen_store::OperationalProvider as Provider;
        let default = Arc::new(FakeSynthesizer {
            provider: Some(Provider::Gtts),
            ..Default::default()
        });
        let piper = Arc::new(FakeSynthesizer {
            provider: Some(Provider::Piper),
            ..Default::default()
        });
        let failing = Arc::new(FakeSynthesizer {
            provider: Some(Provider::Kokoro),
            ..Default::default()
        });
        failing.fails.store(true, Ordering::Relaxed);
        let router = PerUserCommandSynthesizer::new(default, piper, Some(failing), None, None);
        let kokoro = request(SynthesisEngine::Kokoro);
        let piper = request(SynthesisEngine::Piper);
        let (fallback, direct) = tokio::join!(
            router.synthesize_measured(&kokoro),
            router.synthesize_measured(&piper)
        );
        assert_eq!(fallback.unwrap().1, Provider::Gtts);
        assert_eq!(direct.unwrap().1, Provider::Piper);
    }

    #[tokio::test]
    async fn failed_provider_falls_back_without_relabeling_the_provider_request() {
        let default = Arc::new(FakeSynthesizer::default());
        let kokoro = Arc::new(FakeSynthesizer::default());
        kokoro.fails.store(true, Ordering::Relaxed);
        let metrics = Arc::new(RuntimeMetrics::default());
        let router = PerUserCommandSynthesizer::new_with_metrics(
            default.clone(),
            Arc::new(FakeSynthesizer::default()),
            Some(kokoro.clone()),
            None,
            None,
            metrics.clone(),
        );

        router
            .synthesize(&request(SynthesisEngine::Kokoro))
            .await
            .expect("fallback");
        assert_eq!(
            *kokoro.received.lock().expect("received"),
            [SynthesisEngine::Kokoro]
        );
        assert_eq!(
            *default.received.lock().expect("received"),
            [SynthesisEngine::Default]
        );
        assert_eq!(metrics.snapshot().synth_fallbacks, 1);
    }

    #[tokio::test]
    async fn neural_provider_receives_the_neural_route_and_is_available_to_user_preferences() {
        let default = Arc::new(FakeSynthesizer::default());
        let neural = Arc::new(FakeSynthesizer::default());
        let router = PerUserCommandSynthesizer::new(
            default.clone(),
            Arc::new(FakeSynthesizer::default()),
            None,
            None,
            Some(neural.clone()),
        );

        router
            .synthesize(&request(SynthesisEngine::Neural))
            .await
            .expect("neural provider");
        assert_eq!(
            *neural.received.lock().expect("received"),
            [SynthesisEngine::Neural]
        );
        assert!(default.received.lock().expect("received").is_empty());
    }
}
