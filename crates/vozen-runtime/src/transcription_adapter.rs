//! Bounded Discord attachment -> ffmpeg -> Whisper adapter for the Rust migration.
//!
//! The adapter is inert until `RUST_TRANSCRIBE_MESSAGE_ENABLED=true`. It keeps the sidecar behind
//! a single mutex, applies timeouts to every external boundary, and removes its temporary files
//! on success and failure.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

#[cfg(test)]
use std::time::SystemTime;

use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, Semaphore},
    time::timeout,
};
use uuid::Uuid;
use vozen_discord::{
    AttachmentAdmission, AttachmentTranscriptionLimits, DiscordAudioAttachment,
    admit_discord_audio_attachment, bound_transcript_text, within_attachment_duration,
};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15);
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(20);
const WHISPER_TIMEOUT: Duration = Duration::from_secs(30);
const WHISPER_READY_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TRANSCRIPT_UTF16: usize = 1_800;
const STT_ORPHAN_MIN_AGE_MS: i64 = 5 * 60 * 1_000;
#[cfg(feature = "voice-driver")]
const LIVE_PCM_SAMPLE_RATE: usize = 48_000;
#[cfg(feature = "voice-driver")]
const LIVE_PCM_CHANNELS: usize = 2;
#[cfg(feature = "voice-driver")]
const LIVE_MAX_SECONDS: usize = 20;

#[derive(Debug, Clone)]
pub struct TranscriptionRuntimeOptions {
    pub python: PathBuf,
    pub script: PathBuf,
    pub model: Option<String>,
    pub ffmpeg: PathBuf,
    pub max_concurrency: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentTranscript {
    pub text: String,
    pub language: String,
    pub duration_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum TranscriptionError {
    #[error("attachment rejected: {0}")]
    Rejected(String),
    #[error("transcription is busy")]
    Busy,
    #[error("attachment processing failed")]
    Processing,
    #[error("whisper sidecar unavailable")]
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FfmpegHealth {
    Available { version: String },
    Unavailable { reason: String },
}

fn media_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.kill_on_drop(true);
    command
}

/// Performs the same early, non-fatal FFmpeg probe as the Node runtime. The caller logs the
/// result, but a missing binary never prevents the control-plane gateway from starting.
pub async fn check_ffmpeg(path: &Path) -> FfmpegHealth {
    let result = timeout(
        Duration::from_secs(5),
        media_command(path).arg("-version").output(),
    )
    .await;
    match result {
        Ok(Ok(output)) if output.status.success() => FfmpegHealth::Available {
            version: output
                .stdout
                .split(|byte| *byte == b'\n' || *byte == b'\r')
                .next()
                .filter(|line| !line.is_empty())
                .map(|line| String::from_utf8_lossy(line).into_owned())
                .unwrap_or_else(|| "unknown".to_owned()),
        },
        Ok(Ok(output)) => FfmpegHealth::Unavailable {
            reason: format!("process exited with {}", output.status),
        },
        Ok(Err(error)) => FfmpegHealth::Unavailable {
            reason: error.to_string(),
        },
        Err(_) => FfmpegHealth::Unavailable {
            reason: "probe timed out after 5 seconds".to_owned(),
        },
    }
}

/// Removes temporary STT workspaces left by a process that was killed between conversion and
/// cleanup. Only entries created by this runtime (`vozen-stt-*`) and older than five minutes are
/// eligible. Recent entries and symlinks are deliberately skipped so a live session or an
/// unexpected filesystem link can never be removed by startup hygiene.
pub fn sweep_orphan_stt_temps(dir: &Path, now_ms: i64) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("vozen-stt-") {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let modified_ms = modified
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok());
        let Some(modified_ms) = modified_ms else {
            continue;
        };
        if now_ms.saturating_sub(modified_ms) < STT_ORPHAN_MIN_AGE_MS {
            continue;
        }
        let result = if file_type.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if result.is_ok() {
            removed += 1;
        }
    }
    removed
}

