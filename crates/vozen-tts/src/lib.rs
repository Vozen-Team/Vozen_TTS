#![forbid(unsafe_code)]

//! Local Piper synthesis boundary for the Rust runtime.
//!
//! It performs no network I/O. Cache misses are bounded by a shared semaphore and Piper is
//! spawned with argument vectors, never a shell, so a Discord-provided model cannot alter a
//! command line or escape the configured model directory.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{io::AsyncWriteExt, process::Command, sync::Semaphore, time::timeout};
use uuid::Uuid;
use vozen_core::{RuntimeMetrics, SynthRequest};

mod cache_retention;
mod gcloud;
mod gtts;
mod kokoro;
mod neural;
mod wav_concat;

use cache_retention::non_empty_file;
pub use cache_retention::{
    AUDIO_CACHE_MAX_AGE, AUDIO_CACHE_SWEEP_INTERVAL, purge_expired_audio_cache,
};
pub use gcloud::{
    GcloudEngine, GcloudLedgerError, GcloudLimits, GcloudOptions, GcloudUsageLedger,
    bcp47_of_model, monthly_limit_for,
};
pub use gtts::{GttsEngine, GttsOptions, chunk_text, gtts_lang_of_model, lower_all_caps_runs};
pub use kokoro::{
    KokoroCommand, KokoroEngine, KokoroOptions, KokoroVoice, kokoro_voice_for_model,
    language_key as kokoro_language_key, parse_command as parse_kokoro_command,
};
pub use neural::{NeuralEngine, NeuralOptions, openai_voice_for_model};
pub use wav_concat::{
    WavError, WavFormat, concat_wavs, parse_wav, prepend_silence_wav, silence_wav,
};

pub const PIPER_TIMEOUT: Duration = Duration::from_secs(15);
/// Matches the Node AudioCache default. WAVs are regenerable and must not grow without bound.
pub const DEFAULT_MAX_CACHE_FILES: usize = 500;

#[derive(Debug, Error)]
pub enum TtsError {
    #[error("invalid Piper model name")]
    InvalidModel,
    #[error("Piper model was not found")]
    ModelMissing,
    #[error("Piper synthesis timed out")]
    Timeout,
    #[error("Piper process failed")]
    ProcessFailed,
    #[error("Piper did not produce a non-empty WAV")]
    EmptyOutput,
    #[error("Piper WAV composition failed: {0}")]
    Wav(#[from] WavError),
    #[error("gTTS request failed")]
    GttsRequest,
    #[error("gTTS request timed out")]
    GttsTimeout,
    #[error("gTTS returned an invalid response")]
    GttsResponse,
    #[error("gTTS audio conversion failed")]
    GttsConversion,
    #[error("Google Cloud TTS is not configured")]
    GcloudConfiguration,
    #[error("Google Cloud TTS request failed")]
    GcloudRequest,
    #[error("Google Cloud TTS request timed out")]
    GcloudTimeout,
    #[error("Google Cloud TTS returned an invalid response")]
    GcloudResponse,
    #[error("Google Cloud TTS budget is missing")]
    GcloudBudgetMissing,
    #[error("Google Cloud TTS budget denied the request")]
    GcloudBudgetDenied,
    #[error("OpenAI TTS is not configured")]
    NeuralConfiguration,
    #[error("OpenAI TTS request failed")]
    NeuralRequest,
    #[error("OpenAI TTS request timed out")]
    NeuralTimeout,
    #[error("OpenAI TTS returned an invalid response")]
    NeuralResponse,
    #[error("Kokoro sidecar is not configured")]
    KokoroConfiguration,
    #[error("Kokoro sidecar process failed")]
    KokoroProcess,
    #[error("Kokoro sidecar timed out")]
    KokoroTimeout,
    #[error("Kokoro sidecar returned an invalid response")]
    KokoroResponse,
    #[error("Kokoro does not support this language")]
    KokoroUnsupportedLanguage,
    #[error("TTS I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Returns false for every path-like or empty identifier before a filesystem path is built.
pub fn is_safe_model_name(model: &str) -> bool {
    !model.is_empty()
        && model != "."
        && model != ".."
        && !model.contains('/')
        && !model.contains('\\')
        && !model.contains('\0')
}

#[async_trait]
pub trait PiperRunner: Send + Sync {
    async fn run(
        &self,
        model: &Path,
        output: &Path,
        text: &str,
        speed: f64,
    ) -> Result<(), TtsError>;
}

/// Production runner. `Command` passes arguments directly to Piper; no user text becomes an arg.
pub struct CommandPiperRunner {
    executable: PathBuf,
    timeout: Duration,
}

impl CommandPiperRunner {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            timeout: PIPER_TIMEOUT,
        }
    }

