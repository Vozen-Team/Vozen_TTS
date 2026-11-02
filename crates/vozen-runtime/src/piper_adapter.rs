//! Runtime composition of the local Piper engine and the Discord command service.
//!
//! `vozen-tts` deliberately knows nothing about Discord. This one-way adapter maps only a
//! synthesis failure into the command service's content-free error, so neither a filesystem path
//! nor a Piper process diagnostic can reach Discord through a user request. A content-free local
//! diagnostic remains in the process log so an operator can distinguish synthesis from playback.

use std::{path::PathBuf, sync::Arc, time::Instant};

use async_trait::async_trait;
use vozen_core::{RuntimeMetrics, SynthRequest, SynthesisEngine};
use vozen_discord::{CommandSpeechSynthesizer, CommandSynthesisError};
use vozen_tts::CommandPiperRunner;
use vozen_tts::{PiperEngine, PiperRunner};

#[derive(Clone)]
pub struct PiperCommandSynthesizer<R = vozen_tts::CommandPiperRunner> {
    engine: Arc<PiperEngine<R>>,
    metrics: Arc<RuntimeMetrics>,
}

impl<R> PiperCommandSynthesizer<R> {
    #[must_use]
    #[allow(dead_code)]
    pub fn new(engine: Arc<PiperEngine<R>>) -> Self {
        Self {
            engine,
            metrics: Arc::new(RuntimeMetrics::default()),
        }
    }

    #[must_use]
    pub fn new_with_metrics(engine: Arc<PiperEngine<R>>, metrics: Arc<RuntimeMetrics>) -> Self {
        Self { engine, metrics }
    }
}

impl PiperCommandSynthesizer<CommandPiperRunner> {
    #[must_use]
    pub fn production(
        executable: impl Into<PathBuf>,
        models_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        concurrency: usize,
    ) -> Self {
        Self::production_with_metrics(
            executable,
            models_dir,
            cache_dir,
            concurrency,
            Arc::new(RuntimeMetrics::default()),
        )
    }

    #[must_use]
    pub fn production_with_metrics(
        executable: impl Into<PathBuf>,
        models_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        concurrency: usize,
        metrics: Arc<RuntimeMetrics>,
    ) -> Self {
        Self::new_with_metrics(
            Arc::new(PiperEngine::production_with_metrics(
                executable,
                models_dir,
                cache_dir,
                concurrency,
                metrics.clone(),
            )),
            metrics,
        )
    }
}

#[async_trait]
impl<R> CommandSpeechSynthesizer for PiperCommandSynthesizer<R>
where
    R: PiperRunner + 'static,
{
    fn provider(&self) -> vozen_store::OperationalProvider {
        vozen_store::OperationalProvider::Piper
    }

    async fn synthesize(&self, request: &SynthRequest) -> Result<PathBuf, CommandSynthesisError> {
        // A Rust canary that only installed Piper must never acknowledge a paid/provider-specific
        // preference and then synthesize it with Piper. The future router owns these routes.
        if !matches!(
            request.engine,
            SynthesisEngine::Default | SynthesisEngine::Piper
        ) {
            return Err(CommandSynthesisError);
        }
        let started = Instant::now();
        let result = self.engine.synth(request).await;
        self.metrics
            .record_synth_latency_ms(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        if let Err(error) = &result {
            self.metrics.record_synth_error();
            eprintln!("[tts:piper] synthesis failed: {error}");
        }
        result.map_err(|_| CommandSynthesisError)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use async_trait::async_trait;
    use vozen_tts::TtsError;

    use super::*;

    #[derive(Default)]
    struct FailingRunner {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl PiperRunner for FailingRunner {
        async fn run(
            &self,
            _model: &Path,
            _output: &Path,
            _text: &str,
            _speed: f64,
        ) -> Result<(), TtsError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(TtsError::ProcessFailed)
        }
    }

    fn request() -> SynthRequest {
        SynthRequest {
            text: "private text".into(),
            model: "en_US-amy-medium".into(),
            asset_path: None,
            speed: 1.0,
            engine: SynthesisEngine::Default,
            gcloud_budget: None,
            segments: None,
            single_voice: None,
            emphasis_source: None,
            lead_silence_ms: 0,
        }
    }

    #[tokio::test]
    async fn piper_failures_are_mapped_to_a_content_free_command_error() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "vozen-piper-adapter-{}-{nonce}",
            std::process::id()
        ));
        let models = root.join("models");
        std::fs::create_dir_all(&models).expect("models directory");
        std::fs::write(models.join("en_US-amy-medium.onnx"), b"placeholder")
            .expect("model placeholder");
        let runner = Arc::new(FailingRunner::default());
        let adapter = PiperCommandSynthesizer::new(Arc::new(PiperEngine::new(
            runner.clone(),
            &models,
            root.join("cache"),
            1,
        )));

        assert!(adapter.synthesize(&request()).await.is_err());
        assert_eq!(runner.calls.load(Ordering::Relaxed), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unsupported_paid_engines_never_fall_through_to_piper() {
        let root = std::env::temp_dir().join(format!(
            "vozen-piper-adapter-unsupported-{}",
            std::process::id()
        ));
        let models = root.join("models");
        std::fs::create_dir_all(&models).expect("models directory");
        std::fs::write(models.join("en_US-amy-medium.onnx"), b"placeholder")
            .expect("model placeholder");
        let runner = Arc::new(FailingRunner::default());
        let adapter = PiperCommandSynthesizer::new(Arc::new(PiperEngine::new(
            runner.clone(),
            &models,
            root.join("cache"),
            1,
        )));
        let mut paid_request = request();
        paid_request.engine = SynthesisEngine::Kokoro;

        assert!(adapter.synthesize(&paid_request).await.is_err());
        assert_eq!(runner.calls.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(root);
    }
}
