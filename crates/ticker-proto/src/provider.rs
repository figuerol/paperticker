//! The catalog of price data providers.
//!
//! Metadata only — which providers exist and what they need. The daemon holds
//! the implementations; clients need this list to render the choices, so it
//! lives with the wire contract.
//!
//! **A provider belongs here only if its terms permit what this tool actually
//! does:** automated fetches on a schedule, and keeping the closes in a local
//! cache. That is the entry requirement, not a preference — there is no
//! "unofficial" tier and no way for an operator to opt into one.

use serde::{Deserialize, Serialize};

/// What a provider needs and what using it commits you to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderInfo {
    /// Stable identifier used on the wire and in the config file.
    pub id: String,
    /// Name to show a human.
    pub label: String,
    /// Whether an API key must be configured before it can fetch.
    pub requires_key: bool,
    /// Where to get a key, for the providers that need one.
    pub signup_url: Option<String>,
    /// One line on coverage and free-tier shape.
    pub description: String,
}

/// Every provider the daemon can be configured to use.
pub fn catalog() -> Vec<ProviderInfo> {
    vec![ProviderInfo {
        id: "alphavantage".into(),
        label: "Alpha Vantage".into(),
        requires_key: true,
        signup_url: Some("https://www.alphavantage.co/support/#api-key".into()),
        description: "Documented daily time-series API. Free tier is \
                      rate-limited but ample for a cached daily refresh, \
                      and permits the cache. Non-commercial use only."
            .into(),
    }]
}

/// Look up one provider by its wire id.
pub fn lookup(id: &str) -> Option<ProviderInfo> {
    catalog().into_iter().find(|p| p.id == id)
}

/// What the daemon currently has configured. Deliberately carries no key —
/// only whether one is on file. A configured credential never travels back
/// out of the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStatus {
    /// `None` until the operator chooses one; the daemon fetches nothing
    /// until then.
    pub selected: Option<ProviderInfo>,
    /// Why `selected` is `None` despite a provider being named on disk or
    /// in the environment — a typo, usually. Without it, `provider status`
    /// would report "none configured" to someone whose config does name
    /// one, which is the opposite of a useful answer.
    #[serde(default)]
    pub unavailable: Option<String>,
    /// Whether a key is stored for the selected provider.
    pub key_configured: bool,
    /// Whether the selected provider is ready to fetch.
    pub ready: bool,
    /// Everything that could be selected.
    pub available: Vec<ProviderInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_are_unique() {
        let mut ids: Vec<_> = catalog().into_iter().map(|p| p.id).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate provider id in the catalog");
    }

    #[test]
    fn key_requiring_providers_say_where_to_get_one() {
        for p in catalog() {
            if p.requires_key {
                assert!(p.signup_url.is_some(), "{} needs a key but no signup_url", p.id);
            }
        }
    }

    #[test]
    fn lookup_matches_the_catalog() {
        assert_eq!(lookup("alphavantage").unwrap().id, "alphavantage");
        assert!(lookup("nope").is_none());
    }
}
