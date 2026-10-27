//! Local-only provider adapters: no credentials, purchases or production subscriptions.
use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, Ordering},
};
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
use vozen_store::{SqliteStore, StripeSubscriptionInput};
const NOW: i64 = 1_800_000_000_000;
const END: i64 = NOW / 1000 + 30 * 86_400;
type Posts = Vec<(String, Vec<(String, String)>, String)>;
struct Identity;
#[async_trait]
impl DiscordIdentityVerifier for Identity {
    async fn resolve_identity(&self, token: &str) -> Result<DiscordIdentity, ()> {
        if !["owner", "other"].contains(&token) {
            return Err(());
        }
        Ok(DiscordIdentity {
            id: token.into(),
            username: "Fixture".into(),
            avatar: None,
            avatar_decoration_asset: None,
        })
    }
    async fn resolve_activation_identity(
        &self,
        _: &str,
    ) -> Result<ActivationIdentity, ActivationIdentityError> {
        Err(ActivationIdentityError::InvalidToken)
    }
}
struct Provider {
    subscription: Mutex<Value>,
    posts: Mutex<Posts>,
    amount: AtomicI64,
    fail_schedule: Mutex<bool>,
}
#[async_trait]
impl StripeSubscriptionLookup for Provider {
    async fn schedule(&self, _: &str) -> Result<Value, ()> {
        self.subscription
            .lock()
            .unwrap()
            .get("qa_schedule")
            .cloned()
            .ok_or(())
    }
    async fn price(&self, id: &str) -> Result<Value, ()> {
        Ok(
            json!({"id":id,"active":true,"livemode":false,"currency":"eur","unit_amount":1499,
        "recurring":{"interval":if id.ends_with("year"){"year"}else{"month"},"interval_count":1}}),
        )
    }
    async fn retrieve(&self, _: &str) -> Result<Value, ()> {
        Ok(self.subscription.lock().unwrap().clone())
    }
    async fn post(&self, route: &str, form: &[(String, String)], key: &str) -> Result<Value, ()> {
        self.posts
            .lock()
            .unwrap()
            .push((route.into(), form.to_vec(), key.into()));
        if route == "invoices/create_preview" {
            return Ok(
                json!({"livemode":false,"amount_due":self.amount.load(Ordering::SeqCst),"currency":"eur"}),
            );
        }
        if route == "subscriptions/sub_owned" {
            self.subscription.lock().unwrap()["pending_update"] = json!({"expires_at":END});
            return Ok(
                json!({"id":"sub_owned","livemode":false,"pending_update":{"expires_at":END},"latest_invoice":{"hosted_invoice_url":"https://invoice.stripe.com/i/test_fixture"}}),
            );
        }
        if route == "subscription_schedules" {
            return Ok(
                json!({"id":"sub_sched_owned","livemode":false,"subscription":"sub_owned","current_phase":{"start_date":NOW/1000}}),
            );
        }
        if route == "subscription_schedules/sub_sched_owned" {
            if *self.fail_schedule.lock().unwrap() {
                return Err(());
            }
            self.subscription.lock().unwrap()["schedule"] = json!("sub_sched_owned");
            return Ok(json!({"id":"sub_sched_owned","livemode":false}));
        }
        if route == "subscription_schedules/sub_sched_owned/release" {
            self.subscription.lock().unwrap()["schedule"] = Value::Null;
            return Ok(json!({"id":"sub_sched_owned","livemode":false,"status":"released"}));
        }
        Err(())
    }
}
fn fixture() -> (
    Router,
    Arc<Mutex<SqliteStore>>,
    Arc<Provider>,
    Arc<AtomicI64>,
) {
    fixture_with_purchases(true)
}

