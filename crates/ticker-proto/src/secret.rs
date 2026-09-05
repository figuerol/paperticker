//! A string that doesn't print itself.
//!
//! API keys travel over the socket inside [`Request`](crate::Request), which
//! derives `Debug` — one `warn!(?req)` on an error path would otherwise put a
//! live credential in the daemon's logs. Wrapping the key makes that
//! impossible by construction rather than by remembering not to log.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroize;

/// A secret string. `Debug` and `Display` render `[redacted]`; the value comes
/// out only through [`expose_secret`](SecretString::expose_secret), which is
/// deliberately ugly to read and easy to grep for in review.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The plaintext. Every call site is a place a credential can escape —
    /// keep them few, and never pass the result somewhere that formats it.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString([redacted])")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        // Best effort. A `String` that has been reallocated may have left
        // copies of earlier contents elsewhere on the heap; this clears the
        // buffer we still own, which is the copy most likely to be read back.
        self.0.zeroize();
    }
}

impl Serialize for SecretString {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // Serialized in the clear: the wire is a 0600 socket on the loopback
        // of the filesystem, and the daemon needs the actual key. This impl
        // exists so that *only* an explicit serialize can emit it — the
        // derived `Debug` above cannot.
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(SecretString)
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

#[cfg(test)]
mod tests {
    use super::SecretString;

    const KEY: &str = "sk-live-do-not-log-me";

    #[test]
    fn debug_does_not_leak() {
        let s = SecretString::new(KEY);
        assert_eq!(format!("{s:?}"), "SecretString([redacted])");
        assert!(!format!("{s:?}").contains(KEY));
    }

    #[test]
    fn display_does_not_leak() {
        let s = SecretString::new(KEY);
        assert_eq!(s.to_string(), "[redacted]");
        assert!(!s.to_string().contains(KEY));
    }

    #[test]
    fn debug_of_a_containing_struct_does_not_leak() {
        // The case that actually matters: `warn!(?req)` on a Request that
        // carries a key.
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Wrapper {
            provider: &'static str,
            api_key: Option<SecretString>,
        }
        let w = Wrapper {
            provider: "alphavantage",
            api_key: Some(SecretString::new(KEY)),
        };
        assert!(!format!("{w:?}").contains(KEY), "leaked: {w:?}");
    }

    #[test]
    fn round_trips_over_the_wire() {
        let s = SecretString::new(KEY);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, format!("\"{KEY}\""));
        let back: SecretString = serde_json::from_str(&json).unwrap();
        assert_eq!(back.expose_secret(), KEY);
    }
}
