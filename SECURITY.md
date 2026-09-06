# Security

`paperticker` is a **simulation**. It moves no money and places no trades;
the database is a paper ledger of made-up positions. That bounds the blast
radius of most of what follows — with one exception worth stating plainly: it
does hold one real credential, the API key for whichever price data provider
you configure. See [API keys](#api-keys).

## Reporting a vulnerability

Use GitHub's private vulnerability reporting — the **Security** tab on this
repository, "Report a vulnerability". That opens a private advisory visible
only to the maintainers.

Please don't open a public issue for something exploitable. This is a
spare-time project: expect a first response within a couple of weeks, and
no bounty.

## Trust model

**The socket is the trust boundary, and it has no authentication.**

`tickerd` accepts any request arriving on its Unix domain socket. There is no
token, no handshake, no per-client identity — anyone who can `connect()` to
the socket can read the whole portfolio, and can append buys and sells to the
ledger. Access is controlled entirely by filesystem permissions:

| Path | Mode |
| ---- | ---- |
| `$XDG_RUNTIME_DIR/paperticker.sock` | `0600`, inside a directory the login session already keeps private |
| `/tmp/paperticker-<uid>/` (fallback) | `0700`, created by `tickerd` |
| `/tmp/paperticker-<uid>/paperticker.sock` | `0600` |
| `$XDG_CONFIG_HOME/paperticker/` | `0700`, created by `tickerd` |
| `$XDG_CONFIG_HOME/paperticker/credentials.json` | `0600`, created by `tickerd` |
| `$XDG_DATA_HOME/paperticker/` | `0700`, created by `tickerd` |
| `$XDG_DATA_HOME/paperticker/portfolio.db` | `0600`, set by `tickerd` on open |

The fallback socket lives *inside* a private directory rather than directly in
`/tmp`. This is deliberate: `bind()` creates a socket at whatever the umask
allows and the mode can only be tightened afterwards, so a socket sitting bare
in world-traversable `/tmp` is briefly reachable by other local users. A `0700`
parent closes that window. On startup `tickerd` refuses to bind if that
directory already exists and is a symlink or owned by another uid, and
tightens it if its mode is loose.

Consequences worth being explicit about:

- **A local attacker running as your uid has full access.** Same-uid isolation
  is not something Unix permissions provide, and this project does not attempt
  it. Anything running as you can already read the database file directly.
  Other users cannot: the ledger is a complete record of what you hold, so it
  is kept `0600` in a `0700` directory like the credentials beside it. SQLite
  takes no mode argument, so `tickerd` tightens the file after opening it —
  the private directory is what closes the window, and a database created by
  an older build is repaired on the next start.
- **`root` has full access.** As always.
- **Multi-user machines are the case that matters.** On a single-user laptop
  the boundary is mostly theoretical.

## API keys

The one real secret this project handles. How it is treated:

- **Never an argument.** There is no `--key` flag. A key in `argv` is recorded
  in shell history and readable from `ps` by every user on the machine. It is
  read from a prompt with terminal echo disabled, or from stdin when piped.
- **Never written by a client.** `tickerctl` sends it to the daemon over the
  0600 socket; the daemon persists it, the same way the daemon owns the
  database. Clients hold it only in memory, briefly.
- **Written without a readable window.** The credentials file is created at
  `0600` via `O_CREAT|O_EXCL` in a `0700` directory and installed by atomic
  rename — never written first and `chmod`ed after, which would leave the key
  briefly world-readable. A file found with looser permissions is tightened on
  load, with a warning.
- **Never logged.** The key travels inside a `SecretString` whose `Debug` and
  `Display` render `[redacted]`, so a `warn!(?req)` on any error path cannot
  print it. This is enforced by the type, not by remembering.
- **Never returned.** `provider status` reports only whether a key is on file.
  No protocol response can carry a stored credential back out of the daemon.
- **Kept out of URLs where possible.** Alpha Vantage — the only provider that
  takes a key — requires it as a query parameter rather than a header, so it
  never quotes a request URL in an error, and scrubs the key from any response
  body it does include in one. A provider that can authenticate by header
  should.
- **Zeroized on drop**, best effort — a `String` that has reallocated may have
  left copies elsewhere on the heap.

To keep the key out of this project's storage entirely, set
`PAPERTICKER_API_KEY` in the daemon's environment; it overrides the file and
is never persisted.

What this does *not* protect against: anything running as your uid can read
the credentials file directly, exactly as it can read the database. See the
trust model above.

## What is validated

- **Ticker symbols** are uppercased and validated at the dispatch boundary
  (`tickerd/src/server.rs`) before reaching a URL or a client's terminal:
  ASCII alphanumeric plus `.` and `-`, 1–10 characters. This is what keeps a
  stray `&`, `?`, or `../` out of the provider request URL, which is built by
  string concatenation.
- **Share counts** must be finite and strictly positive — the check rejects
  `NaN` explicitly, since every `NaN` comparison is false and a naive
  `s <= 0.0` would let it through.
- **Requests** are newline-delimited JSON parsed by `serde`; a malformed line
  produces an error response rather than terminating the connection. A single
  request is capped at 64 KiB — without a bound, a client that opens a line
  and never closes it grows the daemon's buffer until the process dies. That
  cap is per request, not per connection, so a long-lived client is unaffected.
  Oversized lines get an error and the connection closes, since there is no way
  to resynchronize mid-line.

Prices are *not* validated beyond parsing, from any provider — a wrong number
upstream becomes a wrong number in your paper portfolio. The providers warrant
nothing about accuracy either. Treat every figure here as unverified.

## Third-party data

**No provider is configured by default and the daemon fetches nothing until an
operator selects one.** There is no silent default, which is the point: the
tool should not contact a third party nobody chose.

There is one, and it is a documented API used with a key under its own
published terms: **Alpha Vantage**, whose free tier is for personal,
non-commercial use. Read their
[terms](https://www.alphavantage.co/terms_of_service/) and decide whether you
qualify — the daemon does not and cannot enforce it.

Whether a provider's terms permit automated fetching and *storing* the data is
a gating question here, not a footnote: the scheduled refresh and the price
cache are both the design, and most free price sources fail one or both. There
is no "unofficial" tier and no flag that opts an operator into one — if a
provider is in the catalog, its terms permit what the daemon will do with it.

`tickerd` sends an honest `User-Agent` (`paperticker/0.1 (simulation)`) and
does not impersonate a browser. It caches daily closes, so normal use
re-fetches a ticker at most once a day — an explicit `refresh` always
re-fetches.

Note that refreshing tells your provider which symbols you follow. Those
requests go out over TLS, but they are tied to your IP and, for the keyed
providers, to your account.

## Dependencies

All dependencies come from crates.io and are pinned by `Cargo.lock`, which is
committed. CI runs `cargo test --locked` and `cargo clippy --locked` on every
push and pull request, and release builds run `cargo test --locked` and
`cargo build --locked`, so neither a merge nor a release can silently pick up
a different dependency tree. No crate in
the workspace is published (`publish = false`).
