---
name: ticker-portfolio
description: Query and modify the user's simulated stock portfolio via the `tickerctl` CLI. Use when the user asks about their portfolio, holdings, current prices, performance, weights, ticker history, Bollinger bands, or wants to simulate buying/selling shares. This is paper trading — no real money, no real trades. Do NOT use for live market data or real brokerage operations.
---

# ticker-portfolio

The user runs a paper-trading dashboard in this repo. A daemon (`tickerd`)
caches daily closes from a user-selected price data provider in SQLite and
serves a Unix-socket protocol. Two clients exist:

- `tickerc` — the interactive ratatui TUI. **You can't drive this** — it's
  for the human.
- `tickerctl` — a scriptable CLI. **Use this for every operation.**

`tickerctl` is installed at `~/.local/bin/tickerctl` and on the user's
`PATH`. Run `tickerctl --help` (or `<subcommand> --help`) any time you need
to re-check the surface.

## Daemon readiness

```sh
tickerctl ping
```

Prints `pong` when the daemon is up. If you get `tickerd not reachable …`
**ask the user to start it** (`tickerd &`). Don't launch the daemon
yourself — it's a long-lived background process the user manages.

If the user reports `another tickerd is already listening`, that means one is
already running and healthy — they don't need a second one. Tell them to check
with `tickerctl ping` and only replace it (`pkill tickerd && tickerd &`) if
they've just upgraded the binaries.

## Commands

Default output is human-readable text. Add `--json` to any command for the
raw protocol response (use this when you want to parse fields rather than
quote text back).

### `tickerctl summary`

Portfolio totals plus every holding with shares, avg cost, current price,
value, weight, gain, gain %, and the date each row's price was last
cached. The header line shows the most-recent refresh date across all
holdings.

```sh
tickerctl summary
tickerctl summary --json   # totals + rows as structured JSON
```

### `tickerctl holdings`

Same per-ticker rows as `summary`, but without the totals block. Use when
the user only wants the position list.

### `tickerctl history <TICKER>`

6 months of daily closes + 20-day SMA + ±2σ Bollinger bands for one
ticker. Human output shows the last ~10 days as a table; `--json` returns
all 120-ish points and bands aligned by index.

```sh
tickerctl history AAPL
tickerctl history AAPL --json
```

The first 19 band entries are always `null` (the 20-day window hasn't
filled yet) — don't report that as a bug.

### `tickerctl buy <TICKER> <SHARES> [--price N]`

Simulates a paper buy. Without `--price`, fills at today's cached close.

```sh
tickerctl buy AAPL 10              # fill at today's close
tickerctl buy AAPL 10 --price 150  # user-specified fill
```

**Always confirm before running.** Restate the order — ticker, shares,
fill source ("today's close at $X.XX" or "$X user-specified"), resulting
cash impact — and wait for explicit "yes". Exit code 0 = success.

### `tickerctl sell <TICKER> <SHARES>`

Simulates a paper sell at today's cached close. The daemon rejects sells
that exceed current holdings; don't try to bypass that. Same confirmation
rule as buys.

```sh
tickerctl sell AAPL 5
```

### `tickerctl provider status`

Which price data provider is configured, and whether it can fetch. Run this
first when any command fails with "no price data provider is configured".

**Never run `tickerctl provider set` yourself.** It selects a third-party
service and prompts for an API key, under terms the user has to accept for
themselves. That is their decision — tell them to run it and stop there.

### `tickerctl refresh`

Refreshes held tickers that don't already have today's close. Roughly a
second per ticker fetched — the daemon paces itself to stay inside the
provider's rate limit. `--force` re-fetches everything, including tickers
already current today.

**Don't run this casually, and never pass `--force` on your own initiative.**
The daily cache is intentional, and providers cap how many requests a day you
get — `--force` spends one per holding every time it runs. Only refresh when
the user asks, or when `last_updated` is genuinely stale and a recent number
actually matters for the decision at hand.

### `tickerctl transactions [TICKER]`

Lists transactions (newest first), optionally filtered to one ticker.
Buys show as `+shares`, sells as `-shares`.

```sh
tickerctl transactions
tickerctl transactions AAPL
```

## Exit codes

- `0` — success.
- `1` — daemon returned `{"kind":"error"}` (e.g. oversell, unknown ticker,
  bad price). The error message is printed on stderr.
- `2` — the command never reached the daemon: couldn't connect, unexpected
  response shape, or an interactive prompt was cancelled.

If a command fails with "no price data provider is configured", that is not a
bug and not something to work around: report it and ask the user to run
`tickerctl provider set`.

When you see exit `1`, surface the daemon's error message verbatim to the
user — don't paraphrase financial constraints.

## Working with the data

- **Money values are in the symbol's quoted currency.** Don't convert. If
  a ticker is in EUR, it stays in EUR.
- **Weights** sum to ~100% of `total_value`.
- **Staleness**: if the `Updated` column shows a date that isn't today, say
  so — "AAPL last refreshed 3 days ago, want me to refresh first?". Don't
  pretend the number is current. (The summary header shows `last refresh
  <date>` for the freshest cached row.)
- **Cost basis on partial sells** is running-average, not FIFO/LIFO. This
  is paper trading, not tax-grade accounting. Don't claim otherwise.

## Trading guardrails

This is a **simulation**, but the user treats it as a ledger. Treat it
the same:

1. Before any `buy` / `sell`, restate the order: ticker, shares, price
   source (today's close vs. override), cash impact. Wait for explicit
   confirmation.
2. Never invent prices — use the cache or what the user supplied.
3. Never run `refresh` as a side effect of another command.
4. If the user describes a trade in dollars ("buy $1000 of AAPL"),
   compute shares using `tickerctl --json summary | jq '.rows[] | select(.ticker=="AAPL").current_price'`
   (or `tickerctl history AAPL --json | jq .current_price`) and **show
   your math** before sending.
5. If the user names a ticker you can't verify exists, run
   `tickerctl history <SYM> --json` first — the daemon fetches from the
   configured provider on a cache miss; failure is exit code 1 with a clear
   message.

## When NOT to use this skill

- Real-time intraday prices — the daemon caches daily closes only.
- Real trades or live brokerage — this is paper trading.
- Drawing charts in chat — the user's `tickerc` TUI renders proper Braille
  charts; suggest they open it instead.
- Ticker symbol discovery — symbols must be exact US-market tickers (e.g.
  `BRK-B`, not `BRK.B`). Ask the user to confirm the symbol when in doubt.

## Reset / debug

- SQLite file: `~/.local/share/ticker-follow/portfolio.db`
  (or `$XDG_DATA_HOME/ticker-follow/portfolio.db`).
  To wipe: stop the daemon, `rm` the file, restart.
- `RUST_LOG=debug tickerd` for verbose daemon logging.
- `tickerctl --socket /path/to.sock <cmd>` to override the socket path
  (rare — useful if the user is running a non-default setup).
