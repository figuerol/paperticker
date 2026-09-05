//! The `tickerctl provider` command group.
//!
//! Key handling rules, all of them deliberate:
//!
//! - **No `--key` flag.** A key passed as an argument lands in shell history
//!   and in every `ps` listing on the machine. The key is read from a prompt
//!   with echo disabled, or from a pipe.
//! - **The key is never printed back**, not by this command and not by
//!   `status` — the daemon reports only whether one is on file.
//! - **The client never writes it to disk.** It goes over the socket and the
//!   daemon persists it, the same way the daemon owns the database.

use std::io::{IsTerminal, Write};

use anyhow::{anyhow, bail, Result};
use ticker_proto::{provider, ProviderInfo, ProviderStatus, Request, SecretString};

/// Build a `SetProvider` request, prompting for whatever is missing.
pub fn build_set_request(id: Option<&str>) -> Result<Request> {
    let info = match id {
        Some(id) => provider::lookup(id).ok_or_else(|| {
            let known: Vec<_> = provider::catalog().into_iter().map(|p| p.id).collect();
            anyhow!("unknown provider {id:?} — available: {}", known.join(", "))
        })?,
        None => choose_interactively()?,
    };

    let api_key = if info.requires_key {
        Some(read_key(&info)?)
    } else {
        None
    };

    Ok(Request::SetProvider { provider: info.id, api_key })
}

fn choose_interactively() -> Result<ProviderInfo> {
    let catalog = provider::catalog();
    if !std::io::stdin().is_terminal() {
        let ids: Vec<_> = catalog.into_iter().map(|p| p.id).collect();
        bail!("no provider given and stdin is not a terminal — pass one of: {}", ids.join(", "));
    }

    eprintln!("Choose a price data provider:\n");
    for (i, p) in catalog.iter().enumerate() {
        eprintln!("  {}) {}", i + 1, p.label);
        eprintln!("     {}", p.description);
        match &p.signup_url {
            Some(url) => eprintln!("     Needs a free API key: {url}"),
            None => eprintln!("     No API key needed."),
        }
        eprintln!();
    }

    eprint!("Number [1-{}]: ", catalog.len());
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;

    let n: usize = line
        .trim()
        .parse()
        .map_err(|_| anyhow!("not a number: {:?}", line.trim()))?;
    catalog
        .into_iter()
        .nth(n.checked_sub(1).ok_or_else(|| anyhow!("choice must be 1 or greater"))?)
        .ok_or_else(|| anyhow!("no such choice: {n}"))
}

/// Read the key without echoing it.
///
/// From a pipe (`printf %s "$KEY" | tickerctl provider set alphavantage`) it reads
/// stdin directly, which keeps the key out of `argv` for automation too.
fn read_key(info: &ProviderInfo) -> Result<SecretString> {
    let key = if std::io::stdin().is_terminal() {
        if let Some(url) = &info.signup_url {
            eprintln!("\n{} needs an API key. Get one at:\n  {url}\n", info.label);
        }
        // Echo disabled by rpassword; the prompt goes to the tty, not stdout,
        // so `tickerctl ... > file` doesn't capture it.
        rpassword::prompt_password(format!("{} API key (input hidden): ", info.label))?
    } else {
        // Piped. There is no echo to suppress, and rpassword would try to
        // open /dev/tty — which fails outright with no controlling terminal
        // (cron, CI, a container), exactly where piping is the only option.
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        buf
    };

    let key = key.trim().to_string();
    if key.is_empty() {
        bail!("no key entered — provider unchanged");
    }
    Ok(SecretString::new(key))
}

pub fn print_status(st: &ProviderStatus) {
    match &st.selected {
        None => {
            match &st.unavailable {
                Some(why) => println!("provider: unavailable — {why}"),
                None => println!("provider: none configured"),
            }
            println!();
            println!("No prices will be fetched until you choose one:");
            println!("  tickerctl provider set");
        }
        Some(p) => {
            println!("provider:  {}", p.label);
            // Only meaningful for providers that use one. A key can still be
            // on file from a previous selection; saying "configured" next to
            // a provider that ignores it just reads as confusing.
            if p.requires_key {
                println!("api key:   {}", if st.key_configured { "configured" } else { "not set" });
            } else {
                println!("api key:   not required");
            }
            println!("ready:     {}", if st.ready { "yes" } else { "no" });
            if !st.ready && p.requires_key && !st.key_configured {
                println!();
                println!("Set the key with: tickerctl provider set {}", p.id);
            }
        }
    }
}

pub fn print_list(st: &ProviderStatus) {
    let selected = st.selected.as_ref().map(|p| p.id.as_str());
    for p in &st.available {
        let marker = if Some(p.id.as_str()) == selected { "*" } else { " " };
        println!("{marker} {:<14} {}", p.id, p.label);
        println!("    {}", p.description);
        match &p.signup_url {
            Some(url) => println!("    Needs a free API key: {url}"),
            None => println!("    No API key needed."),
        }
        println!();
    }
    if selected.is_some() {
        println!("* = currently selected");
    } else {
        println!("None selected. Choose one with: tickerctl provider set");
    }
}
