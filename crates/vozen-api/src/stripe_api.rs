//! Stripe Checkout, Billing Portal and webhook integration.
//!
//! This module creates Stripe Checkout Sessions for both the Stripe-hosted fallback and the
//! on-site Payment Element. Vozen never receives card data or shipping addresses. The only
//! identity sent to Stripe is the already-authenticated Discord user ID.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use vozen_store::{
    SqliteStore, StripeEventApplyOutcome, StripeEventInput, StripeSubscriptionInput,
};

use crate::premium_api::DiscordIdentityVerifier;

type HmacSha256 = Hmac<Sha256>;

const STRIPE_API: &str = "https://api.stripe.com/v1";
// Pin the contract used by the server. Stripe's account default moved to this version, where
// `ui_mode=embedded` was removed in favour of `embedded_page`.
const STRIPE_API_VERSION: &str = "2026-06-24.dahlia";
const SIGNATURE_TOLERANCE_SECONDS: i64 = 300;

/// Convert Stripe's Unix-seconds timestamps to the store's Unix-milliseconds unit.
fn stripe_period_end_ms(seconds: i64) -> Option<i64> {
    seconds.checked_mul(1_000)
}

#[derive(Debug, Clone)]
pub struct StripePriceIds {
    pub plus_monthly: String,
    pub plus_yearly: String,
    pub premium_monthly: String,
    pub premium_yearly: String,
    pub max_monthly: String,
    pub max_yearly: String,
}

impl StripePriceIds {
    fn get(&self, plan: &str, interval: &str) -> Option<(&str, i64)> {
        match (plan, interval) {
            ("plus", "monthly") => Some((&self.plus_monthly, 1)),
            ("plus", "yearly") => Some((&self.plus_yearly, 1)),
            ("premium", "monthly") => Some((&self.premium_monthly, 2)),
            ("premium", "yearly") => Some((&self.premium_yearly, 2)),
            ("max", "monthly") => Some((&self.max_monthly, 5)),
            ("max", "yearly") => Some((&self.max_yearly, 5)),
            _ => None,
        }
    }
}

pub struct StripeApiConfig {
    pub origin: String,
    pub secret_key: String,
    pub publishable_key: String,
    pub webhook_secret: String,
    pub prices: StripePriceIds,
    pub store: Arc<Mutex<SqliteStore>>,
    pub identity_verifier: Arc<dyn DiscordIdentityVerifier>,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripeApiConfigError {
    Origin,
    MissingSecret,
    MissingPublishableKey,
    MissingWebhookSecret,
    MissingPrice,
}

impl std::fmt::Display for StripeApiConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Origin => "Stripe requires a valid site origin",
            Self::MissingSecret => "Stripe secret key is missing",
            Self::MissingPublishableKey => "Stripe publishable key is missing",
            Self::MissingWebhookSecret => "Stripe webhook secret is missing",
            Self::MissingPrice => "a Stripe price ID is missing",
        })
    }
}
impl std::error::Error for StripeApiConfigError {}

#[derive(Clone)]
struct StripeState {
    origin: HeaderValue,
    secret_key: Arc<str>,
    publishable_key: Arc<str>,
    webhook_secret: Arc<str>,
    prices: Arc<StripePriceIds>,
    store: Arc<Mutex<SqliteStore>>,
    verifier: Arc<dyn DiscordIdentityVerifier>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    client: reqwest::Client,
    subscription_lookup: Arc<dyn StripeSubscriptionLookup>,
}

pub fn stripe_router(config: StripeApiConfig) -> Result<Router, StripeApiConfigError> {
    build_stripe_router(config, None)
}

/// Provider boundary for deterministic offline billing tests. Production always uses Stripe's
/// authenticated HTTPS API; this adapter does not change the provider URL or expose credentials.
#[async_trait::async_trait]
pub trait StripeSubscriptionLookup: Send + Sync {
    async fn retrieve(&self, subscription_id: &str) -> Result<Value, ()>;
}

struct ProviderSubscriptionLookup {
    client: reqwest::Client,
    secret: String,
}

