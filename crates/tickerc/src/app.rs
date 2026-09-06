//! UI state machine: tabs, table cursor, form fields, status bar, and the
//! command-loop that translates key events into protocol requests.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;
use ticker_client::Client;
use ticker_proto::{
    HoldingRow, PortfolioSummary, ProviderStatus, Request, Response, TickerHistory,
    TransactionRow,
};

use crate::worker::{Job, JobKind, Update, Worker};

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Tab {
    Portfolio,
    Detail,
    Trade,
    Transactions,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Portfolio, Tab::Detail, Tab::Trade, Tab::Transactions];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Portfolio => "Portfolio",
            Tab::Detail => "Detail",
            Tab::Trade => "Trade",
            Tab::Transactions => "Transactions",
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum TradeSide {
    Buy,
    Sell,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum TradeField {
    Ticker,
    Shares,
    Price,
}

/// Vim-style modality on the Trade tab. `Nav` lets the user keep moving between
/// tabs with arrows/h/l/Tab without their keystrokes leaking into the form.
/// `Edit` captures typing for the focused field.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum TradeMode {
    Nav,
    Edit,
}

pub struct TradeForm {
    pub side: TradeSide,
    pub field: TradeField,
    pub mode: TradeMode,
    pub ticker: String,
    pub shares: String,
    pub price: String,
}

impl Default for TradeForm {
    fn default() -> Self {
        Self {
            side: TradeSide::Buy,
            field: TradeField::Ticker,
            mode: TradeMode::Nav,
            ticker: String::new(),
            shares: String::new(),
            price: String::new(),
        }
    }
}

pub struct App {
    pub client: Client,
    pub tab: Tab,
    pub summary: Option<PortfolioSummary>,
    pub holdings_state: TableState,
    pub txns_state: TableState,
    pub txns: Vec<TransactionRow>,
    pub detail: Option<TickerHistory>,
    pub detail_loading_for: Option<String>,
    pub trade: TradeForm,
    pub status: Status,
    /// Which price provider the daemon has configured, shown in the banner so
    /// it's visible without pressing anything. `None` only before the first
    /// successful query.
    pub provider: Option<ProviderStatus>,
    /// Slow work runs here, on its own daemon connection, so the event loop
    /// keeps drawing and answering keys while it does.
    pub worker: Worker,
    /// The job in flight, if any — drives the progress bar and blocks a
    /// second job from being queued behind it.
    pub job: Option<JobState>,
    /// The success line to print when the running trade lands. Held here
    /// because the form is cleared before the daemon answers.
    pending_trade_note: String,
    /// Ticks the spinner. Advanced once per event-loop pass.
    pub frame: usize,
    pub should_quit: bool,
}

/// Progress for the job currently running.
pub struct JobState {
    /// What is happening, e.g. `Refreshing` or `Buying 5 AAPL`.
    pub label: String,
    /// Items finished. `done`/`total` is the progress fraction.
    pub done: usize,
    pub total: usize,
    /// The item in flight right now.
    pub current: String,
}

impl JobState {
    /// 0.0–1.0, saturating. A zero-length job counts as complete rather than
    /// dividing by zero.
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
    }
}

pub struct Status {
    pub message: String,
    pub kind: StatusKind,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Success,
    Warn,
    Error,
}

impl Default for Status {
    fn default() -> Self {
        Self { message: String::new(), kind: StatusKind::Info }
    }
}

impl App {
    pub fn new(client: Client, worker: Worker) -> Self {
        Self {
            client,
            worker,
            job: None,
            pending_trade_note: String::new(),
            frame: 0,
            tab: Tab::Portfolio,
            summary: None,
            holdings_state: TableState::default().with_selected(Some(0)),
            txns_state: TableState::default().with_selected(Some(0)),
            txns: Vec::new(),
            detail: None,
            detail_loading_for: None,
            trade: TradeForm::default(),
            status: Status::default(),
            provider: None,
            should_quit: false,
        }
    }

