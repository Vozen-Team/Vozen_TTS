//! Isolated QA fixtures: no Stripe calls, real credentials, or production database.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use tower::ServiceExt;
use vozen_api::{
    premium_api::{
        ActivationIdentity, ActivationIdentityError, DiscordIdentity, DiscordIdentityVerifier,
    },
    stripe_api::{
        StripeApiConfig, StripePriceIds, StripeSubscriptionLookup,
        stripe_router_with_subscription_lookup,
    },
};
use vozen_store::SqliteStore;

const NOW: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;
const SECRET: &str = "whsec_isolated_qa_fixture_only";

struct NoIdentity;
#[async_trait]
impl DiscordIdentityVerifier for NoIdentity {
    async fn resolve_identity(&self, _: &str) -> Result<DiscordIdentity, ()> {
        Err(())
    }
    async fn resolve_activation_identity(
        &self,
        _: &str,
    ) -> Result<ActivationIdentity, ActivationIdentityError> {
        Err(ActivationIdentityError::InvalidToken)
    }
}

struct Lookup {
    value: Option<Value>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl StripeSubscriptionLookup for Lookup {
    async fn retrieve(&self, _: &str) -> Result<Value, ()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
        self.value.clone().ok_or(())
    }
}

fn provider_subscription() -> Value {
    json!({"id":"sub_qa","customer":"cus_qa","livemode":false,"status":"active",
        "metadata":{"discord_user_id":"user_qa","plan":"plus","seats":"1"},
        "items":{"data":[{"quantity":1,"current_period_end":(NOW+30*DAY)/1000,
            "price":{"id":"price_isolated_fixture","recurring":{"interval":"month"}}}]}})
}

fn fixture() -> (Router, Arc<Mutex<SqliteStore>>) {
    let (app, store, _) = fixture_with_lookup(Some(provider_subscription()));
    (app, store)
}

fn fixture_with_lookup(
    value: Option<Value>,
) -> (Router, Arc<Mutex<SqliteStore>>, Arc<AtomicUsize>) {
    fixture_with_clock(value, Arc::new(AtomicI64::new(NOW)))
}

fn fixture_with_clock(
    value: Option<Value>,
    clock: Arc<AtomicI64>,
) -> (Router, Arc<Mutex<SqliteStore>>, Arc<AtomicUsize>) {
    let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap()));
    let price = "price_isolated_fixture".to_string();
    let calls = Arc::new(AtomicUsize::new(0));
    let app = stripe_router_with_subscription_lookup(
        StripeApiConfig {
            origin: "https://vozen.org".into(),
            secret_key: "sk_test_fake_not_a_credential".into(),
            publishable_key: "pk_test_fake_not_a_credential".into(),
            webhook_secret: SECRET.into(),
            prices: StripePriceIds {
                plus_monthly: price.clone(),
                plus_yearly: "price_plus_yearly".into(),
                premium_monthly: "price_premium_monthly".into(),
                premium_yearly: "price_premium_yearly".into(),
                max_monthly: "price_max_monthly".into(),
                max_yearly: "price_max_yearly".into(),
            },
            store: store.clone(),
            identity_verifier: Arc::new(NoIdentity),
            now: Arc::new(move || clock.load(Ordering::SeqCst)),
        },
        Arc::new(Lookup {
            value,
            calls: calls.clone(),
        }),
    )
    .unwrap();
    (app, store, calls)
}

fn event(id: &str, kind: &str, object: Value) -> Value {
    json!({"id":id,"type":kind,"livemode":false,"data":{"object":object}})
}

fn checkout() -> Value {
    event(
        "evt_checkout_qa",
        "checkout.session.completed",
        json!({"id":"cs_test_fixture", "subscription":"sub_qa", "customer":"cus_qa",
        "metadata":{"discord_user_id":"user_qa","plan":"plus","seats":"1"}}),
    )
}

fn invoice(id: &str, end: i64) -> Value {
    event(
        id,
        "invoice.paid",
        json!({"id":"in_qa_same_invoice", "subscription":"sub_qa", "customer":"cus_qa",
        "lines":{"data":[{"period":{"end":end / 1000}}]}}),
    )
}