#[async_trait::async_trait]
impl StripeSubscriptionLookup for ProviderSubscriptionLookup {
    async fn retrieve(&self, subscription_id: &str) -> Result<Value, ()> {
        self.client
            .get(format!("{STRIPE_API}/subscriptions/{subscription_id}"))
            .header("Stripe-Version", STRIPE_API_VERSION)
            .basic_auth(&self.secret, Some(""))
            .send()
            .await
            .map_err(|_| ())?
            .error_for_status()
            .map_err(|_| ())?
            .json()
            .await
            .map_err(|_| ())
    }
}

pub fn stripe_router_with_subscription_lookup(
    config: StripeApiConfig,
    lookup: Arc<dyn StripeSubscriptionLookup>,
) -> Result<Router, StripeApiConfigError> {
    build_stripe_router(config, Some(lookup))
}

fn build_stripe_router(
    config: StripeApiConfig,
    lookup: Option<Arc<dyn StripeSubscriptionLookup>>,
) -> Result<Router, StripeApiConfigError> {
    if config.secret_key.trim().is_empty() {
        return Err(StripeApiConfigError::MissingSecret);
    }
    if config.publishable_key.trim().is_empty() {
        return Err(StripeApiConfigError::MissingPublishableKey);
    }
    if config.webhook_secret.trim().is_empty() {
        return Err(StripeApiConfigError::MissingWebhookSecret);
    }
    if config.prices.get("plus", "monthly").is_none()
        || [
            &config.prices.plus_monthly,
            &config.prices.plus_yearly,
            &config.prices.premium_monthly,
            &config.prices.premium_yearly,
            &config.prices.max_monthly,
            &config.prices.max_yearly,
        ]
        .iter()
        .any(|id| id.trim().is_empty())
    {
        return Err(StripeApiConfigError::MissingPrice);
    }
    let origin = HeaderValue::from_str(&config.origin).map_err(|_| StripeApiConfigError::Origin)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let subscription_lookup = lookup.unwrap_or_else(|| {
        Arc::new(ProviderSubscriptionLookup {
            client: client.clone(),
            secret: config.secret_key.clone(),
        })
    });
    let router = Router::new()
        .route("/api/billing/checkout", any(checkout))
        .route("/api/billing/checkout/elements", any(checkout_elements))
        .route("/api/billing/checkout/status", any(checkout_status))
        .route("/api/billing/portal", any(portal))
        .route("/webhook/stripe", any(webhook))
        .with_state(StripeState {
            origin,
            secret_key: Arc::from(config.secret_key),
            publishable_key: Arc::from(config.publishable_key),
            webhook_secret: Arc::from(config.webhook_secret),
            prices: Arc::new(config.prices),
            store: config.store,
            verifier: config.identity_verifier,
            now: config.now,
            client,
            subscription_lookup,
        })
        .layer(axum::extract::DefaultBodyLimit::max(256_000));
    Ok(router)
}

#[derive(Deserialize)]
struct CheckoutInput {
    plan: String,
    interval: String,
}

#[derive(Deserialize)]
struct CheckoutStatusInput {
    session_id: String,
}

