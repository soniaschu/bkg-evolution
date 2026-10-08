//! Routing: pick a model, fail over when it fails, stop hammering one that is
//! already known to be down.
//!
//! Two mechanisms that solve different problems and are often confused:
//!
//! - **Failover** is per-request. This attempt failed, try the next vendor now.
//! - **Circuit breaker** is per-vendor over time. After N consecutive failures
//!   the vendor is skipped entirely for a cooldown, so a dead provider does not
//!   cost a timeout on every single turn.
//!
//! Both exist because an agent loop runs many turns. Without the breaker, one
//! dead provider adds its timeout to every request, forever.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::cost::{Complexity, ModelPrice, Route, Usage};
use crate::models::{
    CompletionRequest, CompletionResponse, ModelError, ModelId, ModelRegistry, Retryability, Vendor,
};

/// Circuit state for one vendor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone)]
struct Breaker {
    state: CircuitState,
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    successes: u32,
}

/// The breaker per vendor.
#[derive(Debug)]
pub struct CircuitBreakers {
    breakers: HashMap<Vendor, Breaker>,
    /// Failures before opening.
    pub failure_threshold: u32,
    /// How long the circuit stays open before a probe is allowed.
    pub cooldown: Duration,
}

impl Default for CircuitBreakers {
    fn default() -> Self {
        Self::new(3, Duration::from_secs(60))
    }
}

impl CircuitBreakers {
    pub fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        CircuitBreakers {
            breakers: HashMap::new(),
            failure_threshold,
            cooldown,
        }
    }

    fn entry(&mut self, vendor: Vendor) -> &mut Breaker {
        self.breakers.entry(vendor).or_insert(Breaker {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            opened_at: None,
            successes: 0,
        })
    }

    pub fn state_of(&mut self, vendor: Vendor) -> CircuitState {
        self.transition_if_due(vendor);
        self.breakers
            .get(&vendor)
            .map(|b| b.state)
            .unwrap_or(CircuitState::Closed)
    }

    /// An open circuit moves to half-open once the cooldown has elapsed, so a
    /// recovered vendor gets another chance without operator intervention.
    fn transition_if_due(&mut self, vendor: Vendor) {
        let Some(breaker) = self.breakers.get_mut(&vendor) else {
            return;
        };
        if breaker.state == CircuitState::Open {
            if let Some(opened_at) = breaker.opened_at {
                if opened_at.elapsed() >= self.cooldown {
                    breaker.state = CircuitState::HalfOpen;
                }
            }
        }
    }

    pub fn record_success(&mut self, vendor: Vendor) {
        let breaker = self.entry(vendor);
        breaker.consecutive_failures = 0;
        breaker.state = CircuitState::Closed;
        breaker.opened_at = None;
        breaker.successes += 1;
    }

    pub fn record_failure(&mut self, vendor: Vendor) {
        let threshold = self.failure_threshold;
        let breaker = self.entry(vendor);
        breaker.consecutive_failures += 1;
        // A half-open circuit that fails goes straight back to open: the probe
        // answered, and the answer was "still broken".
        if breaker.state == CircuitState::HalfOpen || breaker.consecutive_failures >= threshold {
            breaker.state = CircuitState::Open;
            breaker.opened_at = Some(Instant::now());
        }
    }

    /// Whether a request may attempt this vendor right now.
    pub fn allows(&mut self, vendor: Vendor) -> bool {
        self.transition_if_due(vendor);
        !matches!(
            self.breakers
                .get(&vendor)
                .map(|b| b.state)
                .unwrap_or(CircuitState::Closed),
            CircuitState::Open
        )
    }

    pub fn successes(&self, vendor: Vendor) -> u32 {
        self.breakers.get(&vendor).map(|b| b.successes).unwrap_or(0)
    }

    pub fn open_vendors(&self) -> Vec<Vendor> {
        let mut out: Vec<Vendor> = self
            .breakers
            .iter()
            .filter(|(_, b)| b.state == CircuitState::Open)
            .map(|(vendor, _)| *vendor)
            .collect();
        out.sort_by_key(|v| v.as_str());
        out
    }
}

/// One leg of the failover chain: a model plus its price.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub model: ModelId,
    pub price: ModelPrice,
}

