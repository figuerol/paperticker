//! Unix-socket JSON-line server. One request per line, one response per line.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use tracing::{info, warn};

use ticker_proto::{Request, Response};

use crate::db::Db;
use crate::config::Config;
use crate::portfolio;
use crate::provider::Providers;

pub async fn serve(
    sock_path: &std::path::Path,
    db: Arc<Mutex<Db>>,
    providers: Providers,
) -> Result<()> {
    if let Some(parent) = sock_path.parent() {
        // Create with the mode baked in — `create_dir_all` would use the
        // umask and leave a window where the directory is world-traversable.
        // Existing directories keep their mode, so this never touches
        // `$XDG_RUNTIME_DIR`; the fallback is vetted just below.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .ok();
    }
    // The `/tmp` fallback directory is ours to guarantee. If it already
    // existed we can't assume anything about it: another local user may have
    // created it first to sit inside our socket's parent.
    if sock_path.starts_with("/tmp") {
        if let Some(parent) = sock_path.parent() {
            ensure_private_dir(parent)?;
        }
    }
    if sock_path.exists() {
        // Stale socket from a previous run? Probe it. If nothing answers,
        // remove and rebind — so a crashed daemon never needs manual cleanup.
        if UnixStream::connect(sock_path).await.is_err() {
            std::fs::remove_file(sock_path).ok();
        } else {
            // Something is listening, so this is a live daemon, not debris.
            // Say what to do about it: the bare fact isn't actionable, and
            // the usual answer is "you already have one, just use it".
            let who = match socket_listener_pid(sock_path) {
                Some(pid) => format!(" (PID {pid})"),
                None => String::new(),
            };
            anyhow::bail!(
                "another tickerd{who} is already listening on {}\n\
                 \n\
                 One daemon per user is by design — tickerc and tickerctl find it\n\
                 on their own, so you usually don't need to start a second one.\n\
                 \n\
                 To replace the running one (needed after upgrading, since the\n\
                 daemon and clients share a wire protocol):\n\
                 \n\
                 \x20   pkill tickerd && tickerd &",
                sock_path.display()
            );
        }
    }
    let listener = UnixListener::bind(sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;
    // Belt and braces. The private parent directory above is what actually
    // closes the race — `bind` creates the socket at the process umask and
    // this chmod can only tighten it afterwards, so on its own it leaves a
    // window. Keep it anyway: it is the correct final mode, and it holds even
    // if the socket somehow lands in a directory laxer than we expect.
    std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", sock_path.display()))?;
    info!(?sock_path, "tickerd listening");

    // Clean up the socket file on the ways a daemon actually gets stopped.
    // SIGTERM matters as much as Ctrl-C here: that is what `pkill tickerd`
    // sends, and what a service manager sends — without it, every scripted
    // restart leaves a stale socket for the next start to clear up.
    let sock_for_cleanup = sock_path.to_path_buf();
    tokio::spawn(async move {
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(?e, "no SIGTERM handler; socket cleanup on kill is disabled");
                // Still honour Ctrl-C rather than dropping the task entirely.
                let _ = tokio::signal::ctrl_c().await;
                info!("shutting down");
                let _ = std::fs::remove_file(&sock_for_cleanup);
                std::process::exit(0);
            }
        };
        let signal_name = tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = term.recv() => "SIGTERM",
        };
        info!(signal = signal_name, "shutting down");
        let _ = std::fs::remove_file(&sock_for_cleanup);
        std::process::exit(0);
    });

    loop {
        let (stream, _) = listener.accept().await?;
        let db = Arc::clone(&db);
        let providers = providers.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, db, providers).await {
                warn!(?e, "client handler error");
            }
        });
    }
}

async fn handle(stream: UnixStream, db: Arc<Mutex<Db>>, providers: Providers) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(req, &db, &providers).await,
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut s = serde_json::to_string(&resp)?;
        s.push('\n');
        write.write_all(s.as_bytes()).await?;
        write.flush().await?;
    }
    Ok(())
}