async fn checkout(
    State(state): State<StripeState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method == Method::OPTIONS {
        return cors(StatusCode::NO_CONTENT, "", &state);
    }
    if method != Method::POST {
        return cors(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", &state);
    }
    let Some(user) = identity(&state, &headers).await else {
        return cors(StatusCode::UNAUTHORIZED, "unauthorized", &state);
    };
    let Ok(input) = serde_json::from_slice::<CheckoutInput>(&body) else {
        return cors(StatusCode::BAD_REQUEST, "invalid checkout request", &state);
    };
    let Some((price, seats)) = state.prices.get(&input.plan, &input.interval) else {
        return cors(StatusCode::BAD_REQUEST, "unsupported plan", &state);
    };
    let origin = state
        .origin
        .to_str()
        .unwrap_or("https://vozen.org")
        .trim_end_matches('/');
    let success_url = format!("{origin}/account?checkout=success");
    let cancel_url = format!("{origin}/#premium");
    let form = [
        ("mode", "subscription".to_owned()),
        ("line_items[0][price]", price.to_owned()),
        ("line_items[0][quantity]", "1".to_owned()),
        ("client_reference_id", user.id.clone()),
        ("metadata[discord_user_id]", user.id.clone()),
        ("metadata[plan]", input.plan.clone()),
        ("metadata[seats]", seats.to_string()),
        (
            "subscription_data[metadata][discord_user_id]",
            user.id.clone(),
        ),
        ("subscription_data[metadata][plan]", input.plan.clone()),
        ("subscription_data[metadata][seats]", seats.to_string()),
        // Use Stripe-hosted Checkout so payment remains available even when a CDN security
        // policy blocks third-party scripts or frames on vozen.org. Stripe returns customers to
        // the account page after success and to pricing when they cancel.
        ("ui_mode", "hosted_page".to_owned()),
        ("success_url", success_url),
        ("cancel_url", cancel_url),
        ("allow_promotion_codes", "true".to_owned()),
    ];
    let response = state
        .client
        .post(format!("{STRIPE_API}/checkout/sessions"))
        .header("Stripe-Version", STRIPE_API_VERSION)
        .basic_auth(&*state.secret_key, Some(""))
        .form(&form)
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            match response
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v.get("url").and_then(Value::as_str).map(str::to_owned))
            {
                Some(url) => json_response(StatusCode::OK, json!({ "url": url }), &state),
                None => json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({"error":"stripe_response"}),
                    &state,
                ),
            }
        }
        Ok(response) => {
            let status = response.status();
            let request_id = response
                .headers()
                .get("request-id")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("unknown")
                .to_owned();
            let body = response.text().await.unwrap_or_default();
            let fields = stripe_error_fields(&body);
            eprintln!(
                "[stripe] checkout rejected status={} request_id={} type={} code={} param={}",
                status.as_u16(),
                request_id,
                fields.kind,
                fields.code,
                fields.param
            );
            json_response(
                StatusCode::BAD_GATEWAY,
                json!({"error":"stripe_provider_rejected"}),
                &state,
            )
        }
        Err(error) => {
            eprintln!(
                "[stripe] checkout transport failure timeout={} connect={}",
                error.is_timeout(),
                error.is_connect()
            );
            json_response(
                StatusCode::BAD_GATEWAY,
                json!({"error":"stripe_unavailable"}),
                &state,
            )
        }
    }
}

/// Creates a Checkout Session for Stripe's custom Checkout / Payment Element integration.
/// The client secret is deliberately returned with no-store caching and is never persisted by
/// the browser. Stripe remains responsible for every payment field and confirmation.
async fn checkout_elements(
    State(state): State<StripeState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method == Method::OPTIONS {
        return cors(StatusCode::NO_CONTENT, "", &state);
    }
    if method != Method::POST {
        return cors(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", &state);
    }
    let Some(user) = identity(&state, &headers).await else {
        return cors(StatusCode::UNAUTHORIZED, "unauthorized", &state);
    };
    let Ok(input) = serde_json::from_slice::<CheckoutInput>(&body) else {
        return cors(StatusCode::BAD_REQUEST, "invalid checkout request", &state);
    };
    let Some((price, seats)) = state.prices.get(&input.plan, &input.interval) else {
        return cors(StatusCode::BAD_REQUEST, "unsupported plan", &state);
    };
    let form = [
        ("mode", "subscription".to_owned()),
        ("line_items[0][price]", price.to_owned()),
        ("line_items[0][quantity]", "1".to_owned()),
        ("client_reference_id", user.id.clone()),
        ("metadata[discord_user_id]", user.id.clone()),
        ("metadata[plan]", input.plan.clone()),
        ("metadata[seats]", seats.to_string()),
        (
            "subscription_data[metadata][discord_user_id]",
            user.id.clone(),
        ),
        ("subscription_data[metadata][plan]", input.plan.clone()),
        ("subscription_data[metadata][seats]", seats.to_string()),
        // The production Stripe account rejects Checkout's custom UI mode. Its pinned
        // API contract accepts the embedded page instead, which still keeps Checkout
        // inside Vozen and never sends card data through our application.
        ("ui_mode", "embedded_page".to_owned()),
        ("redirect_on_completion", "never".to_owned()),
        ("allow_promotion_codes", "true".to_owned()),
    ];
    match state
        .client
        .post(format!("{STRIPE_API}/checkout/sessions"))
        .header("Stripe-Version", STRIPE_API_VERSION)
        .basic_auth(&*state.secret_key, Some(""))
        .form(&form)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            let payload = response.json::<Value>().await.ok();
            let session_id = payload
                .as_ref()
                .and_then(|value| value.get("id").and_then(Value::as_str));
            let client_secret = payload
                .as_ref()
                .and_then(|value| value.get("client_secret").and_then(Value::as_str));
            match (session_id, client_secret) {
                (Some(session_id), Some(client_secret)) => json_no_store_response(
                    StatusCode::OK,
                    json!({
                        "sessionId": session_id,
                        "clientSecret": client_secret,
                        "publishableKey": &*state.publishable_key,
                    }),
                    &state,
                ),
                _ => json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({"error":"stripe_response"}),
                    &state,
                ),
            }
        }
        Ok(response) => stripe_checkout_rejected(response, &state).await,
        Err(error) => stripe_checkout_transport_error(error, &state),
    }
}