fn signature(body: &[u8]) -> String {
    signature_at(body, NOW)
}

fn signature_at(body: &[u8], now: i64) -> String {
    let timestamp = now / 1000;
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(body);
    let digest = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<String>();
    format!("t={timestamp},v1={digest}")
}

async fn send(app: &Router, value: Value) -> StatusCode {
    send_at(app, value, NOW).await
}

async fn send_at(app: &Router, value: Value, now: i64) -> StatusCode {
    let body = serde_json::to_vec(&value).unwrap();
    app.clone()
        .oneshot(
            Request::post("/webhook/stripe")
                .header("stripe-signature", signature_at(&body, now))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

fn priced_invoice(id: &str, plan: &str, start: i64, end: i64) -> Value {
    let price = match plan {
        "plus" => "price_isolated_fixture",
        "premium" => "price_premium_monthly",
        "max" => "price_max_monthly",
        _ => panic!("test plan"),
    };
    event(
        &format!("evt_{id}"),
        "invoice.paid",
        json!({"id":id,"subscription":"sub_qa","customer":"cus_qa",
        "status":"paid","paid":true,"created":start/1000,
        "lines":{"has_more":false,"data":[{"amount":699,"quantity":1,
            "parent":{"type":"subscription_item_details","subscription_item_details":{"subscription":"sub_qa"}},
            "pricing":{"type":"price_details","price_details":{"price":price}},
            "period":{"start":start/1000,"end":end/1000}}]}}),
    )
}

#[tokio::test]
async fn all_paid_upgrades_and_period_end_downgrades_reconcile_exact_seats() {
    for (before, after) in [
        ("plus", "premium"),
        ("plus", "max"),
        ("premium", "max"),
        ("max", "premium"),
        ("premium", "plus"),
        ("max", "plus"),
    ] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let (app, store, _) = fixture_with_clock(Some(provider_subscription()), clock.clone());
        assert_eq!(send(&app, checkout()).await, StatusCode::OK);
        assert_eq!(
            send(
                &app,
                priced_invoice("in_initial", before, NOW, NOW + 30 * DAY)
            )
            .await,
            StatusCode::OK
        );
        if before != "plus" {
            for index in 0..if before == "max" { 5 } else { 2 } {
                store
                    .lock()
                    .unwrap()
                    .activate_seat("user_qa", &format!("guild_{index}"), NOW)
                    .unwrap();
            }
        }
        let downgrade = (if before == "max" {
            5
        } else if before == "premium" {
            2
        } else {
            1
        }) > (if after == "max" {
            5
        } else if after == "premium" {
            2
        } else {
            1
        });
        let unpaid = event(
            "evt_unpaid_change",
            "customer.subscription.updated",
            json!({"id":"sub_qa","status":"active","current_period_end":(NOW+30*DAY)/1000,
            "items":{"data":[{"quantity":1,"price":{"id":"price_max_monthly"}}]}}),
        );
        assert_eq!(send(&app, unpaid).await, StatusCode::OK);
        assert_eq!(
            store
                .lock()
                .unwrap()
                .stripe_subscription("sub_qa")
                .unwrap()
                .unwrap()
                .plan,
            before
        );
        let change_at = if downgrade { NOW + 30 * DAY } else { NOW + DAY };
        clock.store(change_at, Ordering::SeqCst);
        let end = if downgrade {
            NOW + 60 * DAY
        } else {
            NOW + 30 * DAY
        };
        let changed = priced_invoice("in_changed", after, change_at, end);
        assert_eq!(
            send_at(&app, changed.clone(), change_at).await,
            StatusCode::OK,
            "{before}->{after}"
        );
        assert_eq!(send_at(&app, changed, change_at).await, StatusCode::OK);
        let db = store.lock().unwrap();
        let status = db.premium_status("user_qa", change_at).unwrap();
        assert_eq!(status.plus_active, after == "plus", "{before}->{after}");
        assert_eq!(
            status.pass.as_ref().map(|pass| pass.seats),
            match after {
                "max" => Some(5),
                "premium" => Some(2),
                _ => None,
            }
        );
        if before == "max" && after == "premium" {
            assert_eq!(
                status.pass.as_ref().unwrap().used,
                2,
                "only licensed associations count as used"
            );
            assert_eq!(status.pass.as_ref().unwrap().guilds.len(), 2);
            assert_eq!(status.pass.as_ref().unwrap().inactive_guilds.len(), 3);
            assert!(db.is_guild_premium("guild_0", change_at).unwrap());
            assert!(db.is_guild_premium("guild_1", change_at).unwrap());
            assert!(!db.is_guild_premium("guild_2", change_at).unwrap());
            assert!(
                db.resolve_guild_pass_owner("guild_2", change_at)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                db.activate_seat("user_qa", "guild_2", change_at)
                    .unwrap()
                    .status,
                vozen_store::ActivateStatus::NoSeats,
                "a saved over-limit association is not active"
            );
            assert_eq!(
                db.pass_activations("user_qa").unwrap().len(),
                5,
                "associations are preserved"
            );
        }
        if after == "plus" {
            assert!(!db.is_guild_premium("guild_0", change_at).unwrap());
        }
    }
}

#[tokio::test]
async fn failed_upgrade_and_early_paid_downgrade_do_not_change_access() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    assert_eq!(
        send(
            &app,
            priced_invoice("in_max", "max", NOW - DAY, NOW + 30 * DAY)
        )
        .await,
        StatusCode::OK
    );
    let failed = event(
        "evt_failed_upgrade",
        "invoice.payment_failed",
        json!({"subscription":"sub_qa"}),
    );
    assert_eq!(send(&app, failed).await, StatusCode::OK);
    assert_eq!(
        send(
            &app,
            priced_invoice("in_early_downgrade", "premium", NOW, NOW + 30 * DAY)
        )
        .await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let db = store.lock().unwrap();
    assert_eq!(
        db.premium_status("user_qa", NOW)
            .unwrap()
            .pass
            .unwrap()
            .seats,
        5
    );
    assert!(!db.stripe_event_processed("evt_in_early_downgrade").unwrap());
}

#[tokio::test]
async fn delayed_invoice_cannot_undo_upgrade_and_negative_proration_is_not_new_plan() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    let old = priced_invoice("in_old", "plus", NOW - DAY, NOW + 30 * DAY);
    assert_eq!(send(&app, old.clone()).await, StatusCode::OK);
    let mut upgraded = priced_invoice("in_upgrade", "max", NOW, NOW + 30 * DAY);
    let mut credit = old["data"]["object"]["lines"]["data"][0].clone();
    credit["amount"] = json!(-399);
    upgraded["data"]["object"]["lines"]["data"]
        .as_array_mut()
        .unwrap()
        .insert(0, credit);
    assert_eq!(send(&app, upgraded).await, StatusCode::OK);
    let mut replay = old;
    replay["id"] = json!("evt_old_new_delivery");
    assert_eq!(send(&app, replay).await, StatusCode::OK);
    let db = store.lock().unwrap();
    assert!(!db.is_user_premium("user_qa", NOW).unwrap());
    assert_eq!(
        db.premium_status("user_qa", NOW)
            .unwrap()
            .pass
            .unwrap()
            .seats,
        5
    );
}