    pub fn with_timeout(executable: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            executable: executable.into(),
            timeout,
        }
    }
}

#[async_trait]
impl PiperRunner for CommandPiperRunner {
    async fn run(
        &self,
        model: &Path,
        output: &Path,
        text: &str,
        speed: f64,
    ) -> Result<(), TtsError> {
        let mut child = Command::new(&self.executable)
            .arg("--model")
            .arg(model)
            .arg("--output_file")
            .arg(output)
            .arg("--length_scale")
            .arg(speed_to_length_scale(speed).to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(TtsError::Io)?;
        let Some(mut stdin) = child.stdin.take() else {
            return Err(TtsError::ProcessFailed);
        };
        stdin.write_all(text.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        drop(stdin);
        match timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(_)) | Ok(Err(_)) => Err(TtsError::ProcessFailed),
            Err(_) => {
                let _ = child.kill().await;
                Err(TtsError::Timeout)
            }
        }
    }
}

/// A local WAV cache backed by immutable SHA-256 file names.
pub struct PiperEngine<R = CommandPiperRunner> {
    runner: Arc<R>,
    models_dir: PathBuf,
    cache_dir: PathBuf,
    permits: Arc<Semaphore>,
    segment_concurrency: usize,
    metrics: Arc<RuntimeMetrics>,
}

impl PiperEngine<CommandPiperRunner> {
    pub fn production(
        executable: impl Into<PathBuf>,
        models_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        concurrency: usize,
    ) -> Self {
        Self::new_with_metrics(
            Arc::new(CommandPiperRunner::new(executable)),
            models_dir,
            cache_dir,
            concurrency,
            Arc::new(RuntimeMetrics::default()),
        )
    }

    pub fn production_with_metrics(
        executable: impl Into<PathBuf>,
        models_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        concurrency: usize,
        metrics: Arc<RuntimeMetrics>,
    ) -> Self {
        Self::new_with_metrics(
            Arc::new(CommandPiperRunner::new(executable)),
            models_dir,
            cache_dir,
            concurrency,
            metrics,
        )
    }
}

impl<R: PiperRunner> PiperEngine<R> {
    pub fn new(
        runner: Arc<R>,
        models_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        concurrency: usize,
    ) -> Self {
        Self::new_with_metrics(
            runner,
            models_dir,
            cache_dir,
            concurrency,
            Arc::new(RuntimeMetrics::default()),
        )
    }

    pub fn new_with_metrics(
        runner: Arc<R>,
        models_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        concurrency: usize,
        metrics: Arc<RuntimeMetrics>,
    ) -> Self {
        let segment_concurrency = concurrency.max(1);
        Self {
            runner,
            models_dir: models_dir.into(),
            cache_dir: cache_dir.into(),
            permits: Arc::new(Semaphore::new(segment_concurrency)),
            segment_concurrency,
            metrics,
        }
    }

    /// Returns the cached immutable WAV path, synthesising at most `concurrency` misses.
    pub async fn synth(&self, request: &SynthRequest) -> Result<PathBuf, TtsError> {
        if let Some(asset_path) = request.asset_path.as_deref() {
            return validate_asset_wav(asset_path).await;
        }
        let Some(segments) = request
            .segments
            .as_deref()
            .filter(|segments| !segments.is_empty())
        else {
            return self.synth_single(request).await;
        };
        if segments.len() == 1 {
            let mut single = single_segment_request(request, &segments[0]);
            single.lead_silence_ms = request.lead_silence_ms;
            return self.synth_single(&single).await;
        }

        // Explicit segments arrive from the policy layer with their own voice selections. Each
        // part is independently cached, then the canonical PCM WAVs are composed with a small
        // anti-click gap. A bad/missing segment falls back to the request's resolved base voice,
        // matching the legacy MultiSegmentEngine's never-drop-content rule.
        match self.synth_segments(request, segments).await {
            Ok(path) => Ok(path),
            Err(_) => {
                // Cache the base-voice fallback under its own single-voice key. Writing it under
                // the multi-voice key would turn one transient segment failure into a permanent
                // wrong-voice cache hit.
                let mut base = request.clone();
                base.segments = None;
                self.synth_single(&base).await
            }
        }
    }

