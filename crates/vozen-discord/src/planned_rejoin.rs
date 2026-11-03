//! One-shot recovery authorization for voice sessions after a planned restart.
//!
//! Persisted presence alone must never make a normal guild session rejoin after an unexpected
//! crash. A fresh marker, written only during a clean shutdown/deploy, authorizes recovery once.

use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use vozen_store::VoicePresence;

pub const PLANNED_REJOIN_MARKER: &str = ".vozen-rejoin-after-deploy";
pub const MAX_PLANNED_REJOIN_AGE: Duration = Duration::from_secs(10 * 60);

/// Explicit recovery authority. The on-disk marker only grants access to exact guilds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedRejoinScope {
    All,
    Guilds(BTreeSet<String>),
}

#[derive(Debug, Serialize)]
struct RejoinMarker<'a> {
    // Keep the on-disk shape compatible with the Node runtime. This marker can
    // survive a mixed-runtime clean shutdown during the migration.
    #[serde(rename = "guildIds")]
    guild_ids: &'a [String],
}

#[derive(Debug, Deserialize)]
struct ParsedRejoinMarker {
    // `guild_ids` was emitted by an early Rust-only implementation. Accept it
    // as an alias, but emit the canonical Node-compatible `guildIds` above.
    #[serde(alias = "guild_ids")]
    #[serde(rename = "guildIds")]
    guild_ids: Vec<String>,
}

fn temporary_marker_path(path: &Path) -> PathBuf {
    path.with_file_name(format!("{PLANNED_REJOIN_MARKER}-{}", Uuid::new_v4()))
}

