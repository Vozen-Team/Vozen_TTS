# Plan 030: Prevent duplicate Stripe subscriptions, stale statuses and partial Ko-fi orders

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`. Money paths: when in doubt, STOP.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-api/src/stripe_api.rs crates/vozen-store/src/stripe.rs crates/vozen-core/src/kofi.rs crates/vozen-api/src/kofi_webhook.rs`
> Any change → compare excerpts; mismatch = STOP.

## Status

- **Priority**: P3 · **Effort**: M · **Risk**: MED (checkout behaviour) · **Depends on**: none · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

(1) Checkout never passes an existing Stripe `customer` and never checks for an active subscription, so a
user can pay twice for the **same** plan, every purchase creates a new Stripe customer, and the billing portal
opens only the most recently _updated_ subscription's customer — the user may be unable to cancel the other one.
(2) `customer.subscription.updated/deleted` events overwrite `status` unconditionally, and a missing status
defaults to `"active"`, so a late `updated` after `deleted` revives a cancelled subscription in our records
(access itself comes from invoices, so this is a records/admin bug). (3) A Ko-fi shop order with quantity > 1 or
several mapped items grants only the first item, returns 200, and Ko-fi never retries.

## Current state

- `crates/vozen-api/src/stripe_api.rs:355-376` (checkout form) and `:467-487` (second checkout form) send
  `mode=subscription`, `line_items[0][price]`, `client_reference_id`, metadata `discord_user_id`/`plan`/`seats` —
  no `customer`, no existing-subscription check. `stripe_api.rs:642-666` `portal` uses
  `store.stripe_customer_for_user(&user.id)`.
- `crates/vozen-store/src/stripe.rs:191-200`:
  ```sql
  SELECT customer_id FROM stripe_subscription WHERE user_id = ?1 ORDER BY updated_at DESC LIMIT 1
  ```
  Other helpers: `stripe_subscriptions_for_user` (line 87), `stripe_subscription` (167).
- `stripe.rs:371-399` `SubscriptionUpdated { subscription_id, period_end, status }` →
  `UPDATE stripe_subscription SET current_period_end=?2, status=?3, updated_at=?4 WHERE subscription_id=?1` (no guard).
- `stripe_api.rs:1123-1127`: `status: object.get("status").and_then(Value::as_str).unwrap_or("active").into()`.
- `crates/vozen-core/src/kofi.rs:70-95` reads only `variation_name` and `direct_link_code` per shop item (no
  `quantity`); `kofi.rs:194-207` `map_kofi_to_grant` returns the **first** mapped code.
  `crates/vozen-api/src/kofi_webhook.rs:109-123` reports to the operator (`on_unmapped_shop`) only when nothing maps.
- Frontend: `site/js/main-v51.js` calls checkout and has no special handling for HTTP 409; do **not** edit `site/`.
- Tests: `stripe.rs` `mod tests` (line 526+, 11 tests), `crates/vozen-api/tests/billing_webhooks.rs`,
  `crates/vozen-api/tests/plan_changes.rs`, `kofi.rs` tests (line 323+), `kofi_webhook.rs` tests (line 148+).

## Scope

**In scope**: `stripe_api.rs`, `stripe.rs`, `kofi.rs`, `kofi_webhook.rs`, their unit tests, and
`crates/vozen-api/tests/billing_webhooks.rs`.
**Out of scope**: `site/`, `stripe_api/plan_changes.rs`, SQLite schema/contracts, prices configuration.

## Steps

### Step 1: Reuse the customer, block same-plan duplicates

- Store: add `active_stripe_subscription_for_plan(user_id, plan, now) -> Option<(subscription_id, customer_id)>`
  (status in `active`/`trialing`/`past_due` and `current_period_end > now`), and make
  `stripe_customer_for_user` prefer an active subscription's customer
  (`ORDER BY (status IN ('active','trialing','past_due')) DESC, updated_at DESC`).
- Both checkout handlers: if an active subscription exists for **the same `input.plan`**, return
  `409` JSON `{"error":"subscription_exists"}` (use the same `json_response` helper as the portal's 404).
  Otherwise, if `stripe_customer_for_user` returns a customer, add `("customer", id)` to the form.
  A user with Plus may still buy a different plan (e.g. a server plan).

### Step 2: Monotonic subscription status

In `SubscriptionUpdated` handling: never change a row whose status is `canceled` to anything else; never move
`current_period_end` backwards. In `stripe_api.rs:1123-1127`, a missing `status` must not default to `"active"` —
keep the stored status (make the field `Option<String>` and skip the status column when `None`).

### Step 3: Ko-fi quantities

Parse `quantity` (default 1, clamp 1..=100) per shop item. In `map_kofi_to_grant`: if all mapped units are the
same product, multiply `days` by total quantity; if mapped units span different products, return the first grant
**and** make the webhook report the order to the operator through the existing `on_unmapped_shop` reporter (or a
sibling reporter with the same redaction rules) so it can be completed manually.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-store stripe --locked`,
`bash advisor-plans/docker-check.sh cargo test -p vozen-api --locked`,
`bash advisor-plans/docker-check.sh cargo test -p vozen-core kofi --locked` → pass.

## Test plan

- `stripe.rs`: two subscriptions (A active, B canceled but updated later) → portal customer is A's; an `updated`
  event with status `active` after `deleted` leaves `canceled`; period end never decreases.
- `billing_webhooks.rs` (model after its existing webhook tests): checkout for a plan the user already has → 409;
  for a different plan → Stripe form includes `customer`.
- `kofi.rs`: one item with `quantity: 3` → days × 3; two different mapped products → first grant + report flag.

## Done criteria

- [ ] `grep -n 'unwrap_or("active")' crates/vozen-api/src/stripe_api.rs` → no output
- [ ] targeted tests pass; `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] `node tools/check-rust-contracts.mjs` exit 0
- [ ] only in-scope files changed; one commit, e.g. `Stop duplicate Stripe subscriptions and honour Ko-fi quantities`

## STOP conditions

- The two checkout handlers sell plans whose "same plan" identity is not simply `input.plan` (e.g. seats or
  per-guild plans where two subscriptions of the same plan are legitimate) — report before blocking anything.
- Passing `customer` conflicts with an existing Stripe option in the form (e.g. `customer_email`).
- Ko-fi payloads in tests show `quantity` with a different name or type.

## Maintenance notes

- Follow-up for the site: show a friendly "you already have this plan — manage it in the portal" message on 409.
