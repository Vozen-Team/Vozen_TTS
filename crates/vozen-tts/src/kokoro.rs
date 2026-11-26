//! Kokoro neural TTS through the repository's persistent Python sidecar.
//!
//! The sidecar speaks a deliberately tiny JSON-lines protocol. Rust owns process lifecycle,
//! serialization, timeouts, cache writes and validation; the Python process only owns the
//! Kokoro/ONNX inference. A sidecar failure is returned to the provider router, which then
//! applies the same configured-default fallback as the Node implementation.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::timeout,
};
use uuid::Uuid;
use vozen_core::SynthRequest;

use crate::{
    TtsError, concat_wavs, lower_all_caps_runs, non_empty_file, parse_wav, prepend_silence_wav,
};

const DEFAULT_SYNTH_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_CACHE_FILES: usize = 500;

/// Supported Kokoro locale mapping from the Node provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KokoroVoice {
    pub lang: &'static str,
    pub voice: &'static str,
}

pub fn kokoro_voice_for_model(model: &str) -> Option<KokoroVoice> {
    match language_key(model).as_str() {
        "en" => Some(KokoroVoice {
            lang: "en-us",
            voice: "af_heart",
        }),
        "es" => Some(KokoroVoice {
            lang: "es",
            voice: "ef_dora",
        }),
        "fr" => Some(KokoroVoice {
            lang: "fr-fr",
            voice: "ff_siwis",
        }),
        "hi" => Some(KokoroVoice {
            lang: "hi",
            voice: "hf_alpha",
        }),
        "it" => Some(KokoroVoice {
            lang: "it",
            voice: "if_sara",
        }),
        "pt" => Some(KokoroVoice {
            lang: "pt-br",
            voice: "pf_dora",
        }),
        "ja" => Some(KokoroVoice {
            lang: "ja",
            voice: "jf_alpha",
        }),
        _ => None,
    }
}

/// Prefix before the first underscore, matching Node's `langKeyOfModel`.
pub fn language_key(model: &str) -> String {
    model
        .split_once('_')
        .map_or_else(|| "en".to_owned(), |(prefix, _)| prefix.to_owned())
}

/// A shell-free executable plus argument vector. The command is never passed through a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KokoroCommand {
    pub executable: PathBuf,
    pub args: Vec<String>,
}

/// Parse the documented `KOKORO_CMD` form while respecting simple double/single quotes.
pub fn parse_command(command: &str) -> Option<KokoroCommand> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in command.trim().chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        match quote {
            Some(delimiter) if ch == delimiter => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    let executable = words.first()?.clone();
    Some(KokoroCommand {
        executable: executable.into(),
        args: words.into_iter().skip(1).collect(),
    })
}

#[derive(Debug, Clone)]
pub struct KokoroOptions {
    pub command: KokoroCommand,
    pub cache_dir: PathBuf,
    pub synth_timeout: Duration,
    pub ready_timeout: Duration,
    pub max_cache_files: usize,
    pub allowed_languages: Option<Vec<String>>,
}

impl KokoroOptions {
    #[must_use]
    pub fn production(command: KokoroCommand, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            command,
            cache_dir: cache_dir.into(),
            synth_timeout: DEFAULT_SYNTH_TIMEOUT,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            max_cache_files: DEFAULT_MAX_CACHE_FILES,
            allowed_languages: None,
        }
    }
}

struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Sidecar {
    fn spawn(command: &KokoroCommand) -> Result<Self, TtsError> {
        let mut child = Command::new(&command.executable)
            .args(&command.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|_| TtsError::KokoroProcess)?;
        let stdin = child.stdin.take().ok_or(TtsError::KokoroProcess)?;
        let stdout = child.stdout.take().ok_or(TtsError::KokoroProcess)?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn kill(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

#[derive(Debug, Error)]
enum ProtocolError {
    #[error("sidecar protocol returned invalid JSON")]
    Json,
}

/// Persistent, serial Kokoro engine. The mutex intentionally serializes requests because the
/// Python sidecar consumes one JSON line at a time and writes one response at a time.
pub struct KokoroEngine {
    options: KokoroOptions,
    sidecar: Mutex<Option<Sidecar>>,
    sequence: AtomicU64,
}

impl KokoroEngine {
    pub fn new(options: KokoroOptions) -> Result<Self, TtsError> {
        if options.command.executable.as_os_str().is_empty() {
            return Err(TtsError::KokoroConfiguration);
        }
        Ok(Self {
            options,
            sidecar: Mutex::new(None),
            sequence: AtomicU64::new(0),
        })
    }

    async fn ensure_ready<'a>(&'a self, sidecar: &'a mut Sidecar) -> Result<(), TtsError> {
        let warmup = json!({"warmup": true}).to_string() + "\n";
        sidecar
            .stdin
            .write_all(warmup.as_bytes())
            .await
            .map_err(|_| TtsError::KokoroProcess)?;
        sidecar
            .stdin
            .flush()
            .await
            .map_err(|_| TtsError::KokoroProcess)?;
        let line = read_line(&mut sidecar.stdout, self.options.ready_timeout).await?;
        let ready = parse_response(&line).map_err(|_| TtsError::KokoroResponse)?;
        if ready.ready == Some(true) {
            Ok(())
        } else {
            Err(TtsError::KokoroResponse)
        }
    }

    async fn request_sidecar(
        &self,
        text: &str,
        voice: KokoroVoice,
        speed: f64,
        output: &Path,
    ) -> Result<(), TtsError> {
        let mut guard = self.sidecar.lock().await;
        if guard.is_none() {
            let mut process = Sidecar::spawn(&self.options.command)?;
            if let Err(error) = self.ensure_ready(&mut process).await {
                process.kill().await;
                return Err(error);
            }
            *guard = Some(process);
        }

        let payload = json!({
            "text": text,
            "out": output,
            "lang": voice.lang,
            "voice": voice.voice,
            "speed": speed,
        })
        .to_string()
            + "\n";

        // The outer error restarts the sidecar (I/O, timeout, malformed or out-of-step replies).
        // The inner error is a well-formed `{"ok": false}` for this one request: the sidecar is
        // built to survive those, so it stays warm instead of reloading the model.
        let result: Result<Result<(), TtsError>, TtsError> = async {
            let process = guard.as_mut().ok_or(TtsError::KokoroProcess)?;
            process
                .stdin
                .write_all(payload.as_bytes())
                .await
                .map_err(|_| TtsError::KokoroProcess)?;
            process
                .stdin
                .flush()
                .await
                .map_err(|_| TtsError::KokoroProcess)?;
            let line = read_line(&mut process.stdout, self.options.synth_timeout).await?;
            let response = parse_response(&line).map_err(|_| TtsError::KokoroResponse)?;
            match response.ok {
                Some(false) => Ok(Err(TtsError::KokoroResponse)),
                // A reply naming another output path means stdout is out of step with requests.
                Some(true) if response.out.as_deref() == output.to_str() => Ok(Ok(())),
                _ => Err(TtsError::KokoroResponse),
            }
        }
        .await;

        match result {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(process) = guard.take() {
                    process.kill().await;
                }
                Err(error)
            }
        }
    }

    pub async fn synth(&self, request: &SynthRequest) -> Result<PathBuf, TtsError> {
        if let Some(asset) = request.asset_path.as_deref() {
            return validate_asset(asset).await;
        }
        let Some(segments) = request
            .segments
            .as_deref()
            .filter(|segments| !segments.is_empty())
        else {
            return self.synth_single(request).await;
        };
        if segments.len() == 1 {
            let mut single = request.clone();
            single.text = segments[0].text.clone();
            single.model = segments[0].model.clone();
            single.segments = None;
            single.single_voice = Some(true);
            single.lead_silence_ms = request.lead_silence_ms;
            return self.synth_single(&single).await;
        }

        let destination = self
            .options
            .cache_dir
            .join(format!("{}.wav", cache_key(request)));
        if non_empty_file(&destination).await? {
            return Ok(destination);
        }
        let mut wavs = Vec::with_capacity(segments.len());
        for segment in segments {
            let mut single = request.clone();
            single.text = segment.text.clone();
            single.model = segment.model.clone();
            single.segments = None;
            single.single_voice = Some(true);
            single.lead_silence_ms = 0;
            wavs.push(tokio::fs::read(self.synth_single(&single).await?).await?);
        }
        let combined = concat_wavs(&wavs, 20)?;
        let combined = prepend_silence_wav(&combined, request.lead_silence_ms)?;
        write_cached(
            &self.options.cache_dir,
            destination,
            combined,
            self.options.max_cache_files,
        )
        .await
    }

    async fn synth_single(&self, request: &SynthRequest) -> Result<PathBuf, TtsError> {
        let voice =
            kokoro_voice_for_model(&request.model).ok_or(TtsError::KokoroUnsupportedLanguage)?;
        if self
            .options
            .allowed_languages
            .as_ref()
            .is_some_and(|langs| {
                !langs
                    .iter()
                    .any(|language| language.eq_ignore_ascii_case(&language_key(&request.model)))
            })
        {
            return Err(TtsError::KokoroUnsupportedLanguage);
        }
        let key = cache_key(request);
        let destination = self.options.cache_dir.join(format!("{key}.wav"));
        if non_empty_file(&destination).await? {
            return Ok(destination);
        }
        tokio::fs::create_dir_all(&self.options.cache_dir).await?;
        let temporary = std::env::temp_dir().join(format!(
            "vozen-kokoro-{}-{}-{}.wav",
            std::process::id(),
            self.sequence.fetch_add(1, Ordering::Relaxed),
            Uuid::new_v4()
        ));
        let result = self
            .request_sidecar(
                &lower_all_caps_runs(&request.text),
                voice,
                request.speed,
                &temporary,
            )
            .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
        let bytes = tokio::fs::read(&temporary).await?;
        let _ = tokio::fs::remove_file(&temporary).await;
        parse_wav(&bytes)?;
        // The cache key includes `lead_silence_ms`, so the cached file must contain it too.
        let bytes = prepend_silence_wav(&bytes, request.lead_silence_ms)?;
        write_cached(
            &self.options.cache_dir,
            destination,
            bytes,
            self.options.max_cache_files,
        )
        .await
    }
}

#[derive(Debug)]
struct Response {
    ok: Option<bool>,
    ready: Option<bool>,
    out: Option<String>,
}

fn parse_response(line: &str) -> Result<Response, ProtocolError> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|_| ProtocolError::Json)?;
    Ok(Response {
        ok: value.get("ok").and_then(serde_json::Value::as_bool),
        ready: value.get("ready").and_then(serde_json::Value::as_bool),
        out: value
            .get("out")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

async fn read_line(
    stdout: &mut BufReader<ChildStdout>,
    duration: Duration,
) -> Result<String, TtsError> {
    let mut line = String::new();
    let read = timeout(duration, stdout.read_line(&mut line))
        .await
        .map_err(|_| TtsError::KokoroTimeout)?
        .map_err(|_| TtsError::KokoroProcess)?;
    if read == 0 {
        return Err(TtsError::KokoroProcess);
    }
    Ok(line)
}

fn cache_key(request: &SynthRequest) -> String {
    let mut digest = Sha256::new();
    digest.update((request.text.len() as u64).to_be_bytes());
    digest.update(request.text.as_bytes());
    digest.update((request.model.len() as u64).to_be_bytes());
    digest.update(request.model.as_bytes());
    digest.update(request.speed.to_bits().to_be_bytes());
    crate::hash_segments(&mut digest, request.segments.as_deref());
    digest.update(request.lead_silence_ms.to_be_bytes());
    format!("{:x}", digest.finalize())
}

async fn validate_asset(path: &Path) -> Result<PathBuf, TtsError> {
    let metadata = tokio::fs::metadata(path).await?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(TtsError::EmptyOutput);
    }
    let bytes = tokio::fs::read(path).await?;
    parse_wav(&bytes)?;
    Ok(path.to_owned())
}

async fn write_cached(
    directory: &Path,
    destination: PathBuf,
    bytes: Vec<u8>,
    max_files: usize,
) -> Result<PathBuf, TtsError> {
    tokio::fs::create_dir_all(directory).await?;
    if non_empty_file(&destination).await? {
        return Ok(destination);
    }
    let temporary = directory.join(format!(".{}.{}.tmp", std::process::id(), Uuid::new_v4()));
    tokio::fs::write(&temporary, bytes).await?;
    match tokio::fs::rename(&temporary, &destination).await {
        Ok(()) => {
            evict_cache(directory, &destination, max_files).await;
            Ok(destination)
        }
        Err(_) if non_empty_file(&destination).await.unwrap_or(false) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Ok(destination)
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Err(TtsError::Io(error))
        }
    }
}

