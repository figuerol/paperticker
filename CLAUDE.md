# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`paperticker` is a **simulation-only** paper-trading portfolio for the terminal. No real trades, no real money. It caches daily closes from a user-selected price data provider in SQLite and renders holdings plus Bollinger-band charts.

## Commands

```sh
cargo build                       # debug build of all crates
cargo build --release             # optimized (LTO + strip; slow, do this once per release)
cargo check                       # fast type-check, no codegen
cargo clippy --all-targets        # lints

cargo run -p tickerd              # start the daemon (must be running for clients to work)
cargo run -p tickerc              # the ratatui TUI (for humans, not scriptable)
cargo run -p tickerctl -- ping    # the scriptable CLI; args after `--`

cargo test                        # all tests
cargo test -p ticker-proto bands_have_correct_shape   # a single test
RUST_LOG=debug cargo run -p tickerd                    # verbose daemon logging
```

Test coverage is thin — only `ticker-proto` (the `bollinger_bands` shape test) has any. There is no integration test harness; the daemon and clients are validated by running them.

## Architecture

A Cargo workspace of five crates. The hard rule: **three binaries, one daemon owns all state, everyone else talks to it over a socket.**

```
ticker-proto   shared wire types + bollinger_bands()  ← the contract
ticker-client  synchronous IPC client (std UnixStream)
   ↑ used by both clients
tickerd        async daemon (tokio): owns SQLite + provider fetches + the socket
tickerc        ratatui TUI client
tickerctl      clap CLI client
```

### The protocol is the contract (`ticker-proto/src/lib.rs`)

Clients and daemon communicate with **newline-delimited JSON over a Unix domain socket** — one `Request` per line, one `Response` per line. `Request` is `#[serde(tag = "op")]`, `Response` is `#[serde(tag = "kind")]`. The structs in `ticker-proto` *are* the wire format: changing a field renames/reshapes the JSON, so **the daemon and both clients must be rebuilt together** or they'll fail to parse each other. When adding a feature that touches the wire, edit `ticker-proto` first, then the daemon's `dispatch` (`tickerd/src/server.rs`), then the clients.

Socket path: `$XDG_RUNTIME_DIR/paperticker.sock`, falling back to `/tmp/paperticker-<uid>/paperticker.sock`. One daemon per user. `tickerd` probes a pre-existing socket on startup and removes it if dead, else refuses to start.

### The daemon is the only writer (`tickerd/`)

Clients never touch SQLite or a provider directly — every read and write goes through the socket. Inside the daemon:

- `db.rs` — SQLite (rusqlite, bundled). Two tables: `transactions` (append-only ledger; buys `shares > 0`, sells `shares < 0`) and `price_cache` (one row per ticker: current price, 6mo of closes as JSON, `last_updated`). The whole `Db` sits behind a single `Arc<Mutex<Db>>` — connection access is serialized, so don't expect concurrency within DB work.
- `provider/` — one `PriceSource` impl per source (today only `alphavantage`), behind an `async_trait`. `Providers::current()` resolves the configured one **per fetch**, so `tickerctl provider set` takes effect without restarting the daemon. **There is no default provider**: with none configured every fetch path returns a clear error and the daemon touches no network. The shared catalog (ids, labels, whether a key or an acknowledgment is needed) lives in `ticker-proto/src/provider.rs` so clients can render the choices — a new provider needs an entry there *and* an arm in `provider::build`, and a test enforces the pair. Each impl hardcodes its own ~6-month history window in its own dialect (Alpha Vantage `outputsize=compact`); `alphavantage` calls only `TIME_SERIES_DAILY`, and reaching for their Economic Indicators or Commodities endpoints would pull in the separate FRED® terms — see `docs/providers.md`. **Alpha Vantage is rate-limited to one request per second** (`PAPERTICKER_ALPHAVANTAGE_RPM` overrides it for premium plans, read once at startup, never persisted — **the shipped default must stay the free-tier rate**, and an invalid value falls back to it rather than to "unlimited") by a `Throttle` (`provider/mod.rs`) held in a `static` in `alphavantage.rs` — it must be a static because `Providers` rebuilds the `PriceSource` per fetch, so per-instance state would reset every call. That's also why a refresh costs ~1s per holding. The throttle is per-provider, not global. Alpha Vantage's `Note`/`Information` messages are surfaced verbatim (key-scrubbed) rather than flattened — keep it that way, they're how the service explains a cap.
- `config.rs` — provider selection + API key at `$XDG_CONFIG_HOME/paperticker/credentials.json`, `0600` in a `0700` dir, written by atomic rename from an `O_EXCL` `0600` temp file. `PAPERTICKER_PROVIDER` / `PAPERTICKER_API_KEY` override it and are never persisted.
- `portfolio.rs` — derives holdings by **replaying the ledger**. `positions()` iterates transactions in *reverse* because `db.transactions()` returns them DESC (newest first). Cost basis is running-average, reduced pro-rata on sells — good enough for paper trading, not tax-grade.
- `server.rs` — accept loop + `dispatch()`. This is where tickers are `.to_uppercase()`d and where cache-miss-triggers-fetch logic lives (History/Buy fetch from the provider if the ticker isn't cached). Also owns the `SetProvider` / `ProviderStatus` / `ClearProvider` ops. **`Refresh { force }` skips tickers already carrying today's date** (the same day-based test `refresh_stale` uses) — a provider's *daily* request allowance is the scarce resource, and re-fetching a current ticker cannot return anything new; only `force` overrides it. `RefreshTicker { ticker }` refreshes one, and exists so a client can drive the loop and report progress — the protocol is strictly one response per request, so a single `Refresh` can't report anything until every fetch is done. It is not a way to fetch faster: the rate limit is applied per request either way.

**The cache is a licensing constraint, not just a design one.** Every close fetched is written to `price_cache` and kept until the date rolls over, so a provider is only usable here if its terms permit persisting the data. Don't widen what's retained (longer history, a second table, logging raw responses) without re-checking the provider's terms. Provider terms live in `docs/providers.md`, not in the README. Symptom-level troubleshooting lives in `docs/troubleshooting.md`; the README keeps the install and usage path and links out to both.

**Cache invalidation is day-based, not timestamp-based.** A ticker is "stale" when its `last_updated` date string ≠ today's UTC date (`refresh_stale` in `main.rs`). The daemon refreshes stale tickers at startup and again every hour — this is deliberately resilient to suspend/resume and missed-midnight crossings: what matters is "do we have today's close", not "did a timer fire". Don't replace this with a fixed-interval scheduler.

### Two clients, one library

`ticker-client` is a **synchronous** `std::os::unix::net::UnixStream` wrapper — note the daemon is async (tokio) but the clients are not; they share only `ticker-proto`, never tokio.

- `tickerc` (`app.rs` + `ui.rs` + `worker.rs`) — the TUI is a state machine in `App`. The Trade tab is **modal** (vim-style `Nav`/`Edit` in `TradeMode`) so tab-navigation keys don't leak into the form; `b`/`s` jump straight into Edit, tab-cycling lands in Nav. On boot it auto-refreshes if the cache is ≥1 day stale (`max_staleness_days`). **Refreshes and trades run on `worker.rs`, not the event loop** — both can take seconds (a paced refresh, or a buy that triggers a fetch), and a blocking call reads as a hung terminal. The worker holds its **own** connection: one response per request per socket means sharing the app's would just re-serialize them. `App::job` drives the progress bar and guards against a second job; **it is claimed synchronously in `start_refresh`/`submit_trade`, not when `Update::Started` arrives** — the gap between the two is long enough to queue a duplicate job and spend the daily allowance twice. `r` refreshes what's stale, `R` forces everything.
- `tickerctl` — one-shot subcommands, `--json` for raw protocol output. **Exit codes are a contract**: `0` success, `1` daemon returned `{"kind":"error"}` (message on stderr — e.g. oversell), `2` couldn't reach the daemon. Preserve these.

`bollinger_bands(closes, window=20, n_std=2.0)` lives in `ticker-proto` (shared so clients *could* recompute, though today only the daemon calls it). The first `window-1` entries are always `None` — that's correct, not a bug.

### API keys are a typed secret

The key crosses the socket inside `SecretString` (`ticker-proto/src/secret.rs`), whose `Debug`/`Display` render `[redacted]` — so `warn!(?req)` on an error path cannot leak it. **Keep it that way**: don't add a `--key` CLI flag (shell history, `ps`), don't log a request that carries one, don't return one in a `Response`, and don't interpolate one into a URL that could reach an error message. Alpha Vantage has to put the key in a query string, so it never quotes a request URL in an error and scrubs the key from response bodies it does quote.

**There is no "unofficial provider" tier, and don't add one.** A provider goes in the catalog only if its terms permit both things this daemon does unattended: fetch on a schedule, and keep the data on disk. Check the candidate's terms for *both* before writing any code — most free price sources fail one, and the popular undocumented endpoints prohibit automated access outright. An acknowledgment prompt is not a substitute for that test: it just moves the exposure onto the operator while the project still ships the endpoint.

## Conventions

- Tickers are normalized to uppercase **at the daemon boundary** (`dispatch`), so the rest of the daemon assumes uppercase. Symbols are US-market tickers with a dash for share classes (`BRK-B`, not `BRK.B`).
- Money values are in each symbol's quoted currency; nothing converts currencies.
- The release profile uses `panic = "abort"` — no unwinding in release builds.

## The bundled Claude Code skill

`.claude/skills/ticker-portfolio/SKILL.md` ships with the repo and auto-loads here. It drives `tickerctl` (installed at `~/.local/bin/tickerctl`) to answer portfolio questions and simulate trades. Its rules matter when you act as that skill: **confirm before any buy/sell**, never invent prices, never run `refresh` as a side effect, and surface daemon error messages verbatim. If `tickerctl ping` fails, ask the user to start `tickerd &` — don't launch the long-lived daemon yourself. If a fetch fails because no provider is configured, tell the user to run `tickerctl provider set` — **never** run it for them: it selects a third-party service and prompts for a key under terms the user has to accept, which is their decision.