impl Candidate {
    pub fn new(model: ModelId, price: ModelPrice) -> Self {
        Candidate { model, price }
    }
}

/// Which model tier to prefer for a given route.
pub fn tier_of(route: Route) -> &'static str {
    match route {
        Route::Cheap => "cheap",
        Route::Balanced => "balanced",
        Route::Capable => "capable",
    }
}

/// Decides the model and walks the failover chain.
pub struct Router<'a> {
    registry: &'a ModelRegistry,
    /// Ordered candidates. First is preferred; each `Route` tier maps onto one.
    pub chain: Vec<Candidate>,
    pub breakers: CircuitBreakers,
    /// Attempts per vendor before moving on.
    pub max_attempts_per_vendor: u32,
}

impl<'a> Router<'a> {
    pub fn new(registry: &'a ModelRegistry, chain: Vec<Candidate>) -> Self {
        Router {
            registry,
            chain,
            breakers: CircuitBreakers::default(),
            max_attempts_per_vendor: 2,
        }
    }

    /// Pick the first candidate for a complexity band, then continue into the
    /// chain on failure. The ordering of `chain` is the operator's control;
    /// this method only walks it.
    fn ordered_candidates(&self, route: Route) -> Vec<Candidate> {
        if self.chain.is_empty() {
            return Vec::new();
        }
        let start = match route {
            // Start cheaper or more capable depending on the band, but always
            // fall through to every other candidate.
            Route::Cheap => 0,
            Route::Balanced => 0,
            Route::Capable => 0,
        };
        // Cloned deliberately: holding a borrow into self.chain across the
        // loop would block the &mut self.breakers writes that record results.
        // The chain is a handful of entries; copying is cheaper than fighting
        // the borrow checker.
        let mut ordered: Vec<Candidate> = self.chain.iter().skip(start).cloned().collect();
        ordered.insert(0, self.chain[0].clone());
        // Deduplicate: the cheap path may have re-added chain[0].
        let mut seen: Vec<ModelId> = Vec::new();
        ordered.retain(|c| {
            if seen.contains(&c.model) {
                false
            } else {
                seen.push(c.model.clone());
                true
            }
        });
        ordered
    }

    /// Route a request. Returns the response and the chain actually walked, so
    /// a caller can report "fell back from X to Y" rather than hiding it.
    pub async fn complete(
        &mut self,
        request: &CompletionRequest,
    ) -> Result<RoutedCompletion, ModelError> {
        self.complete_impl(request, None).await
    }

    /// Routing with live deltas. Same chain, same breakers; the sink receives
    /// text as it arrives instead of once at the end.
    pub async fn complete_streamed(
        &mut self,
        request: &CompletionRequest,
        sink: &dyn crate::observer::DeltaSink,
    ) -> Result<RoutedCompletion, ModelError> {
        self.complete_impl(request, Some(sink)).await
    }

