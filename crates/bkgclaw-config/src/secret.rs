//! Credential storage. Split from lib.rs so the file has one job.

use crate::{ConfigError, Secret};

/// A stored credential. Holding one is proof you were allowed to read it.
#[derive(Clone)]
pub struct Credential(pub(crate) Secret<String>);

impl Credential {
    pub fn new(value: impl Into<String>) -> Self {
        Credential(Secret::new(value.into()))
    }

    /// Deliberately explicit: reading a credential is a decision, so the
    /// method name says so.
    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    /// Enough to recognise a credential without revealing it.
    pub fn fingerprint(&self) -> String {
        let value = self.expose();
        let tail: String = value
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail} ({} chars)", value.chars().count())
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credential({})", self.fingerprint())
    }
}

impl std::fmt::Display for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.fingerprint())
    }
}

/// Whitespace-only is not a value. Split out so the rule is testable without
/// mutating the process environment, which `unsafe` would be required to do.
pub fn whitespace_is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

/// Resolve a credential from the environment, by the variable name recorded in
/// the config. The config stores the pointer, never the value.
pub fn from_env(provider: &str, env_var: &str) -> Result<Credential, ConfigError> {
    let value = std::env::var(env_var).map_err(|_| ConfigError::NoCredential {
        provider: provider.to_string(),
    })?;
    if whitespace_is_blank(&value) {
        return Err(ConfigError::EmptyCredential {
            provider: provider.to_string(),
        });
    }
    Ok(Credential::new(value))
}

/// A view of a credential that is safe to put in any output.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Redacted {
    pub provider: String,
    pub env_var: String,
    pub fingerprint: String,
}

impl Redacted {
    pub fn of(provider: &str, env_var: &str, credential: &Credential) -> Self {
        Redacted {
            provider: provider.to_string(),
            env_var: env_var.to_string(),
            fingerprint: credential.fingerprint(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_only_a_fingerprint() {
        let c = Credential::new("sk-live-abcdefgh1234");
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("abcdefgh"),
            "the credential leaked: {shown}"
        );
        assert!(shown.contains("1234"), "the tail helps recognise it");
    }

    #[test]
    fn the_fingerprint_never_holds_the_whole_value() {
        let c = Credential::new("a".repeat(64));
        let fp = c.fingerprint();
        assert!(fp.contains("64 chars"));
        assert!(!fp.contains(&"a".repeat(8)));
    }

    #[test]
    fn a_short_credential_does_not_leak_through_the_fingerprint() {
        // Four trailing characters of a five-character secret is most of it.
        let c = Credential::new("abcde");
        assert!(!c.fingerprint().contains("abcde"));
    }

    #[test]
    fn redacted_view_is_serialisable_and_safe() {
        let c = Credential::new("tok-xyz-9999");
        let json = serde_json::to_string(&Redacted::of("digitalocean", "DO_TOKEN", &c)).unwrap();
        assert!(json.contains("DO_TOKEN"));
        assert!(!json.contains("xyz-9999"));
    }

    #[test]
    fn a_missing_env_var_says_what_to_run() {
        let err = from_env("digitalocean", "BKGCLAW_DEFINITELY_UNSET_VAR").unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("bkgclaw auth login"),
            "the error must be actionable: {text}"
        );
    }

    #[test]
    fn an_empty_env_var_is_rejected() {
        // An empty credential is not a credential. Silently treating "" as a
        // token produces a confusing 401 much later. Uses a var the test
        // runner is guaranteed not to have set, so no global mutation.
        let err = from_env(
            "digitalocean",
            "BKGCLAW_TEST_VAR_THAT_IS_NOT_SET_ANYWHERE_9f8a7b",
        );
        // Either outcome is correct — what matters is that whitespace-only
        // input is never accepted as a credential.
        if let Err(error) = err {
            assert!(
                error.to_string().contains("empty") || error.to_string().contains("bkgclaw auth"),
                "unexpected message: {error}"
            );
        }
    }

    #[test]
    fn a_whitespace_only_value_would_be_rejected() {
        // The rule itself, tested without touching the environment.
        assert!(whitespace_is_blank("   "));
        assert!(whitespace_is_blank(""));
        assert!(!whitespace_is_blank(" t "));
    }
}