    pub fn boot(&mut self) -> Result<()> {
        self.reload_provider()?;
        self.reload_summary()?;
        self.reload_transactions()?;

        // Nothing can be fetched without a provider, so say that up front
        // rather than letting the user discover it by pressing 'r'.
        if !self.provider_ready() {
            self.set_status(
                StatusKind::Warn,
                "No price provider configured — run `tickerctl provider set` in a shell",
            );
            return Ok(());
        }

        // Self-recovery: if the daemon's been offline (machine asleep, laptop
        // closed, etc.) the cache may be days old. Catch up automatically so
        // the user sees current data without thinking.
        if let Some(days) = self.max_staleness_days() {
            if days >= 1 {
                let msg = format!(
                    "Cache is {} day{} stale — catching up…",
                    days,
                    if days == 1 { "" } else { "s" }
                );
                self.set_status(StatusKind::Info, &msg);
                // Queued, not awaited: the TUI must come up immediately and
                // show this catching up, rather than holding a blank terminal
                // for a second per stale ticker before it draws anything.
                self.start_refresh(false);
            }
        }
        Ok(())
    }

    /// Ask the daemon which provider is configured. Cheap, and the answer can
    /// change from another terminal, so it is re-asked on every refresh.
    fn reload_provider(&mut self) -> Result<()> {
        if let Response::ProviderStatus(st) = self.client.call(&Request::ProviderStatus)? {
            self.provider = Some(st);
        }
        Ok(())
    }

    /// Whether the daemon can actually fetch. Unknown provider state counts as
    /// ready so a daemon that failed to answer doesn't suppress a refresh.
    fn provider_ready(&self) -> bool {
        self.provider.as_ref().is_none_or(|p| p.ready)
    }

    /// Largest "days since last_updated" across all holdings, or `None` if
    /// there's no portfolio yet.
    fn max_staleness_days(&self) -> Option<i64> {
        let s = self.summary.as_ref()?;
        compute_max_staleness_days(&s.rows, chrono::Utc::now().date_naive())
    }

    fn reload_summary(&mut self) -> Result<()> {
        match self.client.call(&Request::Summary)? {
            Response::Summary(s) => {
                let n = s.rows.len();
                self.summary = Some(s);
                if n == 0 {
                    self.holdings_state.select(None);
                } else if self.holdings_state.selected().unwrap_or(0) >= n {
                    self.holdings_state.select(Some(n - 1));
                } else if self.holdings_state.selected().is_none() {
                    self.holdings_state.select(Some(0));
                }
            }
            Response::Error { message } => self.set_status(StatusKind::Error, &message),
            _ => self.set_status(StatusKind::Error, "unexpected response from daemon"),
        }
        Ok(())
    }

    fn reload_transactions(&mut self) -> Result<()> {
        match self.client.call(&Request::Transactions { ticker: None })? {
            Response::Transactions { rows } => {
                let n = rows.len();
                self.txns = rows;
                if n == 0 {
                    self.txns_state.select(None);
                } else if self.txns_state.selected().unwrap_or(0) >= n {
                    self.txns_state.select(Some(n - 1));
                }
            }
            Response::Error { message } => self.set_status(StatusKind::Error, &message),
            _ => self.set_status(StatusKind::Error, "unexpected response from daemon"),
        }
        Ok(())
    }

    pub fn selected_holding(&self) -> Option<&HoldingRow> {
        let summary = self.summary.as_ref()?;
        let idx = self.holdings_state.selected()?;
        summary.rows.get(idx)
    }