#[tokio::test]
async fn independent_grants_survive_plan_changes_without_lending_expired_seats() {
    let (app, store) = fixture();
    store
        .lock()
        .unwrap()
        .grant_user_premium("user_qa", 90, "manual", NOW)
        .unwrap();
    store
        .lock()
        .unwrap()
        .grant_guild_pass("user_qa", 2, 90, "kofi", NOW)
        .unwrap();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    assert_eq!(
        send(
            &app,
            priced_invoice("in_paid_max", "max", NOW, NOW + 30 * DAY)
        )
        .await,
        StatusCode::OK
    );
    let db = store.lock().unwrap();
    assert!(db.is_user_premium("user_qa", NOW).unwrap());
    assert_eq!(
        db.premium_status("user_qa", NOW)
            .unwrap()
            .pass
            .unwrap()
            .seats,
        5
    );
    assert_eq!(
        db.premium_status("user_qa", NOW + 31 * DAY)
            .unwrap()
            .pass
            .unwrap()
            .seats,
        2
    );
    assert_eq!(
        db.user_premium_expiry("user_qa").unwrap(),
        Some(NOW + 90 * DAY)
    );
}

#[tokio::test]
async fn new_paid_projection_preserves_unattributed_legacy_stripe_rights_until_expiry() {
    let (app, store) = fixture();
    // Legacy aggregates cannot identify which of several subscriptions paid for a grant.
    store
        .lock()
        .unwrap()
        .grant_guild_pass("user_qa", 5, 10, "stripe", NOW)
        .unwrap();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    assert_eq!(
        send(
            &app,
            priced_invoice("in_new_subscription", "premium", NOW, NOW + 30 * DAY)
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .premium_status("user_qa", NOW)
            .unwrap()
            .pass
            .unwrap()
            .seats,
        5
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .premium_status("user_qa", NOW + 11 * DAY)
            .unwrap()
            .pass
            .unwrap()
            .seats,
        2
    );
}

#[tokio::test]
async fn malformed_or_wrong_mode_plan_invoice_does_not_consume_event_or_grant() {
    let cases = [
        (
            "/data/object/lines/data/0/pricing/price_details/price",
            json!("price_unknown"),
        ),
        ("/data/object/lines/data/0/quantity", json!(2)),
        ("/data/object/lines/has_more", json!(true)),
        ("/data/object/status", json!("open")),
        ("/data/object/customer", json!("cus_other")),
        (
            "/data/object/lines/data/0/parent/subscription_item_details/subscription",
            json!("sub_other"),
        ),
        ("/livemode", json!(true)),
    ];
    for (pointer, value) in cases {
        let (app, store) = fixture();
        assert_eq!(send(&app, checkout()).await, StatusCode::OK);
        let mut paid = priced_invoice("in_bad", "max", NOW, NOW + 30 * DAY);
        *paid.pointer_mut(pointer).unwrap() = value;
        assert_eq!(send(&app, paid).await, StatusCode::BAD_REQUEST, "{pointer}");
        let db = store.lock().unwrap();
        assert!(!db.stripe_event_processed("evt_in_bad").unwrap());
        assert!(db.premium_status("user_qa", NOW).unwrap().pass.is_none());
    }
}

#[tokio::test]
async fn paid_price_change_replaces_checkout_plan_without_granting_on_update() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    let mut paid = invoice("evt_paid_plan_change", NOW + 30 * DAY);
    paid["data"]["object"]["created"] = json!(NOW / 1000);
    paid["data"]["object"]["status"] = json!("paid");
    paid["data"]["object"]["lines"]["data"][0]["type"] = json!("subscription");
    paid["data"]["object"]["lines"]["data"][0]["price"] = json!({"id":"price_premium_monthly"});
    paid["data"]["object"]["lines"]["data"][0]["amount"] = json!(699);
    paid["data"]["object"]["lines"]["data"][0]["quantity"] = json!(1);
    paid["data"]["object"]["lines"]["data"][0]["period"]["start"] = json!(NOW / 1000);
    assert_eq!(send(&app, paid).await, StatusCode::OK);
    let db = store.lock().unwrap();
    let pass = db
        .premium_status("user_qa", NOW)
        .unwrap()
        .pass
        .expect("paid Premium pass");
    assert_eq!(pass.seats, 2);
    assert!(!db.is_user_premium("user_qa", NOW).unwrap());
    assert_eq!(
        db.stripe_subscription("sub_qa").unwrap().unwrap().plan,
        "premium"
    );
}

