# Plan 002: Complete privacy tombstones in the Postgres mirror

> **Drift check**: `git diff --stat 99eef7d..HEAD -- crates/vozen-store/src/data_lifecycle.rs crates/vozen-runtime/src/postgres_outbox.rs crates/vozen-store/src/gcloud_usage.rs crates/vozen-store/src/premium.rs`

> **Reconciliation (2026-08-01)**: The working tree contains uncommitted user changes in
> `data_lifecycle.rs` and `postgres_outbox.rs` that already enqueue privacy tombstones and add a
> generic remote handler. Those changes are preserved and are not part of the executor worktree.
> This plan therefore executes the missing shared purge-column semantics and regression coverage
> against the clean baseline, with the final review explicitly checking that the delta composes with
> the local changes without deleting retained paid-entitlement data.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `99eef7d`, 2026-08-01

## Why this matters

Local `/privacy erase` deletes `kofi_supporter` rows by `discord_id` and personal/pass `gcloud_usage`
rows by `key`. The Postgres tombstone handler only iterates `USER_ERASE_TABLES`, which excludes both
tables, so mirror mode can retain identifiers and usage after a user erasure request.

## Current state

- `crates/vozen-store/src/data_lifecycle.rs:36-55` lists generic `user_id` tables.
- `crates/vozen-store/src/data_lifecycle.rs:92-102` performs two explicit non-`user_id` deletes.
- `crates/vozen-runtime/src/postgres_outbox.rs:268-301` generates SQL using only `user_id` or `guild_id`.
- `CONTRIBUTING.md` requires every stored user datum to have a deletion path and `PRIVACY.md` must stay aligned.

## Scope

**In scope**: the shared privacy-delete specification, SQLite/Postgres application, migrations/contracts if
needed, and tests. **Out of scope**: deleting payment entitlements or HMAC anti-abuse ledgers that the
current policy intentionally retains.

## Steps

### Step 1: Model explicit purge columns

Replace the implicit table-name-only loop with a reviewed table/column specification that represents
`user_id`, `discord_id`, and `key + scope IN ('user','pass')`. Reuse it for local and remote deletion.

**Verify**: `rg -n "kofi_supporter|gcloud_usage|USER_ERASE_TABLES|privacy" crates/vozen-store crates/vozen-runtime` shows both paths share the same reviewed semantics.

### Step 2: Add mirror privacy tests

Create fixture rows in local and remote test schemas for a supporter, user gcloud usage, pass gcloud
usage, and guild gcloud usage. Apply a user tombstone and assert only the first three are removed;
guild-level usage remains according to policy. Test replay/idempotency of the tombstone.

**Verify**: `cargo test -p vozen-runtime postgres --locked` and `cargo test -p vozen-store data_lifecycle --locked` pass. If a live Postgres test is unavailable, add a deterministic SQL-plan/unit test and report the environment limitation.

### Step 3: Run gates

**Verify**: `cargo fmt --check`; `cargo clippy --workspace --all-targets --locked -- -D warnings`; `cargo test --workspace --locked`; `node tools/check-rust-replica-contract.mjs` — all exit 0.

## Done criteria

- [ ] Local and Postgres erasure delete the same personal rows.
- [ ] Guild-level retention and paid entitlement retention remain unchanged.
- [ ] Tests prove supporter, user usage, pass usage, and guild usage behavior.
- [ ] No secrets or production database values appear in tests or logs.

## STOP conditions

- Stop if a retained table is legally required for payment/accounting; document the decision instead of deleting it.
- Stop if the Postgres schema does not contain the corresponding table or column.

## Maintenance notes

Every new personal table must be added to this explicit specification and tested in both local and
mirror deletion paths.


## Current Rust Google HD quota retention (2026-10-28)

The approved Rust implementation removes raw user/pass identifiers on an actual future
privacy erase while retaining the current UTC month's character count as a domain-separated
HMAC of scope, month and user ID. New personal/pass counter writes use that HMAC; reads
and reservations include any existing raw row, without a startup data migration. The
existing stable VOTE_REDEMPTION_SECRET and pinned fingerprint are reused, with no new
schema, dependency or configuration variable. Missing or mismatched keys fail closed.
The daily cleanup removes HMAC counters after month end; existing legacy raw-row, guild
and global retention remains unchanged. Counter rows contain no text or audio. Pending
user-data invalidation, paid grants and the global daily budget remain intact. Mirror
tombstones still delete raw user/pass rows; no existing database is rewritten by deployment.