struct SidecarState {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// A persistent line-oriented Whisper sidecar matching `tools/whisper_sidecar.py`.
pub struct WhisperSidecar {
    options: TranscriptionRuntimeOptions,
    state: Mutex<Option<SidecarState>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelled_media_process_is_terminated() {
        let child = media_command(Path::new("/bin/sleep"))
            .arg("30")
            .spawn()
            .expect("synthetic media process");
        let process = PathBuf::from(format!("/proc/{}", child.id().expect("child pid")));
        assert!(process.exists());
        drop(child);
        for _ in 0..100 {
            if !process.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("cancelled media process is still alive");
    }

    fn test_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("vozen-stt-sweep-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn sweep_removes_prefixed_workspaces_and_preserves_unrelated_data() {
        let root = test_directory();
        fs::create_dir_all(&root).expect("test root");
        let old = root.join("vozen-stt-old");
        let another = root.join("vozen-stt-another");
        let unrelated = root.join("other-process-data");
        fs::create_dir_all(&old).expect("old workspace");
        fs::create_dir_all(&another).expect("another workspace");
        fs::write(&unrelated, b"keep").expect("unrelated file");
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as i64;

        assert_eq!(
            sweep_orphan_stt_temps(&root, now_ms + STT_ORPHAN_MIN_AGE_MS + 1),
            2
        );
        assert!(!old.exists());
        assert!(!another.exists());
        assert!(unrelated.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn sweep_keeps_recent_workspace() {
        let root = test_directory();
        fs::create_dir_all(root.join("vozen-stt-live")).expect("workspace");
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as i64;

        assert_eq!(sweep_orphan_stt_temps(&root, now_ms), 0);
        assert!(root.join("vozen-stt-live").exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn ffmpeg_health_keeps_a_missing_binary_non_fatal() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let result = runtime.block_on(check_ffmpeg(Path::new("definitely-not-a-vozen-ffmpeg")));
        assert!(matches!(result, FfmpegHealth::Unavailable { .. }));
    }
}

impl WhisperSidecar {
    pub fn new(options: TranscriptionRuntimeOptions) -> Self {
        Self {
            options,
            state: Mutex::new(None),
        }
    }

    async fn spawn_locked(&self) -> Result<SidecarState, TranscriptionError> {
        let mut command = media_command(&self.options.python);
        command.arg(&self.options.script);
        if let Some(model) = &self.options.model {
            // Passar também como argumento torna a seleção determinística mesmo
            // com versões antigas do sidecar que não liam WHISPER_MODEL.
            command
                .arg("--model")
                .arg(model)
                .env("WHISPER_MODEL", model);
        }
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| TranscriptionError::Unavailable)?;
        let stdin = child.stdin.take().ok_or(TranscriptionError::Unavailable)?;
        let stdout = child.stdout.take().ok_or(TranscriptionError::Unavailable)?;
        let mut state = SidecarState {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        let ready = timeout(
            WHISPER_READY_TIMEOUT,
            state.stdout.read_line(&mut String::new()),
        )
        .await
        .map_err(|_| TranscriptionError::Unavailable)?
        .map_err(|_| TranscriptionError::Unavailable)?;
        if ready == 0 {
            return Err(TranscriptionError::Unavailable);
        }
        Ok(state)
    }

    pub async fn transcribe(
        &self,
        wav_path: &Path,
    ) -> Result<AttachmentTranscript, TranscriptionError> {
        self.transcribe_with_language(wav_path, None).await
    }

    pub async fn transcribe_with_language(
        &self,
        wav_path: &Path,
        language: Option<&str>,
    ) -> Result<AttachmentTranscript, TranscriptionError> {
        let mut guard = self.state.lock().await;
        if guard.is_none() {
            *guard = Some(self.spawn_locked().await?);
        }
        let state = guard.as_mut().ok_or(TranscriptionError::Unavailable)?;
        let request = match language.map(str::trim).filter(|value| !value.is_empty()) {
            Some(language) => serde_json::json!({
                "path": wav_path.display().to_string(),
                "lang": language,
            })
            .to_string(),
            None => wav_path.display().to_string(),
        } + "\n";
        if state.stdin.write_all(request.as_bytes()).await.is_err() {
            *guard = None;
            return Err(TranscriptionError::Unavailable);
        }
        if state.stdin.flush().await.is_err() {
            *guard = None;
            return Err(TranscriptionError::Unavailable);
        }
        let mut line = String::new();
        let read = timeout(WHISPER_TIMEOUT, state.stdout.read_line(&mut line)).await;
        let count = match read {
            Ok(Ok(count)) if count > 0 => count,
            _ => {
                if let Some(mut stale) = guard.take() {
                    let _ = stale.child.kill().await;
                }
                return Err(TranscriptionError::Unavailable);
            }
        };
        let _ = count;
        let response: SidecarResponse =
            serde_json::from_str(line.trim()).map_err(|_| TranscriptionError::Processing)?;
        if let Some(error) = response.error {
            let _ = error;
            return Err(TranscriptionError::Processing);
        }
        Ok(AttachmentTranscript {
            text: bound_transcript_text(
                response.text.as_deref().unwrap_or_default(),
                MAX_TRANSCRIPT_UTF16,
            ),
            language: response.lang.unwrap_or_default(),
            duration_ms: 0,
        })
    }
}

#[derive(Debug, Deserialize)]
struct SidecarResponse {
    text: Option<String>,
    lang: Option<String>,
    error: Option<String>,
}

pub struct AttachmentTranscriber {
    client: reqwest::Client,
    ffmpeg: PathBuf,
    sidecar: Arc<WhisperSidecar>,
    semaphore: Arc<Semaphore>,
}

impl AttachmentTranscriber {
    pub fn new(options: TranscriptionRuntimeOptions) -> Result<Self, TranscriptionError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(DOWNLOAD_TIMEOUT)
            .build()
            .map_err(|_| TranscriptionError::Unavailable)?;
        Ok(Self {
            client,
            ffmpeg: options.ffmpeg.clone(),
            sidecar: Arc::new(WhisperSidecar::new(options.clone())),
            semaphore: Arc::new(Semaphore::new(options.max_concurrency.max(1))),
        })
    }

    pub async fn transcribe(
        &self,
        attachment: DiscordAudioAttachment<'_>,
        limits: AttachmentTranscriptionLimits,
    ) -> Result<AttachmentTranscript, TranscriptionError> {
        let admission = admit_discord_audio_attachment(attachment, limits.max_bytes);
        let url = match admission {
            AttachmentAdmission::Accepted(url) => url,
            AttachmentAdmission::Rejected(reason) => {
                return Err(TranscriptionError::Rejected(reason.to_string()));
            }
        };
        let permit = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| TranscriptionError::Busy)?;
        let workspace = std::env::temp_dir().join(format!("vozen-stt-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&workspace)
            .await
            .map_err(|_| TranscriptionError::Processing)?;
        let input = workspace.join("input.audio");
        let wav = workspace.join("output.wav");
        let result = self.run_pipeline(&url, &input, &wav, limits).await;
        let _ = tokio::fs::remove_dir_all(&workspace).await;
        drop(permit);
        result
    }

    /// Transcribes one bounded, consented live utterance captured by Songbird. The input format
    /// matches Node's receiver exactly: signed little-endian 16-bit PCM, 48 kHz, stereo. The raw
    /// buffer is converted to the same 24 kHz mono WAV used by attachment transcription and is
    /// removed together with the WAV before this method returns.
    #[cfg(feature = "voice-driver")]
    pub async fn transcribe_pcm(
        &self,
        pcm: &[i16],
        duration_ms: u64,
        language: Option<&str>,
    ) -> Result<AttachmentTranscript, TranscriptionError> {
        let max_samples = LIVE_PCM_SAMPLE_RATE
            .saturating_mul(LIVE_PCM_CHANNELS)
            .saturating_mul(LIVE_MAX_SECONDS);
        if pcm.is_empty() || pcm.len() > max_samples {
            return Err(TranscriptionError::Rejected("duration".into()));
        }
        let permit = self
            .semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| TranscriptionError::Busy)?;
        let workspace = std::env::temp_dir().join(format!("vozen-stt-live-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&workspace)
            .await
            .map_err(|_| TranscriptionError::Processing)?;
        let raw = workspace.join("input.raw");
        let wav = workspace.join("output.wav");
        let result = self
            .run_pcm_pipeline(pcm, duration_ms, language, &raw, &wav)
            .await;
        let _ = tokio::fs::remove_dir_all(&workspace).await;
        drop(permit);
        result
    }

    #[cfg(feature = "voice-driver")]
    async fn run_pcm_pipeline(
        &self,
        pcm: &[i16],
        duration_ms: u64,
        language: Option<&str>,
        raw: &Path,
        wav: &Path,
    ) -> Result<AttachmentTranscript, TranscriptionError> {
        let mut bytes = Vec::with_capacity(pcm.len().saturating_mul(2));
        for sample in pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let mut file = tokio::fs::File::create(raw)
            .await
            .map_err(|_| TranscriptionError::Processing)?;
        file.write_all(&bytes)
            .await
            .map_err(|_| TranscriptionError::Processing)?;
        file.flush()
            .await
            .map_err(|_| TranscriptionError::Processing)?;
        let status = timeout(
            FFMPEG_TIMEOUT,
            media_command(&self.ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "s16le",
                    "-ar",
                    "48000",
                    "-ac",
                    "2",
                    "-i",
                ])
                .arg(raw)
                .args(["-ar", "24000", "-ac", "1", "-c:a", "pcm_s16le", "-f", "wav"])
                .arg(wav)
                .arg("-y")
                .output(),
        )
        .await
        .map_err(|_| TranscriptionError::Processing)?
        .map_err(|_| TranscriptionError::Processing)?;
        if !status.status.success() {
            return Err(TranscriptionError::Processing);
        }
        let mut transcript = self.sidecar.transcribe_with_language(wav, language).await?;
        transcript.duration_ms = duration_ms.min((LIVE_MAX_SECONDS as u64) * 1_000);
        Ok(transcript)
    }

    async fn run_pipeline(
        &self,
        url: &reqwest::Url,
        input: &Path,
        wav: &Path,
        limits: AttachmentTranscriptionLimits,
    ) -> Result<AttachmentTranscript, TranscriptionError> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| TranscriptionError::Processing)?;
        if !response.status().is_success() {
            return Err(TranscriptionError::Processing);
        }
        if response
            .content_length()
            .is_some_and(|size| size > limits.max_bytes)
        {
            return Err(TranscriptionError::Rejected("size".into()));
        }
        let mut file = tokio::fs::File::create(input)
            .await
            .map_err(|_| TranscriptionError::Processing)?;
        let mut received = 0u64;
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| TranscriptionError::Processing)?
        {
            received = received.saturating_add(chunk.len() as u64);
            if received > limits.max_bytes {
                return Err(TranscriptionError::Rejected("size".into()));
            }
            file.write_all(&chunk)
                .await
                .map_err(|_| TranscriptionError::Processing)?;
        }
        if received == 0 {
            return Err(TranscriptionError::Rejected("size".into()));
        }
        file.flush()
            .await
            .map_err(|_| TranscriptionError::Processing)?;
        let status = timeout(
            FFMPEG_TIMEOUT,
            media_command(&self.ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-i"])
                .arg(input)
                .args([
                    "-t",
                    &(limits.max_seconds + 1).to_string(),
                    "-ar",
                    "24000",
                    "-ac",
                    "1",
                    "-c:a",
                    "pcm_s16le",
                    "-f",
                    "wav",
                ])
                .arg(wav)
                .arg("-y")
                .output(),
        )
        .await
        .map_err(|_| TranscriptionError::Processing)?
        .map_err(|_| TranscriptionError::Processing)?;
        if !status.status.success() {
            return Err(TranscriptionError::Processing);
        }
        let size = tokio::fs::metadata(wav)
            .await
            .map_err(|_| TranscriptionError::Processing)?
            .len();
        let duration = size.saturating_sub(44) as f64 / 48_000.0;
        if !within_attachment_duration(duration, limits.max_seconds) {
            return Err(TranscriptionError::Rejected("duration".into()));
        }
        let mut transcript = self.sidecar.transcribe(wav).await?;
        transcript.duration_ms = (duration * 1_000.0).round() as u64;
        Ok(transcript)
    }
}