/// Returns only state needed to resume a redirect payment method. The request is bound to the
/// Discord identity recorded on the Checkout Session, so a guessed session ID cannot be queried
/// by another signed-in user.
async fn checkout_status(
    State(state): State<StripeState>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method == Method::OPTIONS {
        return cors(StatusCode::NO_CONTENT, "", &state);
    }
    if method != Method::POST {
        return cors(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", &state);
    }
    let Some(user) = identity(&state, &headers).await else {
        return cors(StatusCode::UNAUTHORIZED, "unauthorized", &state);
    };
    let Ok(input) = serde_json::from_slice::<CheckoutStatusInput>(&body) else {
        return cors(
            StatusCode::BAD_REQUEST,
            "invalid checkout status request",
            &state,
        );
    };
    if !valid_stripe_session_id(&input.session_id) {
        return cors(StatusCode::BAD_REQUEST, "invalid checkout session", &state);
    }
    match state
        .client
        .get(format!(
            "{STRIPE_API}/checkout/sessions/{}",
            input.session_id
        ))
        .header("Stripe-Version", STRIPE_API_VERSION)
        .basic_auth(&*state.secret_key, Some(""))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => match response.json::<Value>().await {
            Ok(session)
                if session.get("client_reference_id").and_then(Value::as_str)
                    == Some(user.id.as_str()) =>
            {
                json_no_store_response(
                    StatusCode::OK,
                    json!({
                        "status": session.get("status").and_then(Value::as_str).unwrap_or("unknown"),
                        "paymentStatus": session.get("payment_status").and_then(Value::as_str).unwrap_or("unknown"),
                    }),
                    &state,
                )
            }
            Ok(_) => json_no_store_response(
                StatusCode::FORBIDDEN,
                json!({"error":"checkout_identity_mismatch"}),
                &state,
            ),
            Err(_) => json_response(
                StatusCode::BAD_GATEWAY,
                json!({"error":"stripe_response"}),
                &state,
            ),
        },
        Ok(response) => stripe_checkout_rejected(response, &state).await,
        Err(error) => stripe_checkout_transport_error(error, &state),
    }
}

fn valid_stripe_session_id(value: &str) -> bool {
    value.starts_with("cs_")
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

async fn stripe_checkout_rejected(response: reqwest::Response, state: &StripeState) -> Response {
    let status = response.status();
    let request_id = response
        .headers()
        .get("request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_owned();
    let body = response.text().await.unwrap_or_default();
    let fields = stripe_error_fields(&body);
    eprintln!(
        "[stripe] checkout rejected status={} request_id={} type={} code={} param={}",
        status.as_u16(),
        request_id,
        fields.kind,
        fields.code,
        fields.param
    );
    json_response(
        StatusCode::BAD_GATEWAY,
        json!({"error":"stripe_provider_rejected"}),
        state,
    )
}

fn stripe_checkout_transport_error(error: reqwest::Error, state: &StripeState) -> Response {
    eprintln!(
        "[stripe] checkout transport failure timeout={} connect={}",
        error.is_timeout(),
        error.is_connect()
    );
    json_response(
        StatusCode::BAD_GATEWAY,
        json!({"error":"stripe_unavailable"}),
        state,
    )
}

async fn portal(State(state): State<StripeState>, method: Method, headers: HeaderMap) -> Response {
    if method == Method::OPTIONS {
        return cors(StatusCode::NO_CONTENT, "", &state);
    }
    if method != Method::POST {
        return cors(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", &state);
    }
    let Some(user) = identity(&state, &headers).await else {
        return cors(StatusCode::UNAUTHORIZED, "unauthorized", &state);
    };
    let customer = state
        .store
        .lock()
        .ok()
        .and_then(|store| store.stripe_customer_for_user(&user.id).ok().flatten());
    let Some(customer) = customer else {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({"error":"no_subscription"}),
            &state,
        );
    };
    let form = [
        ("customer", customer),
        (
            "return_url",
            format!(
                "{}/account",
                state.origin.to_str().unwrap_or("https://vozen.org")
            ),
        ),
    ];
    match state
        .client
        .post(format!("{STRIPE_API}/billing_portal/sessions"))
        .header("Stripe-Version", STRIPE_API_VERSION)
        .basic_auth(&*state.secret_key, Some(""))
        .form(&form)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => match response
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("url").and_then(Value::as_str).map(str::to_owned))
        {
            Some(url) => json_response(StatusCode::OK, json!({"url": url}), &state),
            None => json_response(
                StatusCode::BAD_GATEWAY,
                json!({"error":"stripe_response"}),
                &state,
            ),
        },
        _ => json_response(
            StatusCode::BAD_GATEWAY,
            json!({"error":"stripe_unavailable"}),
            &state,
        ),
    }
}