/// PID of the process actually listening on `sock_path`, for the "already
/// running" message.
///
/// Matching by process name would be wrong: it would happily name a `tickerd`
/// serving some other socket, and a confidently wrong PID in an error is
/// worse than no PID. So this goes the precise route — find the socket's
/// inode in `/proc/net/unix`, then find who holds that inode open. Reading
/// `/proc` is part of why this project is Linux-only. Best effort throughout;
/// `None` just means the message omits the PID.
fn socket_listener_pid(sock_path: &std::path::Path) -> Option<u32> {
    let want = sock_path.to_str()?;

    // /proc/net/unix columns: Num RefCount Protocol Flags Type St Inode Path
    let unix = std::fs::read_to_string("/proc/net/unix").ok()?;
    let inode = unix.lines().find_map(|line| {
        let mut cols = line.split_whitespace();
        let ino = cols.nth(6)?;
        // Path is the last column and may be absent for unnamed sockets.
        (cols.next()? == want).then(|| ino.to_string())
    })?;
    let target = format!("socket:[{inode}]");

    let me = std::process::id();
    for entry in std::fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        // Unreadable /proc/<pid>/fd is normal — other users' processes.
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path()).is_ok_and(|l| l.to_string_lossy() == target) {
                return Some(pid);
            }
        }
    }
    None
}

/// Vet the directory the socket is about to be created in.
///
/// Only meaningful for the `/tmp` fallback: `$XDG_RUNTIME_DIR` is the
/// login session's own private directory. In `/tmp` anyone can `mkdir` our
/// path before we do, and binding inside a directory someone else owns would
/// hand them the socket. Refuse that outright; merely-loose permissions on a
/// directory we do own we can just tighten.
fn ensure_private_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    // `symlink_metadata` so a symlink planted at our path is judged as the
    // symlink it is, not as whatever it points at.
    let meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("stat {}", dir.display()))?;

    if meta.file_type().is_symlink() || !meta.is_dir() {
        anyhow::bail!(
            "{} exists but is not a real directory — refusing to bind inside it",
            dir.display()
        );
    }

    let uid = ticker_proto::current_uid();
    if meta.uid() != uid {
        anyhow::bail!(
            "{} is owned by uid {}, not {} — refusing to bind inside it",
            dir.display(),
            meta.uid(),
            uid
        );
    }

    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        warn!(
            ?dir,
            mode = format!("{mode:04o}"),
            "socket directory was group/world accessible; tightening to 0700"
        );
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", dir.display()))?;
    }
    Ok(())
}

/// Reject anything that isn't a plausible ticker symbol. Applied right after
/// uppercasing at the dispatch boundary, before a ticker can reach a provider
/// URL (several providers interpolate the symbol into a path or query string,
/// so a stray `&`, `/`, or `?` would otherwise inject extra parameters into
/// that request) or get echoed back to a client's terminal.
fn validate_ticker(t: &str) -> Result<(), String> {
    let ok = !t.is_empty()
        && t.len() <= 10
        && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!("invalid ticker {t:?}"))
    }
}

/// Reject share counts that aren't strictly positive finite numbers.
/// A naive `s <= 0.0` check passes NaN through (every NaN comparison is
/// false), which is the bug we're closing here.
fn validate_shares(s: f64) -> Result<(), String> {
    if !s.is_finite() {
        return Err("shares must be a finite number".into());
    }
    if s <= 0.0 {
        return Err("shares must be positive".into());
    }
    Ok(())
}

/// Reject user-supplied price overrides that aren't strictly positive
/// finite numbers. Negative / zero / NaN / Inf all permanently corrupt
/// cost basis once they're in the ledger.
fn validate_price_override(p: f64) -> Result<(), String> {
    if !p.is_finite() {
        return Err("price must be a finite number".into());
    }
    if p <= 0.0 {
        return Err("price must be positive".into());
    }
    Ok(())
}