    pub fn set_status(&mut self, kind: StatusKind, msg: &str) {
        self.status = Status { message: msg.to_string(), kind };
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        // Universal: Ctrl-C / Ctrl-Q always quit.
        if matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('q'), KeyModifiers::CONTROL)
        ) {
            self.should_quit = true;
            return Ok(());
        }

        // Esc: from Edit → Nav; from Nav on Trade → Portfolio; elsewhere → clear status.
        if key.code == KeyCode::Esc {
            match self.tab {
                Tab::Trade if self.trade.mode == TradeMode::Edit => {
                    self.trade.mode = TradeMode::Nav;
                    self.set_status(
                        StatusKind::Info,
                        "Nav mode — press i or Enter to edit a field",
                    );
                }
                Tab::Trade => {
                    self.tab = Tab::Portfolio;
                    self.set_status(StatusKind::Info, "Cancelled trade");
                }
                _ => self.set_status(StatusKind::Info, ""),
            }
            return Ok(());
        }

        // Edit mode on the Trade tab captures typing for the focused field.
        if self.tab == Tab::Trade && self.trade.mode == TradeMode::Edit {
            return self.handle_trade_edit_key(key);
        }

        // Everything below is "navigation mode" — works on every tab,
        // including the Trade tab when it's in Nav mode.

        // Trade tab has a few extra nav-mode bindings on top of the global set.
        if self.tab == Tab::Trade {
            match key.code {
                KeyCode::Char('i') | KeyCode::Char('a') | KeyCode::Enter => {
                    self.trade.mode = TradeMode::Edit;
                    self.set_status(
                        StatusKind::Info,
                        "Edit mode — Esc to leave, Tab next field, Enter submit",
                    );
                    return Ok(());
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.trade.field = next_trade_field(self.trade.field, self.trade.side);
                    return Ok(());
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.trade.field = prev_trade_field(self.trade.field, self.trade.side);
                    return Ok(());
                }
                _ => {} // fall through to the global handler
            }
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => self.should_quit = true,
            (KeyCode::Tab, _)
            | (KeyCode::Char('l'), _)
            | (KeyCode::Right, _) => self.cycle_tab(1)?,
            (KeyCode::BackTab, _)
            | (KeyCode::Char('h'), _)
            | (KeyCode::Left, _) => self.cycle_tab(-1)?,
            (KeyCode::Char('1'), _) => self.tab = Tab::Portfolio,
            (KeyCode::Char('2'), _) => {
                self.tab = Tab::Detail;
                self.maybe_load_detail()?;
            }
            (KeyCode::Char('3'), _) => self.enter_trade_nav(),
            (KeyCode::Char('4'), _) => self.tab = Tab::Transactions,
            (KeyCode::Char('r'), _) => {
                // Re-ask first: the user may have just configured a provider
                // in another terminal, and this is the keypress that notices.
                self.reload_provider()?;
                self.start_refresh(false);
            }
            // Shift-R forces a re-fetch of everything, including tickers that
            // already have today's close. Deliberately the harder key: it
            // spends one provider request per holding every time.
            (KeyCode::Char('R'), _) => {
                self.reload_provider()?;
                self.start_refresh(true);
            }
            (KeyCode::Char('b'), _) => self.start_trade(TradeSide::Buy),
            (KeyCode::Char('s'), _) => self.start_trade(TradeSide::Sell),
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => self.move_cursor(-1)?,
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => self.move_cursor(1)?,
            (KeyCode::Enter, _) | (KeyCode::Char('d'), _) if self.tab == Tab::Portfolio => {
                self.tab = Tab::Detail;
                self.maybe_load_detail()?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Land on the Trade tab in nav mode (used by `3` / Tab cycling).
    fn enter_trade_nav(&mut self) {
        self.tab = Tab::Trade;
        self.trade.mode = TradeMode::Nav;
        self.set_status(
            StatusKind::Info,
            "Trade · Nav mode — press i or Enter to edit, j/k to move between fields",
        );
    }

    fn cycle_tab(&mut self, delta: i32) -> Result<()> {
        let cur = Tab::ALL.iter().position(|t| *t == self.tab).unwrap();
        let next = ((cur as i32 + delta).rem_euclid(Tab::ALL.len() as i32)) as usize;
        self.tab = Tab::ALL[next];
        if self.tab == Tab::Detail {
            self.maybe_load_detail()?;
        }
        if self.tab == Tab::Trade {
            // Tab-cycled in: stay in Nav so further h/l keep cycling.
            self.trade.mode = TradeMode::Nav;
        }
        Ok(())
    }

    fn move_cursor(&mut self, delta: i32) -> Result<()> {
        match self.tab {
            Tab::Portfolio => {
                if let Some(s) = &self.summary {
                    move_table(&mut self.holdings_state, s.rows.len(), delta);
                }
            }
            Tab::Detail => {
                if let Some(s) = &self.summary {
                    move_table(&mut self.holdings_state, s.rows.len(), delta);
                }
                // Selection changed — force a reload of the chart.
                self.detail_loading_for = None;
                self.detail = None;
                self.maybe_load_detail()?;
            }
            Tab::Transactions => {
                move_table(&mut self.txns_state, self.txns.len(), delta);
            }
            _ => {}
        }
        Ok(())
    }

    /// Invoked by `b`/`s` — user expressed explicit intent to trade, so we
    /// jump straight into Edit mode on the ticker field.
    fn start_trade(&mut self, side: TradeSide) {
        self.tab = Tab::Trade;
        self.trade.side = side;
        self.trade.field = TradeField::Ticker;
        self.trade.mode = TradeMode::Edit;
        if let Some(h) = self.selected_holding() {
            self.trade.ticker = h.ticker.clone();
            // Pre-populated ticker: skip to shares field.
            self.trade.field = TradeField::Shares;
        }
        self.trade.shares.clear();
        self.trade.price.clear();
        self.set_status(
            StatusKind::Info,
            "Edit mode — Esc to leave, Tab next field, Enter submit",
        );
    }

    fn handle_trade_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        // Esc is handled by the caller (drops us back to Nav mode).
        match key.code {
            KeyCode::Tab => {
                self.trade.field = next_trade_field(self.trade.field, self.trade.side);
            }
            KeyCode::BackTab => {
                self.trade.field = prev_trade_field(self.trade.field, self.trade.side);
            }
            KeyCode::Left | KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.trade.side = match self.trade.side {
                    TradeSide::Buy => TradeSide::Sell,
                    TradeSide::Sell => TradeSide::Buy,
                };
            }
            KeyCode::Enter => self.submit_trade()?,
            KeyCode::Backspace => {
                self.field_mut().pop();
            }
            // Space is dropped and Ctrl-chords never reach the buffer: no
            // field here takes a space, and a stray Ctrl-key should not type.
            KeyCode::Char(c) if c != ' ' && !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.field_mut().push(c);
            }
            _ => {}
        }
        Ok(())
    }

    fn field_mut(&mut self) -> &mut String {
        match self.trade.field {
            TradeField::Ticker => &mut self.trade.ticker,
            TradeField::Shares => &mut self.trade.shares,
            TradeField::Price => &mut self.trade.price,
        }
    }

    fn submit_trade(&mut self) -> Result<()> {
        let ticker = self.trade.ticker.trim().to_uppercase();
        if ticker.is_empty() {
            self.set_status(StatusKind::Error, "ticker is required");
            return Ok(());
        }
        let shares: f64 = match self.trade.shares.trim().parse() {
            Ok(n) if n > 0.0 => n,
            _ => {
                self.set_status(StatusKind::Error, "shares must be a positive number");
                return Ok(());
            }
        };
        let req = match self.trade.side {
            TradeSide::Buy => {
                let price = if self.trade.price.trim().is_empty() {
                    None
                } else {
                    match self.trade.price.trim().parse::<f64>() {
                        Ok(n) if n > 0.0 => Some(n),
                        _ => {
                            self.set_status(StatusKind::Error, "price must be positive or blank");
                            return Ok(());
                        }
                    }
                };
                Request::Buy { ticker: ticker.clone(), shares, price }
            }
            TradeSide::Sell => Request::Sell { ticker: ticker.clone(), shares },
        };
        // A buy for an uncached ticker makes the daemon fetch a price, which
        // is rate-limited like any other fetch — so this goes to the worker
        // too rather than freezing the form mid-trade.
        if let Some(job) = &self.job {
            self.set_status(StatusKind::Warn, &format!("{} in progress — try again in a moment", job.label));
            return Ok(());
        }
        let (verb, gerund) = match self.trade.side {
            TradeSide::Buy => ("Bought", "Buying"),
            TradeSide::Sell => ("Sold", "Selling"),
        };
        self.pending_trade_note = format!("{verb} {shares} {ticker} (simulated)");
        let label = format!("{gerund} {shares} {ticker}");
        if !self.worker.submit(Job::Trade { request: req, label: label.clone() }) {
            self.set_status(StatusKind::Error, "background worker stopped — restart tickerc");
            return Ok(());
        }
        // Same reason as the refresh: claim the slot synchronously so a
        // second Enter can't submit the trade twice.
        self.job =
            Some(JobState { label: label.clone(), done: 0, total: 1, current: label });
        self.set_status(StatusKind::Info, "Submitting…");
        Ok(())
    }

    /// Which held tickers still need a fetch today.
    ///
    /// The cache is day-based, so a ticker already stamped with today's date
    /// cannot be improved by spending another provider request on it — and
    /// providers cap how many of those you get per day. `force` overrides
    /// that for someone who wants a fresher intraday price.
    fn tickers_to_refresh(&self, force: bool) -> Vec<String> {
        let Some(summary) = self.summary.as_ref() else {
            return Vec::new();
        };
        stale_tickers(&summary.rows, &chrono::Utc::now().date_naive().to_string(), force)
    }

    /// Hand a refresh to the worker and return immediately.
    fn start_refresh(&mut self, force: bool) {
        if let Some(job) = &self.job {
            self.set_status(StatusKind::Warn, &format!("{} already in progress…", job.label));
            return;
        }
        if !self.provider_ready() {
            self.set_status(
                StatusKind::Warn,
                "No price provider configured — run `tickerctl provider set` in a shell",
            );
            return;
        }
        let tickers = self.tickers_to_refresh(force);
        if tickers.is_empty() {
            let msg = if self.summary.as_ref().is_none_or(|s| s.rows.is_empty()) {
                "Nothing to refresh — no holdings yet".to_string()
            } else {
                "Every holding already has today's close — press R to re-fetch anyway".to_string()
            };
            self.set_status(StatusKind::Info, &msg);
            return;
        }
        let total = tickers.len();
        if !self.worker.submit(Job::Refresh { tickers }) {
            self.set_status(StatusKind::Error, "background worker stopped — restart tickerc");
            return;
        }
        // Claim the slot now, not when `Started` comes back. The worker
        // reports asynchronously, so between this call and the next
        // `pump_worker` the guard above would still see `None` — long enough
        // for a second keypress (or boot's auto-catchup followed by an early
        // 'r') to queue a duplicate job and spend the daily allowance twice.
        self.job = Some(JobState {
            label: "Refreshing".into(),
            done: 0,
            total,
            current: String::new(),
        });
        self.set_status(StatusKind::Info, "Refreshing…");
    }

    /// Fold in whatever the worker has reported since the last pass. Called
    /// once per event-loop tick; never blocks.
    pub fn pump_worker(&mut self) -> Result<()> {
        for update in self.worker.drain() {
            match update {
                Update::Started { label, total } => {
                    // The slot was claimed at submit time; this only
                    // confirms the worker actually picked the job up.
                    if let Some(job) = self.job.as_mut() {
                        job.label = label;
                        job.total = total;
                    }
                }
                Update::Progress { done, current } => {
                    if let Some(job) = self.job.as_mut() {
                        job.done = done;
                        job.current = current;
                    }
                }
                Update::Finished { kind, ok, failed } => {
                    self.job = None;
                    self.on_job_finished(kind, ok, &failed)?;
                }
                Update::Disconnected { message } => {
                    self.job = None;
                    self.set_status(
                        StatusKind::Error,
                        &format!("lost the background connection: {message} — restart tickerc"),
                    );
                }
            }
        }
        Ok(())
    }

    fn on_job_finished(&mut self, kind: JobKind, ok: usize, failed: &[String]) -> Result<()> {
        match kind {
            JobKind::Refresh => {
                if failed.is_empty() {
                    let msg = format!("Refreshed {ok} ticker{}", if ok == 1 { "" } else { "s" });
                    self.set_status(StatusKind::Success, &msg);
                } else {
                    // The provider's own words — a daily-cap notice explains
                    // itself far better than "refresh failed" does.
                    self.set_status(
                        StatusKind::Warn,
                        &format!("Refreshed {ok}, {} failed — {}", failed.len(), failed.join("; ")),
                    );
                }
                self.reload_summary()?;
                if self.tab == Tab::Detail {
                    self.detail_loading_for = None;
                    self.detail = None;
                    self.maybe_load_detail()?;
                }
            }
            JobKind::Trade => {
                if let Some(message) = failed.first() {
                    self.set_status(StatusKind::Error, message);
                    return Ok(());
                }
                self.set_status(StatusKind::Success, &self.pending_trade_note.clone());
                self.trade.shares.clear();
                self.trade.price.clear();
                self.trade.mode = TradeMode::Nav;
                self.reload_summary()?;
                self.reload_transactions()?;
                self.tab = Tab::Portfolio;
            }
        }
        Ok(())
    }

    fn maybe_load_detail(&mut self) -> Result<()> {
        let Some(h) = self.selected_holding() else {
            self.detail = None;
            self.set_status(StatusKind::Info, "Select a holding on the Portfolio tab first");
            return Ok(());
        };
        let ticker = h.ticker.clone();
        if self.detail_loading_for.as_deref() == Some(&ticker) && self.detail.is_some() {
            return Ok(());
        }
        self.detail_loading_for = Some(ticker.clone());
        match self.client.call(&Request::History { ticker: ticker.clone() })? {
            Response::History(h) => {
                self.detail = Some(h);
            }
            Response::Error { message } => {
                self.detail = None;
                self.set_status(StatusKind::Error, &message);
            }
            _ => self.set_status(StatusKind::Error, "unexpected response"),
        }
        Ok(())
    }
}

