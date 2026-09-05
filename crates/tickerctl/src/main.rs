//! `tickerctl` — scriptable CLI for the paper-portfolio daemon. The TUI's
//! sibling client: same socket, same protocol, line-based output suitable
//! for shell scripts and skill integrations.
//!
//! Simulation only. Not for real trades.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ticker_client::Client;
use ticker_proto::{
    HoldingRow, PortfolioSummary, Request, Response, TickerHistory, TransactionRow,
};

mod provider_cmd;

#[derive(Parser)]
#[command(
    name = "tickerctl",
    about = "CLI client for the paperticker portfolio daemon (simulation only).",
    long_about = "Scriptable CLI for the paperticker portfolio daemon.\n\
                  All commands hit a local Unix socket — no real trades, no real money.",
    version,
)]
struct Cli {
    /// Override the daemon's socket path. Defaults to
    /// $XDG_RUNTIME_DIR/paperticker.sock or /tmp/paperticker-<uid>/paperticker.sock.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Emit the raw daemon JSON response instead of a human-readable summary.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Probe the daemon — prints "pong" on success.
    Ping,
    /// Portfolio totals plus every holding.
    Summary,
    /// Just the per-ticker holdings (no totals).
    Holdings,
    /// 6-month daily closes + 20-day SMA + ±2σ Bollinger bands for one ticker.
    History {
        /// Ticker symbol (e.g. AAPL, BRK-B).
        ticker: String,
    },
    /// Simulate a paper buy. Price defaults to today's cached close.
    Buy {
        ticker: String,
        shares: f64,
        /// Override the fill price. Omit to use today's cached close.
        #[arg(long)]
        price: Option<f64>,
    },
    /// Simulate a paper sell. Always fills at today's cached close.
    Sell {
        ticker: String,
        shares: f64,
    },
    /// Refresh prices for held tickers that don't already have today's close.
    Refresh {
        /// Re-fetch every held ticker, even ones already current today.
        ///
        /// Costs one provider request per holding every time. Providers cap
        /// how many you get per day, so reach for this when you want a
        /// fresher intraday price, not as the routine way to refresh.
        #[arg(long)]
        force: bool,
    },
    /// List transactions, optionally filtered to one ticker.
    Transactions {
        /// If given, restrict to this ticker.
        ticker: Option<String>,
    },
    /// Choose and configure the price data provider.
    #[command(subcommand)]
    Provider(ProviderCmd),
}

#[derive(Subcommand)]
enum ProviderCmd {
    /// Show the configured provider and whether it can fetch.
    Status,
    /// List every provider that can be selected.
    List,
    /// Select a provider and store its API key.
    ///
    /// The key is read from a hidden prompt, or from stdin when piped. There
    /// is deliberately no --key flag: an argument would be recorded in shell
    /// history and visible in `ps` to every user on the machine.
    Set {
        /// Catalog id. Omit to choose from a menu.
        provider: Option<String>,
    },
    /// Forget the selected provider and erase the stored key.
    Clear,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tickerctl: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let sock = cli.socket.unwrap_or_else(ticker_proto::default_socket_path);
    let mut client = Client::connect(&sock).with_context(|| {
        format!(
            "tickerd not reachable at {} — start it with `tickerd &`",
            sock.display()
        )
    })?;

    let req = match &cli.cmd {
        Cmd::Ping => Request::Ping,
        Cmd::Summary => Request::Summary,
        Cmd::Holdings => Request::Holdings,
        Cmd::History { ticker } => Request::History { ticker: ticker.clone() },
        Cmd::Buy { ticker, shares, price } => Request::Buy {
            ticker: ticker.clone(),
            shares: *shares,
            price: *price,
        },
        Cmd::Sell { ticker, shares } => Request::Sell {
            ticker: ticker.clone(),
            shares: *shares,
        },
        Cmd::Refresh { force } => Request::Refresh { force: *force },
        Cmd::Transactions { ticker } => Request::Transactions { ticker: ticker.clone() },
        Cmd::Provider(p) => match p {
            ProviderCmd::Status | ProviderCmd::List => Request::ProviderStatus,
            ProviderCmd::Clear => Request::ClearProvider,
            // Interactive: prompts before anything goes over the socket.
            ProviderCmd::Set { provider } => provider_cmd::build_set_request(provider.as_deref())?,
        },
    };

    let resp = client.call(&req)?;

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(match resp {
            Response::Error { .. } => ExitCode::from(1),
            _ => ExitCode::SUCCESS,
        });
    }

    print_human(&cli.cmd, &resp)
}