async fn dispatch(req: Request, db: &Arc<Mutex<Db>>, providers: &Providers) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Summary => match portfolio::summary(&*db.lock().await) {
            Ok(s) => Response::Summary(s),
            Err(e) => Response::Error { message: e.to_string() },
        },
        Request::Holdings => match portfolio::summary(&*db.lock().await) {
            Ok(s) => Response::Holdings { rows: s.rows },
            Err(e) => Response::Error { message: e.to_string() },
        },
        Request::History { ticker } => {
            let ticker = ticker.to_uppercase();
            if let Err(msg) = validate_ticker(&ticker) {
                return Response::Error { message: msg };
            }
            // Ensure we have data — fetch if cache miss.
            let needs_fetch = {
                let g = db.lock().await;
                g.price(&ticker).ok().flatten().is_none()
            };
            if needs_fetch {
                match providers.fetch(&ticker).await {
                    Ok(snap) => {
                        if let Err(e) = db.lock().await.upsert_price(&snap) {
                            return Response::Error { message: format!("db: {e}") };
                        }
                    }
                    Err(e) => return Response::Error { message: e.to_string() },
                }
            }
            match portfolio::history(&*db.lock().await, &ticker) {
                Ok(Some(h)) => Response::History(h),
                Ok(None) => Response::Error { message: format!("no data for {ticker}") },
                Err(e) => Response::Error { message: e.to_string() },
            }
        }
        Request::Transactions { ticker } => {
            // Storage is uppercase (Buy/Sell normalize on insert); apply the
            // same normalization to the filter so `transactions aapl` matches.
            let ticker = ticker.map(|t| t.to_uppercase());
            if let Some(t) = &ticker {
                if let Err(msg) = validate_ticker(t) {
                    return Response::Error { message: msg };
                }
            }
            match db.lock().await.transactions(ticker.as_deref()) {
                Ok(rows) => Response::Transactions { rows },
                Err(e) => Response::Error { message: e.to_string() },
            }
        }
        Request::Buy { ticker, shares, price } => {
            let ticker = ticker.to_uppercase();
            if let Err(msg) = validate_ticker(&ticker) {
                return Response::Error { message: msg };
            }
            if let Err(msg) = validate_shares(shares) {
                return Response::Error { message: msg };
            }
            if let Some(p) = price {
                if let Err(msg) = validate_price_override(p) {
                    return Response::Error { message: msg };
                }
            }
            // Ensure cache so we have a price.
            let snap = {
                let g = db.lock().await;
                g.price(&ticker).ok().flatten()
            };
            let snap = if snap.is_some() {
                snap.unwrap()
            } else {
                match providers.fetch(&ticker).await {
                    Ok(s) => {
                        if let Err(e) = db.lock().await.upsert_price(&s) {
                            return Response::Error { message: format!("db: {e}") };
                        }
                        s
                    }
                    Err(e) => return Response::Error { message: e.to_string() },
                }
            };
            let p = price.unwrap_or(snap.current_price);
            if let Err(e) = db.lock().await.insert_transaction(&ticker, shares, p) {
                return Response::Error { message: e.to_string() };
            }
            Response::Ok
        }
        Request::Sell { ticker, shares } => {
            let ticker = ticker.to_uppercase();
            if let Err(msg) = validate_ticker(&ticker) {
                return Response::Error { message: msg };
            }
            if let Err(msg) = validate_shares(shares) {
                return Response::Error { message: msg };
            }
            let g = db.lock().await;
            let txns = match g.transactions(None) {
                Ok(t) => t,
                Err(e) => return Response::Error { message: e.to_string() },
            };
            let held = portfolio::positions(&txns)
                .get(&ticker)
                .map(|p| p.shares)
                .unwrap_or(0.0);
            if shares > held + 1e-9 {
                return Response::Error {
                    message: format!("only holding {held} of {ticker}"),
                };
            }
            let price = match g.price(&ticker) {
                Ok(Some(s)) => s.current_price,
                Ok(None) => {
                    // Don't silently fill at $0 — that permanently corrupts
                    // cost basis. Refuse and tell the user to refresh.
                    return Response::Error {
                        message: format!(
                            "no cached price for {ticker} — run `tickerctl refresh` and retry"
                        ),
                    };
                }
                Err(e) => return Response::Error { message: e.to_string() },
            };
            if let Err(e) = g.insert_transaction(&ticker, -shares, price) {
                return Response::Error { message: e.to_string() };
            }
            Response::Ok
        }
        Request::Refresh { force } => {
            // Resolve before touching the ledger. Without this, an
            // unconfigured daemon reports "ok" for an empty portfolio and
            // repeats the same "no provider" error once per holding for a
            // non-empty one.
            let source = match providers.current().await {
                Ok(s) => s,
                Err(e) => return Response::Error { message: e.to_string() },
            };
            let tickers = match db.lock().await.held_tickers() {
                Ok(t) => t,
                Err(e) => return Response::Error { message: e.to_string() },
            };
            let today = chrono::Utc::now().date_naive().to_string();
            let mut failed = Vec::new();
            for t in tickers {
                // Same day-based test `refresh_stale` uses. A provider's
                // daily request allowance is the scarce resource here, and a
                // ticker that already carries today's close cannot be
                // improved by spending another request on it.
                if !force && db.lock().await.price_last_updated(&t).ok().flatten().as_deref()
                    == Some(today.as_str())
                {
                    continue;
                }
                match source.fetch(&t).await {
                    Ok(snap) => {
                        if let Err(e) = db.lock().await.upsert_price(&snap) {
                            failed.push(format!("{t}: {e}"));
                        }
                    }
                    Err(e) => failed.push(format!("{t}: {e}")),
                }
            }
            if failed.is_empty() {
                Response::Ok
            } else {
                Response::Error { message: failed.join("; ") }
            }
        }
        Request::RefreshTicker { ticker } => {
            let ticker = ticker.to_uppercase();
            if let Err(msg) = validate_ticker(&ticker) {
                return Response::Error { message: msg };
            }
            let source = match providers.current().await {
                Ok(s) => s,
                Err(e) => return Response::Error { message: e.to_string() },
            };
            match source.fetch(&ticker).await {
                Ok(snap) => match db.lock().await.upsert_price(&snap) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error { message: format!("db: {e}") },
                },
                Err(e) => Response::Error { message: e.to_string() },
            }
        }
        Request::ProviderStatus => provider_status(providers).await,
        Request::SetProvider { provider, api_key } => {
            let Some(info) = ticker_proto::provider::lookup(&provider) else {
                return Response::Error { message: format!("unknown provider {provider:?}") };
            };
            let has_key = api_key.as_ref().is_some_and(|k| !k.is_empty());
            if info.requires_key && !has_key {
                return Response::Error {
                    message: format!("{} requires an API key", info.label),
                };
            }
            let updated = Config {
                provider: Some(info.id.clone()),
                // Don't retain a key for a provider that has no use for one.
                api_key: if info.requires_key { api_key } else { None },
            };
            if let Err(e) = updated.save() {
                return Response::Error { message: format!("saving credentials: {e}") };
            }
            // Re-read rather than reusing `updated`, so the in-memory copy
            // reflects the same environment overrides a restart would apply.
            *providers.config().lock().await = Config::load();
            provider_status(providers).await
        }
        Request::ClearProvider => {
            if let Err(e) = Config::clear() {
                return Response::Error { message: format!("clearing credentials: {e}") };
            }
            *providers.config().lock().await = Config::load();
            provider_status(providers).await
        }
    }
}

