//! Minimal Stripe persistence boundary.
//!
//! Stripe is the payment processor of record. Vozen stores only provider identifiers,
//! entitlement metadata and idempotency evidence; card data, email and addresses never enter
//! this database.

use rusqlite::{OptionalExtension, params};

use crate::{SqliteStore, StoreError};

const ADMIN_STRIPE_PURCHASE_SCAN_CAP: i64 = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeSubscription {
    pub subscription_id: String,
    pub customer_id: String,
    pub user_id: String,
    pub plan: String,
    pub seats: i64,
    /// Unix timestamp in milliseconds. Provider seconds are normalized at the API boundary.
    pub current_period_end: i64,
    pub status: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeSubscriptionInput {
    pub subscription_id: String,
    pub customer_id: String,
    pub user_id: String,
    pub plan: String,
    pub seats: i64,
    /// Unix timestamp in milliseconds. Callers must pass the normalized internal unit.
    pub current_period_end: i64,
    pub status: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripeEventInput {
    Checkout {
        subscription_id: String,
        customer_id: String,
        user_id: String,
        plan: String,
        seats: i64,
    },
    InvoicePaid {
        subscription_id: String,
        period_end: Option<i64>,
    },
    InvoicePaidPlan {
        subscription_id: String,
        period_end: Option<i64>,
        paid: StripePaidPlan,
    },
    SubscriptionUpdated {
        subscription_id: String,
        period_end: Option<i64>,
        status: String,
    },
    InvoiceFailed {
        subscription_id: String,
    },
    Ignored,
}

/// Validated paid invoice line, never subscription metadata or an unpaid update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripePaidPlan {
    pub invoice_id: String,
    pub issued_at: i64,
    pub starts_at: i64,
    pub plan: String,
    pub seats: i64,
    /// Set only after an authenticated canonical lookup resolves second-precision ties.
    pub canonical_previous_invoice: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripeEventApplyOutcome {
    Applied,
    AlreadyApplied,
}

impl SqliteStore {
    pub fn stripe_subscriptions_for_user(&self, user_id: &str) -> Result<Vec<String>, StoreError> {
        let mut statement=self.connection().prepare("SELECT subscription_id FROM stripe_subscription WHERE user_id=?1 ORDER BY updated_at DESC LIMIT 11")?;
        statement
            .query_map([user_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
    pub fn stripe_paid_invoice_stamp(
        &self,
        subscription_id: &str,
    ) -> Result<Option<(String, i64, String)>, StoreError> {
        self.connection().query_row(
            "SELECT invoice_id,issued_at,plan FROM stripe_paid_entitlement WHERE subscription_id=?1",
            [subscription_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional().map_err(StoreError::from)
    }
    /// Returns the most recently updated Stripe subscriptions for the owner console.
    ///
    /// Stripe remains the payment processor of record; this is only a small, provider-backed
    /// projection used by the private admin panel. Customer identifiers are kept out of the
    /// response layer, while the subscription id lets the owner correlate a row with Stripe.
    pub fn list_stripe_subscriptions(
        &self,
        cap: i64,
    ) -> Result<Vec<StripeSubscription>, StoreError> {
        let cap = cap.clamp(1, ADMIN_STRIPE_PURCHASE_SCAN_CAP);
        let mut statement = self.connection().prepare(
            "SELECT subscription_id, customer_id, user_id, plan, seats,
                    current_period_end, status, updated_at
             FROM stripe_subscription
             ORDER BY updated_at DESC, subscription_id DESC
             LIMIT ?1",
        )?;
        statement
            .query_map([cap], |row| {
                Ok(StripeSubscription {
                    subscription_id: row.get(0)?,
                    customer_id: row.get(1)?,
                    user_id: row.get(2)?,
                    plan: row.get(3)?,
                    seats: row.get(4)?,
                    current_period_end: row.get(5)?,
                    status: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn upsert_stripe_subscription(
        &self,
        input: &StripeSubscriptionInput,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO stripe_subscription
             (subscription_id, customer_id, user_id, plan, seats, current_period_end, status, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(subscription_id) DO UPDATE SET
               customer_id=excluded.customer_id,
               user_id=excluded.user_id,
               plan=excluded.plan,
               seats=excluded.seats,
               current_period_end=excluded.current_period_end,
               status=excluded.status,
               updated_at=excluded.updated_at",
            params![
                input.subscription_id,
                input.customer_id,
                input.user_id,
                input.plan,
                input.seats,
                input.current_period_end,
                input.status,
                input.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn stripe_subscription(&self, id: &str) -> Result<Option<StripeSubscription>, StoreError> {
        self.connection()
            .query_row(
                "SELECT subscription_id, customer_id, user_id, plan, seats,
                        current_period_end, status, updated_at
                 FROM stripe_subscription WHERE subscription_id = ?1",
                [id],
                |row| {
                    Ok(StripeSubscription {
                        subscription_id: row.get(0)?,
                        customer_id: row.get(1)?,
                        user_id: row.get(2)?,
                        plan: row.get(3)?,
                        seats: row.get(4)?,
                        current_period_end: row.get(5)?,
                        status: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn stripe_customer_for_user(&self, user_id: &str) -> Result<Option<String>, StoreError> {
        self.connection()
            .query_row(
                "SELECT customer_id FROM stripe_subscription
                 WHERE user_id = ?1 ORDER BY updated_at DESC LIMIT 1",
                [user_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Returns false when the event was already processed.
    pub fn record_stripe_event_once(&self, event_id: &str, now: i64) -> Result<bool, StoreError> {
        Ok(self.connection().execute(
            "INSERT OR IGNORE INTO stripe_event (event_id, processed_at) VALUES (?1, ?2)",
            params![event_id, now],
        )? > 0)
    }

    /// Applies a Stripe event and records its idempotency evidence in one SQLite transaction.
    /// A failed grant rolls back the event reservation, allowing Stripe to retry safely.
    pub fn apply_stripe_event_once(
        &self,
        event_id: &str,
        input: &StripeEventInput,
        now: i64,
    ) -> Result<StripeEventApplyOutcome, StoreError> {
        self.apply_stripe_event_once_with_subscription(event_id, input, None, now)
    }

    /// Recover an unknown paid subscription and apply its invoice atomically. The caller must
    /// obtain this context from the authenticated Stripe API, not from client-supplied metadata.
    pub fn apply_stripe_event_once_with_subscription(
        &self,
        event_id: &str,
        input: &StripeEventInput,
        recovered: Option<&StripeSubscriptionInput>,
        now: i64,
    ) -> Result<StripeEventApplyOutcome, StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO stripe_event (event_id, processed_at) VALUES (?1, ?2)",
            params![event_id, now],
        )?;
        if inserted == 0 {
            return Ok(StripeEventApplyOutcome::AlreadyApplied);
        }
        if let Some(subscription) = recovered {
            if !matches!(input, StripeEventInput::InvoicePaid { subscription_id, .. } | StripeEventInput::InvoicePaidPlan { subscription_id, .. }
                if subscription_id == &subscription.subscription_id)
                || subscription.customer_id.is_empty()
                || subscription.user_id.is_empty()
            {
                return Err(StoreError::IntegrityCheck(
                    "invalid Stripe recovery context".into(),
                ));
            }
            transaction.execute(
                "INSERT INTO stripe_subscription
                 (subscription_id, customer_id, user_id, plan, seats, current_period_end, status, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(subscription_id) DO NOTHING",
                params![subscription.subscription_id, subscription.customer_id, subscription.user_id,
                    subscription.plan, subscription.seats, subscription.current_period_end, subscription.status, now],
            )?;
            verify_subscription_identity(
                &transaction,
                &subscription.subscription_id,
                &subscription.customer_id,
                &subscription.user_id,
            )?;
        }

        match input {
            StripeEventInput::Checkout {
                subscription_id,
                customer_id,
                user_id,
                plan,
                seats,
            } => {
                transaction.execute(
                    "INSERT INTO stripe_subscription
                     (subscription_id, customer_id, user_id, plan, seats, current_period_end, status, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, 'active', ?6)
                     ON CONFLICT(subscription_id) DO NOTHING",
                    params![subscription_id, customer_id, user_id, plan, seats, now],
                )?;
                verify_subscription_identity(&transaction, subscription_id, customer_id, user_id)?;
            }
            StripeEventInput::InvoicePaid {
                subscription_id,
                period_end,
            }
            | StripeEventInput::InvoicePaidPlan {
                subscription_id,
                period_end,
                ..
            } => {
                let Some(subscription) = transaction
                    .query_row(
                        "SELECT subscription_id, customer_id, user_id, plan, seats,
                                current_period_end, status, updated_at
                         FROM stripe_subscription WHERE subscription_id = ?1",
                        [subscription_id],
                        |row| {
                            Ok(StripeSubscription {
                                subscription_id: row.get(0)?,
                                customer_id: row.get(1)?,
                                user_id: row.get(2)?,
                                plan: row.get(3)?,
                                seats: row.get(4)?,
                                current_period_end: row.get(5)?,
                                status: row.get(6)?,
                                updated_at: row.get(7)?,
                            })
                        },
                    )
                    .optional()?
                else {
                    return Err(StoreError::IntegrityCheck(
                        "Stripe invoice references an unknown subscription".into(),
                    ));
                };
                let period_end = period_end.filter(|end| *end > 0).ok_or_else(|| {
                    StoreError::IntegrityCheck("Stripe invoice missing valid paid period".into())
                })?;
                if let StripeEventInput::InvoicePaidPlan { paid, .. } = input {
                    apply_paid_plan(&transaction, &subscription, paid, period_end, now)?;
                    transaction.commit()?;
                    return Ok(StripeEventApplyOutcome::Applied);
                }
                let status = if period_end > now
                    && period_end >= subscription.current_period_end
                    && subscription.status != "canceled"
                {
                    "active"
                } else {
                    &subscription.status
                };
                transaction.execute(
                    "UPDATE stripe_subscription SET current_period_end=?2, status=?3, updated_at=?4
                     WHERE subscription_id=?1",
                    params![
                        subscription.subscription_id,
                        period_end.max(subscription.current_period_end),
                        status,
                        now
                    ],
                )?;
                // Stripe sells a calendar period, not an additive number of days. Project
                // its absolute paid-through date so distinct events for the same invoice,
                // retries and late invoices converge without extending access repeatedly.
                // Other sources' longer grants remain intact; expired invoices grant nothing.
                if period_end > now {
                    if subscription.plan == "plus" {
                        transaction.execute(
                            "INSERT INTO premium_user (user_id, expires_at, source)
                             VALUES (?1, ?2, 'stripe')
                             ON CONFLICT(user_id) DO UPDATE SET
                               expires_at=excluded.expires_at, source=excluded.source
                             WHERE excluded.expires_at > premium_user.expires_at",
                            params![subscription.user_id, period_end],
                        )?;
                    } else {
                        transaction.execute(
                            "INSERT INTO premium_pass (user_id, seats, expires_at, source)
                             VALUES (?1, ?2, ?3, 'stripe')
                             ON CONFLICT(user_id) DO UPDATE SET
                               seats=MAX(premium_pass.seats, excluded.seats),
                               expires_at=MAX(premium_pass.expires_at, excluded.expires_at),
                               source=excluded.source
                             WHERE excluded.expires_at > premium_pass.expires_at
                                OR excluded.seats > premium_pass.seats",
                            params![subscription.user_id, subscription.seats, period_end],
                        )?;
                    }
                }
            }
            StripeEventInput::SubscriptionUpdated {
                subscription_id,
                period_end,
                status,
            } => {
                if let Some(subscription) = transaction
                    .query_row(
                        "SELECT customer_id, user_id, plan, seats, current_period_end
                         FROM stripe_subscription WHERE subscription_id=?1",
                        [subscription_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, i64>(4)?,
                            ))
                        },
                    )
                    .optional()?
                {
                    let period_end = period_end.unwrap_or(subscription.4);
                    transaction.execute(
                        "UPDATE stripe_subscription SET current_period_end=?2, status=?3, updated_at=?4
                         WHERE subscription_id=?1",
                        params![subscription_id, period_end, status, now],
                    )?;
                }
            }
            StripeEventInput::InvoiceFailed { subscription_id } => {
                transaction.execute(
                    "UPDATE stripe_subscription SET status='past_due', updated_at=?2
                     WHERE subscription_id=?1",
                    params![subscription_id, now],
                )?;
            }
            StripeEventInput::Ignored => {}
        }
        transaction.commit()?;
        Ok(StripeEventApplyOutcome::Applied)
    }

    pub fn stripe_event_processed(&self, event_id: &str) -> Result<bool, StoreError> {
        self.connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM stripe_event WHERE event_id = ?1)",
                [event_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value != 0)
            .map_err(StoreError::from)
    }
}

fn apply_paid_plan(
    transaction: &rusqlite::Transaction<'_>,
    subscription: &StripeSubscription,
    paid: &StripePaidPlan,
    period_end: i64,
    now: i64,
) -> Result<(), StoreError> {
    let expected_seats = match paid.plan.as_str() {
        "plus" => 1,
        "premium" => 2,
        "max" => 5,
        _ => 0,
    };
    if paid.invoice_id.is_empty()
        || paid.issued_at <= 0
        || paid.starts_at > now
        || paid.starts_at >= period_end
        || expected_seats == 0
        || paid.seats != expected_seats
    {
        return Err(StoreError::IntegrityCheck(
            "invalid Stripe paid plan".into(),
        ));
    }
    let previous: Option<(String, i64, String, i64)> = transaction.query_row(
        "SELECT invoice_id, issued_at, plan, expires_at FROM stripe_paid_entitlement WHERE subscription_id=?1",
        [&subscription.subscription_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).optional()?;
    if let Some((invoice, issued, plan, expiry)) = &previous {
        if paid.issued_at < *issued || period_end < *expiry {
            return Ok(()); // A historical invoice cannot replace the current paid projection.
        }
        if invoice == &paid.invoice_id {
            if issued != &paid.issued_at || plan != &paid.plan || expiry != &period_end {
                return Err(StoreError::IntegrityCheck(
                    "conflicting Stripe invoice replay".into(),
                ));
            }
            return Ok(());
        }
        if paid.issued_at == *issued
            && plan != &paid.plan
            && paid.canonical_previous_invoice.as_ref() != Some(invoice)
        {
            // Stripe timestamps have only second precision. Do not guess which change won.
            return Err(StoreError::IntegrityCheck(
                "ambiguous Stripe paid plan order".into(),
            ));
        }
        let previous_seats = match plan.as_str() {
            "max" => 5,
            "premium" => 2,
            _ => 1,
        };
        if paid.seats < previous_seats && paid.starts_at < *expiry {
            return Err(StoreError::IntegrityCheck(
                "Stripe downgrade before paid period end".into(),
            ));
        }
    }
    if period_end <= now {
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO stripe_paid_entitlement (subscription_id,user_id,invoice_id,issued_at,starts_at,expires_at,plan,seats)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(subscription_id) DO UPDATE SET invoice_id=excluded.invoice_id,
         issued_at=excluded.issued_at, starts_at=excluded.starts_at, expires_at=excluded.expires_at,
         plan=excluded.plan, seats=excluded.seats",
        params![subscription.subscription_id,subscription.user_id,paid.invoice_id,paid.issued_at,
            paid.starts_at,period_end,paid.plan,paid.seats],
    )?;
    transaction.execute(
        "UPDATE stripe_subscription SET plan=?2,seats=?3,current_period_end=?4,
         status=CASE WHEN status='canceled' THEN status ELSE 'active' END,updated_at=?5 WHERE subscription_id=?1",
        params![subscription.subscription_id,paid.plan,paid.seats,period_end,now],
    )?;
    Ok(())
}

fn verify_subscription_identity(
    transaction: &rusqlite::Transaction<'_>,
    subscription_id: &str,
    customer_id: &str,
    user_id: &str,
) -> Result<(), StoreError> {
    let matches = transaction.query_row(
        "SELECT customer_id = ?2 AND user_id = ?3 FROM stripe_subscription WHERE subscription_id = ?1",
        params![subscription_id, customer_id, user_id], |row| row.get::<_, bool>(0),
    )?;
    if !matches {
        return Err(StoreError::IntegrityCheck(
            "Stripe subscription identity conflict".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn checkout() -> StripeEventInput {
        StripeEventInput::Checkout {
            subscription_id: "sub_test".into(),
            customer_id: "cus_test".into(),
            user_id: "user_test".into(),
            plan: "plus".into(),
            seats: 1,
        }
    }

    fn paid_plan(plan: &str, invoice: &str, issued: i64, start: i64, end: i64) -> StripeEventInput {
        StripeEventInput::InvoicePaidPlan {
            subscription_id: "sub_test".into(),
            period_end: Some(end),
            paid: StripePaidPlan {
                invoice_id: invoice.into(),
                issued_at: issued,
                starts_at: start,
                plan: plan.into(),
                seats: match plan {
                    "max" => 5,
                    "premium" => 2,
                    _ => 1,
                },
                canonical_previous_invoice: None,
            },
        }
    }

    #[test]
    fn paid_plan_failure_rolls_back_ledger_marker_and_subscription_and_retry_converges() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .apply_stripe_event_once("checkout", &checkout(), NOW)
            .unwrap();
        store.connection().execute_batch("CREATE TEMP TRIGGER fail_paid_plan BEFORE INSERT ON stripe_paid_entitlement BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        let paid = paid_plan("premium", "in_plan", NOW, NOW, NOW + 60_000);
        assert!(store.apply_stripe_event_once("paid", &paid, NOW).is_err());
        assert!(!store.stripe_event_processed("paid").unwrap());
        assert!(
            store
                .stripe_paid_invoice_stamp("sub_test")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.stripe_subscription("sub_test").unwrap().unwrap().plan,
            "plus"
        );
        store
            .connection()
            .execute_batch("DROP TRIGGER fail_paid_plan;")
            .unwrap();
        store.apply_stripe_event_once("paid", &paid, NOW).unwrap();
        assert_eq!(
            store
                .premium_status("user_test", NOW)
                .unwrap()
                .pass
                .unwrap()
                .seats,
            2
        );
        store
            .apply_stripe_event_once("paid_new_delivery", &paid, NOW + 1)
            .unwrap();
        assert_eq!(
            store
                .premium_status("user_test", NOW + 1)
                .unwrap()
                .pass
                .unwrap()
                .expires_at,
            NOW + 60_000
        );
    }

    #[test]
    fn invalid_paid_plans_and_conflicting_invoice_replays_do_not_reserve_events() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .apply_stripe_event_once("checkout", &checkout(), NOW)
            .unwrap();
        for (index, event) in [
            paid_plan("unknown", "in_bad", NOW, NOW, NOW + 100),
            paid_plan("plus", "in_future", NOW, NOW + 1, NOW + 100),
            paid_plan("plus", "in_period", NOW, NOW, NOW),
        ]
        .iter()
        .enumerate()
        {
            let id = format!("invalid_{index}");
            assert!(store.apply_stripe_event_once(&id, event, NOW).is_err());
            assert!(!store.stripe_event_processed(&id).unwrap());
        }
        store
            .apply_stripe_event_once(
                "initial",
                &paid_plan("premium", "in_same", NOW, NOW, NOW + 60_000),
                NOW,
            )
            .unwrap();
        assert!(
            store
                .apply_stripe_event_once(
                    "conflict",
                    &paid_plan("max", "in_same", NOW + 1, NOW, NOW + 60_000),
                    NOW + 1
                )
                .is_err()
        );
        assert!(!store.stripe_event_processed("conflict").unwrap());
        assert_eq!(
            store
                .premium_status("user_test", NOW + 1)
                .unwrap()
                .pass
                .unwrap()
                .seats,
            2
        );
    }

    #[test]
    fn paid_grants_are_listed_and_owner_revocation_does_not_cancel_provider_records() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .apply_stripe_event_once("checkout", &checkout(), NOW)
            .unwrap();
        store
            .apply_stripe_event_once(
                "paid",
                &paid_plan("premium", "in_paid", NOW, NOW, NOW + 60_000),
                NOW,
            )
            .unwrap();
        assert_eq!(store.list_active_premium(NOW).unwrap().passes[0].seats, 2);
        assert!(store.revoke_guild_pass("user_test").unwrap());
        assert!(
            !store
                .premium_status("user_test", NOW)
                .unwrap()
                .pass
                .unwrap()
                .active
        );
        assert!(store.stripe_subscription("sub_test").unwrap().is_some());
        assert!(store.stripe_event_processed("paid").unwrap());
    }

    #[test]
    fn stripe_event_replay_is_idempotent() {
        let store = SqliteStore::open_in_memory().expect("store");
        assert_eq!(
            store
                .apply_stripe_event_once("evt_checkout", &checkout(), NOW)
                .expect("first"),
            StripeEventApplyOutcome::Applied
        );
        assert_eq!(
            store
                .apply_stripe_event_once("evt_checkout", &checkout(), NOW + 1)
                .expect("replay"),
            StripeEventApplyOutcome::AlreadyApplied
        );
        assert!(
            store
                .stripe_subscription("sub_test")
                .expect("subscription")
                .is_some()
        );
    }

    #[test]
    fn late_checkout_does_not_reactivate_canceled_subscription() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .apply_stripe_event_once("checkout", &checkout(), NOW)
            .unwrap();
        store
            .apply_stripe_event_once(
                "canceled",
                &StripeEventInput::SubscriptionUpdated {
                    subscription_id: "sub_test".into(),
                    period_end: Some(NOW + 60_000),
                    status: "canceled".into(),
                },
                NOW + 1,
            )
            .unwrap();
        store
            .apply_stripe_event_once("late_checkout", &checkout(), NOW + 2)
            .unwrap();
        assert_eq!(
            store
                .stripe_subscription("sub_test")
                .unwrap()
                .unwrap()
                .status,
            "canceled"
        );
    }

    #[test]
    fn checkout_cannot_rebind_existing_subscription_to_another_user() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .apply_stripe_event_once("checkout", &checkout(), NOW)
            .unwrap();
        let conflicting = StripeEventInput::Checkout {
            subscription_id: "sub_test".into(),
            customer_id: "cus_other".into(),
            user_id: "other_user".into(),
            plan: "plus".into(),
            seats: 1,
        };
        assert!(
            store
                .apply_stripe_event_once("conflict", &conflicting, NOW + 1)
                .is_err()
        );
        assert!(!store.stripe_event_processed("conflict").unwrap());
        assert_eq!(
            store
                .stripe_subscription("sub_test")
                .unwrap()
                .unwrap()
                .user_id,
            "user_test"
        );
    }

    #[test]
    fn failed_invoice_does_not_leave_idempotency_marker() {
        let store = SqliteStore::open_in_memory().expect("store");
        let result = store.apply_stripe_event_once(
            "evt_unknown_invoice",
            &StripeEventInput::InvoicePaid {
                subscription_id: "missing".into(),
                period_end: Some(2_000),
            },
            NOW,
        );
        assert!(result.is_err());
        assert!(
            !store
                .stripe_event_processed("evt_unknown_invoice")
                .expect("marker")
        );
    }

    #[test]
    fn invalid_paid_period_rolls_back_event_and_subscription_changes() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .apply_stripe_event_once("checkout", &checkout(), NOW)
            .unwrap();
        for period_end in [None, Some(0), Some(-1)] {
            let result = store.apply_stripe_event_once(
                "invalid",
                &StripeEventInput::InvoicePaid {
                    subscription_id: "sub_test".into(),
                    period_end,
                },
                NOW,
            );
            assert!(result.is_err());
            assert!(!store.stripe_event_processed("invalid").unwrap());
            assert_eq!(
                store
                    .stripe_subscription("sub_test")
                    .unwrap()
                    .unwrap()
                    .current_period_end,
                0
            );
            assert!(store.user_premium_expiry("user_test").unwrap().is_none());
        }
    }

    #[test]
    fn failed_entitlement_write_rolls_back_paid_period_and_allows_retry() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .apply_stripe_event_once("checkout", &checkout(), NOW)
            .unwrap();
        store
            .connection()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_grant BEFORE INSERT ON premium_user
             BEGIN SELECT RAISE(ABORT, 'isolated grant failure'); END;",
            )
            .unwrap();
        let paid = StripeEventInput::InvoicePaid {
            subscription_id: "sub_test".into(),
            period_end: Some(NOW + 3_600_000),
        };
        assert!(store.apply_stripe_event_once("paid", &paid, NOW).is_err());
        assert!(!store.stripe_event_processed("paid").unwrap());
        assert_eq!(
            store
                .stripe_subscription("sub_test")
                .unwrap()
                .unwrap()
                .current_period_end,
            0
        );
        store
            .connection()
            .execute_batch("DROP TRIGGER fail_grant")
            .unwrap();
        assert_eq!(
            store.apply_stripe_event_once("paid", &paid, NOW).unwrap(),
            StripeEventApplyOutcome::Applied
        );
        assert_eq!(
            store.user_premium_expiry("user_test").unwrap(),
            Some(NOW + 3_600_000)
        );
    }

    #[test]
    fn failed_recovery_grant_rolls_back_identity_and_event_together() {
        let store = SqliteStore::open_in_memory().unwrap();
        let context = StripeSubscriptionInput {
            subscription_id: "sub_test".into(),
            customer_id: "cus_test".into(),
            user_id: "user_test".into(),
            plan: "plus".into(),
            seats: 1,
            current_period_end: NOW + 60_000,
            status: "active".into(),
            updated_at: NOW,
        };
        let paid = StripeEventInput::InvoicePaid {
            subscription_id: "sub_test".into(),
            period_end: Some(NOW + 60_000),
        };
        store.connection().execute_batch("CREATE TEMP TRIGGER fail_recovered_grant BEFORE INSERT ON premium_user BEGIN SELECT RAISE(ABORT, 'isolated failure'); END;").unwrap();
        assert!(
            store
                .apply_stripe_event_once_with_subscription("paid", &paid, Some(&context), NOW)
                .is_err()
        );
        assert!(!store.stripe_event_processed("paid").unwrap());
        assert!(store.stripe_subscription("sub_test").unwrap().is_none());
        store
            .connection()
            .execute_batch("DROP TRIGGER fail_recovered_grant")
            .unwrap();
        store
            .apply_stripe_event_once_with_subscription("paid", &paid, Some(&context), NOW)
            .unwrap();
        assert_eq!(
            store.user_premium_expiry("user_test").unwrap(),
            Some(NOW + 60_000)
        );
    }

    #[test]
    fn admin_purchase_projection_returns_latest_subscriptions_first() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .upsert_stripe_subscription(&StripeSubscriptionInput {
                subscription_id: "sub_old".into(),
                customer_id: "cus_old".into(),
                user_id: "user_old".into(),
                plan: "plus".into(),
                seats: 1,
                current_period_end: 0,
                status: "active".into(),
                updated_at: NOW,
            })
            .expect("old subscription");
        store
            .upsert_stripe_subscription(&StripeSubscriptionInput {
                subscription_id: "sub_new".into(),
                customer_id: "cus_new".into(),
                user_id: "user_new".into(),
                plan: "premium".into(),
                seats: 3,
                current_period_end: 0,
                status: "active".into(),
                updated_at: NOW + 1,
            })
            .expect("new subscription");

        let rows = store
            .list_stripe_subscriptions(10)
            .expect("purchase projection");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].subscription_id, "sub_new");
        assert_eq!(rows[0].user_id, "user_new");
        assert_eq!(rows[0].plan, "premium");
        assert_eq!(rows[1].subscription_id, "sub_old");
    }
}
