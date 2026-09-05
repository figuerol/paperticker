//! Alpha Vantage daily time-series client.
//!
//! Documented API, free tier, key required. It has no header auth — the key
//! must go in the query string — so this module is careful in
//! two ways: no error it produces ever quotes the request URL, and every
//! response body that reaches an error goes through [`Self::scrub`] first, in
//! case the service echoes the key back at us.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;
use ticker_proto::{HistoryPoint, SecretString};
use tracing::{debug, info, warn};

use super::{truncate, PriceSource, Throttle};
use crate::db::PriceSnapshot;

const ENDPOINT: &str = "https://www.alphavantage.co/query";

/// Requests per minute assumed when nothing says otherwise: the free tier's
/// "spread your requests out to one per second", which Alpha Vantage returns
/// in the `Information` field of an otherwise-200 response.
///
/// **The shipped default must stay the free-tier rate.** A premium plan is
/// the exception, and an operator who has one opts in; a binary that assumed
/// otherwise would burst on every free key in the wild.
const FREE_TIER_RPM: u32 = 60;

/// The published rate is measured when a request *arrives* at Alpha Vantage,
/// not when we send it. Pacing exactly on the boundary leaves nothing for
/// network jitter to eat, so every computed interval gets 10% of headroom —
/// on a premium rate as much as on the free one.
const JITTER_MARGIN: f64 = 1.1;

/// Overrides [`FREE_TIER_RPM`]. A number is the requests-per-minute your plan
/// allows; `off` (or `unlimited`, or `0`) removes the pacing entirely.
const RPM_ENV: &str = "PAPERTICKER_ALPHAVANTAGE_RPM";

/// Shared by every `AlphaVantage` in the process, and `None` when an operator
/// has turned pacing off.
///
/// It has to be a `static`: `Providers` rebuilds the provider on each fetch
/// (so `provider set` needs no restart), so a throttle owned by the struct
/// would be born fresh — and empty — for every single request.
///
/// Read from the environment once, on the first fetch, rather than per call:
/// this is a property of the operator's Alpha Vantage plan, and a rate limit
/// that could change under a running daemon would be a poor thing to reason
/// about.
static RATE_LIMIT: LazyLock<Option<Throttle>> = LazyLock::new(|| {
    let raw = std::env::var(RPM_ENV).ok();
    let interval = match interval_from_rpm(raw.as_deref()) {
        Ok(i) => i,
        Err(why) => {
            // A typo must not silently become "unlimited" — fall back to the
            // rate that is safe on every plan, and say so loudly.
            warn!("{RPM_ENV}: {why} — falling back to {FREE_TIER_RPM} requests/minute");
            interval_from_rpm(None).expect("the default is valid")
        }
    };
    match interval {
        Some(i) => {
            info!(interval_ms = i.as_millis() as u64, "alphavantage requests are rate-limited");
            Some(Throttle::new(i))
        }
        None => {
            warn!(
                "alphavantage rate limiting is disabled by {RPM_ENV} — only correct on a \
                 premium plan; on the free tier this will be throttled at the far end"
            );
            None
        }
    }
});

/// Parse the configured requests-per-minute into a minimum request interval.
/// `Ok(None)` means the operator asked for no pacing at all.
///
/// Kept separate from the environment read so it can be tested directly —
/// a process-wide `set_var` in a test would race every other test in the
/// binary, and is `unsafe` besides.
fn interval_from_rpm(setting: Option<&str>) -> Result<Option<Duration>, String> {
    let rpm = match setting.map(str::trim) {
        None | Some("") => FREE_TIER_RPM,
        Some(s) if s.eq_ignore_ascii_case("off") || s.eq_ignore_ascii_case("unlimited") => {
            return Ok(None)
        }
        Some(s) => s
            .parse::<u32>()
            .map_err(|_| format!("expected a requests-per-minute number or `off`, got {s:?}"))?,
    };
    if rpm == 0 {
        return Ok(None);
    }
    Ok(Some(Duration::from_secs_f64(60.0 / f64::from(rpm) * JITTER_MARGIN)))
}

pub struct AlphaVantage {
    http: reqwest::Client,
    key: SecretString,
}

impl AlphaVantage {
    pub fn new(http: reqwest::Client, key: SecretString) -> Self {
        // Resolve the rate limit now rather than on the first fetch, so its
        // log line lands at daemon startup — that is where an operator who
        // just set `PAPERTICKER_ALPHAVANTAGE_RPM` looks to see it took, and
        // a portfolio with no holdings would otherwise never print it. The
        // initializer runs once however many times this is called.
        LazyLock::force(&RATE_LIMIT);
        Self { http, key }
    }

    /// Remove the API key from text that is about to become an error message.
    /// Belt and braces: the service is not known to echo the key, but an
    /// error string can reach both the daemon log and a client's terminal,
    /// and this costs nothing.
    fn scrub(&self, text: &str) -> String {
        let key = self.key.expose_secret();
        if key.is_empty() {
            return text.to_string();
        }
        text.replace(key, "[redacted]")
    }

    fn err_body(&self, body: &str) -> String {
        truncate(&self.scrub(body), 200)
    }
}

/// Alpha Vantage reports failures with HTTP 200 and one of these keys, so the
/// status code alone never tells you whether a call worked.
#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "Error Message")]
    error_message: Option<String>,
    /// Historically the rate-limit message.
    #[serde(rename = "Note")]
    note: Option<String>,
    /// Currently used for both rate-limit and invalid-key responses.
    #[serde(rename = "Information")]
    information: Option<String>,
    #[serde(rename = "Time Series (Daily)")]
    series: Option<BTreeMap<String, Bar>>,
}