fn fixture_with_purchases(
    purchases_enabled: bool,
) -> (
    Router,
    Arc<Mutex<SqliteStore>>,
    Arc<Provider>,
    Arc<AtomicI64>,
) {
    let db = Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap()));
    db.lock()
        .unwrap()
        .upsert_stripe_subscription(&StripeSubscriptionInput {
            subscription_id: "sub_owned".into(),
            customer_id: "cus_owned".into(),
            user_id: "owner".into(),
            plan: "plus".into(),
            seats: 1,
            current_period_end: END * 1000,
            status: "active".into(),
            updated_at: NOW,
        })
        .unwrap();
    let provider = Arc::new(Provider {
        subscription: Mutex::new(
            json!({"id":"sub_owned","customer":"cus_owned","livemode":false,"status":"active","collection_method":"charge_automatically","metadata":{"discord_user_id":"owner","plan":"plus","seats":"1"},"items":{"data":[{"id":"si_owned","quantity":1,"current_period_start":NOW/1000,"current_period_end":END,"price":{"id":"price_plus_month","recurring":{"interval":"month"}}}]}}),
        ),
        posts: Mutex::new(Vec::new()),
        amount: AtomicI64::new(400),
        fail_schedule: Mutex::new(false),
    });
    let clock = Arc::new(AtomicI64::new(NOW));
    let now = clock.clone();
    let app = stripe_router_with_subscription_lookup(
        StripeApiConfig {
            origin: "https://bot.example.com".into(),
            purchases_enabled,
            secret_key: "sk_test_fixture".into(),
            publishable_key: "pk_test_fixture".into(),
            webhook_secret: "whsec_fixture_not_real".into(),
            prices: StripePriceIds {
                plus_monthly: "price_plus_month".into(),
                plus_yearly: "price_plus_year".into(),
                premium_monthly: "price_premium_month".into(),
                premium_yearly: "price_premium_year".into(),
                max_monthly: "price_max_month".into(),
                max_yearly: "price_max_year".into(),
            },
            store: db.clone(),
            identity_verifier: Arc::new(Identity),
            now: Arc::new(move || now.load(Ordering::SeqCst)),
        },
        provider.clone(),
    )
    .unwrap();
    (app, db, provider, clock)
}
async fn request(app: &Router, path: &str, user: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("authorization", format!("Bearer {user}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let data = serde_json::from_slice(&to_bytes(response.into_body(), 64_000).await.unwrap())
        .unwrap_or(Value::Null);
    (status, data)
}
async fn preview(app: &Router, plan: &str, interval: &str) -> Value {
    let (status, value) = request(
        app,
        "/api/billing/change/preview",
        "owner",
        json!({"subscription_id":"sub_owned","plan":plan,"interval":interval}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}
fn has(form: &[(String, String)], key: &str, value: &str) -> bool {
    form.iter().any(|(k, v)| k == key && v == value)
}

#[tokio::test]
async fn paused_purchases_block_before_identity_and_keep_read_only_billing() {
    let (app, db, provider, _) = fixture_with_purchases(false);
    for path in [
        "/api/billing/checkout",
        "/api/billing/checkout/elements",
        "/api/billing/change/preview",
        "/api/billing/change/confirm",
    ] {
        for user in ["owner", "other", "invalid"] {
            let (status, value) = request(&app, path, user, json!({})).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(value["error"], "payments_disabled");
        }
        let response = app
            .clone()
            .oneshot(Request::post(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "https://bot.example.com"
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    assert!(provider.posts.lock().unwrap().is_empty());
    assert!(!db.lock().unwrap().is_user_premium("owner", NOW).unwrap());
    let (status, _) = request(&app, "/api/billing/plans", "owner", json!({})).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn preview_is_read_only_and_confirm_uses_paid_pending_upgrade() {
    let (app, db, provider, _) = fixture();
    let quote = preview(&app, "max", "monthly").await;
    assert_eq!(quote["amountDue"], 400);
    assert_eq!(quote["deferred"], false);
    assert_eq!(provider.posts.lock().unwrap().len(), 1);
    assert!(!db.lock().unwrap().is_user_premium("owner", NOW).unwrap());
    let (status, result) = request(
        &app,
        "/api/billing/change/confirm",
        "owner",
        json!({"token":quote["token"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["status"], "payment_required");
    {
        let posts = provider.posts.lock().unwrap();
        let (_, form, key) = posts
            .iter()
            .find(|(route, _, _)| route == "subscriptions/sub_owned")
            .unwrap();
        assert!(has(form, "payment_behavior", "pending_if_incomplete"));
        assert!(has(form, "proration_behavior", "always_invoice"));
        assert!(has(form, "items[0][quantity]", "1"));
        assert!(key.starts_with("vozen-change-"));
    }
    assert!(
        db.lock()
            .unwrap()
            .premium_status("owner", NOW)
            .unwrap()
            .pass
            .is_none()
    );
    let (duplicate, _) = request(
        &app,
        "/api/billing/change/confirm",
        "owner",
        json!({"token":quote["token"]}),
    )
    .await;
    assert_eq!(duplicate, StatusCode::CONFLICT);
    assert_eq!(
        provider
            .posts
            .lock()
            .unwrap()
            .iter()
            .filter(|(r, _, _)| r == "subscriptions/sub_owned")
            .count(),
        1
    );
}

#[tokio::test]
async fn downgrade_schedules_cross_product_at_exact_paid_boundary_without_charging() {
    let (app, _, provider, _) = fixture();
    provider.subscription.lock().unwrap()["items"]["data"][0]["price"]["id"] =
        json!("price_max_month");
    let quote = preview(&app, "premium", "monthly").await;
    assert_eq!(quote["amountDue"], 0);
    assert_eq!(quote["effectiveAt"], END);
    assert!(provider.posts.lock().unwrap().is_empty());
    let (status, result) = request(
        &app,
        "/api/billing/change/confirm",
        "owner",
        json!({"token":quote["token"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["status"], "scheduled");
    let posts = provider.posts.lock().unwrap();
    assert_eq!(posts.len(), 2);
    let (_, form, _) = posts.last().unwrap();
    assert!(has(form, "phases[0][items][0][price]", "price_max_month"));
    assert!(has(form, "phases[0][end_date]", &END.to_string()));
    assert!(has(form, "phases[1][start_date]", &END.to_string()));
    assert!(has(
        form,
        "phases[1][items][0][price]",
        "price_premium_month"
    ));
    assert!(has(form, "proration_behavior", "none"));
}

#[tokio::test]
async fn confirmation_rejects_tampering_other_user_expiry_changed_amount_and_stale_subscription() {
    for case in [
        "tampered",
        "other_user",
        "expired",
        "amount_changed",
        "stale",
    ] {
        let (app, _, provider, clock) = fixture();
        let quote = preview(&app, "max", "monthly").await;
        let mut token = quote["token"].as_str().unwrap().to_string();
        let mut user = "owner";
        match case {
            "tampered" => token.push('x'),
            "other_user" => user = "other",
            "expired" => clock.store(NOW + 301_000, Ordering::SeqCst),
            "amount_changed" => provider.amount.store(499, Ordering::SeqCst),
            "stale" => {
                provider.subscription.lock().unwrap()["items"]["data"][0]["price"]["id"] =
                    json!("price_premium_month")
            }
            _ => unreachable!(),
        }
        let (status, _) = request(
            &app,
            "/api/billing/change/confirm",
            user,
            json!({"token":token}),
        )
        .await;
        assert!(!status.is_success(), "{case}");
        assert!(
            provider
                .posts
                .lock()
                .unwrap()
                .iter()
                .all(|(route, _, _)| route == "invoices/create_preview")
        );
    }
}

#[tokio::test]
async fn foreign_subscription_and_complex_or_wrong_mode_records_are_not_modified() {
    for (pointer, value) in [
        ("/customer", json!("cus_other")),
        ("/livemode", json!(true)),
        ("/metadata/discord_user_id", json!("other")),
        ("/items/data/0/quantity", json!(2)),
        ("/status", json!("past_due")),
    ] {
        let (app, _, provider, _) = fixture();
        *provider
            .subscription
            .lock()
            .unwrap()
            .pointer_mut(pointer)
            .unwrap() = value;
        let (status, _) = request(
            &app,
            "/api/billing/change/preview",
            "owner",
            json!({"subscription_id":"sub_owned","plan":"max","interval":"monthly"}),
        )
        .await;
        assert!(!status.is_success(), "{pointer}");
        assert!(provider.posts.lock().unwrap().is_empty());
    }
    let (app, _, provider, _) = fixture();
    let (status, _) = request(
        &app,
        "/api/billing/change/preview",
        "other",
        json!({"subscription_id":"sub_owned","plan":"max","interval":"monthly"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(provider.posts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn schedule_configuration_failure_releases_only_its_new_schedule() {
    let (app, _, provider, _) = fixture();
    provider.subscription.lock().unwrap()["items"]["data"][0]["price"]["id"] =
        json!("price_max_month");
    *provider.fail_schedule.lock().unwrap() = true;
    let quote = preview(&app, "premium", "monthly").await;
    let (status, result) = request(
        &app,
        "/api/billing/change/confirm",
        "owner",
        json!({"token":quote["token"]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(result["error"], "schedule_unavailable");
    let posts = provider.posts.lock().unwrap();
    assert_eq!(
        posts.last().unwrap().0,
        "subscription_schedules/sub_sched_owned/release"
    );
    assert!(
        !posts
            .iter()
            .any(|(r, _, _)| r.starts_with("subscriptions/"))
    );
}

#[tokio::test]
async fn completed_owned_schedule_does_not_lock_customer_out_of_a_paid_upgrade() {
    let (app, _, provider, _) = fixture();
    {
        let mut sub = provider.subscription.lock().unwrap();
        sub["items"]["data"][0]["price"]["id"] = json!("price_premium_month");
        sub["schedule"] = json!("sub_sched_owned");
        sub["qa_schedule"] = json!({"id":"sub_sched_owned","subscription":"sub_owned","livemode":false,
            "current_phase":{"start_date":NOW/1000},"metadata":{"vozen_plan_change":"v1","discord_user_id":"owner","target_price":"price_premium_month","effective_at":(NOW/1000).to_string()}});
    }
    let quote = preview(&app, "max", "monthly").await;
    let (status, _) = request(
        &app,
        "/api/billing/change/confirm",
        "owner",
        json!({"token":quote["token"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let posts = provider.posts.lock().unwrap();
    let release = posts
        .iter()
        .position(|(r, _, _)| r == "subscription_schedules/sub_sched_owned/release")
        .unwrap();
    let upgrade = posts
        .iter()
        .position(|(r, _, _)| r == "subscriptions/sub_owned")
        .unwrap();
    assert!(release < upgrade);
}

#[tokio::test]
async fn a_schedule_owned_by_another_integration_is_not_released_or_replaced() {
    let (app, _, provider, _) = fixture();
    {
        let mut sub = provider.subscription.lock().unwrap();
        sub["schedule"] = json!("sub_sched_owned");
        sub["qa_schedule"] = json!({"id":"sub_sched_owned","subscription":"sub_owned","livemode":false,"metadata":{}});
    }
    let (status, _) = request(
        &app,
        "/api/billing/change/preview",
        "owner",
        json!({"subscription_id":"sub_owned","plan":"max","interval":"monthly"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(provider.posts.lock().unwrap().is_empty());
}

fn pending_owned_schedule(provider: &Provider) {
    let mut sub = provider.subscription.lock().unwrap();
    sub["schedule"] = json!("sub_sched_owned");
    sub["qa_schedule"] = json!({"id":"sub_sched_owned","subscription":"sub_owned","customer":"cus_owned","livemode":false,"status":"active",
        "current_phase":{"start_date":NOW/1000,"end_date":END},
        "metadata":{"vozen_plan_change":"v1","discord_user_id":"owner","target_price":"price_premium_month","effective_at":END.to_string()},
        "phases":[{"start_date":NOW/1000,"end_date":END,"items":[{"price":"price_plus_month","quantity":1}]},
        {"start_date":END,"items":[{"price":"price_premium_month","quantity":1}]}]});
}

#[tokio::test]
async fn pending_owned_change_can_be_removed_without_canceling_or_changing_paid_rights() {
    let (app, db, provider, _) = fixture();
    pending_owned_schedule(&provider);
    let before = db.lock().unwrap().stripe_subscription("sub_owned").unwrap();
    let (status, listed) = request(&app, "/api/billing/plans", "owner", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let sub = &listed["subscriptions"][0];
    assert_eq!(sub["changeable"], false);
    let token = sub["cancelToken"]
        .as_str()
        .expect("owned pending-change token");
    assert!(provider.posts.lock().unwrap().is_empty());
    let (status, result) = request(
        &app,
        "/api/billing/change/cancel",
        "owner",
        json!({"token":token}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["status"], "schedule_canceled");
    assert_eq!(
        db.lock().unwrap().stripe_subscription("sub_owned").unwrap(),
        before
    );
    assert_eq!(
        provider.subscription.lock().unwrap()["items"]["data"][0]["price"]["id"],
        "price_plus_month"
    );
    assert_eq!(provider.posts.lock().unwrap().len(), 1);
    assert_eq!(
        provider.posts.lock().unwrap()[0].0,
        "subscription_schedules/sub_sched_owned/release"
    );
    let (status, _) = request(
        &app,
        "/api/billing/change/cancel",
        "owner",
        json!({"token":token}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(provider.posts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn removing_a_pending_change_rechecks_identity_time_schedule_and_mode() {
    for case in [
        "tampered",
        "other_user",
        "expired",
        "changed_schedule",
        "wrong_mode",
        "foreign_metadata",
        "boundary",
    ] {
        let (app, _, provider, clock) = fixture();
        pending_owned_schedule(&provider);
        let (_, listed) = request(&app, "/api/billing/plans", "owner", json!({})).await;
        let mut token = listed["subscriptions"][0]["cancelToken"]
            .as_str()
            .expect("cancel token")
            .to_string();
        let mut user = "owner";
        match case {
            "tampered" => token.push('x'),
            "other_user" => user = "other",
            "expired" => clock.store(NOW + 301_000, Ordering::SeqCst),
            "changed_schedule" => {
                provider.subscription.lock().unwrap()["schedule"] = json!("sub_sched_other")
            }
            "wrong_mode" => {
                provider.subscription.lock().unwrap()["qa_schedule"]["livemode"] = json!(true)
            }
            "foreign_metadata" => {
                provider.subscription.lock().unwrap()["qa_schedule"]["metadata"]["discord_user_id"] =
                    json!("other")
            }
            "boundary" => clock.store(END * 1000, Ordering::SeqCst),
            _ => unreachable!(),
        }
        let (status, _) = request(
            &app,
            "/api/billing/change/cancel",
            user,
            json!({"token":token}),
        )
        .await;
        assert!(!status.is_success(), "{case}");
        assert!(provider.posts.lock().unwrap().is_empty(), "{case}");
    }
}