/// Current provider state. Reports only *whether* a key is on file — a stored
/// credential never travels back out of the daemon, not even to a client that
/// could have set it.
async fn provider_status(providers: &Providers) -> Response {
    let cfg = providers.config().lock().await;
    let selected = cfg.provider.as_deref().and_then(ticker_proto::provider::lookup);
    // A stored id that doesn't resolve is not "nothing configured" — usually
    // a typo in the config file or in `TICKER_FOLLOW_PROVIDER`. `provider
    // status` is the first thing anyone runs when a fetch fails, so it has to
    // name the id rather than imply nothing was ever chosen.
    let unavailable = match (&selected, cfg.provider.as_deref()) {
        (None, Some(id)) => Some(format!("unknown provider {id:?}")),
        _ => None,
    };
    Response::ProviderStatus(ticker_proto::ProviderStatus {
        selected,
        unavailable,
        key_configured: cfg.api_key.as_ref().is_some_and(|k| !k.is_empty()),
        ready: cfg.is_ready(),
        available: ticker_proto::provider::catalog(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::PriceSnapshot;

    // ---- validators (pure unit tests) ----

    #[test]
    fn validate_ticker_accepts_plausible_symbols() {
        assert!(validate_ticker("AAPL").is_ok());
        assert!(validate_ticker("BRK-B").is_ok());
        assert!(validate_ticker("BF.B").is_ok());
    }

    #[test]
    fn validate_ticker_rejects_injection_attempts_and_junk() {
        assert!(validate_ticker("").is_err());
        assert!(validate_ticker("AAPL&foo=bar").is_err());
        assert!(validate_ticker("AAPL?range=1d").is_err());
        assert!(validate_ticker("../etc/passwd").is_err());
        assert!(validate_ticker("AAAAAAAAAAA").is_err(), "over length limit");
    }

    #[test]
    fn validate_shares_accepts_strictly_positive_finite() {
        assert!(validate_shares(10.0).is_ok());
        assert!(validate_shares(0.1).is_ok());
        assert!(validate_shares(f64::MIN_POSITIVE).is_ok());
    }

    #[test]
    fn validate_shares_rejects_zero_and_negative() {
        assert!(validate_shares(0.0).is_err());
        assert!(validate_shares(-1.0).is_err());
    }

    #[test]
    fn validate_shares_rejects_nan_and_infinities() {
        // The bug we're closing: `NaN <= 0.0` is false, so the naive guard
        // passes NaN/Inf through and they reach the ledger.
        assert!(validate_shares(f64::NAN).is_err());
        assert!(validate_shares(f64::INFINITY).is_err());
        assert!(validate_shares(f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn validate_price_rejects_zero_negative_nan_inf() {
        assert!(validate_price_override(0.0).is_err());
        assert!(validate_price_override(-50.0).is_err());
        assert!(validate_price_override(f64::NAN).is_err());
        assert!(validate_price_override(f64::INFINITY).is_err());
        assert!(validate_price_override(100.0).is_ok());
    }

    // ---- dispatch integration (in-memory SQLite, no network) ----

    fn mem_db() -> Arc<Mutex<Db>> {
        Arc::new(Mutex::new(Db::open(std::path::Path::new(":memory:")).unwrap()))
    }

    /// A `Providers` with nothing configured. Every test here either seeds a
    /// price first or expects the fetch to fail, so "no provider" is exactly
    /// the right stub — and it needs no network.
    fn provider_stub() -> Providers {
        Providers::new(
            reqwest::Client::new(),
            Arc::new(Mutex::new(Config::default())),
        )
    }

    async fn seed_price(db: &Arc<Mutex<Db>>, ticker: &str, price: f64) {
        db.lock()
            .await
            .upsert_price(&PriceSnapshot {
                ticker: ticker.into(),
                current_price: price,
                points: vec![],
                fetched_on: "2026-05-26".into(),
            })
            .unwrap();
    }

    #[tokio::test]
    async fn sell_without_cached_price_returns_error_and_inserts_nothing() {
        let db = mem_db();
        // Holding exists (5 AAPL @ $100) but the price_cache row does NOT.
        db.lock().await.insert_transaction("AAPL", 5.0, 100.0).unwrap();
        let resp = dispatch(
            Request::Sell { ticker: "AAPL".into(), shares: 1.0 },
            &db,
            &provider_stub(),
        )
        .await;
        assert!(matches!(resp, Response::Error { .. }), "got: {resp:?}");
        // Critically: the ledger must NOT have a -1 @ $0 row appended.
        let txns = db.lock().await.transactions(None).unwrap();
        assert_eq!(txns.len(), 1, "Sell must not have appended a $0 row; got {txns:?}");
        assert_eq!(txns[0].shares, 5.0);
    }

    #[tokio::test]
    async fn buy_rejects_nan_shares() {
        let db = mem_db();
        // Seeded price prevents any provider fetch fallback path.
        seed_price(&db, "AAPL", 100.0).await;
        let resp = dispatch(
            Request::Buy { ticker: "AAPL".into(), shares: f64::NAN, price: None },
            &db,
            &provider_stub(),
        )
        .await;
        assert!(matches!(resp, Response::Error { .. }), "got: {resp:?}");
        let txns = db.lock().await.transactions(None).unwrap();
        assert!(txns.is_empty(), "NaN buy must not reach the ledger; got {txns:?}");
    }

    #[tokio::test]
    async fn buy_rejects_inf_shares() {
        let db = mem_db();
        seed_price(&db, "AAPL", 100.0).await;
        let resp = dispatch(
            Request::Buy { ticker: "AAPL".into(), shares: f64::INFINITY, price: None },
            &db,
            &provider_stub(),
        )
        .await;
        assert!(matches!(resp, Response::Error { .. }), "got: {resp:?}");
        assert!(db.lock().await.transactions(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn buy_rejects_negative_price_override() {
        let db = mem_db();
        seed_price(&db, "AAPL", 100.0).await;
        let resp = dispatch(
            Request::Buy { ticker: "AAPL".into(), shares: 10.0, price: Some(-50.0) },
            &db,
            &provider_stub(),
        )
        .await;
        assert!(matches!(resp, Response::Error { .. }), "got: {resp:?}");
        assert!(db.lock().await.transactions(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn buy_rejects_nan_price_override() {
        let db = mem_db();
        seed_price(&db, "AAPL", 100.0).await;
        let resp = dispatch(
            Request::Buy { ticker: "AAPL".into(), shares: 10.0, price: Some(f64::NAN) },
            &db,
            &provider_stub(),
        )
        .await;
        assert!(matches!(resp, Response::Error { .. }), "got: {resp:?}");
        assert!(db.lock().await.transactions(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn sell_rejects_nan_shares() {
        let db = mem_db();
        db.lock().await.insert_transaction("AAPL", 5.0, 100.0).unwrap();
        seed_price(&db, "AAPL", 110.0).await;
        let resp = dispatch(
            Request::Sell { ticker: "AAPL".into(), shares: f64::NAN },
            &db,
            &provider_stub(),
        )
        .await;
        assert!(matches!(resp, Response::Error { .. }), "got: {resp:?}");
        let txns = db.lock().await.transactions(None).unwrap();
        assert_eq!(txns.len(), 1, "only the original buy should remain");
    }

    #[tokio::test]
    async fn transactions_filter_matches_lowercase_input_against_uppercase_storage() {
        let db = mem_db();
        // dispatch's Buy arm uppercases on insertion, so seed via the same path.
        seed_price(&db, "AAPL", 100.0).await;
        let _ = dispatch(
            Request::Buy { ticker: "aapl".into(), shares: 1.0, price: Some(100.0) },
            &db,
            &provider_stub(),
        )
        .await;
        // The bug: this arm doesn't uppercase, so 'aapl' doesn't match stored 'AAPL'.
        let resp = dispatch(
            Request::Transactions { ticker: Some("aapl".into()) },
            &db,
            &provider_stub(),
        )
        .await;
        match resp {
            Response::Transactions { rows } => {
                assert_eq!(rows.len(), 1, "lowercase filter must match uppercase storage; got {rows:?}");
                assert_eq!(rows[0].ticker, "AAPL");
            }
            other => panic!("expected Transactions, got {other:?}"),
        }
    }
}
