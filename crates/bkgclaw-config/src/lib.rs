//! Configuration and credentials.
//!
//! The rule this crate enforces: **a credential can be written, but it cannot
//! be read back out.** Every accessor that returns a `Credential` is marked
//! `secret`, and every serialising path runs through `Redacted` so a token
//! cannot leak into a log line, an error message, a `--json` payload or a
//! support bundle.
//!
//! That is not decoration. The templates this replaces print config freely, and
//! `serde_json` will happily serialise a `String` field that holds a token.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

pub mod secret;
pub use secret::{Credential, Redacted, from_env};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no config at {path}")]
    Missing { path: String },
    #[error("config at {path} is malformed: {reason}")]
    Malformed { path: String, reason: String },
    #[error("no credential stored for `{provider}`; run `bkgclaw auth login {provider}`")]
    NoCredential { provider: String },
    #[error("credential for `{provider}` is empty")]
    EmptyCredential { provider: String },
}

/// Non-secret settings. Anything here is safe to print in full.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Which provider `bkgclaw` uses when none is named.
    pub default_provider: Option<String>,
    /// Providers that have been configured at least once.
    pub configured: Vec<String>,
    /// Extra tags applied to every created instance.
    pub default_tags: Vec<String>,
    /// Never print a credential, even in verbose mode. Redundant by design:
    /// redaction is not optional, and this flag documents that to the reader.
    pub redact_always: bool,
}

/// The on-disk config file. Credentials live elsewhere, in the secret store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub settings: Settings,
    /// Which env var holds each provider's credential. The value never appears
    /// here — only the pointer to it.
    pub credential_env: BTreeMap<String, String>,
}

impl Config {
    /// Read the config. A missing file is an empty config, not an error —
    /// first run must work without setup.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&Self::default_path())
    }

    pub fn load_from(path: &std::path::Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Malformed {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        toml::from_str(&text).map_err(|e| ConfigError::Malformed {
            path: path.display().to_string(),
            reason: e.to_string(),
        })
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::default_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::Malformed {
                path: parent.display().to_string(),
                reason: e.to_string(),
            })?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| ConfigError::Malformed {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        std::fs::write(path, text).map_err(|e| ConfigError::Malformed {
            path: path.display().to_string(),
            reason: e.to_string(),
        })
    }

    pub fn default_path() -> std::path::PathBuf {
        std::env::var_os("BKGCLAW_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default();
                home.join(".config").join("bkgclaw").join("config.toml")
            })
            .join("config.toml")
    }

    /// Which env var holds a provider's credential, if configured.
    pub fn env_var_for(&self, provider: &str) -> Option<&str> {
        self.credential_env.get(provider).map(String::as_str)
    }
}

/// A `Secret<T>` that cannot be printed, logged or serialised by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// The only way to get the value out. Every call site is a decision point.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

// The Debug impl is the whole point: it must never reveal the value.
impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

// Serialising yields a marker, never the value.
impl<T> Serialize for Secret<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_reveals_the_value() {
        let s = Secret::new("sk-live-DO-NOT-LEAK".to_string());
        let debug = format!("{s:?}");
        assert!(
            !debug.contains("DO-NOT-LEAK"),
            "Debug leaked the secret: {debug}"
        );
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn display_never_reveals_the_value() {
        let s = Secret::new("tok-abc".to_string());
        assert_eq!(s.to_string(), "<redacted>");
    }

    #[test]
    fn serialization_never_reveals_the_value() {
        #[derive(Serialize)]
        struct Holder {
            token: Secret<String>,
        }
        let json = serde_json::to_string(&Holder {
            token: Secret::new("super-secret".into()),
        })
        .unwrap();
        assert!(
            !json.contains("super-secret"),
            "serde leaked the secret: {json}"
        );
        assert!(json.contains("redacted"));
    }

    #[test]
    fn expose_is_the_only_way_out() {
        let s = Secret::new(42);
        assert_eq!(*s.expose(), 42);
    }

    #[test]
    fn a_config_round_trips() {
        let mut config = Config::default();
        config.settings.default_provider = Some("digitalocean".into());
        config
            .credential_env
            .insert("digitalocean".into(), "DO_TOKEN".into());
        let text = toml::to_string(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_ignored() {
        // A typo in a config key must fail loudly; silently ignoring it leaves
        // the user with a setting that does nothing.
        let result = toml::from_str::<Config>("[settings]\ndefualt_provider = \"x\"\n");
        assert!(result.is_err(), "a misspelled key must not be accepted");
    }
}
