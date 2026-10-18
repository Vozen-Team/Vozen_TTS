# Stripe plan changes

The authenticated account flow retains the existing six Prices and three Products.
It does not enable global price switching in the Stripe customer portal or create
a unified catalog. Public-site new-checkout availability is a separate policy.

## Policy and authority

- Upgrades use `always_invoice` and `pending_if_incomplete`. Only a verified paid
  invoice projects access; HTTP success, checkout metadata and subscription updates
  do not prove a newly purchased tier.
- Lower tiers and yearly-to-monthly changes start at the paid period end through
  a Subscription Schedule. Plus has no server licenses. A smaller server pass keeps
  the oldest saved links up to its limit, ordered by activation time and guild ID.
  Excess links are saved but not authorized. Independent benefits keep their own
  expiry and legacy financial aggregates are not reassigned to another subscription.
- A completed schedule created by this flow can be released when confirming another
  change. An owned pending change can instead be removed explicitly with its own
  short-lived signed token. That releases only the schedule, not the subscription,
  and leaves paid rights and renewal settings unchanged. Foreign schedules, unpaid
  or complex subscriptions fail closed. To cancel renewal after removing a pending
  change, use billing management. This addresses the [Stripe portal limitation](https://docs.stripe.com/customer-management#limitations)
  that scheduled updates block portal subscription cancellation.

POST `/api/billing/plans`, `/api/billing/change/preview`, and
`/api/billing/change/confirm`, and `/api/billing/change/cancel` require the verified Discord account and matching
stored/provider ownership. Preview performs no subscription write. A five-minute
signed token binds the current item, period, target Price, quote and identity;
confirmation rechecks them. Stripe request idempotency is token-specific. On an
uncertain network outcome, check billing for a pending payment before retrying.

## Data and rollback

`stripe_paid_entitlement` is additive and records the latest paid invoice/period per
subscription. Timestamps in SQLite are milliseconds. See `PRIVACY.md` for retained
financial records. Existing direct/Ko-fi/vote/Discord benefits are not deleted.

The preceding image opens the additive schema, but does not read this new paid
projection. An immediate image rollback before any new paid-ledger writes retains
the previous behavior. After billing writes, image rollback alone can omit new
rights: preserve the database and roll forward or reconcile from Stripe under an
approved recovery procedure. Do not restore an old SQLite snapshot over new writes
or edit customer subscriptions as a health check.

## Executed QA scope (2026-10-18)

Linux API/store tests cover signed delivery, replay/order conflicts, invoice Price
authority, concurrent/stale confirmations, ownership/mode boundaries, exact seat
limits, legacy independent benefits, failure atomicity and schedule cleanup.

An isolated runtime and one newly created Stripe test-clock subscription exercised:
initial invoice before checkout; failed then successful upgrade; Max to Premium
and Premium to Plus at actual test-clock renewal boundaries; duplicate and late
invoices; custom browser confirmation; monthly and annual upgrades whose displayed
quotes equaled paid test invoices. Provider objects were replayed into the local
signed endpoint with isolated credentials; this is not proof of live webhook
delivery or settlement. No pre-existing customer subscription was changed.

The final plan-change module, including pending-change removal, measured 92.4% line
coverage in a fresh isolated Linux profile run.
Branch instrumentation was not enabled. Production charges are deliberately not a
QA gate. Record the final CI, deployed revision, health, unchanged live-subscription
fingerprint and public asset verification in the operational release receipt.
