//! Background job runner, so slow protocol calls don't freeze the UI.
//!
//! The TUI is a single synchronous loop: whatever it calls, it waits for. That
//! was fine when every request was a SQLite read, but a refresh is now paced
//! at roughly a second per ticker and a buy can trigger a provider fetch on a
//! cache miss — long enough that a blocking call reads as a hung terminal.
//!
//! So the slow ones move here. The worker owns its **own** connection to the
//! daemon rather than sharing the app's: the protocol is one response per
//! request on a given socket, so a shared connection would serialize the two
//! anyway and interleave their replies. The daemon spawns a task per accepted
//! connection and holds no database lock across a provider fetch, so the app's
//! connection stays answerable while a refresh is in flight.
//!
//! Progress is per ticker because the worker drives the loop itself with
//! `RefreshTicker` instead of sending one `Refresh`. That is not a way to
//! fetch faster — the daemon's rate limit is applied per request either way —
//! it is the only way to learn which ticker is in flight before the whole
//! sweep has finished.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use ticker_client::Client;
use ticker_proto::{Request, Response};

/// What kind of work is running, so the app knows what to do when it lands.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum JobKind {
    Refresh,
    Trade,
}

pub enum Job {
    /// Refresh these tickers, one request each. The caller decides which ones
    /// (the app skips those already carrying today's close), so this list is
    /// exactly what will be spent on the provider's daily allowance.
    Refresh { tickers: Vec<String> },
    /// A buy or sell. A single request, but it can trigger a provider fetch,
    /// so it is as slow as a refresh step and belongs off the event loop.
    Trade { request: Request, label: String },
}

pub enum Update {
    Started { label: String, total: usize },
    /// `done` items are finished; `current` is the one now in flight.
    Progress { done: usize, current: String },
    Finished { kind: JobKind, ok: usize, failed: Vec<String> },
    /// The worker's connection broke. It sends this and stops; the app keeps
    /// running on its own connection rather than tearing the UI down.
    Disconnected { message: String },
}

pub struct Worker {
    jobs: Sender<Job>,
    updates: Receiver<Update>,
}

impl Worker {
    pub fn spawn(mut client: Client) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (up_tx, up_rx) = mpsc::channel::<Update>();

        thread::spawn(move || {
            // Ends when the app drops its `Sender`, i.e. on quit.
            for job in job_rx {
                if run_job(&mut client, &job, &up_tx).is_err() {
                    // Either the socket died or the app is gone. Both mean
                    // this thread has nothing useful left to do.
                    return;
                }
            }
        });

        Self { jobs: job_tx, updates: up_rx }
    }

    /// Queue work. `false` means the worker thread is gone.
    pub fn submit(&self, job: Job) -> bool {
        self.jobs.send(job).is_ok()
    }

    /// Everything that has arrived since the last call. Never blocks — the
    /// event loop calls this every tick and must stay responsive to keys.
    pub fn drain(&self) -> Vec<Update> {
        self.updates.try_iter().collect()
    }
}

/// `Err(())` means stop the thread: the connection or the app is gone.
fn run_job(client: &mut Client, job: &Job, up: &Sender<Update>) -> Result<(), ()> {
    match job {
        Job::Refresh { tickers } => {
            let total = tickers.len();
            send(up, Update::Started { label: "Refreshing".into(), total })?;

            let mut ok = 0usize;
            let mut failed = Vec::new();
            for (i, ticker) in tickers.iter().enumerate() {
                // Announced before the call, not after: this is the ticker
                // the user is currently waiting on.
                send(up, Update::Progress { done: i, current: ticker.clone() })?;
                match client.call(&Request::RefreshTicker { ticker: ticker.clone() }) {
                    Ok(Response::Ok) => ok += 1,
                    Ok(Response::Error { message }) => failed.push(format!("{ticker}: {message}")),
                    Ok(_) => failed.push(format!("{ticker}: unexpected response")),
                    Err(e) => {
                        // A transport error leaves the connection in an
                        // unknown state, so don't keep using it.
                        send(up, Update::Disconnected { message: e.to_string() })?;
                        return Err(());
                    }
                }
            }
            send(up, Update::Finished { kind: JobKind::Refresh, ok, failed })?;
        }
        Job::Trade { request, label } => {
            send(up, Update::Started { label: label.clone(), total: 1 })?;
            send(up, Update::Progress { done: 0, current: label.clone() })?;
            match client.call(request) {
                Ok(Response::Ok) => {
                    send(up, Update::Finished { kind: JobKind::Trade, ok: 1, failed: vec![] })?
                }
                Ok(Response::Error { message }) => send(
                    up,
                    Update::Finished { kind: JobKind::Trade, ok: 0, failed: vec![message] },
                )?,
                Ok(_) => send(
                    up,
                    Update::Finished {
                        kind: JobKind::Trade,
                        ok: 0,
                        failed: vec!["unexpected response".into()],
                    },
                )?,
                Err(e) => {
                    send(up, Update::Disconnected { message: e.to_string() })?;
                    return Err(());
                }
            }
        }
    }
    Ok(())
}

/// A send failure means the app has dropped the receiver — it quit.
fn send(up: &Sender<Update>, u: Update) -> Result<(), ()> {
    up.send(u).map_err(|_| ())
}
