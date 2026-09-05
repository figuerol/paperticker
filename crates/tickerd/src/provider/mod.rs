//! Price data providers.
//!
//! One trait, one implementation per source. The daemon holds an
//! `Arc<dyn PriceSource>` built from whatever the operator configured; no
//! provider is selected by default, so the daemon reaches no third-party
//! service until someone has chosen one.
//!
//! **Ask for roughly six months of daily closes.** Enough to fill the 20-day
//! Bollinger window many times over and to redraw the chart after a long
//! offline gap. Each provider expresses that in its own dialect — Alpha
//! Vantage's `outputsize=compact` is its last 100 points — so the window
//! lives in each implementation rather than in a shared constant.
//!
//! **A provider only gets an implementation here if its terms permit
//! automated fetches and the local cache.** That gate is documented with the
//! catalog in `ticker_proto::provider`.
//!
//! **Respect the provider's request rate.** A provider whose service
//! publishes a rate limit enforces it here with a [`Throttle`], because
//! nothing above this module paces its calls: a refresh walks every held
//! ticker in a tight loop, and a cache miss can fire a fetch from any client
//! connection at any time. The throttle therefore has to be **process-wide
//! and outlive a single fetch** — [`Providers`] rebuilds the `PriceSource`
//! per fetch on purpose, so per-instance state would reset every call and
//! enforce nothing.
//!
//! **Providers must never put a key in an error.** Alpha Vantage — the only
//! one here that takes a key — requires it as a query parameter, so its
//! errors are built from the provider name and ticker alone and never quote
//! the request URL. A provider that can authenticate by header should, since
//! that keeps the credential out of the URL entirely. Anything a provider
//! returns as `Err` can end up in the daemon log and in a client's terminal.

pub mod alphavantage;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use ticker_proto::SecretString;
// tokio's `Instant`, not `std`'s, so the throttle follows the runtime clock
// and its tests can run on paused time instead of really sleeping.
use tokio::time::Instant;

use crate::db::PriceSnapshot;

#[async_trait]
pub trait PriceSource: Send + Sync {
    /// Catalog id — matches `ticker_proto::provider::catalog()`.
    fn id(&self) -> &'static str;

    /// Daily closes plus a current price for one symbol, oldest point first.
    async fn fetch(&self, ticker: &str) -> Result<PriceSnapshot>;
}

/// Build the configured provider.
///
/// `key` is required for providers whose catalog entry says so; passing one
/// to a provider that takes no key is not an error, it is ignored.
pub fn build(
    id: &str,
    key: Option<&SecretString>,
    http: reqwest::Client,
) -> Result<Arc<dyn PriceSource>> {
    let info = ticker_proto::provider::lookup(id)
        .ok_or_else(|| anyhow!("unknown provider {id:?}"))?;

    let key = if info.requires_key {
        Some(
            key.filter(|k| !k.is_empty())
                .ok_or_else(|| anyhow!("provider {id} requires an API key"))?,
        )
    } else {
        None
    };

    match id {
        "alphavantage" => Ok(Arc::new(alphavantage::AlphaVantage::new(
            http,
            // The catalog says this one requires a key, so the branch above
            // has already refused a missing one.
            key.expect("alphavantage requires a key").clone(),
        ))),
        _ => Err(anyhow!("provider {id} is in the catalog but has no implementation")),
    }
}

/// Resolves the configured provider at the moment of use.
///
/// Deliberately rebuilt per fetch rather than cached: it is only an `Arc`
/// clone and a `reqwest::Client` clone (itself refcounted), and it means a
/// `tickerctl provider set` takes effect on the next refresh instead of
/// requiring the operator to restart the daemon.
#[derive(Clone)]
pub struct Providers {
    http: reqwest::Client,
    config: Arc<tokio::sync::Mutex<crate::config::Config>>,
}

impl Providers {
    pub fn new(
        http: reqwest::Client,
        config: Arc<tokio::sync::Mutex<crate::config::Config>>,
    ) -> Self {
        Self { http, config }
    }

    /// The active provider, or an error naming what the operator must do.
    /// This is the single point at which "nothing configured" is enforced —
    /// there is no default, so the daemon cannot reach a third party until
    /// someone has chosen one.
    pub async fn current(&self) -> Result<Arc<dyn PriceSource>> {
        let cfg = self.config.lock().await;
        let Some(id) = cfg.provider.as_deref() else {
            return Err(anyhow!(
                "no price data provider is configured — run `tickerctl provider set` to choose one"
            ));
        };
        build(id, cfg.api_key.as_ref(), self.http.clone())
    }

    pub async fn fetch(&self, ticker: &str) -> Result<PriceSnapshot> {
        self.current().await?.fetch(ticker).await
    }

    /// The live config, shared with the daemon. `dispatch` mutates this when
    /// the operator selects a provider, and `current()` reads it on the next
    /// fetch — which is what makes a change take effect without a restart.
    pub fn config(&self) -> &Arc<tokio::sync::Mutex<crate::config::Config>> {
        &self.config
    }
}

