#![forbid(unsafe_code)]

use axum::Router;
use std::{
    env,
    error::Error,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use vozen_api::entitlements_api::{EntitlementsConfig, entitlements_router};
use vozen_store::SqliteStore;

type BoxError = Box<dyn Error + Send + Sync>;

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let secret = env::var("VOZEN_ENTITLEMENTS_SERVICE_SECRET")
        .map_err(|_| "VOZEN_ENTITLEMENTS_SERVICE_SECRET is required")?;
    if secret.trim().len() < 32 {
        return Err("VOZEN_ENTITLEMENTS_SERVICE_SECRET must be at least 32 bytes".into());
    }
    let database_path = PathBuf::from(env_or("VOZEN_ENTITLEMENTS_DATABASE_PATH", "tts.db"));
    let bind_addr: SocketAddr = env_or("VOZEN_ENTITLEMENT_BIND_ADDR", "127.0.0.1:3011")
        .parse()
        .map_err(|_| "VOZEN_ENTITLEMENT_BIND_ADDR must be host:port")?;
    let store = SqliteStore::open(&database_path)?;
    store.verify_integrity()?;
    let router: Router = entitlements_router(EntitlementsConfig {
        store: Arc::new(Mutex::new(store)),
        service_secret: secret,
    });
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    println!(
        "vozen entitlement daemon listening on {bind_addr} using {}",
        database_path.display()
    );
    axum::serve(listener, router).await?;
    Ok(())
}
