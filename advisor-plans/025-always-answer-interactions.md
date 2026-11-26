# Plan 025: Always answer component and slash interactions

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-runtime/src/voice_preference_sink.rs crates/vozen-runtime/src/setup_wizard.rs crates/vozen-runtime/src/translation_text_sink.rs crates/vozen-runtime/src/file_export_sink.rs crates/vozen-runtime/src/core_voice_sink.rs`
> Plan 024 legitimately changed `core_voice_sink.rs` (game functions only). Search by the quoted code.

## Status

- **Priority**: P2 · **Effort**: M · **Risk**: LOW · **Depends on**: 024 · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

Discord shows "This interaction failed" when a bot never answers within 3 s, and leaves "thinking…"
for 15 minutes when a deferred reply is never edited. Several paths do that today: expired
`/voice config`, cast and randomizer panels return without any response (the intended "expired"
reply is unreachable); adding a translation mapping makes four REST calls before the first reply; a
couple of errors after a defer skip the edit. The setup wizard serialises **every guild's** clicks
behind one process-wide lock held across REST calls and a voice join, so one slow guild makes other
guilds' clicks time out — and a timeout after a successful Confirm tells the admin to start over.

## Current state

- `crates/vozen-runtime/src/voice_preference_sink.rs` (at `c1dd4a9`): `prune_config_sessions`
  (line 120) retains sessions with `now - issued_at <= CONFIG_TTL_MS`; the component handler (from
  line 473) prunes, looks up, and at line 484 `let Some(mut session) = session else { return Ok(true); };`
  — so the expiry branch that renders `voice.config.expired` (~line 501) can never run.
- `crates/vozen-runtime/src/core_voice_sink.rs` cast handler (~3255-3282): `prune_cast_sessions` then
  `let Some(session) = sessions.get(&session_id) else { return Ok(true); };` and the invalid/expired branch
  also `return Ok(true)` with no response. Randomizer handler (~2448-2480): same pattern
  (`prune_randomizer_sessions`, missing → `Ok(true)`). Only `voice.config.expired`,
  `voice.browse.expired`, `redeem.expired` keys exist; **do not add new i18n keys**.
- `crates/vozen-runtime/src/translation_text_sink.rs:462-470` awaits `channels_can_be_mapped(…)?`
  (four sequential REST calls, lines 287-312) **before** `respond_ephemeral` (lines 122-139 send the first
  response); any `?` there leaves the interaction unanswered.
- `crates/vozen-runtime/src/file_export_sink.rs:238`
  `PrivateExportWorkspace::new().map_err(|_| GatewayEventDispatchError)?;` and line 267
  `workspace.close().map_err(…)?;` run after `defer_ephemeral` (line 204) without editing the reply.
- `core_voice_sink.rs` ~4083-4123: after `defer_ephemeral`/`defer`, `executor.execute(…).await.map_err(…)?;`
  returns without editing the deferred reply.
- `crates/vozen-runtime/src/setup_wizard.rs:443-444` `actions: tokio::sync::Mutex<()>` ("Serialize
  lookup/create/confirm so concurrent clicks cannot create duplicate channels"); line 590
  `let _guard = self.actions.lock().await;` inside `apply_action`; line 1010-1014 wraps `apply_action` in a
  20 s `tokio::time::timeout`, and on timeout (lines 1021-1045) shows `wizard.reopen`. Confirm (lines
  ~908-943) saves the config and marks the session confirmed, then sends the completion message.

## Scope

**In scope**: the five files above (only the functions named) and their tests.
**Out of scope**: i18n catalogues and `contracts/`; the setup wizard's public completion-message buttons
(kept as designed); plan 024's game functions.

## Steps

### Step 1: Expired panels answer

- `/voice config`: when the session is missing **or** expired, respond with `UpdateMessage` using
  `voice.config.expired` and no components (reuse the existing branch body). Check expiry before pruning,
  or treat "missing after prune" the same as expired.
- Cast and randomizer: when the session is missing/invalid, respond with
  `CreateInteractionResponse::UpdateMessage(CreateInteractionResponseMessage::new().components(Vec::new()))`
  (keeps the content, removes the dead controls). Keep returning `Ok(true)`.

### Step 2: Defer before slow checks

In `translation_text_sink.rs`, call `command.defer_ephemeral(context)` before `channels_can_be_mapped`, and
switch the replies on that path (success, validation failure, and any error) to `edit_response`. Make
`channels_can_be_mapped` errors produce an edited error message instead of `?`.

### Step 3: No silent errors after a defer

- `file_export_sink.rs` lines 238 and 267: on error, edit the deferred response with `ttsFile.failed` and
  return `Ok(())` (drop the permit as the existing failure branch does).
- `core_voice_sink.rs` executor error after a defer: edit the deferred response with `error.generic`.

### Step 4: Per-guild setup lock and honest confirm

Replace the single `actions` mutex with a per-guild map, e.g.
`Mutex<HashMap<GuildId, Arc<tokio::sync::Mutex<()>>>>` (create on demand; it is fine to never shrink it, or
remove the entry when the session ends). Take the guild's lock in `apply_action`. In the timeout branch,
if the session is already `confirmed`, show the normal completion edit instead of `wizard.reopen`.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-runtime setup_wizard --locked`,
`… voice_preference_sink --locked`, `… translation_text_sink --locked`, `… file_export_sink --locked` → pass.

## Test plan

- `setup_wizard.rs` tests (18 existing, from line 1101): two different guilds' locks are independent (unit-test
  the lock-map helper); same guild returns the same `Arc`.
- `voice_preference_sink.rs` tests (from line 1398): a session older than `CONFIG_TTL_MS` produces the expired
  response path (extract the "is this session usable" decision into a small pure function if needed).
- Keep the existing duplicate-channel protection test(s) green.

## Done criteria

- [ ] `grep -n "actions: tokio::sync::Mutex<()>" crates/vozen-runtime/src/setup_wizard.rs` → no output
- [ ] targeted tests pass; `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] `node tools/check-rust-contracts.mjs` exit 0 (no new i18n keys)
- [ ] only in-scope files changed; one commit, e.g. `Answer expired panels and slow setup steps instead of failing silently`

## STOP conditions

- The duplicate-channel guarantee depends on cross-guild serialisation (e.g. a shared category) — report.
- Any fix needs a new localisation key.

## Maintenance notes

- New component handlers should follow the rule: every early `return Ok(true)` must have sent a response.
- Open PR Vozen-Team/Vozen_TTS#67 (`fix/tts-voice-engine-language-20261124`) also edits
  `voice_preference_sink.rs` (engine options in the `/voice config` panel). Whichever merges second
  needs a small rebase in that file; the session lookup changed here does not overlap its hunks.