async fn webhook(State(state): State<StripeState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(signature) = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let now = (state.now)() / 1000;
    if !verify_signature(&body, signature, &state.webhook_secret, now) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let event_id = event.get("id").and_then(Value::as_str).unwrap_or_default();
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_id.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let object = event
        .pointer("/data/object")
        .cloned()
        .unwrap_or(Value::Null);
    let input = match stripe_event_input(event_type, &object) {
        Ok(input) => input,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    match state
        .store
        .lock()
        .map_err(|_| ())
        .and_then(|db| db.stripe_event_processed(event_id).map_err(|_| ()))
    {
        Ok(true) => return StatusCode::OK.into_response(),
        Ok(false) => {}
        Err(()) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    let recovered = if let StripeEventInput::InvoicePaid {
        subscription_id, ..
    } = &input
    {
        let existing = match state
            .store
            .lock()
            .map_err(|_| ())
            .and_then(|db| db.stripe_subscription(subscription_id).map_err(|_| ()))
        {
            Ok(existing) => existing,
            Err(()) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        if let Some(existing) = existing {
            if object
                .get("customer")
                .and_then(Value::as_str)
                .is_some_and(|id| id != existing.customer_id)
            {
                return StatusCode::BAD_REQUEST.into_response();
            }
            None
        } else {
            if !subscription_id.starts_with("sub_")
                || subscription_id.len() > 200
                || !subscription_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                return StatusCode::BAD_REQUEST.into_response();
            }
            let Some(customer) = object
                .get("customer")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let value = match state.subscription_lookup.retrieve(subscription_id).await {
                Ok(value) => value,
                Err(()) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };
            let Some(recovered) = recovered_subscription(&value, subscription_id, customer, &state)
            else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            Some(recovered)
        }
    } else {
        None
    };
    let outcome = state.store.lock().map_err(|_| ()).and_then(|store| {
        store
            .apply_stripe_event_once_with_subscription(
                event_id,
                &input,
                recovered.as_ref(),
                (state.now)(),
            )
            .map_err(|_| ())
    });
    match outcome {
        Ok(StripeEventApplyOutcome::Applied | StripeEventApplyOutcome::AlreadyApplied) => {
            StatusCode::OK.into_response()
        }
        Err(()) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn recovered_subscription(
    value: &Value,
    id: &str,
    customer: &str,
    state: &StripeState,
) -> Option<StripeSubscriptionInput> {
    let live = state.secret_key.starts_with("sk_live_") || state.secret_key.starts_with("rk_live_");
    if value.get("id")?.as_str()? != id
        || value.get("customer")?.as_str()? != customer
        || value.get("livemode")?.as_bool()? != live
    {
        return None;
    }
    let metadata = value.get("metadata")?;
    let user_id = metadata.get("discord_user_id")?.as_str()?.trim();
    if user_id.is_empty() {
        return None;
    }
    let plan = metadata.get("plan")?.as_str()?;
    let items = value.pointer("/items/data")?.as_array()?;
    if items.len() != 1 || items[0].get("quantity")?.as_i64()? != 1 {
        return None;
    }
    let interval = match items[0].pointer("/price/recurring/interval")?.as_str()? {
        "month" => "monthly",
        "year" => "yearly",
        _ => return None,
    };
    let (price, seats) = state.prices.get(plan, interval)?;
    if items[0].pointer("/price/id")?.as_str()? != price
        || metadata.get("seats")?.as_str()?.parse::<i64>().ok()? != seats
    {
        return None;
    }
    let seconds = value
        .get("current_period_end")
        .and_then(Value::as_i64)
        .or_else(|| items[0].get("current_period_end").and_then(Value::as_i64))?;
    let period_end = stripe_period_end_ms(seconds).filter(|v| *v > 0)?;
    let status = value.get("status")?.as_str()?;
    if !matches!(
        status,
        "active"
            | "past_due"
            | "canceled"
            | "unpaid"
            | "trialing"
            | "incomplete"
            | "incomplete_expired"
            | "paused"
    ) {
        return None;
    }
    Some(StripeSubscriptionInput {
        subscription_id: id.into(),
        customer_id: customer.into(),
        user_id: user_id.into(),
        plan: plan.into(),
        seats,
        current_period_end: period_end,
        status: status.into(),
        updated_at: (state.now)(),
    })
}

fn stripe_event_input(event_type: &str, object: &Value) -> Result<StripeEventInput, String> {
    match event_type {
        "checkout.session.completed" => {
            let subscription_id = object
                .get("subscription")
                .and_then(Value::as_str)
                .ok_or("missing subscription id")?;
            let customer_id = object
                .get("customer")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let metadata = object
                .get("metadata")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let user_id = metadata
                .get("discord_user_id")
                .and_then(Value::as_str)
                .or_else(|| object.get("client_reference_id").and_then(Value::as_str))
                .unwrap_or_default();
            let plan = metadata
                .get("plan")
                .and_then(Value::as_str)
                .unwrap_or("plus");
            let seats = metadata
                .get("seats")
                .and_then(Value::as_str)
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            if customer_id.is_empty() || user_id.is_empty() {
                return Err("Stripe checkout missing identity metadata".into());
            }
            Ok(StripeEventInput::Checkout {
                subscription_id: subscription_id.into(),
                customer_id: customer_id.into(),
                user_id: user_id.into(),
                plan: plan.into(),
                seats,
            })
        }
        "invoice.paid" => Ok(StripeEventInput::InvoicePaid {
            subscription_id: invoice_subscription_id(object)?.into(),
            // A missing/invalid paid-through date must never invent a new access period.
            period_end: Some(
                object
                    .pointer("/lines/data/0/period/end")
                    .and_then(Value::as_i64)
                    .filter(|seconds| *seconds > 0)
                    .and_then(stripe_period_end_ms)
                    .ok_or("invoice missing valid paid period")?,
            ),
        }),
        "customer.subscription.updated" | "customer.subscription.deleted" => {
            Ok(StripeEventInput::SubscriptionUpdated {
                subscription_id: object
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("missing subscription id")?
                    .into(),
                period_end: object
                    .get("current_period_end")
                    .and_then(Value::as_i64)
                    .or_else(|| {
                        // Vozen subscriptions contain one recurring price. Since Basil,
                        // its current period lives on the subscription item instead.
                        object
                            .pointer("/items/data/0/current_period_end")
                            .and_then(Value::as_i64)
                    })
                    .and_then(stripe_period_end_ms),
                status: object
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("active")
                    .into(),
            })
        }
        "invoice.payment_failed" => Ok(StripeEventInput::InvoiceFailed {
            subscription_id: invoice_subscription_id(object)?.into(),
        }),
        _ => Ok(StripeEventInput::Ignored),
    }
}

/// Stripe Basil and later moved invoice.subscription under a discriminated parent.
/// Keep legacy webhooks compatible, but reject ambiguous identities rather than guessing.
fn invoice_subscription_id(object: &Value) -> Result<&str, String> {
    let legacy = object
        .get("subscription")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or("invalid invoice subscription")
        })
        .transpose()?;
    let current = object
        .get("parent")
        .filter(|value| !value.is_null())
        .map(|parent| {
            if parent.get("type").and_then(Value::as_str) != Some("subscription_details") {
                return Err("invoice parent is not a subscription");
            }
            parent
                .pointer("/subscription_details/subscription")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or("invoice parent missing subscription")
        })
        .transpose()?;
    if matches!((legacy, current), (Some(a), Some(b)) if a != b) {
        return Err("invoice has conflicting subscriptions".into());
    }
    legacy
        .or(current)
        .ok_or_else(|| "invoice missing subscription".into())
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then_some(token)
}

async fn identity(
    state: &StripeState,
    headers: &HeaderMap,
) -> Option<crate::premium_api::DiscordIdentity> {
    state
        .verifier
        .resolve_identity(bearer_token(headers)?)
        .await
        .ok()
}

fn verify_signature(body: &[u8], header: &str, secret: &str, now: i64) -> bool {
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        let mut pieces = part.splitn(2, '=');
        match (pieces.next(), pieces.next()) {
            (Some("t"), Some(value)) => timestamp = value.parse::<i64>().ok(),
            (Some("v1"), Some(value)) => signatures.push(value),
            _ => {}
        }
    }
    let Some(timestamp) = timestamp else {
        return false;
    };
    if (now - timestamp).abs() > SIGNATURE_TOLERANCE_SECONDS {
        return false;
    }
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    signatures.iter().any(|value| {
        let Ok(found) = hex_bytes(value) else {
            return false;
        };
        found.as_slice().ct_eq(expected.as_slice()).into()
    })
}

fn hex_bytes(value: &str) -> Result<Vec<u8>, ()> {
    if value.len() != 64 {
        return Err(());
    }
    let mut out = Vec::with_capacity(32);
    let bytes = value.as_bytes();
    for i in (0..64).step_by(2) {
        let hi = (bytes[i] as char).to_digit(16).ok_or(())?;
        let lo = (bytes[i + 1] as char).to_digit(16).ok_or(())?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
struct StripeErrorFields {
    kind: String,
    code: String,
    param: String,
}

/// Extract only safe diagnostic fields from a Stripe error. The provider's message is omitted:
/// it can contain request-specific values, while type/code/param are enough to debug the
/// integration in server logs without copying payment or identity data into the log stream.
fn stripe_error_fields(body: &str) -> StripeErrorFields {
    let error = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("error").cloned())
        .unwrap_or(Value::Null);
    let field = |name: &str| {
        error
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned()
    };
    StripeErrorFields {
        kind: field("type"),
        code: field("code"),
        param: field("param"),
    }
}

fn json_response(status: StatusCode, value: Value, state: &StripeState) -> Response {
    let mut response = (status, axum::Json(value)).into_response();
    add_cors(response.headers_mut(), state);
    response
}

fn json_no_store_response(status: StatusCode, value: Value, state: &StripeState) -> Response {
    let mut response = json_response(status, value, state);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn cors(status: StatusCode, message: &str, state: &StripeState) -> Response {
    let mut response = (status, message.to_owned()).into_response();
    add_cors(response.headers_mut(), state);
    response
}
fn add_cors(headers: &mut HeaderMap, state: &StripeState) {
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, state.origin.clone());
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization,content-type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST,OPTIONS"),
    );
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signature_accepts_valid_payload_and_rejects_stale() {
        let body = br#"{"id":"evt_test"}"#;
        let timestamp = 1_000;
        let mut mac = HmacSha256::new_from_slice(b"whsec_test").unwrap();
        mac.update(format!("{timestamp}.").as_bytes());
        mac.update(body);
        let digest = mac.finalize().into_bytes();
        let signature = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert!(verify_signature(
            body,
            &format!("t={timestamp},v1={signature}"),
            "whsec_test",
            timestamp
        ));
        assert!(!verify_signature(
            body,
            &format!("t={timestamp},v1={signature}"),
            "whsec_test",
            timestamp + 301
        ));
    }

    #[test]
    fn stripe_error_fields_omit_provider_message_and_unknown_shape_is_safe() {
        let fields = stripe_error_fields(
            r#"{"error":{"type":"invalid_request_error","code":"parameter_unknown","param":"ui_mode","message":"do not copy this into logs"}}"#,
        );
        assert_eq!(
            fields,
            StripeErrorFields {
                kind: "invalid_request_error".into(),
                code: "parameter_unknown".into(),
                param: "ui_mode".into(),
            }
        );
        assert_eq!(stripe_error_fields("not json").kind, "unknown");
    }

    #[test]
    fn checkout_status_only_accepts_safe_stripe_session_ids() {
        assert!(valid_stripe_session_id("cs_test_123"));
        assert!(!valid_stripe_session_id("pi_test_123"));
        assert!(!valid_stripe_session_id("cs_test/../other"));
        assert!(!valid_stripe_session_id("cs_test space"));
    }
}
