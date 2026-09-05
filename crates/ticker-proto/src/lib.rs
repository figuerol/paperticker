//! Wire protocol shared by `tickerd` and `tickerc`.
//!
//! Newline-delimited JSON over a Unix domain socket. One [`Request`] per line
//! from the client, one [`Response`] per line back from the daemon.

use serde::{Deserialize, Serialize};

pub mod provider;
pub mod secret;

pub use provider::{ProviderInfo, ProviderStatus};
pub use secret::SecretString;

pub const DEFAULT_SOCK_NAME: &str = "ticker-follow.sock";

/// Default socket location. Honors `$XDG_RUNTIME_DIR`, falls back to `/tmp`.
///
/// The fallback puts the socket *inside* a per-uid directory rather than
/// naming it `/tmp/ticker-follow-<uid>.sock` directly. `bind()` creates a
/// socket at whatever the umask allows and it can only be tightened
/// afterwards, so a socket sitting straight in world-traversable `/tmp` is
/// briefly reachable by other local users. A `0700` parent removes that
/// window: nobody else can traverse into the directory to reach the socket,
/// whatever mode it is born with.
pub fn default_socket_path() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return std::path::PathBuf::from(dir).join(DEFAULT_SOCK_NAME);
    }
    default_socket_fallback_dir().join(DEFAULT_SOCK_NAME)
}

/// The private directory holding the socket when `$XDG_RUNTIME_DIR` is unset.
/// `tickerd` is responsible for creating this `0700`; clients only read it.
pub fn default_socket_fallback_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/ticker-follow-{}", current_uid()))
}

/// Real uid of the calling process.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    libc_getuid()
}

#[cfg(unix)]
fn libc_getuid() -> u32 {
    // Avoid pulling the `libc` crate just for this — read /proc/self/status.
    // Best-effort: any failure returns 0, which still produces a valid path.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(str::to_owned))
        })
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Real uid of the calling process.
#[cfg(not(unix))]
pub fn current_uid() -> u32 {
    libc_getuid()
}

#[cfg(not(unix))]
fn libc_getuid() -> u32 {
    0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Summary,
    Holdings,
    /// Fetch price history + Bollinger bands for a ticker (does not require holding it).
    History {
        ticker: String,
    },
    Transactions {
        ticker: Option<String>,
    },
    Buy {
        ticker: String,
        shares: f64,
        /// Override price. If `None`, the daemon uses the latest cached close.
        price: Option<f64>,
    },
    Sell {
        ticker: String,
        shares: f64,
    },
    /// Refresh held tickers that don't already have today's close.
    ///
    /// Skipping the ones already current is what keeps a provider's daily
    /// request allowance from being spent on data the cache already has —
    /// the cache is day-based, so a second refresh the same day buys nothing
    /// but a slightly fresher intraday price. `force` re-fetches everything
    /// regardless, and is what a user asking for it explicitly gets.
    ///
    /// `#[serde(default)]` so an older client's bare `{"op":"refresh"}` still
    /// parses, and lands on the frugal behaviour rather than the costly one.
    Refresh {
        #[serde(default)]
        force: bool,
    },
    /// Refresh exactly one ticker, held or not.
    ///
    /// Exists so a client can drive the loop itself and report progress per
    /// ticker: the protocol is strictly one response per request, so a single
    /// `Refresh` cannot report anything until every fetch has finished. The
    /// daemon's rate limit is applied per request either way, so a client
    /// looping over this cannot outpace one that sends `Refresh`.
    RefreshTicker {
        ticker: String,
    },
    /// Which provider is configured, and is it ready. Never returns the key.
    ProviderStatus,
    /// Choose the price data provider and, if it needs one, store its key.
    ///
    /// The key is persisted by the daemon — clients never write credentials
    /// to disk themselves, the same way they never touch SQLite.
    SetProvider {
        provider: String,
        api_key: Option<SecretString>,
    },
    /// Forget the selected provider and erase any stored key.
    ClearProvider,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong,
    Error { message: String },
    Summary(PortfolioSummary),
    Holdings { rows: Vec<HoldingRow> },
    History(TickerHistory),
    Transactions { rows: Vec<TransactionRow> },
    ProviderStatus(ProviderStatus),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioSummary {
    pub total_value: f64,
    pub total_cost: f64,
    pub total_gain: f64,
    pub total_gain_pct: f64,
    pub last_refresh: Option<String>,
    pub rows: Vec<HoldingRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoldingRow {
    pub ticker: String,
    pub shares: f64,
    pub avg_cost: f64,
    pub current_price: f64,
    pub value: f64,
    pub cost_basis: f64,
    pub gain: f64,
    pub gain_pct: f64,
    pub weight: f64,
    pub last_updated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickerHistory {
    pub ticker: String,
    pub current_price: f64,
    pub last_updated: String,
    pub points: Vec<HistoryPoint>,
    /// Bollinger band values aligned 1:1 with `points`. `None` for indices < window-1.
    pub bands: Vec<BandPoint>,
    pub holding: Option<HoldingRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPoint {
    pub date: String, // YYYY-MM-DD
    pub close: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandPoint {
    pub sma: Option<f64>,
    pub upper: Option<f64>,
    pub lower: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionRow {
    pub id: i64,
    pub ticker: String,
    pub shares: f64,
    pub price: f64,
    pub txn_date: String,
}

/// 20-day, 2-sigma Bollinger bands. Returns one [`BandPoint`] per close.
pub fn bollinger_bands(closes: &[f64], window: usize, n_std: f64) -> Vec<BandPoint> {
    closes
        .iter()
        .enumerate()
        .map(|(i, _)| {
            if i + 1 < window {
                return BandPoint { sma: None, upper: None, lower: None };
            }
            let w = &closes[i + 1 - window..=i];
            let mean = w.iter().sum::<f64>() / window as f64;
            let var = w.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / window as f64;
            let std = var.sqrt();
            BandPoint {
                sma: Some(mean),
                upper: Some(mean + n_std * std),
                lower: Some(mean - n_std * std),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_have_correct_shape() {
        let closes = vec![1.0; 25];
        let bands = bollinger_bands(&closes, 20, 2.0);
        assert_eq!(bands.len(), 25);
        assert!(bands[18].sma.is_none());
        assert!(bands[19].sma.is_some());
        // Constant series -> SMA equals value, std is zero.
        assert!((bands[24].sma.unwrap() - 1.0).abs() < 1e-9);
        assert!((bands[24].upper.unwrap() - 1.0).abs() < 1e-9);
    }
}
