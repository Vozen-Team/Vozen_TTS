//! Opt-in Discord adapter for private `/tts-file` exports.
//!
//! This adapter is intentionally independent of Songbird and voice-state membership: exporting a
//! private attachment is not speaking in a call. It only claims a contract-valid `/tts-file`
//! interaction after `RUST_TTS_FILE_ENABLED=true`; all other traffic remains with Node.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use serenity::{
    builder::{CreateAttachment, EditInteractionResponse},
    client::Context,
    model::application::Interaction,
};
use vozen_discord::{
    GatewayEventDispatchError, GatewayEventSink, TtsFileExportInvocation, TtsFileExportOutcome,
    TtsFileExportService, VoiceResponseLocalizer, parse_tts_file_command,
};
use vozen_store::SqliteStore;

use crate::{
    TtsFileRuntimeOptions, engine_router::PerUserCommandSynthesizer,
    piper_adapter::PiperCommandSynthesizer, system_now_ms,
};

async fn take_export_attachment(path: &std::path::Path) -> serenity::Result<CreateAttachment> {
    let attachment = CreateAttachment::path(path).await;
    let removed = tokio::fs::remove_file(path).await;
    let attachment = attachment?;
    match removed {
        Ok(()) => Ok(attachment),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(attachment),
        Err(error) => Err(error.into()),
    }
}

/// A request owns this directory until its audio has been read into the upload buffer.
/// Drop also removes partial output when synthesis, attachment reads or the request fail.
struct PrivateExportWorkspace {
    path: PathBuf,
}

impl PrivateExportWorkspace {
    fn new() -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("vozen-tts-file-{}", uuid::Uuid::new_v4()));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let mut builder = builder;
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(Self { path })
    }

    fn close(self) -> std::io::Result<()> {
        std::fs::remove_dir_all(&self.path)
    }
}

impl Drop for PrivateExportWorkspace {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("[tts-file] temporary audio cleanup failed: {error}");
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportResponseKind {
    UploadAttachment,
    ContentOnly,
    AttachmentReadFailed,
}

/// Classifies the response boundary after the service has accepted a request. Keeping this
/// decision pure makes the deferred-ack/upload/edit contract testable without a Discord client.
#[cfg(test)]
fn export_response_kind(
    outcome: &TtsFileExportOutcome,
    attachment_readable: bool,
) -> ExportResponseKind {
    match outcome {
        TtsFileExportOutcome::Ready(_) if attachment_readable => {
            ExportResponseKind::UploadAttachment
        }
        TtsFileExportOutcome::Ready(_) => ExportResponseKind::AttachmentReadFailed,
        _ => ExportResponseKind::ContentOnly,
    }
}

pub struct TtsFileGatewaySink {
    service: TtsFileExportService<PerUserCommandSynthesizer>,
    localizer: VoiceResponseLocalizer,
    piper_path: PathBuf,
    models_dir: PathBuf,
    export_slots: tokio::sync::Semaphore,
}

impl TtsFileGatewaySink {
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        options: TtsFileRuntimeOptions,
    ) -> Result<Self, GatewayEventDispatchError> {
        let synthesizer =
            PerUserCommandSynthesizer::piper_only(PiperCommandSynthesizer::production(
                options.piper_path.clone(),
                options.models_dir.clone(),
                options.cache_dir,
                options.piper_concurrency,
            ));
        let localizer = VoiceResponseLocalizer::from_generated_contract()
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(Self {
            service: TtsFileExportService::new(
                store,
                synthesizer,
                options.settings,
                Arc::new(system_now_ms),
            ),
            localizer,
            piper_path: options.piper_path,
            models_dir: options.models_dir,
            export_slots: tokio::sync::Semaphore::new(options.piper_concurrency),
        })
    }