/// Spaces outbound requests so a provider's published rate limit is never
/// exceeded, however many callers are asking at once.
///
/// Holds the time the last request was *released*, not completed — the limit
/// providers publish is on how often requests arrive, not on how many may be
/// in flight, so a slow response does not earn the next caller a free slot.
///
/// The lock is deliberately held across the sleep. That is what makes
/// concurrent callers queue: each one waits for the caller ahead of it to
/// pick its slot before computing its own. Releasing the lock first would let
/// N tasks all read the same `last`, all sleep the same amount, and all fire
/// together — exactly the burst this exists to prevent.
pub struct Throttle {
    interval: Duration,
    last: tokio::sync::Mutex<Option<Instant>>,
}

impl Throttle {
    /// `const` so a provider can hold one in a `static` — the throttle has to
    /// outlive the per-fetch `PriceSource` instances to mean anything.
    pub const fn new(interval: Duration) -> Self {
        Self { interval, last: tokio::sync::Mutex::const_new(None) }
    }

    /// Wait until sending is within the rate limit, then claim that slot.
    ///
    /// Returns how long it waited, which the caller can log — a refresh that
    /// looks hung is usually just this doing its job.
    pub async fn acquire(&self) -> Duration {
        let mut last = self.last.lock().await;
        let waited = match *last {
            // `checked_sub` rather than `-`: `Instant` subtraction panics if
            // the deadline has already passed, which is the common case.
            Some(prev) => match (prev + self.interval).checked_duration_since(Instant::now()) {
                Some(remaining) => {
                    tokio::time::sleep(remaining).await;
                    remaining
                }
                None => Duration::ZERO,
            },
            None => Duration::ZERO,
        };
        *last = Some(Instant::now());
        waited
    }
}

/// Trim a response body for inclusion in an error, without slicing through a
/// multi-byte codepoint (provider error pages routinely contain curly quotes
/// and accented text).
pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut cut = n;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[test]
    fn every_catalog_entry_has_an_implementation() {
        // Catches a provider added to the shared catalog without a matching
        // impl here — the client would offer it and the daemon would refuse.
        for info in ticker_proto::provider::catalog() {
            let key = info.requires_key.then(|| SecretString::new("test-key"));
            let built = build(&info.id, key.as_ref(), http());
            assert!(built.is_ok(), "no implementation for {}: {:?}", info.id, built.err());
            assert_eq!(built.unwrap().id(), info.id);
        }
    }

    #[test]
    fn key_requiring_providers_refuse_to_build_without_one() {
        for info in ticker_proto::provider::catalog() {
            if info.requires_key {
                assert!(build(&info.id, None, http()).is_err(), "{} built with no key", info.id);
                let empty = SecretString::new("");
                assert!(
                    build(&info.id, Some(&empty), http()).is_err(),
                    "{} built with an empty key",
                    info.id
                );
            }
        }
    }

    #[test]
    fn unknown_provider_is_rejected() {
        assert!(build("definitely-not-a-provider", None, http()).is_err());
    }

    // The throttle tests run on tokio's paused clock: `sleep` advances virtual
    // time instead of really waiting, so they assert exact spacing in
    // milliseconds and still finish instantly.
    const TICK: Duration = Duration::from_millis(1_000);

    #[tokio::test(start_paused = true)]
    async fn the_first_request_is_not_delayed() {
        let t = Throttle::new(TICK);
        assert_eq!(t.acquire().await, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn consecutive_requests_are_spaced_by_the_interval() {
        let t = Throttle::new(TICK);
        let start = Instant::now();
        t.acquire().await;
        t.acquire().await;
        t.acquire().await;
        // Three requests, two gaps.
        assert_eq!(start.elapsed(), 2 * TICK);
    }

    #[tokio::test(start_paused = true)]
    async fn a_caller_that_waited_long_enough_is_not_delayed_again() {
        let t = Throttle::new(TICK);
        t.acquire().await;
        tokio::time::sleep(TICK * 3).await;
        // The interval is a floor, not a schedule: idling past it must not
        // bank credit, but it must not cost anything either.
        assert_eq!(t.acquire().await, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_queue_instead_of_bursting() {
        // The regression this guards: if `acquire` released the lock before
        // sleeping, all four tasks would read the same `last`, sleep the same
        // amount, and fire together — one interval of total elapsed time
        // instead of three.
        let t = Arc::new(Throttle::new(TICK));
        let start = Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let t = Arc::clone(&t);
            tasks.spawn(async move {
                t.acquire().await;
                Instant::now()
            });
        }
        let mut fired: Vec<Instant> = tasks.join_all().await;
        fired.sort();

        assert_eq!(start.elapsed(), 3 * TICK, "four requests should span three intervals");
        for pair in fired.windows(2) {
            assert!(
                pair[1].duration_since(pair[0]) >= TICK,
                "two requests fired {:?} apart, under the {TICK:?} limit",
                pair[1].duration_since(pair[0])
            );
        }
    }

    #[test]
    fn truncate_does_not_panic_on_multibyte_boundary() {
        let mut s = "a".repeat(199);
        s.push('é');
        s.push_str(&"b".repeat(50));
        assert!(truncate(&s, 200).ends_with('…'));
        assert_eq!(truncate("héllo", 0), "…");
        assert_eq!(truncate("hello", 100), "hello");
    }
}
