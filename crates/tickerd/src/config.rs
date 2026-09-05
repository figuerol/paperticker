//! Provider selection and credential storage.
//!
//! The daemon owns this the way it owns the database: clients ask it to store
//! a key over the socket, they never write one to disk themselves.
//!
//! On-disk shape (`$XDG_CONFIG_HOME/ticker-follow/credentials.json`, or
//! `~/.config/...`):
//!
//! ```json
//! { "provider": "alphavantage", "api_key": "…" }
//! ```
//!
//! The file is `0600` inside a `0700` directory, written by rename from a
//! temporary file created at `0600` — never `write`-then-`chmod`, which would
//! leave the key briefly world-readable.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use ticker_proto::SecretString;
use tracing::warn;

/// Overrides the stored key without touching disk. For CI, headless hosts,
/// and anyone keeping secrets in their own vault. Never persisted.
const KEY_ENV: &str = "TICKER_FOLLOW_API_KEY";
/// Overrides the stored provider selection.
const PROVIDER_ENV: &str = "TICKER_FOLLOW_PROVIDER";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// `None` until the operator chooses. The daemon fetches nothing while
    /// this is unset — that is what keeps it from silently reaching a
    /// third-party service on first run.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretString>,
}

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("ticker-follow");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("ticker-follow")
}

pub fn config_path() -> PathBuf {
    config_dir().join("credentials.json")
}

impl Config {
    /// Read the stored config, then let the environment override it.
    ///
    /// A missing or unreadable file is not an error: it means "nothing
    /// configured yet", which the daemon handles by refusing to fetch.
    pub fn load() -> Self {
        let mut cfg = Self::load_file(&config_path()).unwrap_or_default();

        if let Ok(p) = std::env::var(PROVIDER_ENV) {
            if !p.is_empty() {
                cfg.provider = Some(p);
            }
        }
        if let Ok(k) = std::env::var(KEY_ENV) {
            if !k.is_empty() {
                cfg.api_key = Some(SecretString::new(k));
            }
        }
        cfg
    }

    fn load_file(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;

        // A credentials file that other users can read is a finding, not a
        // detail. Say so loudly and keep going — refusing to start would be
        // worse for someone whose umask did this to them.
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                warn!(
                    ?path,
                    mode = format!("{mode:04o}"),
                    "credentials file is group/world readable; tightening to 0600"
                );
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }

        match serde_json::from_str(&raw) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                // Deliberately not logging the body — it holds the key.
                warn!(?path, error = %e, "ignoring unparseable credentials file");
                None
            }
        }
    }

    /// Persist atomically with the key never visible to other users.
    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .ok();

        let path = config_path();
        // Same-directory temp file so the rename below is atomic, and so a
        // crash can't leave a half-written credentials file in place.
        let tmp = dir.join(format!(".credentials.{}.tmp", std::process::id()));

        let body = serde_json::to_vec_pretty(self).context("serializing credentials")?;
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                // 0600 at creation. A later chmod would leave a window in
                // which the key is readable by anyone who can see the file.
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&body).with_context(|| format!("writing {}", tmp.display()))?;
            f.sync_all().ok();
        }

        std::fs::rename(&tmp, &path).with_context(|| {
            let _ = std::fs::remove_file(&tmp);
            format!("installing {}", path.display())
        })?;
        Ok(())
    }

    /// Forget the selection and erase the stored key.
    pub fn clear() -> Result<()> {
        let path = config_path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// Whether this config can actually fetch: a provider is selected, and if
    /// it needs a key there is one.
    pub fn is_ready(&self) -> bool {
        let Some(id) = self.provider.as_deref() else { return false };
        let Some(info) = ticker_proto::provider::lookup(id) else { return false };
        !info.requires_key || self.api_key.as_ref().is_some_and(|k| !k.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests all move `$XDG_CONFIG_HOME`, which is process-wide, so
    /// they must not run concurrently. Every test takes this lock first.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Points the config at a scratch dir and holds the env lock until the
    /// end of the test.
    struct Scratch {
        _dir: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Scratch {
        fn new() -> Self {
            // A test that panics poisons the lock; the env still needs
            // resetting for everyone else, so recover rather than cascade.
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::TempDir::new().unwrap();
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            std::env::remove_var(KEY_ENV);
            std::env::remove_var(PROVIDER_ENV);
            Self { _dir: dir, _guard: guard }
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let _s = Scratch::new();
        let cfg = Config {
            provider: Some("alphavantage".into()),
            api_key: Some(SecretString::new("abc123")),
        };
        cfg.save().unwrap();

        let back = Config::load();
        assert_eq!(back.provider.as_deref(), Some("alphavantage"));
        assert_eq!(back.api_key.unwrap().expose_secret(), "abc123");
    }

    #[test]
    fn saved_file_and_dir_are_owner_only() {
        let _s = Scratch::new();
        Config {
            provider: Some("alphavantage".into()),
            api_key: Some(SecretString::new("abc123")),
        }
        .save()
        .unwrap();

        let file_mode = std::fs::metadata(config_path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "credentials file mode {file_mode:04o}");

        let dir_mode = std::fs::metadata(config_dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "config dir mode {dir_mode:04o}");
    }

    #[test]
    fn no_temp_file_survives_a_save() {
        let _s = Scratch::new();
        Config { provider: Some("alphavantage".into()), ..Default::default() }.save().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(config_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    }

    #[test]
    fn a_loose_credentials_file_gets_tightened_on_load() {
        let _s = Scratch::new();
        Config {
            provider: Some("alphavantage".into()),
            api_key: Some(SecretString::new("abc123")),
        }
        .save()
        .unwrap();
        std::fs::set_permissions(config_path(), std::fs::Permissions::from_mode(0o644)).unwrap();

        let _ = Config::load();

        let mode = std::fs::metadata(config_path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "load() left it at {mode:04o}");
    }

    #[test]
    fn env_overrides_the_stored_values() {
        let _s = Scratch::new();
        Config {
            provider: Some("stored-provider".into()),
            api_key: Some(SecretString::new("stored")),
        }
        .save()
        .unwrap();

        std::env::set_var(PROVIDER_ENV, "alphavantage");
        std::env::set_var(KEY_ENV, "from-env");
        let cfg = Config::load();
        std::env::remove_var(PROVIDER_ENV);
        std::env::remove_var(KEY_ENV);

        assert_eq!(cfg.provider.as_deref(), Some("alphavantage"));
        assert_eq!(cfg.api_key.unwrap().expose_secret(), "from-env");
    }

    #[test]
    fn missing_config_is_not_ready_and_not_an_error() {
        let _s = Scratch::new();
        let cfg = Config::load();
        assert!(cfg.provider.is_none());
        assert!(!cfg.is_ready());
    }

    #[test]
    fn readiness_requires_a_key_only_where_the_catalog_says_so() {
        let missing = Config { provider: Some("alphavantage".into()), ..Default::default() };
        assert!(!missing.is_ready(), "alphavantage needs a key");

        let empty = Config {
            provider: Some("alphavantage".into()),
            api_key: Some(SecretString::new("")),
        };
        assert!(!empty.is_ready(), "an empty key is not a key");
    }

    #[test]
    fn an_unrecognized_provider_is_never_ready() {
        // A typo in the config file or in TICKER_FOLLOW_PROVIDER must not
        // look fetchable, whatever key happens to sit next to it.
        let stale = Config {
            provider: Some("not-a-provider".into()),
            api_key: Some(SecretString::new("abc123")),
        };
        assert!(!stale.is_ready(), "an unknown provider must not report ready");
    }

    #[test]
    fn clear_erases_the_file() {
        let _s = Scratch::new();
        Config {
            provider: Some("alphavantage".into()),
            api_key: Some(SecretString::new("abc123")),
        }
        .save()
        .unwrap();
        assert!(config_path().exists());

        Config::clear().unwrap();
        assert!(!config_path().exists());
        // Clearing again is not an error.
        Config::clear().unwrap();
    }
}
