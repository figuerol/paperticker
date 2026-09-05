# Price data providers

Detail behind the summary in the [README](../README.md#price-data-providers):
what the provider's terms permit, why a refresh is paced, and where your API
key lives. The terms are linked, not summarized — read them yourself.

**No provider is configured out of the box, and the daemon fetches nothing
until you choose one.** There is no silent default — `paperticker` should not
reach a third-party service you did not pick.

```sh
tickerctl provider list     # what's available
tickerctl provider set      # choose one, interactively
tickerctl provider status   # what's configured now
```

## Choosing one

A provider is only offered here if its terms permit what this tool actually
does: **fetch on a schedule, and store the result.** `paperticker` refreshes
automatically and writes every close into a local SQLite table; neither is an
optimization that can be switched off. Plenty of free price sources fail one
or both — the popular undocumented ones generally prohibit automated access
outright — which is why there is exactly one here rather than a menu.

`paperticker` is not affiliated with, endorsed by, or sponsored by any price
data provider.

## Alpha Vantage

The recommended option: a documented daily time-series API with a free key.
The free tier is rate-limited — a once-a-day refresh of a small portfolio does
not come close — and it permits the local cache.

Its free tier is for personal, non-commercial use, and "commercial" is defined
more broadly than you might assume. **Read the [terms of
service](https://www.alphavantage.co/terms_of_service/) and decide whether you
qualify.** Signing up is what accepts them.

`alphavantage.rs` calls only `TIME_SERIES_DAILY`. Their other endpoint families
carry additional terms, so check before reaching for one.

### Why a refresh can be slow

**The daemon paces itself to one Alpha Vantage request per second, so a
refresh takes roughly one second per holding.** Twenty holdings means about
twenty seconds. `tickerctl refresh` sits there for the duration; `tickerc`
runs it in the background and shows a progress bar, so the UI stays usable.
This is working as intended, not a hang.

A refresh only fetches holdings that don't already carry today's close, so the
cost is paid once a day rather than once per refresh — repeating it when
everything is current returns immediately and spends nothing. `tickerctl
refresh --force`, and `R` in the TUI, re-fetch everything regardless, which is
what you want for a fresher intraday price and what to avoid if the daily
allowance is tight.

Alpha Vantage asks callers to spread requests out to one per second. Rather
than fire a burst and get throttled, `tickerd` spaces its own requests: it
waits out the remaining interval before each call, so every path that fetches
a price — the refresh at startup, the hourly stale check, `tickerctl refresh`,
and the on-demand fetch behind a `history` or `buy` for an uncached ticker —
queues through the same limit. Concurrent clients queue too; they do not each
get their own budget. The actual spacing is one second plus a small margin,
because the limit is measured when a request reaches Alpha Vantage and pacing
exactly on the boundary leaves no room for network jitter.

Run the daemon with `RUST_LOG=debug` to see the waits as they happen.

Pacing does not buy unlimited requests. The free tier also caps how many calls
you get per day, so a large portfolio can exhaust the daily allowance even at a
polite rate — check the current limits on your [account
dashboard](https://www.alphavantage.co/support/#support), since they are theirs
to change.

### If you pay for a higher rate

Alpha Vantage's rate-limit message suggests a premium plan for higher
throughput. If you have one, tell the daemon what it allows and the pacing
loosens to match:

```sh
PAPERTICKER_ALPHAVANTAGE_RPM=600 tickerd &   # requests per minute
PAPERTICKER_ALPHAVANTAGE_RPM=off tickerd &   # no pacing at all
```

Read once at startup, in the daemon's environment — like
`PAPERTICKER_API_KEY`, it is never written to disk, and unlike a provider
change it does not take effect on a running daemon. Under a systemd user unit
put it in the same `EnvironmentFile` as your key.

**The default assumes the free tier and the shipped binaries always will.** An
unset, empty, or unparseable value paces at one request per second; a typo is
logged as a warning and falls back to that same safe rate rather than quietly
becoming unlimited. Set this only to a rate your plan actually permits —
raising it past that just moves the throttling to Alpha Vantage's end, where it
costs you a failed refresh instead of a short wait.

The value is your plan's requests-per-minute, and the 10% jitter margin applies
to it the same way: `600` paces at roughly 110ms rather than 100ms. `off`,
`unlimited`, and `0` all disable pacing outright.

### When Alpha Vantage says something, you see it

Alpha Vantage reports problems with HTTP 200 and a message in the body rather
than an error status, so the status code alone never tells you a call worked.
`tickerd` reads those messages and passes them through verbatim — a rate-limit
notice, a daily-cap notice, a rejected symbol, or an invalid key arrives at
your terminal in Alpha Vantage's own words, prefixed with `alphavantage:`, not
flattened into a generic "fetch failed".

If you see a rate-limit message despite the pacing above, it is almost always
the daily cap rather than the per-second rate. Any API key in the text is
scrubbed before the message is shown or logged.

## Where your API key is stored

The key is read from a hidden prompt (or from stdin when piped) and sent to the
daemon over the Unix socket, which persists it in the tool's XDG config
directory — `0600`, inside a `0700` directory.

There is deliberately **no `--key` flag** — an argument would be recorded in
your shell history and visible in `ps` to every user on the machine. The key is
never echoed, never logged, and never returned by the protocol; `provider
status` reports only whether one is on file.

For scripting, pipe the key in rather than passing it as an argument:

```sh
printf '%s\n' "$MY_KEY" | tickerctl provider set alphavantage
```

### Keeping the key out of this project's storage

`PAPERTICKER_API_KEY` and `PAPERTICKER_PROVIDER` override the stored file
and are never written to disk. Set them in the daemon's environment and
`paperticker` never persists a credential at all — useful if you keep secrets
in a password manager, a vault, or your own env file.

**Why there is no `.env` support in the tool itself.** A `.env` is read relative
to the working directory, and `tickerd` is a long-lived daemon started from
wherever you happen to be — its credentials should not depend on your shell's
`cwd`. Loading one is also a single line of shell, so the daemon does not need
to know the format:

```sh
set -a; . ./my-env; set +a; tickerd &
```

Under a systemd user unit, use the mechanism built for this — the path is
absolute, so it does not depend on where the daemon was started:

```ini
[Service]
EnvironmentFile=%h/.config/paperticker/env
ExecStart=%h/.local/bin/tickerd
```

Whichever you choose, that file holds a live credential: create it `0600`, keep
it out of any directory you might commit, and remember that a `.env` in a repo
is one `git add -A` away from being published.

See [SECURITY.md](../SECURITY.md#api-keys) for the full handling rules.
