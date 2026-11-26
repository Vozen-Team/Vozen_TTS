# Plan 029: Fix outbox timestamp units, oversized batches and stale replica triggers

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-runtime/src/postgres_outbox.rs crates/vozen-store/src/runtime_outbox.rs crates/vozen-store/src/runtime_batch.rs`
> Any change → compare excerpts; mismatch = STOP.

## Status

- **Priority**: P3 · **Effort**: S · **Risk**: LOW (staging mirror + metrics only) · **Depends on**: none · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

The SQLite `runtime_outbox_batch` table feeds the Postgres mirror and runtime metrics. Three bugs:
(1) runtime batches store `created_at` in **seconds** while replica-trigger rows store **milliseconds**, so
ordering by `created_at` is wrong and the admin "oldest pending" age reads as 1970; (2) a runtime batch over
the 256 KiB payload limit is rejected, restored into memory, merged with new data and rejected again forever —
metrics stop flowing and memory grows until restart; (3) replica triggers are created with
`CREATE TRIGGER IF NOT EXISTS`, so columns added by later migrations are never mirrored to Postgres.

## Current state

- `crates/vozen-runtime/src/postgres_outbox.rs:36-62` `persist_buffer`: `buffer.drain()`, then in
  `spawn_blocking` `enqueue_runtime_outbox(RuntimeOutboxEnqueue { batch_id, created_at: time::OffsetDateTime::now_utc().unix_timestamp(), payload })`
  (seconds, line 49) with `.map_err(|_| ())`; on any failure `buffer.restore(event)`.
- `crates/vozen-store/src/runtime_outbox.rs:11` `const MAX_PAYLOAD_BYTES: usize = 256 * 1024;`;
  `validate_batch` (~231-240) returns `StoreError::InvalidRuntimeOutboxPayload` when the payload is too large or not JSON.
  Trigger rows use `CAST(strftime('%s','now') AS INTEGER) * 1000` (line ~109, milliseconds).
- `runtime_outbox.rs:~60-112` `install_replica_triggers`: for disabled tables it already runs
  `DROP TRIGGER IF EXISTS runtime_replica_{table}_{ai|au|ad}`; for enabled tables it reads `PRAGMA table_info`
  and runs `CREATE TRIGGER IF NOT EXISTS …` with the column list baked into the payload SQL.
  Called from `crates/vozen-store/src/lib.rs:419-431` `configure_postgres_replica_outbox(true)` on every start when enabled.
- `crates/vozen-store/src/runtime_batch.rs:35-38` `pub struct RuntimeBatchEvent { payload: String, pending: PendingBatch }`;
  `drain` (136-149) takes the whole pending map; `restore` (151-160) merges back; `encode` (171+) serialises.

## Scope

**In scope**: the three files above and their tests.
**Out of scope**: Postgres-side SQL, existing rows (do not run UPDATE/DELETE on stored data), `contracts/postgres-replica.json`
unless the contract check demands it (then STOP).

## Steps

### Step 1: Milliseconds everywhere

In `postgres_outbox.rs:49` use milliseconds (`(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64`).
Document `RuntimeOutboxEnqueue::created_at` as "Unix milliseconds". Do **not** add a range guard in
`validate_batch`: existing tests (`runtime_outbox.rs:260-310`) enqueue small values like `0`, `1`, `20`.

### Step 2: Never loop on an oversized batch

Add `RuntimeBatchEvent::split(self, max_bytes) -> Vec<RuntimeBatchEvent>` in `runtime_batch.rs` that splits the
pending metrics/speech maps (halving recursively) until every encoded payload is ≤ `max_bytes`; export the limit
from `runtime_outbox.rs` as `pub const`. In `persist_buffer`, split before enqueueing, enqueue each part, and only
`restore` the parts that failed for a reason other than `InvalidRuntimeOutboxPayload` (log and drop those that are
still invalid). Keep the error text free of payload contents.

### Step 3: Recreate replica triggers

In `install_replica_triggers`, for enabled tables run `DROP TRIGGER IF EXISTS` for the three triggers before
creating them, so the column list always matches the current `PRAGMA table_info`.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-store runtime_ --locked` and
`bash advisor-plans/docker-check.sh cargo test -p vozen-runtime postgres_outbox --locked` → pass.

## Test plan

- `runtime_batch.rs` tests (line 212+): a pending batch whose encoding exceeds a small `max_bytes` splits into parts that
  each fit, and the parts' counters sum to the original.
- `runtime_outbox.rs` tests (line 251+): after enabling replication, `ALTER TABLE … ADD COLUMN extra`, re-enable, then an
  insert produces a payload containing `extra`.
- `postgres_outbox.rs` tests (line 365+): a persisted runtime batch has `created_at` within 5 s of the current
  Unix time in **milliseconds** (extract the timestamp into a small function if needed to test it).

## Done criteria

- [ ] `grep -n "unix_timestamp()" crates/vozen-runtime/src/postgres_outbox.rs` → no output
- [ ] targeted tests pass; `node tools/check-rust-replica-contract.mjs` and `node tools/check-rust-contracts.mjs` exit 0
- [ ] `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] only in-scope files changed; one commit, e.g. `Keep runtime outbox batches bounded and timestamps in milliseconds`

## STOP conditions

- `created_at` in seconds is relied on by the Postgres flush or a contract file.
- The replica contract check fails after Step 3.

## Maintenance notes

- Rows already stored in seconds drain normally; no data migration is needed because the outbox is consumed and cleared.
