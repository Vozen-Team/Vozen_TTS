//! Explicit, authenticated plan changes. A preview is not a charge; a signed, short-lived
//! confirmation binds the customer, current item, target Price and amount shown to the user.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::Digest;

#[derive(Deserialize)]
struct Choice {
    subscription_id: String,
    plan: String,
    interval: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Confirmation {
    user: String,
    subscription: String,
    item: String,
    from_price: String,
    price: String,
    plan: String,
    interval: String,
    starts: i64,
    ends: i64,
    issued: i64,
    deferred: bool,
    amount: i64,
    currency: String,
    release_schedule: Option<String>,
}
struct Current {
    value: Value,
    item: String,
    price: String,
    plan: String,
    interval: String,
    starts: i64,
    ends: i64,
    release_schedule: Option<String>,
    pending_schedule: Option<(String, String)>,
}

#[derive(Serialize, Deserialize)]
struct CancelConfirmation {
    user: String,
    subscription: String,
    item: String,
    price: String,
    schedule: String,
    target_price: String,
    ends: i64,
    issued: i64,
}

fn failure(state: &StripeState, status: StatusCode, error: &str) -> Response {
    json_no_store_response(status, json!({"error":error}), state)
}
fn guard(state: &StripeState, method: &Method) -> Option<Response> {
    if *method == Method::OPTIONS {
        Some(cors(StatusCode::NO_CONTENT, "", state))
    } else if *method != Method::POST {
        Some(cors(
            StatusCode::METHOD_NOT_ALLOWED,
            "method not allowed",
            state,
        ))
    } else {
        None
    }
}
fn live(state: &StripeState) -> bool {
    state.secret_key.starts_with("sk_live_") || state.secret_key.starts_with("rk_live_")
}
fn provider_id(id: &str, prefix: &str) -> bool {
    id.starts_with(prefix)
        && id.len() <= 200
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn present(value: &Value, key: &str) -> bool {
    value.get(key).is_some_and(|v| !v.is_null())
}
fn unsupported(value: &Value) -> bool {
    value.get("status").and_then(Value::as_str) != Some("active")
        || value.get("cancel_at_period_end").and_then(Value::as_bool) == Some(true)
        || [
            "cancel_at",
            "pause_collection",
            "billing_thresholds",
            "pending_invoice_item_interval",
        ]
        .iter()
        .any(|key| present(value, key))
        || present(value, "pending_update")
        || present(value, "schedule")
        || value.get("collection_method").and_then(Value::as_str) != Some("charge_automatically")
        || value
            .get("discounts")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
        || value
            .get("default_tax_rates")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
        || value
            .pointer("/automatic_tax/enabled")
            .and_then(Value::as_bool)
            == Some(true)
        || ["transfer_data", "application_fee_percent", "on_behalf_of"]
            .iter()
            .any(|key| present(value, key))
}

async fn current(state: &StripeState, user: &str, id: &str) -> Result<Current, StatusCode> {
    if !provider_id(id, "sub_") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let stored = state
        .store
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .stripe_subscription(id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .filter(|sub| sub.user_id == user)
        .ok_or(StatusCode::NOT_FOUND)?;
    let value = state
        .subscription_lookup
        .retrieve(id)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if value.get("id").and_then(Value::as_str) != Some(id)
        || value.get("customer").and_then(Value::as_str) != Some(stored.customer_id.as_str())
        || value.get("livemode").and_then(Value::as_bool) != Some(live(state))
        || value
            .pointer("/metadata/discord_user_id")
            .and_then(Value::as_str)
            != Some(user)
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let items = value
        .pointer("/items/data")
        .and_then(Value::as_array)
        .filter(|items| items.len() == 1)
        .ok_or(StatusCode::CONFLICT)?;
    let item = &items[0];
    if item.get("quantity").and_then(Value::as_i64) != Some(1)
        || item
            .get("discounts")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
        || item
            .get("tax_rates")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
    {
        return Err(StatusCode::CONFLICT);
    }
    let price = item
        .pointer("/price/id")
        .and_then(Value::as_str)
        .ok_or(StatusCode::CONFLICT)?
        .to_owned();
    let (plan, _) = state
        .prices
        .plan_for_price(&price)
        .ok_or(StatusCode::CONFLICT)?;
    let interval = match item
        .pointer("/price/recurring/interval")
        .and_then(Value::as_str)
    {
        Some("month") => "monthly",
        Some("year") => "yearly",
        _ => return Err(StatusCode::CONFLICT),
    };
    if state.prices.get(plan, interval).map(|(p, _)| p) != Some(price.as_str()) {
        return Err(StatusCode::CONFLICT);
    }
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| provider_id(id, "si_"))
        .ok_or(StatusCode::CONFLICT)?
        .to_owned();
    let starts = item
        .get("current_period_start")
        .or_else(|| value.get("current_period_start"))
        .and_then(Value::as_i64)
        .ok_or(StatusCode::CONFLICT)?;
    let ends = item
        .get("current_period_end")
        .or_else(|| value.get("current_period_end"))
        .and_then(Value::as_i64)
        .ok_or(StatusCode::CONFLICT)?;
    if starts <= 0 || starts >= ends || ends <= (state.now)() / 1000 {
        return Err(StatusCode::CONFLICT);
    }
    let mut value = value;
    let mut release_schedule = None;
    let mut pending_schedule = None;
    if let Some(id) = value
        .get("schedule")
        .and_then(Value::as_str)
        .filter(|id| provider_id(id, "sub_sched_"))
    {
        let schedule = state
            .subscription_lookup
            .schedule(id)
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        let effective = schedule
            .pointer("/metadata/effective_at")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<i64>().ok());
        if schedule.get("id").and_then(Value::as_str) == Some(id)
            && schedule.get("subscription").and_then(Value::as_str)
                == Some(stored.subscription_id.as_str())
            && schedule.get("livemode").and_then(Value::as_bool) == Some(live(state))
            && schedule
                .pointer("/metadata/vozen_plan_change")
                .and_then(Value::as_str)
                == Some("v1")
            && schedule
                .pointer("/metadata/discord_user_id")
                .and_then(Value::as_str)
                == Some(user)
            && schedule
                .pointer("/metadata/target_price")
                .and_then(Value::as_str)
                == Some(price.as_str())
            && effective.is_some_and(|at| at <= (state.now)() / 1000)
            && schedule
                .pointer("/current_phase/start_date")
                .and_then(Value::as_i64)
                == effective
        {
            release_schedule = Some(id.to_owned());
            value["schedule"] = Value::Null;
        } else if schedule.get("id").and_then(Value::as_str) == Some(id)
            && schedule.get("subscription").and_then(Value::as_str)
                == Some(stored.subscription_id.as_str())
            && schedule.get("customer").and_then(Value::as_str) == Some(stored.customer_id.as_str())
            && schedule.get("livemode").and_then(Value::as_bool) == Some(live(state))
            && schedule.get("status").and_then(Value::as_str) == Some("active")
            && schedule
                .pointer("/metadata/vozen_plan_change")
                .and_then(Value::as_str)
                == Some("v1")
            && schedule
                .pointer("/metadata/discord_user_id")
                .and_then(Value::as_str)
                == Some(user)
            && effective == Some(ends)
            && schedule
                .pointer("/current_phase/start_date")
                .and_then(Value::as_i64)
                == Some(starts)
            && schedule
                .pointer("/current_phase/end_date")
                .and_then(Value::as_i64)
                == Some(ends)
            && let Some(target) = schedule
                .pointer("/metadata/target_price")
                .and_then(Value::as_str)
                .filter(|target| state.prices.plan_for_price(target).is_some())
        {
            let phase_matches = schedule
                .get("phases")
                .and_then(Value::as_array)
                .is_some_and(|phases| {
                    phases.iter().any(|phase| {
                        phase.get("start_date").and_then(Value::as_i64) == Some(ends)
                            && phase
                                .get("items")
                                .and_then(Value::as_array)
                                .is_some_and(|items| {
                                    items.len() == 1
                                        && items[0].get("price").and_then(Value::as_str)
                                            == Some(target)
                                        && items[0].get("quantity").and_then(Value::as_i64)
                                            == Some(1)
                                })
                    })
                });
            let mut without_schedule = value.clone();
            without_schedule["schedule"] = Value::Null;
            if phase_matches && !unsupported(&without_schedule) {
                pending_schedule = Some((id.to_owned(), target.to_owned()));
            }
        }
    }
    Ok(Current {
        value,
        item: item_id,
        price,
        plan: plan.into(),
        interval: interval.into(),
        starts,
        ends,
        release_schedule,
        pending_schedule,
    })
}

pub(super) async fn plans(
    State(state): State<StripeState>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = guard(&state, &method) {
        return response;
    }
    let Some(user) = identity(&state, &headers).await else {
        return failure(&state, StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let ids = match state
        .store
        .lock()
        .map_err(|_| ())
        .and_then(|db| db.stripe_subscriptions_for_user(&user.id).map_err(|_| ()))
    {
        Ok(ids) if ids.len() <= 10 => ids,
        _ => return failure(&state, StatusCode::CONFLICT, "billing_unavailable"),
    };
    let mut subscriptions = Vec::new();
    for id in ids {
        if let Ok(sub) = current(&state, &user.id, &id).await {
            let cancel_token = sub.pending_schedule.as_ref().map(|(schedule, target)| {
                sign_cancel(
                    &state,
                    &CancelConfirmation {
                        user: user.id.clone(),
                        subscription: id.clone(),
                        item: sub.item.clone(),
                        price: sub.price.clone(),
                        schedule: schedule.clone(),
                        target_price: target.clone(),
                        ends: sub.ends,
                        issued: (state.now)() / 1000,
                    },
                )
            });
            subscriptions.push(
                json!({"id":id,"plan":sub.plan,"interval":sub.interval,"periodEnd":sub.ends,
                "changeable":!unsupported(&sub.value),"cancelToken":cancel_token}),
            );
        }
    }
    json_no_store_response(
        StatusCode::OK,
        json!({"subscriptions":subscriptions}),
        &state,
    )
}

fn sign_cancel(state: &StripeState, confirmation: &CancelConfirmation) -> String {
    let payload = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(confirmation).expect("serializable cancellation"));
    let mut mac =
        HmacSha256::new_from_slice(state.webhook_secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(b"vozen-cancel-plan-change-v1:");
    mac.update(payload.as_bytes());
    format!(
        "{payload}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

pub(super) async fn cancel_pending(
    State(state): State<StripeState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = guard(&state, &method) {
        return response;
    }
    let Some(user) = identity(&state, &headers).await else {
        return failure(&state, StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let token = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("token").and_then(Value::as_str).map(str::to_owned));
    let parsed = (|| {
        let token = token.as_ref().filter(|token| token.len() <= 8192)?;
        let (payload, signature) = token.split_once('.')?;
        let mut mac = HmacSha256::new_from_slice(state.webhook_secret.as_bytes()).ok()?;
        mac.update(b"vozen-cancel-plan-change-v1:");
        mac.update(payload.as_bytes());
        mac.verify_slice(&URL_SAFE_NO_PAD.decode(signature).ok()?)
            .ok()?;
        serde_json::from_slice::<CancelConfirmation>(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
    })();
    let Some(change) = parsed.filter(|change| {
        change.user == user.id
            && (state.now)() / 1000 >= change.issued
            && (state.now)() / 1000 - change.issued <= 300
    }) else {
        return failure(
            &state,
            StatusCode::BAD_REQUEST,
            "preview_expired_or_invalid",
        );
    };
    let _gate = state.change_gate.lock().await;
    if (state.now)() / 1000 - change.issued > 300 {
        return failure(
            &state,
            StatusCode::BAD_REQUEST,
            "preview_expired_or_invalid",
        );
    }
    let sub = match current(&state, &user.id, &change.subscription).await {
        Ok(sub) => sub,
        Err(status) => return failure(&state, status, "subscription_unavailable"),
    };
    if sub.item != change.item
        || sub.price != change.price
        || sub.ends != change.ends
        || sub.pending_schedule != Some((change.schedule.clone(), change.target_price))
    {
        return failure(&state, StatusCode::CONFLICT, "pending_change_stale");
    }
    let digest = Sha256::digest(token.as_ref().expect("verified token").as_bytes());
    let key = format!("vozen-cancel-change-{}", URL_SAFE_NO_PAD.encode(digest));
    let result = state
        .subscription_lookup
        .post(
            &format!("subscription_schedules/{}/release", change.schedule),
            &[],
            &key,
        )
        .await;
    if !result.is_ok_and(|value| {
        value.get("id").and_then(Value::as_str) == Some(change.schedule.as_str())
            && value.get("livemode").and_then(Value::as_bool) == Some(live(&state))
            && value.get("status").and_then(Value::as_str) == Some("released")
    }) {
        return failure(&state, StatusCode::BAD_GATEWAY, "schedule_needs_review");
    }
    json_no_store_response(
        StatusCode::OK,
        json!({"status":"schedule_canceled"}),
        &state,
    )
}

fn sign(state: &StripeState, confirmation: &Confirmation) -> String {
    let payload = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(confirmation).expect("serializable confirmation"));
    let mut mac =
        HmacSha256::new_from_slice(state.webhook_secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(b"vozen-plan-change-v1:");
    mac.update(payload.as_bytes());
    format!(
        "{payload}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}
fn verify(state: &StripeState, token: &str, user: &str) -> Option<Confirmation> {
    if token.len() > 8192 {
        return None;
    }
    let (payload, signature) = token.split_once('.')?;
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let mut mac = HmacSha256::new_from_slice(state.webhook_secret.as_bytes()).ok()?;
    mac.update(b"vozen-plan-change-v1:");
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature).ok()?;
    let value: Confirmation =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    let now = (state.now)() / 1000;
    if value.user != user || value.issued > now || now - value.issued > 300 {
        return None;
    }
    Some(value)
}

pub(super) async fn preview(
    State(state): State<StripeState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = guard(&state, &method) {
        return response;
    }
    if let Some(response) = purchases_paused(&state) {
        return response;
    }
    let Some(user) = identity(&state, &headers).await else {
        return failure(&state, StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let Ok(choice) = serde_json::from_slice::<Choice>(&body) else {
        return failure(&state, StatusCode::BAD_REQUEST, "invalid_request");
    };
    let Some((price, _)) = state.prices.get(&choice.plan, &choice.interval) else {
        return failure(&state, StatusCode::BAD_REQUEST, "unsupported_plan");
    };
    let sub = match current(&state, &user.id, &choice.subscription_id).await {
        Ok(sub) => sub,
        Err(status) => return failure(&state, status, "subscription_unavailable"),
    };
    if unsupported(&sub.value) || sub.price == price {
        return failure(&state, StatusCode::CONFLICT, "subscription_not_changeable");
    }
    let target = match state.subscription_lookup.price(price).await {
        Ok(value) => value,
        Err(()) => return failure(&state, StatusCode::BAD_GATEWAY, "price_unavailable"),
    };
    if target.get("id").and_then(Value::as_str) != Some(price)
        || target.get("active").and_then(Value::as_bool) != Some(true)
        || target.get("livemode").and_then(Value::as_bool) != Some(live(&state))
        || target.get("currency").and_then(Value::as_str) != Some("eur")
        || target
            .pointer("/recurring/interval")
            .and_then(Value::as_str)
            != Some(if choice.interval == "yearly" {
                "year"
            } else {
                "month"
            })
        || target
            .pointer("/recurring/interval_count")
            .and_then(Value::as_i64)
            != Some(1)
    {
        return failure(&state, StatusCode::BAD_GATEWAY, "invalid_price");
    }
    let Some(recurring_amount) = target
        .get("unit_amount")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
    else {
        return failure(&state, StatusCode::BAD_GATEWAY, "invalid_price");
    };
    let rank = |plan: &str| match plan {
        "max" => 5,
        "premium" => 2,
        _ => 1,
    };
    let deferred = rank(&choice.plan) < rank(&sub.plan)
        || (sub.interval == "yearly" && choice.interval == "monthly");
    let now = (state.now)() / 1000;
    let (amount, currency) = if deferred {
        (0, "eur".to_string())
    } else {
        let form = vec![
            ("subscription".into(), choice.subscription_id.clone()),
            (
                "subscription_details[items][0][id]".into(),
                sub.item.clone(),
            ),
            ("subscription_details[items][0][price]".into(), price.into()),
            (
                "subscription_details[items][0][quantity]".into(),
                "1".into(),
            ),
            (
                "subscription_details[proration_date]".into(),
                now.to_string(),
            ),
            (
                "subscription_details[proration_behavior]".into(),
                "always_invoice".into(),
            ),
        ];
        let value = match state
            .subscription_lookup
            .post("invoices/create_preview", &form, "")
            .await
        {
            Ok(value) => value,
            Err(()) => return failure(&state, StatusCode::BAD_GATEWAY, "preview_unavailable"),
        };
        let Some(amount) = value
            .get("amount_due")
            .and_then(Value::as_i64)
            .filter(|n| *n >= 0)
        else {
            return failure(&state, StatusCode::BAD_GATEWAY, "invalid_preview");
        };
        let Some(currency) = value
            .get("currency")
            .and_then(Value::as_str)
            .filter(|s| *s == "eur")
        else {
            return failure(&state, StatusCode::BAD_GATEWAY, "invalid_preview");
        };
        (amount, currency.into())
    };
    let confirmation = Confirmation {
        user: user.id,
        subscription: choice.subscription_id,
        item: sub.item,
        from_price: sub.price,
        price: price.into(),
        plan: choice.plan,
        interval: choice.interval,
        starts: sub.starts,
        ends: sub.ends,
        issued: now,
        deferred,
        amount,
        currency: currency.clone(),
        release_schedule: sub.release_schedule,
    };
    json_no_store_response(
        StatusCode::OK,
        json!({"token":sign(&state,&confirmation),"amountDue":amount,"currency":currency,
        "deferred":deferred,"effectiveAt":if deferred{sub.ends}else{now},"plan":confirmation.plan,"interval":confirmation.interval,"recurringAmount":recurring_amount}),
        &state,
    )
}

pub(super) async fn confirm(
    State(state): State<StripeState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = guard(&state, &method) {
        return response;
    }
    if let Some(response) = purchases_paused(&state) {
        return response;
    }
    let Some(user) = identity(&state, &headers).await else {
        return failure(&state, StatusCode::UNAUTHORIZED, "unauthorized");
    };
    let token = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("token").and_then(Value::as_str).map(str::to_owned));
    let Some(change) = token
        .as_deref()
        .and_then(|token| verify(&state, token, &user.id))
    else {
        return failure(
            &state,
            StatusCode::BAD_REQUEST,
            "preview_expired_or_invalid",
        );
    };
    // One production runtime owns billing. Re-read after locking; Stripe's deterministic
    // idempotency key additionally protects the external request across process retries.
    let _gate = state.change_gate.lock().await;
    if (state.now)() / 1000 - change.issued > 300 {
        return failure(
            &state,
            StatusCode::BAD_REQUEST,
            "preview_expired_or_invalid",
        );
    }
    let sub = match current(&state, &user.id, &change.subscription).await {
        Ok(sub) => sub,
        Err(status) => return failure(&state, status, "subscription_unavailable"),
    };
    if unsupported(&sub.value)
        || sub.price != change.from_price
        || sub.item != change.item
        || sub.starts != change.starts
        || sub.ends != change.ends
        || sub.release_schedule != change.release_schedule
    {
        return failure(&state, StatusCode::CONFLICT, "preview_stale");
    }
    let digest = Sha256::digest(token.as_ref().expect("verified token").as_bytes());
    let key = format!("vozen-change-{}", URL_SAFE_NO_PAD.encode(digest));
    if let Some(id) = &sub.release_schedule {
        let released = state
            .subscription_lookup
            .post(
                &format!("subscription_schedules/{id}/release"),
                &[],
                &format!("{key}-release-completed"),
            )
            .await;
        if !released.is_ok_and(|value| {
            value.get("id").and_then(Value::as_str) == Some(id.as_str())
                && value.get("livemode").and_then(Value::as_bool) == Some(live(&state))
                && value.get("status").and_then(Value::as_str) == Some("released")
        }) {
            return failure(&state, StatusCode::BAD_GATEWAY, "schedule_needs_review");
        }
    }
    if change.deferred {
        return schedule(&state, &sub, &change, &key).await;
    }
    let preview_form = vec![
        ("subscription".into(), change.subscription.clone()),
        (
            "subscription_details[items][0][id]".into(),
            change.item.clone(),
        ),
        (
            "subscription_details[items][0][price]".into(),
            change.price.clone(),
        ),
        (
            "subscription_details[items][0][quantity]".into(),
            "1".into(),
        ),
        (
            "subscription_details[proration_date]".into(),
            change.issued.to_string(),
        ),
        (
            "subscription_details[proration_behavior]".into(),
            "always_invoice".into(),
        ),
    ];
    let current_quote = state
        .subscription_lookup
        .post("invoices/create_preview", &preview_form, "")
        .await;
    if !current_quote.is_ok_and(|value| {
        value.get("amount_due").and_then(Value::as_i64) == Some(change.amount)
            && value.get("currency").and_then(Value::as_str) == Some(change.currency.as_str())
    }) {
        return failure(&state, StatusCode::CONFLICT, "preview_stale");
    }
    let form = vec![
        ("items[0][id]".into(), sub.item),
        ("items[0][price]".into(), change.price),
        ("items[0][quantity]".into(), "1".into()),
        ("proration_behavior".into(), "always_invoice".into()),
        ("payment_behavior".into(), "pending_if_incomplete".into()),
        ("proration_date".into(), change.issued.to_string()),
        ("expand[0]".into(), "latest_invoice".into()),
    ];
    let value = match state
        .subscription_lookup
        .post(
            &format!("subscriptions/{}", change.subscription),
            &form,
            &key,
        )
        .await
    {
        Ok(value) => value,
        Err(()) => return failure(&state, StatusCode::BAD_GATEWAY, "change_unavailable"),
    };
    if value.get("id").and_then(Value::as_str) != Some(change.subscription.as_str())
        || value.get("livemode").and_then(Value::as_bool) != Some(live(&state))
    {
        return failure(&state, StatusCode::BAD_GATEWAY, "invalid_change_response");
    }
    let invoice = value.get("latest_invoice").unwrap_or(&Value::Null);
    let payment_url = invoice
        .get("hosted_invoice_url")
        .and_then(Value::as_str)
        .filter(|url| {
            url::Url::parse(url)
                .is_ok_and(|u| u.scheme() == "https" && u.host_str() == Some("invoice.stripe.com"))
        });
    // Only invoice.paid projects rights. HTTP success or subscription.updated grants nothing.
    json_no_store_response(
        StatusCode::OK,
        json!({"status":if present(&value,"pending_update"){"payment_required"}else{"processing"},"paymentUrl":payment_url}),
        &state,
    )
}

async fn schedule(
    state: &StripeState,
    sub: &Current,
    change: &Confirmation,
    key: &str,
) -> Response {
    let created = match state
        .subscription_lookup
        .post(
            "subscription_schedules",
            &[("from_subscription".into(), change.subscription.clone())],
            &format!("{key}-create"),
        )
        .await
    {
        Ok(value) => value,
        Err(()) => return failure(state, StatusCode::BAD_GATEWAY, "schedule_unavailable"),
    };
    let Some(id) = created
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| provider_id(id, "sub_sched_"))
    else {
        return failure(state, StatusCode::BAD_GATEWAY, "invalid_schedule_response");
    };
    if created.get("subscription").and_then(Value::as_str) != Some(change.subscription.as_str())
        || created.get("livemode").and_then(Value::as_bool) != Some(live(state))
    {
        return failure(state, StatusCode::BAD_GATEWAY, "invalid_schedule_response");
    }
    let start = created
        .pointer("/current_phase/start_date")
        .and_then(Value::as_i64)
        .unwrap_or(sub.starts);
    let mut form = vec![
        ("metadata[vozen_plan_change]".into(), "v1".into()),
        ("metadata[discord_user_id]".into(), change.user.clone()),
        ("metadata[target_price]".into(), change.price.clone()),
        ("metadata[effective_at]".into(), sub.ends.to_string()),
        ("end_behavior".into(), "release".into()),
        ("proration_behavior".into(), "none".into()),
        ("phases[0][start_date]".into(), start.to_string()),
        ("phases[0][end_date]".into(), sub.ends.to_string()),
        ("phases[0][items][0][price]".into(), sub.price.clone()),
        ("phases[0][items][0][quantity]".into(), "1".into()),
        ("phases[0][proration_behavior]".into(), "none".into()),
        ("phases[1][start_date]".into(), sub.ends.to_string()),
        ("phases[1][items][0][price]".into(), change.price.clone()),
        ("phases[1][items][0][quantity]".into(), "1".into()),
        (
            "phases[1][duration][interval]".into(),
            if change.interval == "yearly" {
                "year".into()
            } else {
                "month".into()
            },
        ),
        ("phases[1][duration][interval_count]".into(), "1".into()),
        ("phases[1][proration_behavior]".into(), "none".into()),
    ];
    // Preserve the existing payment method across both phases. Complex discounts/tax/Connect
    // subscriptions are rejected before creation, rather than silently dropping their settings.
    if let Some(method) = sub
        .value
        .get("default_payment_method")
        .and_then(Value::as_str)
    {
        for phase in [0, 1] {
            form.push((
                format!("phases[{phase}][default_payment_method]"),
                method.into(),
            ));
        }
    }
    match state
        .subscription_lookup
        .post(
            &format!("subscription_schedules/{id}"),
            &form,
            &format!("{key}-configure"),
        )
        .await
    {
        Ok(value) if value.get("id").and_then(Value::as_str) == Some(id) => json_no_store_response(
            StatusCode::OK,
            json!({"status":"scheduled","effectiveAt":sub.ends}),
            state,
        ),
        _ => {
            // Release only the schedule created by this request; never cancel the subscription.
            let released = state
                .subscription_lookup
                .post(
                    &format!("subscription_schedules/{id}/release"),
                    &[],
                    &format!("{key}-release"),
                )
                .await
                .is_ok();
            failure(
                state,
                StatusCode::BAD_GATEWAY,
                if released {
                    "schedule_unavailable"
                } else {
                    "schedule_needs_review"
                },
            )
        }
    }
}