fn move_table(state: &mut TableState, len: usize, delta: i32) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as i32;
    let next = (cur + delta).rem_euclid(len as i32) as usize;
    state.select(Some(next));
}

fn next_trade_field(field: TradeField, side: TradeSide) -> TradeField {
    match (field, side) {
        (TradeField::Ticker, _) => TradeField::Shares,
        (TradeField::Shares, TradeSide::Buy) => TradeField::Price,
        (TradeField::Shares, TradeSide::Sell) => TradeField::Ticker,
        (TradeField::Price, _) => TradeField::Ticker,
    }
}

fn prev_trade_field(field: TradeField, side: TradeSide) -> TradeField {
    match (field, side) {
        (TradeField::Ticker, TradeSide::Buy) => TradeField::Price,
        (TradeField::Ticker, TradeSide::Sell) => TradeField::Shares,
        (TradeField::Shares, _) => TradeField::Ticker,
        (TradeField::Price, _) => TradeField::Shares,
    }
}

/// Which holdings still need a provider request today.
///
/// A row already stamped with `today` is skipped: the cache is day-based, so
/// re-fetching it cannot produce a different close, and every provider caps
/// how many requests you get per day. A row whose date is anything else —
/// including `portfolio::summary`'s `"—"` sentinel for a holding with no
/// cached price at all — does need one.
pub(crate) fn stale_tickers(rows: &[HoldingRow], today: &str, force: bool) -> Vec<String> {
    rows.iter()
        .filter(|r| force || r.last_updated != today)
        .map(|r| r.ticker.clone())
        .collect()
}