#[tokio::test]
async fn valid_signed_payment_grants_only_its_user_and_replay_is_idempotent() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    assert!(
        !store
            .lock()
            .unwrap()
            .is_user_premium("user_qa", NOW)
            .unwrap(),
        "Opening checkout must not grant paid access"
    );
    let paid = invoice("evt_invoice_qa", NOW + 30 * DAY);
    assert_eq!(send(&app, paid.clone()).await, StatusCode::OK);
    assert_eq!(send(&app, paid).await, StatusCode::OK);
    let store = store.lock().unwrap();
    assert_eq!(
        store.user_premium_expiry("user_qa").unwrap(),
        Some(NOW + 30 * DAY)
    );
    assert!(!store.is_user_premium("unrelated_user_qa", NOW).unwrap());
}

#[tokio::test]
async fn altered_signed_body_is_rejected_without_subscription_or_event_write() {
    let (app, store) = fixture();
    let original = serde_json::to_vec(&checkout()).unwrap();
    let mut altered = checkout();
    altered["data"]["object"]["metadata"]["discord_user_id"] = json!("attacker_qa");
    let response = app
        .oneshot(
            Request::post("/webhook/stripe")
                .header("stripe-signature", signature(&original))
                .body(Body::from(serde_json::to_vec(&altered).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let store = store.lock().unwrap();
    assert!(!store.stripe_event_processed("evt_checkout_qa").unwrap());
    assert!(store.stripe_subscription("sub_qa").unwrap().is_none());
}

#[tokio::test]
async fn invoice_before_checkout_recovers_without_event_redelivery() {
    let (app, store) = fixture();
    let paid = invoice("evt_invoice_early_qa", NOW + 30 * DAY);
    assert_eq!(send(&app, paid.clone()).await, StatusCode::OK);
    assert!(
        store
            .lock()
            .unwrap()
            .stripe_event_processed("evt_invoice_early_qa")
            .unwrap()
    );
    assert!(
        store
            .lock()
            .unwrap()
            .is_user_premium("user_qa", NOW)
            .unwrap()
    );
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    assert_eq!(send(&app, paid).await, StatusCode::OK);
    assert_eq!(
        store
            .lock()
            .unwrap()
            .user_premium_expiry("user_qa")
            .unwrap(),
        Some(NOW + 30 * DAY)
    );
}

#[tokio::test]
async fn early_invoice_provider_failure_is_retryable_without_partial_writes() {
    let (app, store, calls) = fixture_with_lookup(None);
    assert_eq!(
        send(&app, invoice("evt_unavailable", NOW + DAY)).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let db = store.lock().unwrap();
    assert!(!db.stripe_event_processed("evt_unavailable").unwrap());
    assert!(db.stripe_subscription("sub_qa").unwrap().is_none());
    assert!(db.user_premium_expiry("user_qa").unwrap().is_none());
}

#[tokio::test]
async fn early_invoice_rejects_untrusted_provider_context_without_writes() {
    for (path, value) in [
        ("/id", json!("sub_other")),
        ("/customer", json!("cus_other")),
        ("/livemode", json!(true)),
        ("/metadata/discord_user_id", json!("")),
        ("/metadata/plan", json!("unknown")),
        ("/metadata/seats", json!("5")),
        ("/items/data/0/quantity", json!(2)),
        ("/items/data/0/price/id", json!("price_other")),
        ("/items/data/0/price/recurring/interval", json!("week")),
        ("/items/data/0/current_period_end", json!(i64::MAX)),
    ] {
        let mut subscription = provider_subscription();
        *subscription.pointer_mut(path).unwrap() = value;
        let (app, store, _) = fixture_with_lookup(Some(subscription));
        assert_eq!(
            send(&app, invoice("evt_bad_context", NOW + DAY)).await,
            StatusCode::BAD_REQUEST,
            "{path}"
        );
        let db = store.lock().unwrap();
        assert!(!db.stripe_event_processed("evt_bad_context").unwrap());
        assert!(db.stripe_subscription("sub_qa").unwrap().is_none());
        assert!(db.user_premium_expiry("user_qa").unwrap().is_none());
    }
}

#[tokio::test]
async fn invalid_signature_does_not_trigger_provider_lookup() {
    let (app, _, calls) = fixture_with_lookup(Some(provider_subscription()));
    let body = serde_json::to_vec(&invoice("evt_bad_sig", NOW + DAY)).unwrap();
    let response = app
        .oneshot(
            Request::post("/webhook/stripe")
                .header("stripe-signature", "t=1,v1=bad")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn processed_invoice_does_not_lookup_or_recreate_deleted_identity() {
    let (app, store, calls) = fixture_with_lookup(None);
    store
        .lock()
        .unwrap()
        .record_stripe_event_once("evt_already_processed", NOW)
        .unwrap();
    assert_eq!(
        send(&app, invoice("evt_already_processed", NOW + DAY)).await,
        StatusCode::OK
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .lock()
            .unwrap()
            .stripe_subscription("sub_qa")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn simultaneous_early_invoices_converge_on_paid_time() {
    let (app, store, calls) = fixture_with_lookup(Some(provider_subscription()));
    let (first, second) = tokio::join!(
        send(&app, invoice("evt_early_a", NOW + DAY)),
        send(&app, invoice("evt_early_b", NOW + 2 * DAY))
    );
    assert_eq!(first, StatusCode::OK);
    assert_eq!(second, StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let db = store.lock().unwrap();
    assert_eq!(
        db.user_premium_expiry("user_qa").unwrap(),
        Some(NOW + 2 * DAY)
    );
    assert!(!db.is_user_premium("other_user", NOW).unwrap());
}

#[tokio::test]
async fn recovered_cancellation_survives_late_checkout() {
    let mut subscription = provider_subscription();
    subscription["status"] = json!("canceled");
    let (app, store, _) = fixture_with_lookup(Some(subscription));
    assert_eq!(
        send(&app, invoice("evt_early_paid", NOW + DAY)).await,
        StatusCode::OK
    );
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    assert_eq!(
        store
            .lock()
            .unwrap()
            .stripe_subscription("sub_qa")
            .unwrap()
            .unwrap()
            .status,
        "canceled"
    );
}

#[tokio::test]
async fn unauthenticated_checkout_paths_do_not_create_provider_sessions() {
    let (app, _) = fixture();
    for path in [
        "/api/billing/plans",
        "/api/billing/change/preview",
        "/api/billing/change/confirm",
        "/api/billing/checkout",
        "/api/billing/checkout/elements",
        "/api/billing/portal",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post(path)
                    .body(Body::from(r#"{"plan":"plus","interval":"monthly"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn separate_events_for_same_paid_invoice_must_not_double_paid_time() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    assert_eq!(
        send(&app, invoice("evt_invoice_a_qa", NOW + 30 * DAY)).await,
        StatusCode::OK
    );
    let first = store
        .lock()
        .unwrap()
        .user_premium_expiry("user_qa")
        .unwrap();
    assert_eq!(
        send(&app, invoice("evt_invoice_b_qa", NOW + 30 * DAY)).await,
        StatusCode::OK
    );
    let second = store
        .lock()
        .unwrap()
        .user_premium_expiry("user_qa")
        .unwrap();
    assert_eq!(
        second, first,
        "Same invoice and paid period must not extend access twice"
    );
}

#[tokio::test]
async fn historical_invoice_must_not_create_new_access_after_paid_period() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    assert_eq!(
        send(&app, invoice("evt_expired_invoice_qa", NOW - 30 * DAY)).await,
        StatusCode::OK
    );
    assert!(
        !store
            .lock()
            .unwrap()
            .is_user_premium("user_qa", NOW)
            .unwrap(),
        "Expired paid period must not grant a fresh day"
    );
}

#[tokio::test]
async fn current_invoice_parent_shape_must_activate_paid_subscription() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    let mut paid = invoice("evt_current_invoice_qa", NOW + 30 * DAY);
    paid["data"]["object"]
        .as_object_mut()
        .unwrap()
        .remove("subscription");
    paid["data"]["object"]["parent"] =
        json!({"type":"subscription_details","subscription_details":{"subscription":"sub_qa"}});
    paid["api_version"] = json!("2026-06-24.dahlia");
    assert_eq!(
        send(&app, paid).await,
        StatusCode::OK,
        "Current invoice parent schema must be parsed"
    );
    assert!(
        store
            .lock()
            .unwrap()
            .is_user_premium("user_qa", NOW)
            .unwrap()
    );
}

#[tokio::test]
async fn paid_through_is_exact_and_renewals_do_not_stack_or_regress() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    for (id, end, expected) in [
        ("evt_short", NOW + 3_600_000, NOW + 3_600_000),
        ("evt_month", NOW + 30 * DAY, NOW + 30 * DAY),
        ("evt_renewal", NOW + 60 * DAY, NOW + 60 * DAY),
        ("evt_delayed", NOW + 15 * DAY, NOW + 60 * DAY),
    ] {
        let mut paid = invoice(id, end);
        paid["data"]["object"]["id"] = json!(format!("in_{id}"));
        assert_eq!(send(&app, paid).await, StatusCode::OK);
        let store = store.lock().unwrap();
        assert_eq!(
            store.user_premium_expiry("user_qa").unwrap(),
            Some(expected)
        );
        assert_eq!(
            store
                .stripe_subscription("sub_qa")
                .unwrap()
                .unwrap()
                .current_period_end,
            expected
        );
    }
}

#[tokio::test]
async fn longer_existing_benefits_are_preserved_for_users_and_guild_passes() {
    for plan in ["plus", "premium", "max"] {
        let (app, store) = fixture();
        {
            let store = store.lock().unwrap();
            store
                .grant_user_premium("user_qa", 90, "kofi", NOW)
                .unwrap();
            store
                .grant_guild_pass("user_qa", 5, 90, "kofi", NOW)
                .unwrap();
        }
        let mut session = checkout();
        session["data"]["object"]["metadata"]["plan"] = json!(plan);
        session["data"]["object"]["metadata"]["seats"] = json!("2");
        assert_eq!(send(&app, session).await, StatusCode::OK);
        assert_eq!(
            send(&app, invoice("evt_paid", NOW + 30 * DAY)).await,
            StatusCode::OK
        );
        let store = store.lock().unwrap();
        assert_eq!(
            store.user_premium_expiry("user_qa").unwrap(),
            Some(NOW + 90 * DAY)
        );
        let pass = store.premium_pass("user_qa").unwrap().unwrap();
        assert_eq!(pass.expires_at, NOW + 90 * DAY);
        assert_eq!(pass.seats, 5);
        assert_eq!(pass.source, "kofi");
    }
}

#[tokio::test]
async fn guild_pass_repeated_and_renewed_invoices_use_absolute_paid_time() {
    for (plan, seats) in [("premium", 2), ("max", 5)] {
        let (app, store) = fixture();
        let mut session = checkout();
        session["data"]["object"]["metadata"]["plan"] = json!(plan);
        session["data"]["object"]["metadata"]["seats"] = json!(seats.to_string());
        assert_eq!(send(&app, session).await, StatusCode::OK);
        for (id, end) in [
            ("evt_a", NOW + 30 * DAY),
            ("evt_b", NOW + 30 * DAY),
            ("evt_c", NOW + 60 * DAY),
        ] {
            assert_eq!(send(&app, invoice(id, end)).await, StatusCode::OK);
            let store = store.lock().unwrap();
            let pass = store.premium_pass("user_qa").unwrap().unwrap();
            assert_eq!(pass.expires_at, end);
            assert_eq!(pass.seats, seats);
            assert_eq!(pass.source, "stripe");
            assert!(!store.is_user_premium("unrelated_user_qa", NOW).unwrap());
        }
    }
}

#[tokio::test]
async fn expired_invoice_does_not_reactivate_cancelled_subscription_or_pass() {
    let (app, store) = fixture();
    let mut session = checkout();
    session["data"]["object"]["metadata"]["plan"] = json!("premium");
    assert_eq!(send(&app, session).await, StatusCode::OK);
    assert_eq!(
        send(
            &app,
            event(
                "evt_cancel",
                "customer.subscription.deleted",
                json!({"id":"sub_qa","status":"canceled","current_period_end":(NOW-DAY)/1000})
            )
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, invoice("evt_expired", NOW - DAY)).await,
        StatusCode::OK
    );
    let store = store.lock().unwrap();
    assert!(store.premium_pass("user_qa").unwrap().is_none());
    assert_eq!(
        store.stripe_subscription("sub_qa").unwrap().unwrap().status,
        "canceled"
    );
    assert!(store.stripe_event_processed("evt_expired").unwrap());
}

#[tokio::test]
async fn invalid_paid_periods_are_rejected_without_consuming_event() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    for invalid in [
        Value::Null,
        json!(-1),
        json!(0),
        json!(i64::MAX),
        json!("tomorrow"),
    ] {
        let mut paid = invoice("evt_invalid_period", NOW + DAY);
        paid["data"]["object"]["lines"]["data"][0]["period"]["end"] = invalid;
        assert_eq!(send(&app, paid).await, StatusCode::BAD_REQUEST);
        let store = store.lock().unwrap();
        assert!(!store.stripe_event_processed("evt_invalid_period").unwrap());
        assert!(store.user_premium_expiry("user_qa").unwrap().is_none());
    }
    assert_eq!(
        send(&app, invoice("evt_invalid_period", NOW + DAY)).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn modern_failed_invoice_preserves_already_paid_access() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    assert_eq!(
        send(&app, invoice("evt_paid", NOW + 30 * DAY)).await,
        StatusCode::OK
    );
    let failed = event(
        "evt_failed",
        "invoice.payment_failed",
        json!({"id":"in_failed",
        "parent":{"type":"subscription_details","subscription_details":{"subscription":"sub_qa"}}}),
    );
    assert_eq!(send(&app, failed).await, StatusCode::OK);
    let store = store.lock().unwrap();
    assert_eq!(
        store.stripe_subscription("sub_qa").unwrap().unwrap().status,
        "past_due"
    );
    assert_eq!(
        store.user_premium_expiry("user_qa").unwrap(),
        Some(NOW + 30 * DAY)
    );
}

#[tokio::test]
async fn invoice_parent_discriminator_and_conflicting_identities_are_rejected() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    for (kind, subscription) in [
        ("quote_details", "sub_qa"),
        ("subscription_details", "sub_other"),
    ] {
        let mut paid = invoice("evt_bad_parent", NOW + DAY);
        paid["data"]["object"]["parent"] = json!({"type":kind,
            "subscription_details":{"subscription":subscription}});
        assert_eq!(send(&app, paid).await, StatusCode::BAD_REQUEST);
        let store = store.lock().unwrap();
        assert!(!store.stripe_event_processed("evt_bad_parent").unwrap());
        assert!(store.user_premium_expiry("user_qa").unwrap().is_none());
    }
}

#[tokio::test]
async fn modern_subscription_items_update_period_without_granting_unpaid_access() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    let updated = event(
        "evt_modern_subscription",
        "customer.subscription.updated",
        json!({
            "id":"sub_qa", "status":"past_due",
            "items":{"data":[{"id":"si_qa", "current_period_end":(NOW + 30 * DAY)/1000}]}
        }),
    );
    assert_eq!(send(&app, updated).await, StatusCode::OK);
    let store = store.lock().unwrap();
    let subscription = store.stripe_subscription("sub_qa").unwrap().unwrap();
    assert_eq!(subscription.current_period_end, NOW + 30 * DAY);
    assert_eq!(subscription.status, "past_due");
    assert!(!store.is_user_premium("user_qa", NOW).unwrap());
}

#[tokio::test]
async fn paid_period_ending_now_grants_neither_plus_nor_guild_pass() {
    for plan in ["plus", "premium"] {
        let (app, store) = fixture();
        let mut session = checkout();
        session["data"]["object"]["metadata"]["plan"] = json!(plan);
        assert_eq!(send(&app, session).await, StatusCode::OK);
        assert_eq!(
            send(&app, invoice("evt_boundary", NOW)).await,
            StatusCode::OK
        );
        let store = store.lock().unwrap();
        assert!(store.user_premium_expiry("user_qa").unwrap().is_none());
        assert!(store.premium_pass("user_qa").unwrap().is_none());
    }
}

#[tokio::test]
async fn matching_legacy_and_current_invoice_ids_are_accepted() {
    let (app, store) = fixture();
    assert_eq!(send(&app, checkout()).await, StatusCode::OK);
    let mut paid = invoice("evt_both_shapes", NOW + DAY);
    paid["data"]["object"]["parent"] = json!({"type":"subscription_details",
        "subscription_details":{"subscription":"sub_qa"}});
    assert_eq!(send(&app, paid).await, StatusCode::OK);
    assert_eq!(
        store
            .lock()
            .unwrap()
            .user_premium_expiry("user_qa")
            .unwrap(),
        Some(NOW + DAY)
    );
}