async fn evict_cache(directory: &Path, just_written: &Path, max_files: usize) {
    if max_files == 0 {
        return;
    }
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return;
    };
    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("wav") || path == just_written {
            continue;
        }
        let modified = entry
            .metadata()
            .await
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        files.push((path, modified));
    }
    if files.len() < max_files {
        return;
    }
    files.sort_by_key(|(_, modified)| *modified);
    let remove_count = files.len().saturating_sub(max_files - 1);
    for (path, _) in files.into_iter().take(remove_count) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_the_same_languages_as_node() {
        assert_eq!(
            kokoro_voice_for_model("en_US-amy-medium").unwrap().voice,
            "af_heart"
        );
        assert_eq!(
            kokoro_voice_for_model("pt_PT-tuga-medium").unwrap().lang,
            "pt-br"
        );
        assert!(kokoro_voice_for_model("zh_CN-x-medium").is_none());
    }

    #[test]
    fn command_parser_never_requires_a_shell() {
        assert_eq!(
            parse_command("python \"tools/kokoro server.py\" --flag").unwrap(),
            KokoroCommand {
                executable: PathBuf::from("python"),
                args: vec!["tools/kokoro server.py".into(), "--flag".into()],
            }
        );
        assert!(parse_command("python \"unterminated").is_none());
    }

    #[test]
    fn language_key_matches_prefix_contract() {
        assert_eq!(language_key("fr_FR-voice-medium"), "fr");
        assert_eq!(language_key("voice-without-locale"), "en");
    }

    #[test]
    fn multi_voice_and_single_voice_renderings_never_share_a_cache_file() {
        let single = SynthRequest {
            text: "olá hello".into(),
            model: "en_US-amy-medium".into(),
            asset_path: None,
            speed: 1.0,
            engine: vozen_core::SynthesisEngine::Default,
            gcloud_budget: None,
            segments: None,
            single_voice: None,
            emphasis_source: None,
            lead_silence_ms: 0,
        };
        let mut multi = single.clone();
        multi.segments = Some(vec![
            vozen_core::SpeechSegment {
                text: "olá".into(),
                model: "pt_PT-tugao-medium".into(),
            },
            vozen_core::SpeechSegment {
                text: "hello".into(),
                model: "en_US-amy-medium".into(),
            },
        ]);
        assert_ne!(cache_key(&single), cache_key(&multi));
    }
}
