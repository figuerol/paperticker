//! `tickerd` — paper portfolio daemon. Caches daily closes from the
//! configured price provider once per UTC day, serves a Unix-socket JSON
//! protocol used by `tickerc`. No real money.
//!
//! No provider is configured by default: until the operator selects one with
//! `tickerctl provider set`, the daemon serves cached data and fetches
//! nothing. That is deliberate — it must never reach a third-party service
//! that the operator has not chosen.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod config;
mod db;
mod portfolio;
mod provider;
mod server;

use config::Config;
use db::Db;
use provider::Providers;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let data_dir = directories::ProjectDirs::from("", "", "ticker-follow")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from(".ticker-follow"));
    std::fs::create_dir_all(&data_dir).context("creating data dir")?;
    let db_path = data_dir.join("portfolio.db");
    info!(?db_path, "opening database");

    let db = Arc::new(Mutex::new(Db::open(&db_path)?));
    let http = reqwest::Client::builder()
        .user_agent("ticker-follow/0.1 (simulation)")
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let cfg = Config::load();
    match cfg.provider.as_deref() {
        Some(id) if cfg.is_ready() => info!(provider = id, "price provider configured"),
        // An unrecognized provider is a different problem from a missing key,
        // and saying "missing API key" when one is right there in the file
        // sends the operator looking in the wrong place.
        Some(id) if ticker_proto::provider::lookup(id).is_none() => warn!(
            provider = id,
            "configured provider is not recognized — no prices will be fetched. \
             Run `tickerctl provider set` to choose one."
        ),
        Some(id) => warn!(
            provider = id,
            "provider selected but not ready (missing API key) — no prices will be fetched"
        ),
        None => info!(
            "no price provider configured — serving cached data only. \
             Run `tickerctl provider set` to choose one."
        ),
    }
    let providers = Providers::new(http, Arc::new(Mutex::new(cfg)));

    refresh_stale(&db, &providers).await;

    {
        let db = Arc::clone(&db);
        let providers = providers.clone();
        tokio::spawn(async move { periodic_refresh_loop(db, providers).await });
    }

    let sock_path = ticker_proto::default_socket_path();
    server::serve(&sock_path, db, providers).await
}

async fn refresh_stale(db: &Arc<Mutex<Db>>, providers: &Providers) {
    let tickers: Vec<String> = {
        let guard = db.lock().await;
        match guard.held_tickers() {
            Ok(t) => t,
            Err(e) => {
                warn!(?e, "could not enumerate held tickers");
                return;
            }
        }
    };
    // Resolve once per cycle rather than per ticker. With none configured
    // this is the normal quiet path, not an error worth a warning per ticker.
    let source = match providers.current().await {
        Ok(s) => s,
        Err(e) => {
            if !tickers.is_empty() {
                info!("skipping refresh: {e}");
            }
            return;
        }
    };

    let today = chrono::Utc::now().date_naive().to_string();
    for ticker in tickers {
        let needs = {
            let guard = db.lock().await;
            match guard.price_last_updated(&ticker) {
                Ok(Some(d)) => d != today,
                Ok(None) => true,
                Err(_) => true,
            }
        };
        if !needs {
            continue;
        }
        match source.fetch(&ticker).await {
            Ok(snapshot) => {
                let guard = db.lock().await;
                if let Err(e) = guard.upsert_price(&snapshot) {
                    warn!(?ticker, ?e, "writing price snapshot failed");
                } else {
                    info!(ticker, provider = source.id(), "refreshed");
                }
            }
            Err(e) => warn!(?ticker, ?e, "price fetch failed"),
        }
    }
}

/// Wake every hour and refresh anything that's stale (i.e. `last_updated`
/// doesn't match today's UTC date). This is resilient to suspend/wake cycles
/// and missed midnight timers — what matters is "do we have today's close",
/// not "did the clock fire at a specific instant".
async fn periodic_refresh_loop(db: Arc<Mutex<Db>>, providers: Providers) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        info!("periodic stale-check");
        refresh_stale(&db, &providers).await;
    }
}