    async fn synth_segments(
        &self,
        request: &SynthRequest,
        segments: &[vozen_core::SpeechSegment],
    ) -> Result<PathBuf, TtsError> {
        let destination = self.cache_dir.join(format!("{}.wav", cache_key(request)));
        if non_empty_file(&destination).await? {
            self.metrics.record_cache_hit();
            return Ok(destination);
        }
        // Keep the work queue bounded by the same provider semaphore used by single requests.
        // Results are tagged with their original index so completion order cannot affect audio
        // order when a later segment finishes first.
        let indexed_wavs = stream::iter(segments.iter().cloned().enumerate().map(
            |(index, segment)| {
                let request = request.clone();
                async move {
                    let path = self
                        .synth_single(&single_segment_request(&request, &segment))
                        .await?;
                    let wav = tokio::fs::read(path).await?;
                    Ok::<_, TtsError>((index, wav))
                }
            },
        ))
        .buffer_unordered(self.segment_concurrency)
        .collect::<Vec<_>>()
        .await;
        let mut indexed_wavs = indexed_wavs
            .into_iter()
            .collect::<Result<Vec<_>, TtsError>>()?;
        indexed_wavs.sort_unstable_by_key(|(index, _)| *index);
        let wavs = indexed_wavs
            .into_iter()
            .map(|(_, wav)| wav)
            .collect::<Vec<_>>();
        let combined =
            concat_wavs(&wavs, wav_concat::DEFAULT_SEGMENT_SILENCE_MS).map_err(TtsError::Wav)?;
        let combined = wav_concat::prepend_silence_wav(&combined, request.lead_silence_ms)
            .map_err(TtsError::Wav)?;
        self.write_cached_wav(destination, combined).await
    }

    async fn synth_single(&self, request: &SynthRequest) -> Result<PathBuf, TtsError> {
        if !is_safe_model_name(&request.model) {
            return Err(TtsError::InvalidModel);
        }
        let model = self.models_dir.join(format!("{}.onnx", request.model));
        if !model.is_file() {
            return Err(TtsError::ModelMissing);
        }
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        let destination = self.cache_dir.join(format!("{}.wav", cache_key(request)));
        if non_empty_file(&destination).await? {
            self.metrics.record_cache_hit();
            return Ok(destination);
        }

        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| TtsError::ProcessFailed)?;
        // Check again after waiting: another request may have completed this exact cache key.
        if non_empty_file(&destination).await? {
            self.metrics.record_cache_hit();
            return Ok(destination);
        }
        self.metrics.record_cache_miss();
        let temporary = self.cache_dir.join(format!(".{}.wav", Uuid::new_v4()));
        let result = self
            .runner
            .run(&model, &temporary, &request.text, request.speed)
            .await;
        let prepared = async {
            result?;
            if !non_empty_file(&temporary).await? {
                return Err(TtsError::EmptyOutput);
            }
            if request.lead_silence_ms > 0 {
                let wav = tokio::fs::read(&temporary).await?;
                let prefixed =
                    prepend_silence_wav(&wav, request.lead_silence_ms).map_err(TtsError::Wav)?;
                tokio::fs::write(&temporary, prefixed).await?;
            }
            Ok::<(), TtsError>(())
        }
        .await;
        // The temporary is a dot-file that eviction skips, so every failure must remove it here.
        if let Err(error) = prepared {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
        // A simultaneous matching request can win the race. Its immutable cache result is valid.
        match tokio::fs::rename(&temporary, &destination).await {
            Ok(()) => {
                let _ =
                    evict_cache_files(&self.cache_dir, &destination, DEFAULT_MAX_CACHE_FILES).await;
                Ok(destination)
            }
            Err(_error) if non_empty_file(&destination).await.unwrap_or(false) => {
                let _ = tokio::fs::remove_file(&temporary).await;
                Ok(destination)
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&temporary).await;
                Err(TtsError::Io(error))
            }
        }
    }

    async fn write_cached_wav(
        &self,
        destination: PathBuf,
        wav: Vec<u8>,
    ) -> Result<PathBuf, TtsError> {
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        if non_empty_file(&destination).await? {
            self.metrics.record_cache_hit();
            return Ok(destination);
        }
        let temporary = self.cache_dir.join(format!(".{}.wav", Uuid::new_v4()));
        tokio::fs::write(&temporary, wav).await?;
        match tokio::fs::rename(&temporary, &destination).await {
            Ok(()) => {
                let _ =
                    evict_cache_files(&self.cache_dir, &destination, DEFAULT_MAX_CACHE_FILES).await;
                Ok(destination)
            }
            Err(_error) if non_empty_file(&destination).await.unwrap_or(false) => {
                let _ = tokio::fs::remove_file(&temporary).await;
                Ok(destination)
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&temporary).await;
                Err(TtsError::Io(error))
            }
        }
    }
}