/// Largest "days since last_updated" across `rows`, or `None` if no row
/// has a parseable date. Rows whose `last_updated` doesn't parse (e.g. the
/// `"—"` sentinel that `portfolio::summary` emits when a holding has no
/// cached price) are SKIPPED — they must not cancel out the staleness
/// signal from the rest of the portfolio.
pub(crate) fn compute_max_staleness_days(
    rows: &[HoldingRow],
    today: chrono::NaiveDate,
) -> Option<i64> {
    let mut max_days: Option<i64> = None;
    for r in rows {
        // Skip — do not short-circuit — when a row's last_updated isn't a
        // YYYY-MM-DD date (e.g. portfolio::summary's "—" sentinel for
        // holdings without a cached price). The old `?` caused a single bad
        // row to disable the boot auto-catchup for the entire portfolio.
        let Ok(d) = chrono::NaiveDate::parse_from_str(&r.last_updated, "%Y-%m-%d") else {
            continue;
        };
        let days = (today - d).num_days().max(0);
        max_days = Some(max_days.map_or(days, |m| m.max(days)));
    }
    max_days
}

#[cfg(test)]
mod tests {
    use super::{compute_max_staleness_days, stale_tickers, JobState};
    use chrono::NaiveDate;
    use ticker_proto::HoldingRow;

