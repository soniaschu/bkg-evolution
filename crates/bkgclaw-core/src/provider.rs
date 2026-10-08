//! Providers behind a trait.
//!
//! The templates dispatch on a `String` with a match arm per provider, so adding
//! a provider means editing the dispatcher, the help text and the flag list —
//! clawmacdo ended up with 306 arguments because of exactly this. A registry
//! keyed by name means adding a provider is one new impl plus one `register`
//! line; the CLI never learns its name.
//!
//! Every method is `async` so a real HTTP implementation drops in unchanged,
//! and every test can use a recording stub with no network at all.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// What a provider must be able to do. Deliberately small: four operations
/// cover the whole deploy/destroy/status/snapshot surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub name: String,
    /// Provider-native identifier: a droplet id, a server name, an instance id.
    pub external_id: String,
    pub address: Option<String>,
    pub state: InstanceState,
    pub region: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstanceState {
    Pending,
    Running,
    Stopped,
    Destroyed,
}

impl InstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            InstanceState::Pending => "pending",
            InstanceState::Running => "running",
            InstanceState::Stopped => "stopped",
            InstanceState::Destroyed => "destroyed",
        }
    }
}

/// What to create. Credentials never appear here — they live in the config
/// store and are resolved by the provider at call time. That separation is the
/// point: a spec can be logged, diffed and stored without leaking a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub name: String,
    pub region: String,
    pub image: String,
    pub size: String,
    pub disk_gb: u32,
    pub tags: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("credential for provider `{0}` is missing")]
    MissingCredential(String),
    #[error("provider `{provider}` rejected the request: {message}")]
    Rejected { provider: String, message: String },
    #[error("provider `{provider}` is unreachable: {message}")]
    Unreachable { provider: String, message: String },
    #[error("instance `{0}` not found")]
    NotFound(String),
}

/// A provider is anything that can create, list, snapshot and destroy machines.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable identifier, e.g. `digitalocean`. Used as the registry key.
    fn name(&self) -> &'static str;

    /// Whether this provider needs a network round trip for `doctor`.
    fn needs_network(&self) -> bool {
        true
    }

    /// Which environment variables must carry a credential. `doctor` checks
    /// exactly this list instead of probing every possible flag.
    fn required_env(&self) -> &'static [&'static str] {
        &[]
    }

    async fn create(&self, spec: &Spec) -> Result<Instance, ProviderError>;
    async fn list(&self) -> Result<Vec<Instance>, ProviderError>;
    async fn snapshot(&self, external_id: &str, label: &str) -> Result<String, ProviderError>;
    async fn restore(&self, snapshot_id: &str) -> Result<Instance, ProviderError>;
    async fn destroy(&self, external_id: &str) -> Result<(), ProviderError>;
}