fn single_segment_request(
    request: &SynthRequest,
    segment: &vozen_core::SpeechSegment,
) -> SynthRequest {
    SynthRequest {
        text: segment.text.clone(),
        model: segment.model.clone(),
        asset_path: None,
        speed: request.speed,
        engine: request.engine,
        gcloud_budget: request.gcloud_budget.clone(),
        segments: None,
        single_voice: Some(true),
        emphasis_source: None,
        lead_silence_ms: 0,
    }
}

/// Keeps only the oldest `max_files` completed WAVs. Cleanup is deliberately best-effort: cache
/// eviction is never allowed to turn a successful synthesis into a Discord-visible failure.
async fn evict_cache_files(
    dir: &Path,
    just_written: &Path,
    max_files: usize,
) -> Result<(), std::io::Error> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || path.extension().and_then(|ext| ext.to_str()) != Some("wav") {
            continue;
        }
        let metadata = match entry.metadata().await {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((path, modified));
    }
    if files.len() <= max_files {
        return Ok(());
    }
    files.sort_by_key(|(_, modified)| *modified);
    let mut remaining = files.len().saturating_sub(max_files);
    for (path, _) in files {
        if remaining == 0 {
            break;
        }
        if path == just_written {
            continue;
        }
        let _ = tokio::fs::remove_file(path).await;
        remaining -= 1;
    }
    Ok(())
}

/// Direct assets are only accepted when they are regular, non-empty WAV files. The command
/// layer supplies paths from the curated repository catalogue; this second gate prevents a
/// future caller from turning the synthesis boundary into an arbitrary file reader.
async fn validate_asset_wav(path: &Path) -> Result<PathBuf, TtsError> {
    let metadata = tokio::fs::metadata(path).await?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(TtsError::EmptyOutput);
    }
    let bytes = tokio::fs::read(path).await?;
    parse_wav(&bytes).map_err(TtsError::Wav)?;
    Ok(path.to_owned())
}

fn cache_key(request: &SynthRequest) -> String {
    let mut digest = Sha256::new();
    let speed = request.speed.to_bits().to_string();
    for value in [&request.text, &request.model, &speed] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    hash_segments(&mut digest, request.segments.as_deref());
    digest.update(request.lead_silence_ms.to_be_bytes());
    format!("{:x}", digest.finalize())
}

/// Keeps single-voice and multi-voice renderings of the same joined text in separate cache
/// files. Every provider's cache key must include it.
pub(crate) fn hash_segments(digest: &mut Sha256, segments: Option<&[vozen_core::SpeechSegment]>) {
    match segments {
        Some(segments) => {
            digest.update([1]);
            digest.update((segments.len() as u64).to_be_bytes());
            for segment in segments {
                for value in [&segment.text, &segment.model] {
                    digest.update((value.len() as u64).to_be_bytes());
                    digest.update(value.as_bytes());
                }
            }
        }
        None => digest.update([0]),
    }
}

