//! SQLite persistence: transactions ledger and a per-ticker price cache.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use ticker_proto::{HistoryPoint, TransactionRow};

pub struct Db {
    conn: Connection,
}

pub struct PriceSnapshot {
    pub ticker: String,
    pub current_price: f64,
    pub points: Vec<HistoryPoint>,
    pub fetched_on: String, // YYYY-MM-DD
}

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path).context("opening sqlite")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS transactions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ticker TEXT NOT NULL,
                shares REAL NOT NULL,
                price REAL NOT NULL,
                txn_date TEXT NOT NULL DEFAULT (date('now'))
            );
            CREATE TABLE IF NOT EXISTS price_cache (
                ticker TEXT PRIMARY KEY,
                current_price REAL NOT NULL,
                history_json TEXT NOT NULL,
                last_updated TEXT NOT NULL
            );
            "#,
        )?;
        Ok(Self { conn })
    }

    pub fn insert_transaction(&self, ticker: &str, shares: f64, price: f64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO transactions (ticker, shares, price) VALUES (?, ?, ?)",
            params![ticker, shares, price],
        )?;
        Ok(())
    }

    pub fn transactions(&self, ticker: Option<&str>) -> Result<Vec<TransactionRow>> {
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<TransactionRow> {
            Ok(TransactionRow {
                id: r.get(0)?,
                ticker: r.get(1)?,
                shares: r.get(2)?,
                price: r.get(3)?,
                txn_date: r.get(4)?,
            })
        };
        let rows = match ticker {
            Some(t) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, ticker, shares, price, txn_date FROM transactions WHERE ticker = ? ORDER BY id DESC",
                )?;
                let iter = stmt.query_map(params![t], map_row)?;
                iter.collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, ticker, shares, price, txn_date FROM transactions ORDER BY id DESC",
                )?;
                let iter = stmt.query_map([], map_row)?;
                iter.collect::<Result<Vec<_>, _>>()?
            }
        };
        Ok(rows)
    }

    pub fn held_tickers(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT ticker, SUM(shares) AS s FROM transactions GROUP BY ticker HAVING s > 1e-9",
        )?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn price_last_updated(&self, ticker: &str) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT last_updated FROM price_cache WHERE ticker = ?",
                params![ticker],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(r)
    }

    pub fn latest_refresh(&self) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT MAX(last_updated) FROM price_cache",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap_or(None);
        Ok(r)
    }

    pub fn upsert_price(&self, snap: &PriceSnapshot) -> Result<()> {
        let history_json = serde_json::to_string(&snap.points)?;
        self.conn.execute(
            r#"
            INSERT INTO price_cache (ticker, current_price, history_json, last_updated)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(ticker) DO UPDATE SET
                current_price = excluded.current_price,
                history_json  = excluded.history_json,
                last_updated  = excluded.last_updated
            "#,
            params![snap.ticker, snap.current_price, history_json, snap.fetched_on],
        )?;
        Ok(())
    }

    pub fn price(&self, ticker: &str) -> Result<Option<PriceSnapshot>> {
        let row = self
            .conn
            .query_row(
                "SELECT ticker, current_price, history_json, last_updated FROM price_cache WHERE ticker = ?",
                params![ticker],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, f64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .ok();
        match row {
            None => Ok(None),
            Some((ticker, current_price, hist, fetched_on)) => {
                let points: Vec<HistoryPoint> = serde_json::from_str(&hist)?;
                Ok(Some(PriceSnapshot { ticker, current_price, points, fetched_on }))
            }
        }
    }
}