#[derive(Deserialize)]
struct Bar {
    /// Values arrive as strings, not numbers.
    #[serde(rename = "4. close")]
    close: String,
}

#[async_trait]
impl PriceSource for AlphaVantage {
    fn id(&self) -> &'static str {
        "alphavantage"
    }

    async fn fetch(&self, ticker: &str) -> Result<PriceSnapshot> {
        // `compact` is the most recent ~100 sessions — comfortably more than
        // the 20-day Bollinger window needs, and a fraction of the payload
        // `full` would return — around five months of trading days.
        // Blocks until this request is within the published rate. Every path
        // that reaches a provider funnels through here — the startup and
        // hourly refresh sweeps, `tickerctl refresh`, and the cache-miss
        // fetches behind History and Buy — so none of them can burst.
        if let Some(limit) = RATE_LIMIT.as_ref() {
            let waited = limit.acquire().await;
            if !waited.is_zero() {
                debug!(ticker, waited_ms = waited.as_millis() as u64, "throttled");
            }
        }

        let resp = self
            .http
            .get(ENDPOINT)
            .query(&[
                ("function", "TIME_SERIES_DAILY"),
                ("symbol", ticker),
                ("outputsize", "compact"),
                ("apikey", self.key.expose_secret()),
            ])
            .send()
            .await
            // Deliberately not `{url}` — it carries the key.
            .with_context(|| format!("alphavantage request for {ticker}"))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .with_context(|| format!("reading alphavantage response for {ticker}"))?;

        if !status.is_success() {
            return Err(anyhow!(
                "alphavantage returned {status} for {}: {}",
                ticker,
                self.err_body(&body)
            ));
        }

        let env: Envelope = serde_json::from_str(&body).with_context(|| {
            format!("parsing alphavantage response for {}: {}", ticker, self.err_body(&body))
        })?;

        if let Some(msg) = env.error_message {
            return Err(anyhow!("alphavantage rejected {}: {}", ticker, self.scrub(&msg)));
        }
        // A rate-limit or bad-key reply arrives as HTTP 200 with one of these.
        if let Some(msg) = env.note.or(env.information) {
            return Err(anyhow!("alphavantage: {}", self.scrub(&msg)));
        }

        let series = env
            .series
            .ok_or_else(|| anyhow!("alphavantage returned no time series for {ticker}"))?;

        // BTreeMap iterates in key order, and the keys are YYYY-MM-DD, so
        // this comes out oldest-first without an explicit sort.
        let points: Vec<HistoryPoint> = series
            .into_iter()
            .filter_map(|(date, bar)| {
                let close = bar.close.parse::<f64>().ok()?;
                close.is_finite().then_some(HistoryPoint { date, close })
            })
            .collect();

        if points.is_empty() {
            return Err(anyhow!("alphavantage returned no usable close prices for {ticker}"));
        }

        let current_price = points.last().expect("non-empty").close;

        Ok(PriceSnapshot {
            ticker: ticker.to_uppercase(),
            current_price,
            points,
            fetched_on: Utc::now().date_naive().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn av(key: &str) -> AlphaVantage {
        AlphaVantage::new(reqwest::Client::new(), SecretString::new(key))
    }

    #[test]
    fn the_default_is_the_free_tier_rate() {
        // The shipped binary must never assume a premium plan.
        let d = interval_from_rpm(None).unwrap().expect("paced by default");
        assert!(d >= Duration::from_secs(1), "default paces faster than 1/sec: {d:?}");
        assert_eq!(interval_from_rpm(Some("")).unwrap(), Some(d), "empty means unset");
        assert_eq!(interval_from_rpm(Some("60")).unwrap(), Some(d));
    }

    #[test]
    fn a_premium_rate_paces_proportionally_faster() {
        let free = interval_from_rpm(Some("60")).unwrap().unwrap();
        let premium = interval_from_rpm(Some("600")).unwrap().unwrap();
        assert!(premium < free);
        // 10x the rate, a tenth of the interval — the jitter margin scales
        // with it rather than being a flat addition.
        assert!((premium.as_secs_f64() * 10.0 - free.as_secs_f64()).abs() < 1e-9);
    }

    #[test]
    fn pacing_can_be_turned_off_explicitly() {
        for off in ["off", "OFF", "unlimited", "Unlimited", "0", "  off  "] {
            assert_eq!(interval_from_rpm(Some(off)).unwrap(), None, "{off:?} should disable");
        }
    }

    #[test]
    fn a_bad_setting_is_an_error_rather_than_no_limit() {
        // The dangerous failure is a typo reading as "unlimited". Every one of
        // these must be reported so the caller can fall back to the free-tier
        // rate, never silently unpaced.
        for bad in ["fast", "-1", "1.5", "60rpm", "yes", "premium"] {
            let err = interval_from_rpm(Some(bad)).unwrap_err();
            assert!(err.contains("off"), "error should say what is accepted: {err}");
        }
    }

    #[test]
    fn scrub_removes_the_key_from_error_text() {
        let p = av("SUPERSECRETKEY");
        let echoed = "Invalid API call with apikey=SUPERSECRETKEY, please retry";
        let out = p.scrub(echoed);
        assert!(!out.contains("SUPERSECRETKEY"), "key survived scrubbing: {out}");
        assert!(out.contains("[redacted]"));
    }

    #[test]
    fn scrub_is_a_noop_without_a_key() {
        let p = av("");
        assert_eq!(p.scrub("nothing to redact"), "nothing to redact");
    }

    #[test]
    fn err_body_truncates_and_scrubs_together() {
        let p = av("KEY123");
        let long = format!("KEY123 {}", "x".repeat(400));
        let out = p.err_body(&long);
        assert!(!out.contains("KEY123"));
        assert!(out.ends_with('…'));
    }
}