/// Piper's `length_scale` runs inversely to the user-facing speed. Clamp avoids a malformed
/// profile making an unusably slow or fast process while the dashboard itself enforces 0.5..=2.
fn speed_to_length_scale(speed: f64) -> f64 {
    1.0 / speed.clamp(0.5, 2.0)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    struct FakeRunner {
        calls: AtomicUsize,
        bytes: Vec<u8>,
    }

    struct DelayedRunner {
        active: AtomicUsize,
        peak: AtomicUsize,
        specs: HashMap<String, (u64, u8)>,
    }

    impl DelayedRunner {
        fn record_peak(&self, active: usize) {
            let mut peak = self.peak.load(Ordering::Relaxed);
            while active > peak {
                match self
                    .peak
                    .compare_exchange(peak, active, Ordering::Relaxed, Ordering::Relaxed)
                {
                    Ok(_) => break,
                    Err(observed) => peak = observed,
                }
            }
        }
    }

    #[async_trait]
    impl PiperRunner for FakeRunner {
        async fn run(
            &self,
            _model: &Path,
            output: &Path,
            _text: &str,
            _speed: f64,
        ) -> Result<(), TtsError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            tokio::fs::write(output, &self.bytes).await?;
            Ok(())
        }
    }

    #[async_trait]
    impl PiperRunner for DelayedRunner {
        async fn run(
            &self,
            _model: &Path,
            output: &Path,
            text: &str,
            _speed: f64,
        ) -> Result<(), TtsError> {
            let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
            self.record_peak(active);
            let (delay_ms, marker) = self.specs.get(text).copied().unwrap_or((0, 0x7f));
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let mut wav = wav_concat::silence_wav(10);
            wav[44] = marker;
            let result = tokio::fs::write(output, wav).await.map_err(TtsError::Io);
            self.active.fetch_sub(1, Ordering::Relaxed);
            result
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "vozen-rust-{label}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    fn request() -> SynthRequest {
        SynthRequest {
            text: "hello".into(),
            model: "en_US-amy-medium".into(),
            asset_path: None,
            speed: 1.0,
            engine: vozen_core::SynthesisEngine::Default,
            gcloud_budget: None,
            segments: None,
            single_voice: None,
            emphasis_source: None,
            lead_silence_ms: 0,
        }
    }

    #[tokio::test]
    async fn rejects_path_like_model_before_touching_the_runner() {
        let root = temp_dir("unsafe");
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            bytes: b"wav".to_vec(),
        });
        let engine = PiperEngine::new(runner.clone(), root.join("models"), root.join("cache"), 1);
        let mut bad = request();
        bad.model = "../outside".into();
        assert!(matches!(
            engine.synth(&bad).await,
            Err(TtsError::InvalidModel)
        ));
        assert_eq!(runner.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn returns_curated_wav_assets_without_touching_piper_or_models() {
        let root = temp_dir("asset");
        tokio::fs::create_dir_all(&root).await.expect("root");
        let asset = root.join("clip.wav");
        tokio::fs::write(&asset, silence_wav(1))
            .await
            .expect("asset");
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            bytes: b"not used".to_vec(),
        });
        let engine = PiperEngine::new(
            runner.clone(),
            root.join("missing-models"),
            root.join("cache"),
            1,
        );
        let mut request = request();
        request.asset_path = Some(asset.clone());
        let output = engine.synth(&request).await.expect("asset output");
        assert_eq!(output, asset);
        assert_eq!(runner.calls.load(Ordering::Relaxed), 0);

        request.asset_path = Some(root.join("invalid.wav"));
        tokio::fs::write(request.asset_path.as_ref().expect("path"), b"nope")
            .await
            .expect("invalid asset");
        assert!(matches!(
            engine.synth(&request).await,
            Err(TtsError::Wav(_))
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn composes_each_explicit_voice_segment_and_caches_the_result() {
        let root = temp_dir("segments");
        let models = root.join("models");
        tokio::fs::create_dir_all(&models).await.expect("models");
        for model in ["en_US-amy-medium", "pt_PT-tugao-medium"] {
            tokio::fs::write(models.join(format!("{model}.onnx")), b"model")
                .await
                .expect("model");
        }
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            bytes: wav_concat::silence_wav(1),
        });
        let engine = PiperEngine::new(runner.clone(), models, root.join("cache"), 1);
        let mut segmented = request();
        segmented.segments = Some(vec![
            vozen_core::SpeechSegment {
                text: "hello".into(),
                model: "en_US-amy-medium".into(),
            },
            vozen_core::SpeechSegment {
                text: "olá".into(),
                model: "pt_PT-tugao-medium".into(),
            },
        ]);
        let first = engine.synth(&segmented).await.expect("combined WAV");
        let second = engine.synth(&segmented).await.expect("cached WAV");
        assert_eq!(first, second);
        assert_eq!(runner.calls.load(Ordering::Relaxed), 2);
        let wav = tokio::fs::read(first).await.expect("WAV");
        assert!(parse_wav(&wav).is_ok());
    }

    #[tokio::test]
    async fn failed_segment_falls_back_to_the_resolved_base_voice_without_dropping_text() {
        let root = temp_dir("segment-fallback");
        let models = root.join("models");
        tokio::fs::create_dir_all(&models).await.expect("models");
        tokio::fs::write(models.join("en_US-amy-medium.onnx"), b"model")
            .await
            .expect("base model");
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            bytes: wav_concat::silence_wav(1),
        });
        let engine = PiperEngine::new(runner.clone(), models, root.join("cache"), 1);
        let mut segmented = request();
        segmented.text = "full original message".into();
        segmented.segments = Some(vec![
            vozen_core::SpeechSegment {
                text: "missing".into(),
                model: "pt_PT-missing-medium".into(),
            },
            vozen_core::SpeechSegment {
                text: "part".into(),
                model: "en_US-amy-medium".into(),
            },
        ]);
        let fallback = engine.synth(&segmented).await.expect("base fallback");
        assert!(parse_wav(&tokio::fs::read(fallback).await.expect("WAV")).is_ok());
        // The valid segment may already be in flight when the missing model fails; the caller
        // still falls back to the resolved base voice without dropping the original text.
        assert_eq!(runner.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn segment_fallback_is_cached_apart_so_a_transient_failure_does_not_stick() {
        let root = temp_dir("segment-fallback-key");
        let models = root.join("models");
        tokio::fs::create_dir_all(&models).await.expect("models");
        tokio::fs::write(models.join("en_US-amy-medium.onnx"), b"model")
            .await
            .expect("base model");
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            bytes: wav_concat::silence_wav(1),
        });
        let engine = PiperEngine::new(runner.clone(), &models, root.join("cache"), 1);
        let mut segmented = request();
        segmented.text = "olá hello".into();
        segmented.segments = Some(vec![
            vozen_core::SpeechSegment {
                text: "olá".into(),
                model: "pt_PT-tugao-medium".into(),
            },
            vozen_core::SpeechSegment {
                text: "hello".into(),
                model: "en_US-amy-medium".into(),
            },
        ]);
        let mut base = segmented.clone();
        base.segments = None;

        let fallback = engine.synth(&segmented).await.expect("fallback");
        assert_eq!(
            fallback.file_name().and_then(|name| name.to_str()),
            Some(format!("{}.wav", cache_key(&base)).as_str())
        );

        // Once the missing voice is available, the multi-voice rendering is produced instead of
        // the earlier fallback being served from the cache.
        tokio::fs::write(models.join("pt_PT-tugao-medium.onnx"), b"model")
            .await
            .expect("segment model");
        let recovered = engine.synth(&segmented).await.expect("multi-voice");
        assert_eq!(
            recovered.file_name().and_then(|name| name.to_str()),
            Some(format!("{}.wav", cache_key(&segmented)).as_str())
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    struct FailingRunner;

    #[async_trait]
    impl PiperRunner for FailingRunner {
        async fn run(
            &self,
            _model: &Path,
            output: &Path,
            _text: &str,
            _speed: f64,
        ) -> Result<(), TtsError> {
            // Piper may already have written part of its output before it fails or times out.
            tokio::fs::write(output, b"RIFF partial").await?;
            Err(TtsError::ProcessFailed)
        }
    }

    #[tokio::test]
    async fn failed_synthesis_leaves_no_temporary_files_behind() {
        let root = temp_dir("temporary-cleanup");
        let models = root.join("models");
        let cache = root.join("cache");
        tokio::fs::create_dir_all(&models).await.expect("models");
        tokio::fs::write(models.join("en_US-amy-medium.onnx"), b"model")
            .await
            .expect("model");
        let engine = PiperEngine::new(Arc::new(FailingRunner), &models, &cache, 1);
        assert!(engine.synth(&request()).await.is_err());
        let mut entries = tokio::fs::read_dir(&cache).await.expect("cache dir");
        let leftover = entries.next_entry().await.expect("entry");
        assert!(
            leftover.is_none(),
            "unexpected leftover file {:?}",
            leftover.map(|entry| entry.file_name())
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn caches_non_empty_wavs_and_rejects_empty_outputs() {
        let root = temp_dir("cache");
        let models = root.join("models");
        tokio::fs::create_dir_all(&models).await.expect("models");
        tokio::fs::write(models.join("en_US-amy-medium.onnx"), b"model")
            .await
            .expect("model");
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            bytes: b"wav".to_vec(),
        });
        let engine = PiperEngine::new(runner.clone(), &models, root.join("cache"), 1);
        let first = engine.synth(&request()).await.expect("first");
        let second = engine.synth(&request()).await.expect("cached");
        assert_eq!(first, second);
        assert_eq!(runner.calls.load(Ordering::Relaxed), 1);

        let empty = PiperEngine::new(
            Arc::new(FakeRunner {
                calls: AtomicUsize::new(0),
                bytes: Vec::new(),
            }),
            &models,
            root.join("empty"),
            1,
        );
        assert!(matches!(
            empty.synth(&request()).await,
            Err(TtsError::EmptyOutput)
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn parallel_segments_preserve_order_and_provider_bound() {
        let root = temp_dir("parallel-segments");
        let models = root.join("models");
        tokio::fs::create_dir_all(&models).await.expect("models");
        tokio::fs::write(models.join("en_US-amy-medium.onnx"), b"model")
            .await
            .expect("model");
        let runner = Arc::new(DelayedRunner {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            specs: HashMap::from([
                ("first".to_owned(), (80, 0x31)),
                ("second".to_owned(), (5, 0x52)),
                ("third".to_owned(), (5, 0x73)),
            ]),
        });
        let engine = PiperEngine::new(runner.clone(), &models, root.join("cache"), 2);
        let mut segmented = request();
        segmented.segments = Some(
            ["first", "second", "third"]
                .into_iter()
                .map(|text| vozen_core::SpeechSegment {
                    text: text.to_owned(),
                    model: "en_US-amy-medium".to_owned(),
                })
                .collect(),
        );

        let output = engine.synth(&segmented).await.expect("combined WAV");
        let wav = tokio::fs::read(output).await.expect("WAV");
        let data = parse_wav(&wav).expect("parsed").data;
        let first = data
            .iter()
            .position(|byte| *byte == 0x31)
            .expect("first marker");
        let second = data
            .iter()
            .position(|byte| *byte == 0x52)
            .expect("second marker");
        let third = data
            .iter()
            .position(|byte| *byte == 0x73)
            .expect("third marker");
        assert!(first < second && second < third, "segment order changed");
        assert_eq!(runner.peak.load(Ordering::Relaxed), 2);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn lead_silence_is_part_of_the_cache_key_and_is_added_once() {
        let root = temp_dir("lead-silence");
        let models = root.join("models");
        tokio::fs::create_dir_all(&models).await.expect("models");
        tokio::fs::write(models.join("en_US-amy-medium.onnx"), b"model")
            .await
            .expect("model");
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            bytes: silence_wav(1),
        });
        let engine = PiperEngine::new(runner.clone(), &models, root.join("cache"), 1);

        let plain = engine.synth(&request()).await.expect("plain");
        let mut delayed_request = request();
        delayed_request.lead_silence_ms = 1_000;
        let delayed = engine.synth(&delayed_request).await.expect("delayed");
        let delayed_again = engine
            .synth(&delayed_request)
            .await
            .expect("cached delayed");

        assert_ne!(plain, delayed);
        assert_eq!(delayed, delayed_again);
        assert_eq!(runner.calls.load(Ordering::Relaxed), 2);
        let delayed_wav = tokio::fs::read(delayed).await.expect("WAV");
        let parsed = parse_wav(&delayed_wav).expect("parsed");
        assert_eq!(parsed.data.len(), 44 + 44_100);
        assert!(parsed.data[..44_100].iter().all(|sample| *sample == 0));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn cache_eviction_keeps_the_bound_and_new_file() {
        let root = temp_dir("eviction");
        tokio::fs::create_dir_all(&root).await.expect("cache dir");
        for name in ["old.wav", "middle.wav", "new.wav"] {
            tokio::fs::write(root.join(name), b"wav")
                .await
                .expect("cache entry");
        }
        let just_written = root.join("new.wav");
        evict_cache_files(&root, &just_written, 2)
            .await
            .expect("eviction");
        let mut count = 0;
        let mut entries = tokio::fs::read_dir(&root).await.expect("read cache");
        while let Some(entry) = entries.next_entry().await.expect("entry") {
            if entry.path().extension().and_then(|ext| ext.to_str()) == Some("wav") {
                count += 1;
            }
        }
        assert_eq!(count, 2);
        assert!(just_written.is_file());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[test]
    fn safe_names_and_speed_mapping_are_bounded() {
        assert!(is_safe_model_name("pt_PT-google-medium"));
        assert!(!is_safe_model_name(""));
        assert!(!is_safe_model_name("..\\voice"));
        assert_eq!(speed_to_length_scale(1.0), 1.0);
        assert_eq!(speed_to_length_scale(4.0), 0.5);
    }
}