    fn named_row(ticker: &str, last_updated: &str) -> HoldingRow {
        HoldingRow { ticker: ticker.into(), ..row(last_updated) }
    }

    #[test]
    fn rows_already_current_today_are_not_refetched() {
        // The whole point of the filter: these cost provider requests out of
        // a daily allowance and cannot return anything new.
        let rows = [named_row("AAA", "2026-05-25"), named_row("BBB", "2026-05-25")];
        assert!(stale_tickers(&rows, "2026-05-25", false).is_empty());
    }

    #[test]
    fn stale_and_never_fetched_rows_are_refreshed() {
        let rows = [
            named_row("FRESH", "2026-05-25"),
            named_row("OLD", "2026-05-20"),
            // portfolio::summary's sentinel for a holding with no cached
            // price — it has never been fetched, so it very much needs one.
            named_row("NEW", "—"),
        ];
        assert_eq!(stale_tickers(&rows, "2026-05-25", false), vec!["OLD", "NEW"]);
    }

    #[test]
    fn force_refetches_everything_including_current_rows() {
        let rows = [named_row("AAA", "2026-05-25"), named_row("BBB", "2026-05-25")];
        assert_eq!(stale_tickers(&rows, "2026-05-25", true), vec!["AAA", "BBB"]);
    }

