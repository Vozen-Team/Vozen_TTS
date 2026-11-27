# Plan 026: Isolate gateway sink failures and per-guild game dispatch

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-discord/src/gateway_composite.rs crates/vozen-runtime/src/core_voice_sink.rs`
> Plans 024/025 legitimately changed `core_voice_sink.rs`; search by the quoted code.

## Status

- **Priority**: P2 · **Effort**: M · **Risk**: MED · **Depends on**: 024, 025 · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

All gateway features are composed in `CompositeGatewayEventSink`, which forwards each event to every
sink **in order and stops at the first `Err`**. A transient failure in one feature silently drops the
event for every later feature. Concrete cases: a failed game-thread post during
`on_voice_state_update` stops live transcription from releasing its capacity slot when the bot leaves;
a failed planned-session recovery in `on_ready` skips the Premium sink's entitlement sync at startup.
Separately, one 250 ms game tick task processes **every guild's** game events inline, including speech
synthesis and REST calls, so a slow guild delays round deadlines in all other guilds.

## Current state

- `crates/vozen-discord/src/gateway_composite.rs` — doc comment (lines 1-5): "Each slice still validates
  and claims only its own Discord events. Dispatching all sinks is safe…". Every method is:
  ```rust
  for sink in &self.sinks {
      sink.on_ready(context.clone()).await?;
  }
  Ok(())
  ```
  (`on_ready` 27-31, `on_entitlement_change` 34-41, `on_guild_create` 44-48, `on_guild_create_details`
  51-60, `on_message` 63-71, `on_reaction_add` 74-83, `on_interaction` 86-95, `on_voice_state_update`
  98-108, `on_guild_delete` 111-115). It has no tests. Built once at
  `crates/vozen-runtime/src/main.rs:2836`.
- `crates/vozen-runtime/src/core_voice_sink.rs:497-526` `start_tick`: one `tokio::spawn` loop;
  every 250 ms it calls `coordinator.advance_with_guild(now)` and then, **sequentially**, `let _ =
runtime.dispatch_event(&context, event, speech_allowed, Some(&guild_id)).await;` for each `(guild_id, event)`.
  `dispatch_event` can synthesise speech and call Discord REST.

## Scope

**In scope**: `crates/vozen-discord/src/gateway_composite.rs` (+ a new `#[cfg(test)] mod tests`),
`crates/vozen-runtime/src/core_voice_sink.rs` (`start_tick` and a new per-guild dispatcher helper only).

**Out of scope**: the order of sinks in `main.rs`; individual sinks' internals; running sinks concurrently
(deferred — see Maintenance notes).

## Steps

### Step 1: Call every sink even after an error

Rewrite each composite method to call all sinks, log each failure with the method name and sink index
(`eprintln!("[gateway] sink {index} failed in on_message")`), remember whether any failed, and return
`Err(GatewayEventDispatchError)` at the end only if at least one failed (so callers' logging is unchanged).
Keep the sequential order. Update the module doc comment to state the new contract.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-discord gateway_composite --locked` → pass.

### Step 2: Tests for the composite

Add `mod tests` with two fake sinks (implement `GatewayEventSink` with counters; check the trait's default
methods so the fakes stay small): first returns `Err` from `on_voice_state_update` / `on_ready`, second
counts calls. Assert the second sink is still called and the composite returns `Err`. If constructing a
serenity `Context` in tests is impossible, test through `on_guild_create` / `on_guild_delete`, which take
only `&str`, and say so in the commit body.

### Step 3: Per-guild ordered game dispatch

Replace the inline loop body in `start_tick` with a per-guild queue: a
`Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<(GameManagerEvent, bool)>>>` on the sink; for each
`(guild_id, event)` send to that guild's sender, creating the sender and a worker task on first use. The
worker loops `while let Some((event, speech)) = rx.recv().await { let _ = runtime.dispatch_event(…).await; }`,
so events stay ordered per guild while guilds run independently. When a `Finished`/`Stopped` event has been
dispatched and the coordinator no longer has the guild active, drop that guild's sender so the worker ends.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-runtime core_voice_sink --locked` → pass.

## Test plan

- Composite: Step 2 tests.
- Game dispatch: unit-test the map helper (same guild → same sender; two guilds → two senders); if feasible,
  a test with a fake slow dispatch for guild A shows guild B's event is processed first.

## Done criteria

- [ ] `grep -c "await?;" crates/vozen-discord/src/gateway_composite.rs` → 0
- [ ] targeted tests pass; `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] only in-scope files changed; one commit, e.g. `Keep gateway features independent when one of them fails`

## STOP conditions

- A sink relies on the short-circuit (e.g. a later sink must not run after an earlier one failed) — search
  `main.rs` around the sink list (lines ~2680-2836) and comments mentioning order; report if found.
- `GameManagerEvent` is not `Send` or `dispatch_event` cannot be called from a spawned task without
  restructuring `CoreVoiceGatewaySink`.

## Maintenance notes

- Running `on_message` sinks concurrently (so automatic translation does not wait behind TTS synthesis) is a
  possible follow-up; it changes timing guarantees and is deliberately not in this plan.