    fn message(
        &self,
        key: &str,
        interaction_locale: &str,
        guild_locale: Option<&str>,
        parameters: &BTreeMap<&str, String>,
    ) -> Result<String, GatewayEventDispatchError> {
        self.localizer
            .render_key(key, Some(interaction_locale), guild_locale, parameters)
            .ok_or(GatewayEventDispatchError)
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for TtsFileGatewaySink {
    async fn on_message(
        &self,
        _context: Context,
        _message: serenity::model::channel::Message,
    ) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    async fn on_interaction(
        &self,
        context: Context,
        interaction: Interaction,
    ) -> Result<(), GatewayEventDispatchError> {
        let Interaction::Command(command) = interaction else {
            return Ok(());
        };
        let Some(parsed) =
            parse_tts_file_command(&command.data).map_err(|_| GatewayEventDispatchError)?
        else {
            return Ok(());
        };
        // The parser runs before defer, so an interaction outside this exact promotion boundary
        // stays unclaimed for the Node process.
        command
            .defer_ephemeral(&context)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let guild_id = command.guild_id.map(|id| id.get().to_string());
        let preference_scope = guild_id.as_deref().unwrap_or("@user-app");
        let user_id = command.user.id.get().to_string();
        let permit = self
            .export_slots
            .acquire()
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let workspace = PrivateExportWorkspace::new().map_err(|_| GatewayEventDispatchError)?;
        // Each request gets a separate cache namespace, so identical concurrent exports
        // cannot delete or reuse another request's audio. The shared semaphore bounds Piper.
        let synthesizer =
            PerUserCommandSynthesizer::piper_only(PiperCommandSynthesizer::production(
                self.piper_path.clone(),
                self.models_dir.clone(),
                workspace.path.clone(),
                1,
            ));
        let outcome = self
            .service
            .execute_with_synthesizer(
                TtsFileExportInvocation {
                    guild_id: guild_id.as_deref(),
                    preference_scope,
                    user_id: &user_id,
                    raw: &parsed.text,
                },
                &synthesizer,
            )
            .await;
        let parameters = BTreeMap::new();
        let guild_locale = command.guild_locale.as_deref();
        let content = match &outcome {
            TtsFileExportOutcome::TooLong => {
                let mut parameters = BTreeMap::new();
                parameters.insert("max", vozen_discord::MAX_TTS_FILE_CHARS.to_string());
                self.message(
                    "ttsFile.tooLong",
                    &command.locale,
                    guild_locale,
                    &parameters,
                )?
            }
            TtsFileExportOutcome::Empty => self.message(
                "tts.nothingToRead",
                &command.locale,
                guild_locale,
                &parameters,
            )?,
            TtsFileExportOutcome::RateLimited => {
                self.message("tts.tooFast", &command.locale, guild_locale, &parameters)?
            }
            TtsFileExportOutcome::FullyBlocked => {
                self.message("tts.blocked", &command.locale, guild_locale, &parameters)?
            }
            TtsFileExportOutcome::VoiceUnavailable => self.message(
                "ttsFile.unavailable",
                &command.locale,
                guild_locale,
                &parameters,
            )?,
            TtsFileExportOutcome::SynthesisFailed | TtsFileExportOutcome::StoreUnavailable => {
                self.message("ttsFile.failed", &command.locale, guild_locale, &parameters)?
            }
            TtsFileExportOutcome::Ready(_) => {
                self.message("ttsFile.ready", &command.locale, guild_locale, &parameters)?
            }
        };
        let response = match outcome {
            TtsFileExportOutcome::Ready(path) => {
                // Read the private temporary audio into memory and remove it before delivery.
                let attachment = match take_export_attachment(&path).await {
                    Ok(mut attachment) => {
                        // Keep Node's stable download name instead of exposing the cache key.
                        attachment.filename = "vozen-audio.wav".to_owned();
                        attachment
                    }
                    Err(_) => {
                        let failed = self.message(
                            "ttsFile.failed",
                            &command.locale,
                            guild_locale,
                            &BTreeMap::new(),
                        )?;
                        workspace.close().map_err(|_| GatewayEventDispatchError)?;
                        drop(permit);
                        command
                            .edit_response(&context, EditInteractionResponse::new().content(failed))
                            .await
                            .map_err(|_| GatewayEventDispatchError)?;
                        return Ok(());
                    }
                };
                EditInteractionResponse::new()
                    .content(content)
                    .new_attachment(attachment)
            }
            _ => EditInteractionResponse::new().content(content),
        };
        // Remove partial synthesis output as well as completed files. Do not announce a
        // successful export if local cleanup failed. No workspace is held during upload.
        let response = if workspace.close().is_ok() {
            response
        } else {
            EditInteractionResponse::new().content(self.message(
                "ttsFile.failed",
                &command.locale,
                guild_locale,
                &BTreeMap::new(),
            )?)
        };
        drop(permit);
        command
            .edit_response(&context, response)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    async fn on_guild_delete(&self, guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        self.service.forget_scope(guild_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn private_export_attachment_keeps_upload_bytes_but_removes_disk_audio() {
        let path =
            std::env::temp_dir().join(format!("vozen-private-export-{}.wav", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"synthetic private audio").expect("write fixture");
        let attachment = take_export_attachment(&path)
            .await
            .expect("read attachment");
        let retained = path.exists();
        if retained {
            std::fs::remove_file(&path).expect("clean failed regression fixture");
        }
        assert_eq!(attachment.data.as_slice(), b"synthetic private audio");
        assert!(
            !retained,
            "private export must remove audio before an upload can begin"
        );
    }

    #[tokio::test]
    async fn private_export_read_failure_cleans_all_request_output() {
        let workspace = PrivateExportWorkspace::new().expect("workspace");
        let root = workspace.path.clone();
        std::fs::write(root.join("partial.wav"), b"partial private audio").expect("partial output");
        assert!(
            take_export_attachment(&root.join("missing.wav"))
                .await
                .is_err()
        );
        drop(workspace);
        assert!(
            !root.exists(),
            "failed reads must leave no request workspace"
        );
    }

    #[tokio::test]
    async fn private_export_identical_names_are_isolated_during_concurrent_reads() {
        let first = PrivateExportWorkspace::new().expect("first workspace");
        let second = PrivateExportWorkspace::new().expect("second workspace");
        let first_path = first.path.join("same-content-key.wav");
        let second_path = second.path.join("same-content-key.wav");
        std::fs::write(&first_path, b"private export one").expect("first audio");
        std::fs::write(&second_path, b"private export two").expect("second audio");
        let (one, two) = tokio::join!(
            take_export_attachment(&first_path),
            take_export_attachment(&second_path),
        );
        assert_eq!(one.expect("first upload").data, b"private export one");
        assert_eq!(two.expect("second upload").data, b"private export two");
        assert!(!first_path.exists());
        assert!(!second_path.exists());
        first.close().expect("first cleanup");
        second.close().expect("second cleanup");
    }

    #[tokio::test]
    async fn private_export_cancelled_request_removes_partial_workspace() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let workspace = PrivateExportWorkspace::new().expect("workspace");
            std::fs::write(
                workspace.path.join("partial.wav"),
                b"cancelled private audio",
            )
            .expect("partial audio");
            sender
                .send(workspace.path.clone())
                .expect("send workspace path");
            std::future::pending::<()>().await;
            drop(workspace);
        });
        let root = receiver.await.expect("workspace ready");
        task.abort();
        assert!(task.await.expect_err("cancelled task").is_cancelled());
        assert!(
            !root.exists(),
            "cancellation must drop the request-owned workspace"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_export_workspace_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = PrivateExportWorkspace::new().expect("workspace");
        assert_eq!(
            std::fs::metadata(&workspace.path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    fn command(payload: &str) -> serenity::model::application::CommandData {
        serde_json::from_str(payload).expect("valid Discord command payload")
    }

    #[test]
    fn boundary_accepts_private_command_and_ignores_other_commands() {
        let accepted = parse_tts_file_command(&command(
            r#"{"id":"1","name":"tts-file","type":1,"options":[{"name":"text","type":3,"value":"hello"}]}"#,
        ))
        .expect("parse accepted command");
        assert!(accepted.is_some());

        let ignored = parse_tts_file_command(&command(
            r#"{"id":"1","name":"tts","type":1,"options":[{"name":"text","type":3,"value":"hello"}]}"#,
        ))
        .expect("parse unrelated command");
        assert!(ignored.is_none());
    }

    #[test]
    fn deferred_response_boundary_covers_upload_and_failure_paths() {
        assert_eq!(
            export_response_kind(
                &TtsFileExportOutcome::Ready(PathBuf::from("audio.wav")),
                true,
            ),
            ExportResponseKind::UploadAttachment
        );
        assert_eq!(
            export_response_kind(
                &TtsFileExportOutcome::Ready(PathBuf::from("missing.wav")),
                false,
            ),
            ExportResponseKind::AttachmentReadFailed
        );
        assert_eq!(
            export_response_kind(&TtsFileExportOutcome::SynthesisFailed, true),
            ExportResponseKind::ContentOnly
        );
    }

    #[tokio::test]
    async fn attachment_upload_boundary_reports_missing_files_without_network_io() {
        let result =
            CreateAttachment::path(PathBuf::from("C:/tmp/vozen-no-such-audio-file.wav")).await;
        assert!(
            result.is_err(),
            "missing cache files must take the edit-failure path"
        );
    }
}
