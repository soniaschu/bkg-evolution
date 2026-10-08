//! The provider registry.
//!
//! Adding a provider is one impl plus one line here. The CLI, `doctor` and
//! `providers` learn about it automatically, because none of them enumerate
//! providers — they ask the registry.

use async_trait::async_trait;
use bkgclaw_config::{Config, from_env};
use bkgclaw_core::{Instance, Provider, ProviderError, Registry, Spec};

/// A provider backed by a real HTTP API, described but not yet wired to HTTP.
/// The point is that the *shape* is correct: `doctor` can already validate its
/// credential requirements without a single network call.
struct DigitalOcean;

#[async_trait]
impl Provider for DigitalOcean {
    fn name(&self) -> &'static str {
        "digitalocean"
    }
    fn required_env(&self) -> &'static [&'static str] {
        &["DIGITALOCEAN_TOKEN"]
    }

    async fn create(&self, _spec: &Spec) -> Result<Instance, ProviderError> {
        Err(ProviderError::Unreachable {
            provider: "digitalocean".into(),
            message: "HTTP transport not wired yet".into(),
        })
    }
    async fn list(&self) -> Result<Vec<Instance>, ProviderError> {
        Err(ProviderError::Unreachable {
            provider: "digitalocean".into(),
            message: "HTTP transport not wired yet".into(),
        })
    }
    async fn snapshot(&self, _id: &str, _label: &str) -> Result<String, ProviderError> {
        Err(ProviderError::Unreachable {
            provider: "digitalocean".into(),
            message: "HTTP transport not wired yet".into(),
        })
    }
    async fn restore(&self, _id: &str) -> Result<Instance, ProviderError> {
        Err(ProviderError::Unreachable {
            provider: "digitalocean".into(),
            message: "HTTP transport not wired yet".into(),
        })
    }
    async fn destroy(&self, _id: &str) -> Result<(), ProviderError> {
        Err(ProviderError::Unreachable {
            provider: "digitalocean".into(),
            message: "HTTP transport not wired yet".into(),
        })
    }
}

/// Local, offline provider. Always works, needs no credential, and is what the
/// test suite and the local demo use.
struct Local;

#[async_trait]
#[async_trait::async_trait]
impl Provider for Local {
    fn name(&self) -> &'static str {
        "local"
    }
    fn needs_network(&self) -> bool {
        false
    }

    async fn create(&self, spec: &Spec) -> Result<Instance, ProviderError> {
        let config = Config::load().unwrap_or_default();
        if config.settings.configured.iter().any(|p| p == "local") {
            // Reading a credential is the deliberate act it should be: the
            // value is fetched here and nowhere else.
            let _credential = from_env("local", "BKGCLAW_LOCAL_TOKEN");
        }
        // Reading the credential is the point where a value becomes usable.
        if config.settings.configured.iter().any(|p| p == "local") {
            let _ = from_env("local", "BKGCLAW_LOCAL_TOKEN");
        }
        Ok(Instance {
            id: format!("local-{}", spec.region),
            name: spec.name.clone(),
            external_id: "local-0".into(),
            address: Some("127.0.0.1".into()),
            state: bkgclaw_core::InstanceState::Running,
            region: spec.region.clone(),
        })
    }

    async fn list(&self) -> Result<Vec<Instance>, ProviderError> {
        Ok(vec![Instance {
            id: "local-1".into(),
            name: "bkgclaw-local".into(),
            external_id: "local-1".into(),
            address: Some("127.0.0.1".into()),
            state: bkgclaw_core::InstanceState::Running,
            region: "local".into(),
        }])
    }

    async fn snapshot(&self, id: &str, label: &str) -> Result<String, ProviderError> {
        Ok(format!("{id}-{label}"))
    }

    async fn restore(&self, snapshot_id: &str) -> Result<Instance, ProviderError> {
        Ok(Instance {
            id: "local-restored".into(),
            name: "restored".into(),
            external_id: snapshot_id.to_string(),
            address: Some("127.0.0.1".into()),
            state: bkgclaw_core::InstanceState::Pending,
            region: "local".into(),
        })
    }

    async fn destroy(&self, _id: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

pub fn registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(Box::new(Local));
    registry.register(Box::new(DigitalOcean));
    registry
}