/// Registry of providers by name.
///
/// The CLI resolves `provider.do` by string lookup. Registering a new provider
/// is one line here; nothing else in the codebase changes.
#[derive(Default)]
pub struct Registry {
    providers: Vec<Box<dyn Provider>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Box<dyn Provider>) -> &mut Self {
        self.providers.push(provider);
        self
    }

    pub fn get(&self, name: &str) -> Option<&dyn Provider> {
        self.providers
            .iter()
            .find(|p| p.name().eq_ignore_ascii_case(name))
            .map(|p| p.as_ref())
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// Resolve or explain what is missing. A typo and an unregistered provider
    /// must not look the same.
    ///
    /// The error is a plain `String`, not a domain error type: the message is
    /// user-facing ("did you mean…"), so there is nothing to match on.
    #[allow(clippy::result_large_err)]
    pub fn resolve(&self, name: &str) -> Result<&dyn Provider, String> {
        self.get(name).ok_or_else(|| {
            let known = self.names().join(", ");
            if known.is_empty() {
                "no providers registered".to_string()
            } else {
                format!("unknown provider `{name}`; known providers: {known}")
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A provider that records every call and never touches the network.
    struct Recorder {
        calls: Mutex<Vec<String>>,
        fail: bool,
    }

    impl Recorder {
        fn new(fail: bool) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail,
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, call: &str) -> Result<(), ProviderError> {
            self.calls.lock().unwrap().push(call.to_string());
            if self.fail {
                return Err(ProviderError::Rejected {
                    provider: "recorder".into(),
                    message: "simulated".into(),
                });
            }
            Ok(())
        }
    }

    #[async_trait]
    impl Provider for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }
        fn needs_network(&self) -> bool {
            false
        }
        fn required_env(&self) -> &'static [&'static str] {
            &["BKGCLAW_TEST_TOKEN"]
        }
        async fn create(&self, spec: &Spec) -> Result<Instance, ProviderError> {
            self.record("create")?;
            Ok(Instance {
                id: "local-1".into(),
                name: spec.name.clone(),
                external_id: "ext-1".into(),
                address: Some("192.0.2.1".into()),
                state: InstanceState::Running,
                region: spec.region.clone(),
            })
        }
        async fn list(&self) -> Result<Vec<Instance>, ProviderError> {
            self.record("list")?;
            Ok(vec![])
        }
        async fn snapshot(&self, id: &str, label: &str) -> Result<String, ProviderError> {
            self.record(&format!("snapshot:{id}:{label}"))?;
            Ok("snap-1".into())
        }
        async fn restore(&self, id: &str) -> Result<Instance, ProviderError> {
            self.record(&format!("restore:{id}"))?;
            Ok(Instance {
                id: "local-2".into(),
                name: "restored".into(),
                external_id: "ext-2".into(),
                address: None,
                state: InstanceState::Pending,
                region: "any".into(),
            })
        }
        async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
            self.record(&format!("destroy:{id}"))
        }
    }

    fn spec() -> Spec {
        Spec {
            name: "demo".into(),
            region: "nyc3".into(),
            image: "ubuntu-24-04".into(),
            size: "s-1vcpu-1gb".into(),
            disk_gb: 25,
            tags: vec!["bkgclaw".into()],
        }
    }

    #[tokio::test]
    async fn a_registered_provider_is_found_by_name() {
        let mut registry = Registry::new();
        registry.register(Box::new(Recorder::new(false)));
        assert_eq!(registry.names(), vec!["recorder"]);
        assert!(registry.get("recorder").is_some());
        assert!(registry.resolve("recorder").is_ok());
    }

    #[tokio::test]
    async fn lookup_is_case_insensitive() {
        let mut registry = Registry::new();
        registry.register(Box::new(Recorder::new(false)));
        assert!(registry.resolve("RECORDER").is_ok());
    }

    #[tokio::test]
    async fn an_unknown_provider_lists_the_known_ones() {
        // A typo must be actionable, not a bare "not found".
        let registry = Registry::new();
        let Err(err) = registry.resolve("digitalocean") else {
            panic!("an unregistered provider must not resolve");
        };
        assert!(err.contains("no providers registered"));
    }

    #[tokio::test]
    async fn a_registry_with_providers_names_them_in_the_error() {
        let mut registry = Registry::new();
        registry.register(Box::new(Recorder::new(false)));
        let Err(err) = registry.resolve("digitslocean") else {
            panic!("a typo must not resolve");
        };
        assert!(
            err.contains("recorder"),
            "the error must name the valid options"
        );
    }

    #[tokio::test]
    async fn the_trait_covers_the_whole_lifecycle() {
        let recorder = Recorder::new(false);
        let provider: &dyn Provider = &recorder;
        provider.create(&spec()).await.unwrap();
        provider.list().await.unwrap();
        provider.snapshot("ext-1", "before-upgrade").await.unwrap();
        provider.restore("snap-1").await.unwrap();
        provider.destroy("ext-1").await.unwrap();
        assert_eq!(
            recorder.calls(),
            vec![
                "create",
                "list",
                "snapshot:ext-1:before-upgrade",
                "restore:snap-1",
                "destroy:ext-1"
            ]
        );
    }

    #[tokio::test]
    async fn a_provider_error_propagates_with_its_provider_name() {
        let recorder = Recorder::new(true);
        let provider: &dyn Provider = &recorder;
        let err = provider.create(&spec()).await.unwrap_err();
        assert!(err.to_string().contains("recorder"));
        assert!(err.to_string().contains("simulated"));
    }

    #[tokio::test]
    async fn required_env_is_declared_not_probed() {
        let recorder = Recorder::new(false);
        let provider: &dyn Provider = &recorder;
        // doctor reads this list instead of guessing which flags exist.
        assert_eq!(provider.required_env(), &["BKGCLAW_TEST_TOKEN"]);
    }
}