fn print_human(cmd: &Cmd, resp: &Response) -> Result<ExitCode> {
    if let Response::Error { message } = resp {
        eprintln!("error: {message}");
        return Ok(ExitCode::from(1));
    }
    match (cmd, resp) {
        (Cmd::Ping, Response::Pong) => println!("pong"),
        (Cmd::Summary, Response::Summary(s)) => print_summary(s),
        (Cmd::Holdings, Response::Holdings { rows }) => print_holdings(rows, None),
        (Cmd::History { .. }, Response::History(h)) => print_history(h),
        (Cmd::Buy { ticker, shares, price }, Response::Ok) => {
            let p = match price {
                Some(v) => format!("${v:.2}"),
                None => "today's close".to_string(),
            };
            println!("ok — bought {shares} {} @ {p} (simulated)", ticker.to_uppercase());
        }
        (Cmd::Sell { ticker, shares }, Response::Ok) => {
            println!("ok — sold {shares} {} @ today's close (simulated)", ticker.to_uppercase());
        }
        (Cmd::Refresh { force }, Response::Ok) => {
            if *force {
                println!("ok — every held ticker re-fetched");
            } else {
                println!("ok — held tickers without today's close refreshed");
            }
        }
        (Cmd::Transactions { ticker }, Response::Transactions { rows }) => {
            print_transactions(rows, ticker.as_deref());
        }
        (Cmd::Provider(ProviderCmd::List), Response::ProviderStatus(st)) => {
            provider_cmd::print_list(st);
        }
        (Cmd::Provider(ProviderCmd::Set { .. }), Response::ProviderStatus(st)) => {
            println!("ok — provider configured");
            println!();
            provider_cmd::print_status(st);
        }
        (Cmd::Provider(ProviderCmd::Clear), Response::ProviderStatus(st)) => {
            println!("ok — provider cleared, stored key erased");
            println!();
            provider_cmd::print_status(st);
        }
        (Cmd::Provider(ProviderCmd::Status), Response::ProviderStatus(st)) => {
            provider_cmd::print_status(st);
        }
        // Any other (cmd, resp) combination is a daemon-side bug.
        _ => {
            eprintln!("unexpected response shape: {resp:?}");
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_summary(s: &PortfolioSummary) {
    let asof = s.last_refresh.as_deref().unwrap_or("never");
    println!("Portfolio summary (last refresh {asof})");
    println!("  Value:  ${:.2}", s.total_value);
    println!("  Cost:   ${:.2}", s.total_cost);
    let sign = if s.total_gain >= 0.0 { "+" } else { "" };
    println!(
        "  Gain:   {sign}${:.2} ({sign}{:.2}%)",
        s.total_gain, s.total_gain_pct
    );
    println!();
    print_holdings(&s.rows, Some(asof));
}

fn print_holdings(rows: &[HoldingRow], asof: Option<&str>) {
    if rows.is_empty() {
        println!("No holdings.");
        return;
    }
    println!(
        "{:<8} {:>10} {:>10} {:>10} {:>12} {:>7} {:>12} {:>9}  Updated",
        "Ticker", "Shares", "Avg Cost", "Price", "Value", "Weight", "Gain", "Return"
    );
    for r in rows {
        let sign = if r.gain >= 0.0 { "+" } else { "" };
        let updated = if Some(r.last_updated.as_str()) == asof {
            "—".to_string()
        } else {
            r.last_updated.clone()
        };
        println!(
            "{:<8} {:>10.4} {:>10} {:>10} {:>12} {:>6.1}% {:>12} {:>8.2}%  {}",
            r.ticker,
            r.shares,
            format!("${:.2}", r.avg_cost),
            format!("${:.2}", r.current_price),
            format!("${:.2}", r.value),
            r.weight,
            format!("{sign}${:.2}", r.gain),
            r.gain_pct,
            updated,
        );
    }
}

fn print_history(h: &TickerHistory) {
    println!("{} — current ${:.2} (cached {})", h.ticker, h.current_price, h.last_updated);
    if let Some(pos) = &h.holding {
        let sign = if pos.gain >= 0.0 { "+" } else { "" };
        println!(
            "Holding: {:.4} sh, avg ${:.2}, value ${:.2}, gain {sign}${:.2} ({sign}{:.2}%)",
            pos.shares, pos.avg_cost, pos.value, pos.gain, pos.gain_pct,
        );
    }
    println!(
        "History: {} daily closes from {} to {}",
        h.points.len(),
        h.points.first().map(|p| p.date.as_str()).unwrap_or("?"),
        h.points.last().map(|p| p.date.as_str()).unwrap_or("?"),
    );
    if h.points.len() < 2 {
        return;
    }
    println!();
    println!(
        "{:<12} {:>9} {:>9} {:>9} {:>9}",
        "Date", "Close", "SMA(20)", "Upper", "Lower"
    );
    let start = h.points.len().saturating_sub(10);
    for (i, p) in h.points.iter().enumerate().skip(start) {
        let b = &h.bands[i];
        println!(
            "{:<12} {:>9.2} {:>9} {:>9} {:>9}",
            p.date,
            p.close,
            b.sma.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into()),
            b.upper.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into()),
            b.lower.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into()),
        );
    }
}

fn print_transactions(rows: &[TransactionRow], filter: Option<&str>) {
    if rows.is_empty() {
        let f = filter.map(|t| format!(" for {t}")).unwrap_or_default();
        println!("No transactions{f}.");
        return;
    }
    println!(
        "{:<12} {:<8} {:>10} {:>10} {:>14}",
        "Date", "Ticker", "Shares", "Price", "Total"
    );
    for t in rows {
        let total = t.shares * t.price;
        let sign = if t.shares >= 0.0 { "+" } else { "" };
        let total_sign = if total >= 0.0 { "+" } else { "" };
        println!(
            "{:<12} {:<8} {:>10} {:>10} {:>14}",
            t.txn_date,
            t.ticker,
            format!("{sign}{:.4}", t.shares),
            format!("${:.2}", t.price),
            format!("{total_sign}${:.2}", total),
        );
    }
}