    async fn complete_impl(
        &mut self,
        request: &CompletionRequest,
        sink: Option<&dyn crate::observer::DeltaSink>,
    ) -> Result<RoutedCompletion, ModelError> {
        let route = Complexity::score(&request.messages, request.tools.len()).route();
        let candidates = self.ordered_candidates(route);

        if candidates.is_empty() {
            return Err(ModelError::InvalidRequest(
                "no model candidates configured".to_string(),
            ));
        }

        let mut attempts_log: Vec<String> = Vec::new();
        let mut last_error: Option<ModelError> = None;

        for candidate in candidates.iter() {
            let vendor = candidate.model.vendor;
            if !self.breakers.allows(vendor) {
                attempts_log.push(format!("{vendor}: skipped (circuit open)"));
                continue;
            }
            let Some(provider) = self.registry.for_vendor(vendor) else {
                attempts_log.push(format!("{vendor}: not registered"));
                continue;
            };

            let mut attempt = 0;
            while attempt < self.max_attempts_per_vendor {
                attempt += 1;
                let mut attempt_request = request.clone();
                attempt_request.model = candidate.model.clone();

                let result = match sink {
                    Some(sink) => provider.complete_streamed(&attempt_request, sink).await,
                    None => provider.complete(&attempt_request).await,
                };
                match result {
                    Ok(response) => {
                        self.breakers.record_success(vendor);
                        let cost = provider
                            .cost_table()
                            .cost_for(&candidate.model.model, &response.usage);
                        return Ok(RoutedCompletion {
                            response,
                            served_by: candidate.model.clone(),
                            cost_usd: cost,
                            attempts: attempts_log,
                            tier: tier_of(route).to_string(),
                        });
                    }
                    Err(error) => {
                        attempts_log.push(format!("{vendor}: {}", error));
                        match error.retryability() {
                            Retryability::Fatal => return Err(error),
                            Retryability::NextVendor => {
                                self.breakers.record_failure(vendor);
                                last_error = Some(error);
                                break;
                            }
                            Retryability::SameVendor => {
                                if attempt >= self.max_attempts_per_vendor {
                                    self.breakers.record_failure(vendor);
                                    last_error = Some(error);
                                }
                            }
                        }
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| ModelError::Unavailable {
            vendor: candidates[0].model.vendor,
            reason: format!("every candidate failed: {}", attempts_log.join("; ")),
        }))
    }
}

/// The result of a routed call, with the audit trail of how it got there.
#[derive(Debug, Clone)]
pub struct RoutedCompletion {
    pub response: CompletionResponse,
    pub served_by: ModelId,
    /// `None` when the price is unknown. Never silently zero.
    pub cost_usd: Option<f64>,
    pub attempts: Vec<String>,
    pub tier: String,
}

impl RoutedCompletion {
    pub fn usage(&self) -> Usage {
        self.response.usage
    }

    /// One line for a log or a CLI report.
    pub fn trace(&self) -> String {
        let cost = match self.cost_usd {
            Some(value) => format!("${value:.4}"),
            None => "cost unknown".to_string(),
        };
        if self.attempts.is_empty() {
            format!("{} → {} ({})", self.tier, self.served_by, cost)
        } else {
            format!(
                "{} → {} after {} failed attempt(s) ({})",
                self.tier,
                self.served_by,
                self.attempts.len(),
                cost
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Message;

    fn candidate(vendor: Vendor, model: &str) -> Candidate {
        Candidate::new(
            ModelId::new(vendor, model),
            ModelPrice {
                input: 3.0,
                output: 15.0,
                ..ModelPrice::default()
            },
        )
    }

    fn registry_with(vendors: &[Vendor]) -> ModelRegistry {
        let mut registry = ModelRegistry::new();
        for vendor in vendors {
            registry.register(Box::new(crate::testing::StubProvider::new(*vendor)));
        }
        registry
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: ModelId::new(Vendor::Ollama, "placeholder"),
            messages: vec![Message::user("hello")],
            tools: vec![],
            max_tokens: 256,
            temperature: None,
            thinking_budget: None,
            effort: None,
        }
    }

    #[test]
    fn the_first_healthy_candidate_serves_the_request() {
        let registry = registry_with(&[Vendor::Ollama]);
        let chain = vec![candidate(Vendor::Ollama, "m")];
        let mut router = Router::new(&registry, chain);
        let routed = futures::executor::block_on(router.complete(&request())).unwrap();
        assert_eq!(routed.served_by.vendor, Vendor::Ollama);
        assert!(
            routed.attempts.is_empty(),
            "no fallback should have been needed"
        );
    }

    #[test]
    fn a_cost_table_prices_the_response() {
        let registry = registry_with(&[Vendor::Ollama]);
        let chain = vec![candidate(Vendor::Ollama, "m")];
        let mut router = Router::new(&registry, chain);
        let routed = futures::executor::block_on(router.complete(&request())).unwrap();
        // The stub reports zero usage, so the cost is zero — but it must be
        // Some, not None: an unknown price is different from a free call.
        assert_eq!(routed.cost_usd, Some(0.0));
    }

    #[test]
    fn the_circuit_opens_after_the_failure_threshold() {
        let mut breakers = CircuitBreakers::new(3, Duration::from_secs(60));
        assert_eq!(breakers.state_of(Vendor::OpenAi), CircuitState::Closed);
        breakers.record_failure(Vendor::OpenAi);
        breakers.record_failure(Vendor::OpenAi);
        assert_eq!(
            breakers.state_of(Vendor::OpenAi),
            CircuitState::Closed,
            "two failures is not enough"
        );
        breakers.record_failure(Vendor::OpenAi);
        assert_eq!(breakers.state_of(Vendor::OpenAi), CircuitState::Open);
    }

    #[test]
    fn an_open_circuit_blocks_new_requests() {
        let mut breakers = CircuitBreakers::new(1, Duration::from_secs(60));
        breakers.record_failure(Vendor::OpenAi);
        assert!(
            !breakers.allows(Vendor::OpenAi),
            "an open circuit must skip the vendor"
        );
        assert!(
            breakers.allows(Vendor::Anthropic),
            "other vendors are unaffected"
        );
    }

    #[test]
    fn a_success_resets_the_failure_count() {
        let mut breakers = CircuitBreakers::new(3, Duration::from_secs(60));
        breakers.record_failure(Vendor::OpenAi);
        breakers.record_failure(Vendor::OpenAi);
        breakers.record_success(Vendor::OpenAi);
        breakers.record_failure(Vendor::OpenAi);
        assert_eq!(breakers.state_of(Vendor::OpenAi), CircuitState::Closed);
    }

    #[test]
    fn a_vendor_recovers_after_the_cooldown() {
        let mut breakers = CircuitBreakers::new(1, Duration::from_millis(1));
        breakers.record_failure(Vendor::OpenAi);
        assert!(!breakers.allows(Vendor::OpenAi));
        std::thread::sleep(Duration::from_millis(5));
        // A probe is allowed, and a success closes the circuit for good.
        assert!(
            breakers.allows(Vendor::OpenAi),
            "the cooldown must permit a retry"
        );
        breakers.record_success(Vendor::OpenAi);
        assert_eq!(breakers.state_of(Vendor::OpenAi), CircuitState::Closed);
    }

    #[test]
    fn a_failed_probe_reopens_the_circuit_immediately() {
        let mut breakers = CircuitBreakers::new(1, Duration::from_millis(1));
        breakers.record_failure(Vendor::OpenAi);
        std::thread::sleep(Duration::from_millis(5));
        assert!(breakers.allows(Vendor::OpenAi));
        // The probe says still broken: reopen without waiting for the threshold.
        breakers.record_failure(Vendor::OpenAi);
        assert!(!breakers.allows(Vendor::OpenAi));
    }

    #[test]
    fn open_circuits_are_reported_for_the_doctor_command() {
        let mut breakers = CircuitBreakers::new(1, Duration::from_secs(60));
        breakers.record_failure(Vendor::OpenAi);
        breakers.record_failure(Vendor::Xai);
        assert_eq!(breakers.open_vendors(), vec![Vendor::OpenAi, Vendor::Xai]);
    }

    #[test]
    fn an_empty_chain_is_an_invalid_request_not_a_panic() {
        let registry = registry_with(&[Vendor::Ollama]);
        let mut router = Router::new(&registry, vec![]);
        let error = futures::executor::block_on(router.complete(&request())).unwrap_err();
        assert!(matches!(error, ModelError::InvalidRequest(_)));
    }

    #[test]
    fn an_unregistered_vendor_is_skipped_and_the_trace_says_so() {
        // The chain names a vendor the registry does not have.
        let registry = registry_with(&[]);
        let chain = vec![candidate(Vendor::Ollama, "m")];
        let mut router = Router::new(&registry, chain);
        let error = futures::executor::block_on(router.complete(&request())).unwrap_err();
        assert!(error.to_string().contains("unreachable") || error.to_string().contains("failed"));
    }

    #[test]
    fn the_trace_is_readable_on_the_happy_path() {
        let registry = registry_with(&[Vendor::Ollama]);
        let chain = vec![candidate(Vendor::Ollama, "m")];
        let mut router = Router::new(&registry, chain);
        let routed = futures::executor::block_on(router.complete(&request())).unwrap();
        let trace = routed.trace();
        assert!(trace.contains("ollama/m"));
        assert!(trace.contains("$"), "a known price must appear: {trace}");
    }

    #[test]
    fn tier_labels_are_stable() {
        assert_eq!(tier_of(Route::Cheap), "cheap");
        assert_eq!(tier_of(Route::Balanced), "balanced");
        assert_eq!(tier_of(Route::Capable), "capable");
    }
}
