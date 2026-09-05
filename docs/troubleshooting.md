# Troubleshooting

The failure modes you are actually likely to hit, by symptom. The
[README](../README.md) covers install and normal use; this file covers the
moments where something looks broken.

`tickerctl`'s exit code tells you which half of the system to look at: `0`
success, `1` the daemon answered with an error (the message is on stderr — an
oversell, a provider failure), `2` the command never reached the daemon.

## The daemon can't be reached

```
tickerctl: connecting to /run/user/1000/ticker-follow.sock: No such file or directory
```

Exit code `2`, with any connection error in place of that one: nothing is
listening on the socket, so the daemon isn't running. Start it and leave it
running:

```sh
tickerd &
tickerctl ping
```

Every client goes through `tickerd` — `tickerc` and `tickerctl` never open the
database or fetch prices themselves — so nothing works until it is up.

## Another tickerd is already running

```
another tickerd (PID 12345) is already listening on /run/user/1000/ticker-follow.sock
```

One daemon per user is by design: one instance owns the database and the
socket, and both clients find it on their own. This message means one is
**already running and healthy**, so usually the right move is to use it
(`tickerctl ping` to confirm).

You only need to replace it after an upgrade, since the daemon and clients
share a wire protocol:

```sh
pkill tickerd && tickerd &
```

A daemon that crashed or was killed leaves its socket file behind, and that
case needs no cleanup: the next `tickerd` probes the socket, finds nothing
answering, removes it and takes over. So a leftover socket file is never the
thing to delete by hand — if `tickerd` refuses to start, something really is
listening.

## Version skew after an upgrade

```
error: unrecognized subcommand 'provider'
bad request: unknown variant `ProviderStatus`
```

The daemon and both clients share the wire protocol defined in `ticker-proto`,
so they have to be replaced together: a new `tickerctl` against an old daemon
reports an unrecognized subcommand, and an old daemon that doesn't know a newer
op answers `bad request`.

Replace all three binaries and restart the daemon — see
[Upgrading](../README.md#upgrading). Mixing a release archive with binaries
from a checkout counts as skew too; check which comes first on your `PATH`.

## No price data provider is configured

```
error: no price data provider is configured — run `tickerctl provider set` to choose one
```

Nothing is configured out of the box and the daemon fetches nothing until you
choose a provider — deliberate, not a missing default. Every refresh, new
ticker, and uncached price reports this until you run:

```sh
tickerctl provider set      # interactive: pick one, enter the key
tickerctl provider status   # confirm it's ready
```

Browsing an already-cached portfolio keeps working without one.
[docs/providers.md](providers.md) covers which to pick and what each one's
terms commit you to.

## The provider is selected but not ready

```
provider:  Alpha Vantage (official)
api key:   not set
ready:     no
```

The provider needs an API key and none is on file. Re-run `tickerctl provider
set alphavantage`; the key is read from a hidden prompt (or piped stdin), never
from a flag. If you supply it through the environment instead,
`TICKER_FOLLOW_API_KEY` has to be set in **the daemon's** environment, not in
the shell where you run `tickerctl` — see
[docs/providers.md](providers.md#keeping-the-key-out-of-this-projects-storage).

## The configured provider no longer exists

`tickerctl provider status` reports `provider: none configured` even though
`credentials.json` names one, and nothing fetches. That is a provider this
project has since removed: the daemon says which and why at startup, and so
does `tickerctl provider set <id>` — you get a real reason rather than "unknown
provider". Removed ids are never reused. Pick another:

```sh
tickerctl provider set
```

Your portfolio and transaction history are untouched; only the price source
changes. See [Removed providers](providers.md#removed-providers).

## Alpha Vantage errors and rate limits

Alpha Vantage returns rate-limit and bad-key replies as ordinary HTTP 200
bodies, so they reach you as that provider's own wording, prefixed
`alphavantage:`. The two common ones:

- **Rate limited.** There are two separate caps and the message often mentions
  both, which makes it easy to misread. The **per-second** rate is handled for
  you — the daemon paces itself and never sends faster than one request a
  second. The **daily** allowance is the one you can actually exhaust, and no
  amount of pacing helps: it is a budget, not a speed limit.

  A plain `tickerctl refresh` only fetches holdings that don't already have
  today's close, so repeating it costs nothing once the day's prices are in.
  `--force` (and `R` in the TUI) re-fetches everything, spending one request
  per holding every time — that is the usual way a day's allowance disappears.
  Wait for the day to roll over, or subscribe to a plan with a higher
  allowance; see [If you pay for a higher
  rate](providers.md#if-you-pay-for-a-higher-rate).
- **Invalid key.** Check `tickerctl provider status`, then re-enter the key
  with `tickerctl provider set alphavantage`.

The key is scrubbed from any provider text that reaches you, so these messages
are safe to paste into a bug report.

## A sell is refused

```
error: only holding 5 of AAPL
```

The sell exceeds the shares the ledger holds — there is no short selling.
`tickerctl` exits `1` here: the daemon answered, it just refused the trade.

## Prices look days old

If the machine has been off or asleep, the cache catches up on its own:

1. At startup `tickerd` checks every held ticker and fetches anything not
   refreshed today.
2. Every provider returns months of daily closes per request, so a single
   fetch backfills the whole gap.
3. `tickerc` auto-triggers a refresh on connect when the cache is ≥1 day
   stale, and shows `Caught up after N day(s) offline`.
4. The daemon's hourly stale-check picks up long idle sessions without a
   restart.

The Holdings table title shows the last-refresh date with an `(N days ago)`
suffix whenever the cache isn't from today. If that suffix survives a refresh,
the fetch itself is failing — run `tickerctl refresh` in a terminal and read
the error.

Staleness is by date, not by a timer: a ticker is re-fetched when its
`last_updated` isn't today's UTC date. Around the UTC rollover, and on days a
market is closed, today's close may simply not exist yet.

## Starting over

Nothing but the SQLite file holds portfolio state, so deleting it is a clean
reset — of the ledger as well as the price cache. Back it up first if you want
it; the commands are under
[Reset the database](../README.md#reset-the-database).

To drop only the provider and its stored key, leaving the portfolio alone, use
`tickerctl provider clear`.