/// Records the exact live calls on a clean administrator-initiated shutdown. A failed write is
/// intentionally non-fatal: it should not turn an orderly shutdown into downtime.
pub fn write_planned_rejoin_marker(
    guild_ids: impl IntoIterator<Item = String>,
    path: &Path,
) -> io::Result<()> {
    let guild_ids = guild_ids
        .into_iter()
        .filter(|guild_id| !guild_id.trim().is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if guild_ids.is_empty() {
        return Ok(());
    }

    let payload = serde_json::to_vec(&RejoinMarker {
        guild_ids: &guild_ids,
    })?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_marker_path(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = file.write_all(&payload).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// Claims a restart marker once before checking its age and exact guild scope.
pub fn consume_planned_rejoin_marker(path: &Path, now: SystemTime) -> Option<PlannedRejoinScope> {
    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let claimed = temporary_marker_path(path);
    fs::rename(path, &claimed).ok()?;
    let marker = (|| {
        let metadata = fs::symlink_metadata(&claimed).ok()?;
        if !metadata.is_file() {
            return None;
        }
        Some((metadata.modified().ok()?, fs::read(&claimed).ok()?))
    })();
    let _ = fs::remove_file(claimed);
    let (modified, raw) = marker?;

    // Match Node's `age >= 0 && age <= MAX_MARKER_AGE_MS` rule. A future-dated
    // marker is invalid too, but must still be consumed so it cannot authorize
    // a later restart.
    if !matches!(
        now.duration_since(modified),
        Ok(age) if age <= MAX_PLANNED_REJOIN_AGE
    ) {
        return None;
    }
    let parsed: ParsedRejoinMarker = serde_json::from_slice(&raw).ok()?;
    if parsed.guild_ids.is_empty()
        || parsed
            .guild_ids
            .iter()
            .any(|guild_id| guild_id.trim().is_empty())
    {
        return None;
    }
    Some(PlannedRejoinScope::Guilds(
        parsed.guild_ids.into_iter().collect(),
    ))
}

/// State of a persisted Discord voice channel resolved at startup by the gateway adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejoinChannelState {
    Ready,
    NoPermissions,
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RejoinPlan {
    pub rejoin: Vec<VoicePresence>,
    pub forget: Vec<String>,
}

/// Creates a pure, fail-closed startup plan. Premium `stay_in_call` may recover without a marker;
/// all other guilds need the current one-shot marker. Deleted channels are forgotten; permission
/// failures remain recorded for a later legitimate restart, but are never joined blindly.
pub fn plan_rejoin(
    presences: Vec<VoicePresence>,
    scope: Option<&PlannedRejoinScope>,
    stay_in_call: impl Fn(&str) -> bool,
    channel_state: impl Fn(&str, &str) -> RejoinChannelState,
) -> RejoinPlan {
    let mut plan = RejoinPlan::default();
    for presence in presences {
        let marker_authorizes = match scope {
            Some(PlannedRejoinScope::All) => true,
            Some(PlannedRejoinScope::Guilds(guild_ids)) => guild_ids.contains(&presence.guild_id),
            None => false,
        };
        if !stay_in_call(&presence.guild_id) && !marker_authorizes {
            plan.forget.push(presence.guild_id);
            continue;
        }

        match channel_state(&presence.guild_id, &presence.channel_id) {
            RejoinChannelState::Ready => plan.rejoin.push(presence),
            RejoinChannelState::Gone => plan.forget.push(presence.guild_id),
            RejoinChannelState::NoPermissions => {}
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "vozen-rust-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        directory
    }

    fn presence(guild_id: &str, channel_id: &str) -> VoicePresence {
        VoicePresence {
            guild_id: guild_id.into(),
            channel_id: channel_id.into(),
            updated_at: 1,
        }
    }

    #[test]
    fn marker_is_one_shot_and_scoped_to_clean_shutdown_calls() {
        let directory = test_directory("marker");
        let path = directory.join("persisted").join("tts.db");
        write_planned_rejoin_marker(
            ["guild-a".into(), "guild-a".into(), "guild-b".into()],
            &path,
        )
        .expect("write marker");
        assert_eq!(
            fs::read_to_string(&path).expect("marker payload"),
            r#"{"guildIds":["guild-a","guild-b"]}"#
        );
        assert_eq!(
            consume_planned_rejoin_marker(&path, SystemTime::now()),
            Some(PlannedRejoinScope::Guilds(
                ["guild-a".into(), "guild-b".into()].into_iter().collect()
            ))
        );
        assert_eq!(
            consume_planned_rejoin_marker(&path, SystemTime::now()),
            None
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn marker_accepts_legacy_json_and_rejects_invalid_stale_or_future_markers() {
        let directory = test_directory("all");
        let path = directory.join("tts.db");
        for invalid in [b"".as_slice(), b"{", b"\xFF", br#"{"guildIds":[]}"#] {
            fs::write(&path, invalid).expect("write invalid marker");
            assert_eq!(
                consume_planned_rejoin_marker(&path, SystemTime::now()),
                None
            );
            assert!(!path.exists());
        }

        fs::write(&path, r#"{"guild_ids":["guild-a"]}"#).expect("write legacy marker");
        assert_eq!(
            consume_planned_rejoin_marker(&path, SystemTime::now()),
            Some(PlannedRejoinScope::Guilds(
                ["guild-a".into()].into_iter().collect()
            ))
        );

        fs::write(&path, r#"{"guildIds":["guild-a"]}"#).expect("write stale marker");
        let stale_now = SystemTime::now() + MAX_PLANNED_REJOIN_AGE + Duration::from_secs(1);
        assert_eq!(consume_planned_rejoin_marker(&path, stale_now), None);
        assert!(!path.exists());
        fs::write(&path, r#"{"guildIds":["guild-a"]}"#).expect("write future marker");
        let future_now = SystemTime::now()
            .checked_sub(Duration::from_secs(1))
            .expect("future timestamp");
        assert_eq!(consume_planned_rejoin_marker(&path, future_now), None);
        assert!(!path.exists());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn empty_shutdown_never_creates_a_marker() {
        let directory = test_directory("empty");
        let path = directory.join("persisted").join("tts.db");
        write_planned_rejoin_marker([], &path).expect("empty shutdown");
        assert!(!path.parent().expect("marker directory").exists());
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn competing_consumers_cannot_both_claim_a_marker() {
        let directory = test_directory("claim");
        let path = directory.join("tts.db");
        write_planned_rejoin_marker(["guild-a".into()], &path).expect("write marker");
        let now = SystemTime::now();
        let (first, second) = std::thread::scope(|threads| {
            let first = threads.spawn(|| consume_planned_rejoin_marker(&path, now));
            let second = threads.spawn(|| consume_planned_rejoin_marker(&path, now));
            (
                first.join().expect("first reader"),
                second.join().expect("second reader"),
            )
        });
        assert_eq!([first, second].into_iter().flatten().count(), 1);
        assert_eq!(
            fs::read_dir(&directory).expect("marker directory").count(),
            0
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn normal_calls_need_marker_while_premium_calls_can_recover() {
        let plan = plan_rejoin(
            vec![
                presence("premium", "voice-a"),
                presence("planned", "voice-b"),
                presence("crashed", "voice-c"),
                presence("gone", "voice-d"),
                presence("permissions", "voice-e"),
            ],
            Some(&PlannedRejoinScope::Guilds(
                ["planned".into()].into_iter().collect(),
            )),
            |guild_id| guild_id == "premium",
            |guild_id, _| match guild_id {
                "gone" => RejoinChannelState::Gone,
                "permissions" => RejoinChannelState::NoPermissions,
                _ => RejoinChannelState::Ready,
            },
        );

        assert_eq!(
            plan.rejoin,
            vec![
                presence("premium", "voice-a"),
                presence("planned", "voice-b")
            ]
        );
        // `permissions` is not premium and was not covered by the marker, so it is stale before
        // the channel permission check matters. Only an eligible `NoPermissions` row is retained.
        assert_eq!(plan.forget, vec!["crashed", "gone", "permissions"]);
    }
}
