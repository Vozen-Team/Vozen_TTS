# Plan 024: Clean up games whose start or finish fails

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-runtime/src/core_voice_sink.rs crates/vozen-core/src/game_chess.rs`
> Any change → compare excerpts; mismatch = STOP.

## Status

- **Priority**: P2 · **Effort**: S · **Risk**: LOW (error paths only) · **Depends on**: none · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

`/game` registers the game in the coordinator **before** posting its first messages. If that post
fails (no send permission in the fallback channel, Discord error), the user gets no answer, the
deferred reply stays on "thinking…", and an invisible game keeps running: every later `/game play`
says "already active" and the 250 ms tick keeps driving and even speaking it. At the end of a game,
the first failed post (thread deleted, permission lost) aborts the handler before scores are saved,
so the scores are lost and the thread/engine state leaks. Separately, chess rejects a legal castle
that gives check when typed in lowercase (`o-o+`).

## Current state (`crates/vozen-runtime/src/core_voice_sink.rs`)

- Start: lines 643-667 create the thread (`create_game_thread`, falls back to the parent channel at
  646-648) and call `coordinator.lock()…start(GamePlayRequest { … })`. Lines 688-691 insert
  `session_engines`. Lines 693-703 `self.send_actions(…).await?;` — an error here returns with the game
  still registered and no reply. The coordinator has `end_guild(&mut self, guild_id)`
  (`crates/vozen-discord/src/game_coordinator.rs:183`).
- `/game play`: lines 1061-1077 `defer_ephemeral` then `self.start_game(…).await?;` — on error the
  deferred reply is never edited.
- `gameStart:` button: lines 1431-1441 `defer` and `.remove(&key)` the pending pick, then lines
  1469-1481 `self.start_game(…).await?;` — on error the picker stays and the next click says the pick timed out.
- Finish: lines 412-489 (`GameManagerEvent::Finished`): `send_actions(…).await?` first, then
  `let _ = store.persist_game_scores(…)` (428-430, error ignored), standings `to_user` lookups,
  `send_rendered_actions(…).await?`, winner post `send_content(…).await?`, `schedule_thread_delete`,
  and only then `session_engines.remove`. Callers discard the error (`let _ = runtime.dispatch_event(…)` at 521-523).
- `cleanup_forced` lines 361-376: posts `game.thread.ended` with `?` **before** `schedule_thread_delete`.
- Generic error text exists: key `error.generic` in `contracts/voice-response-i18n.json`; render it with
  `self.render("error.generic", Some(&locale), guild_locale, &BTreeMap::new())`.
- Chess, `crates/vozen-core/src/game_chess.rs:296-311` `parse_move`: uppercases only when the whole text
  is exactly `o-o`/`o-o-o`; `looks_like_move` (287-288) already strips `+`/`#` before checking.

## Scope

**In scope**: `crates/vozen-runtime/src/core_voice_sink.rs` (the functions above + tests),
`crates/vozen-core/src/game_chess.rs` (`parse_move` + tests).

**Out of scope**: `game_coordinator.rs` / `game_manager.rs` logic, the game tick loop structure (plan 026),
i18n catalogues.

## Steps

### Step 1: Roll back a failed start

In `start_game`, wrap everything after a successful `coordinator.start` (`session_engines` insert,
`send_actions`, thread notice) so that on error it: calls `coordinator.end_guild(&guild_id)`, removes
the `session_engines` entry, deletes the created thread (same best-effort delete as lines 671-674), and
then returns the error.

### Step 2: Always answer the user

- `/game play` (1061-1077): on `start_game` error, edit the deferred response with `error.generic`
  (best-effort) and return `Ok(true)`.
- `gameStart:` (1469-1481): same, editing the original interaction response with `error.generic` and no components.

### Step 3: Save first, post best-effort

In the `Finished` arm: persist scores first and log (`eprintln!("[game] persist scores failed: …")`)
instead of `let _`; remove `session_engines` early; turn every Discord post (`send_actions`,
`send_rendered_actions`, winner `send_content`) into log-and-continue; always call
`schedule_thread_delete` when `parent_channel_id` is `Some`. In `cleanup_forced`, schedule the delete
even if the post fails.

### Step 4: Lowercase castling with check

In `parse_move`, strip a trailing `+`/`#` from `lower` before the castling comparison and uppercase the
castling part (`o-o+` → `O-O+`, `o-o-o#` → `O-O-O#`).

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-core game_chess --locked` and
`bash advisor-plans/docker-check.sh cargo test -p vozen-runtime core_voice_sink --locked` → pass.

## Test plan

- `game_chess.rs` tests (model after existing castling tests near line 335+): a position where `O-O`
  gives check accepts `o-o+`; `o-o-o#` parses when legal.
- `core_voice_sink.rs` tests (line 4427+): if there is a seam to make `send_actions` fail, test that after a
  failed start `coordinator.active(guild)` is false. If no seam exists, extract the rollback into a small
  function taking the coordinator and engines map and unit-test that instead.

## Done criteria

- [ ] `grep -n "let _ = store.persist_game_scores" crates/vozen-runtime/src/core_voice_sink.rs` → no output
- [ ] targeted tests pass; `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] only the two in-scope files changed; one commit, e.g. `Roll back failed game starts and keep scores when end posts fail`

## STOP conditions

- `end_guild` has side effects beyond removing the session (e.g. posts) that would double-post on rollback.
- Fixing the deferred reply needs a new i18n key.

## Maintenance notes

- Plan 026 moves game dispatch per guild; keep the "save first, post best-effort" order when it does.
