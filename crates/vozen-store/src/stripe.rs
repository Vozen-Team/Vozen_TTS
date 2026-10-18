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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripeEventApplyOutcome {
    Applied,
    AlreadyApplied,
}

impl SqliteStore {
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
            if !matches!(input, StripeEventInput::InvoicePaid { subscription_id, .. }
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
