//! Aggregate transactions into holdings and build the responses sent back to
//! the TUI client.

use std::collections::BTreeMap;

use anyhow::Result;
use ticker_proto::{
    bollinger_bands, HistoryPoint, HoldingRow, PortfolioSummary, TickerHistory, TransactionRow,
};

use crate::db::Db;

pub struct Position {
    pub shares: f64,
    pub cost_basis: f64,
}

/// Reduce the transaction ledger into per-ticker positions.
///
/// Buys add shares and cost. Sells reduce shares pro-rata: cost basis tracks
/// remaining shares at the running average cost (FIFO-ish, but simpler — good
/// enough for a paper portfolio).
pub fn positions(txns: &[TransactionRow]) -> BTreeMap<String, Position> {
    let mut out: BTreeMap<String, Position> = BTreeMap::new();
    // Replay oldest first; the DB returns DESC, so iterate in reverse.
    for t in txns.iter().rev() {
        let entry = out
            .entry(t.ticker.clone())
            .or_insert(Position { shares: 0.0, cost_basis: 0.0 });
        if t.shares >= 0.0 {
            entry.shares += t.shares;
            entry.cost_basis += t.shares * t.price;
        } else if entry.shares > 0.0 {
            let avg = entry.cost_basis / entry.shares;
            entry.shares += t.shares;
            entry.cost_basis = (entry.shares * avg).max(0.0);
        } else {
            entry.shares += t.shares;
        }
    }
    out.retain(|_, p| p.shares > 1e-9);
    out
}

pub fn summary(db: &Db) -> Result<PortfolioSummary> {
    let txns = db.transactions(None)?;
    let positions = positions(&txns);
    let mut rows = Vec::new();
    let mut total_value = 0.0;
    let mut total_cost = 0.0;
    for (ticker, pos) in &positions {
        let snap = db.price(ticker)?;
        let (current_price, last_updated) = match snap {
            Some(s) => (s.current_price, s.fetched_on),
            None => (0.0, "—".to_string()),
        };
        let avg = if pos.shares > 0.0 { pos.cost_basis / pos.shares } else { 0.0 };
        let value = pos.shares * current_price;
        total_value += value;
        total_cost += pos.cost_basis;
        let gain = value - pos.cost_basis;
        let gain_pct = if avg > 0.0 && current_price > 0.0 {
            (current_price - avg) / avg * 100.0
        } else {
            0.0
        };
        rows.push(HoldingRow {
            ticker: ticker.clone(),
            shares: pos.shares,
            avg_cost: avg,
            current_price,
            value,
            cost_basis: pos.cost_basis,
            gain,
            gain_pct,
            weight: 0.0, // filled below
            last_updated,
        });
    }
    for r in &mut rows {
        r.weight = if total_value > 0.0 { r.value / total_value * 100.0 } else { 0.0 };
    }
    rows.sort_by(|a, b| b.value.partial_cmp(&a.value).unwrap_or(std::cmp::Ordering::Equal));
    let total_gain = total_value - total_cost;
    let total_gain_pct = if total_cost > 0.0 { total_gain / total_cost * 100.0 } else { 0.0 };
    Ok(PortfolioSummary {
        total_value,
        total_cost,
        total_gain,
        total_gain_pct,
        last_refresh: db.latest_refresh()?,
        rows,
    })
}

pub fn history(db: &Db, ticker: &str) -> Result<Option<TickerHistory>> {
    let Some(snap) = db.price(ticker)? else { return Ok(None) };
    let closes: Vec<f64> = snap.points.iter().map(|p| p.close).collect();
    let bands = bollinger_bands(&closes, 20, 2.0);

    let txns = db.transactions(None)?;
    let positions = positions(&txns);
    let holding = positions.get(ticker).map(|p| {
        let avg = p.cost_basis / p.shares;
        let value = p.shares * snap.current_price;
        HoldingRow {
            ticker: ticker.to_string(),
            shares: p.shares,
            avg_cost: avg,
            current_price: snap.current_price,
            value,
            cost_basis: p.cost_basis,
            gain: value - p.cost_basis,
            gain_pct: if avg > 0.0 { (snap.current_price - avg) / avg * 100.0 } else { 0.0 },
            weight: 0.0,
            last_updated: snap.fetched_on.clone(),
        }
    });

    Ok(Some(TickerHistory {
        ticker: ticker.to_string(),
        current_price: snap.current_price,
        last_updated: snap.fetched_on,
        points: snap.points.into_iter().collect::<Vec<HistoryPoint>>(),
        bands,
        holding,
    }))
}