    #[test]
    fn job_fraction_tracks_progress_and_never_divides_by_zero() {
        let job = |done, total| JobState {
            label: "Refreshing".into(),
            done,
            total,
            current: String::new(),
        };
        assert_eq!(job(0, 7).fraction(), 0.0);
        assert_eq!(job(7, 7).fraction(), 1.0);
        assert!((job(3, 6).fraction() - 0.5).abs() < 1e-9);
        // An empty job would otherwise be 0/0 = NaN, which renders as a
        // bar of `NaN` blocks — i.e. a panic in `repeat`.
        assert_eq!(job(0, 0).fraction(), 1.0);
    }

    fn row(last_updated: &str) -> HoldingRow {
        HoldingRow {
            ticker: "X".into(),
            shares: 1.0,
            avg_cost: 1.0,
            current_price: 1.0,
            value: 1.0,
            cost_basis: 1.0,
            gain: 0.0,
            gain_pct: 0.0,
            weight: 0.0,
            last_updated: last_updated.to_string(),
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 5, 25).unwrap()
    }

    #[test]
    fn empty_rows_returns_none() {
        assert_eq!(compute_max_staleness_days(&[], today()), None);
    }

    #[test]
    fn returns_max_gap_across_rows() {
        let rows = [row("2026-05-22"), row("2026-05-18"), row("2026-05-24")];
        assert_eq!(compute_max_staleness_days(&rows, today()), Some(7));
    }

    #[test]
    fn one_unparseable_row_does_not_short_circuit_others() {
        // The bug: `?` on `parse_from_str(...).ok()?` returns None for the
        // WHOLE portfolio when any single row is "—", disabling auto-catchup.
        let rows = [row("—"), row("2026-05-20")];
        assert_eq!(compute_max_staleness_days(&rows, today()), Some(5));
    }

    #[test]
    fn all_unparseable_rows_return_none() {
        let rows = [row("—"), row("never")];
        assert_eq!(compute_max_staleness_days(&rows, today()), None);
    }

    #[test]
    fn future_date_clamps_to_zero_not_negative() {
        let rows = [row("2026-06-01")];
        assert_eq!(compute_max_staleness_days(&rows, today()), Some(0));
    }
}
