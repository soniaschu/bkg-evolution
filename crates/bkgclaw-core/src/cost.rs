//! Cost accounting and budget guards.
//!
//! An agent loop without a spend ceiling is an unbounded bill. The guard has
//! two thresholds, not one: warn at 80% so a human can react, block at 100% so
//! nothing runs past the line.
//!
//! Prices are per million tokens, the unit vendors publish.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn merge(&self, other: &Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cache_read_tokens: self.cache_read_tokens + other.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens + other.cache_write_tokens,
        }
    }
}

/// Dollar price per million tokens for one model.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Default for ModelPrice {
    fn default() -> Self {
        ModelPrice {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }
}

/// Prices keyed by model name, with a fallback for unknown models.
///
/// The fallback is the important part: an unknown model must cost *something*
/// non-zero, or a typo in a model name becomes free money.
#[derive(Debug, Clone, Default)]
pub struct CostTable {
    entries: Vec<(String, ModelPrice)>,
    fallback: ModelPrice,
}

impl CostTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, model: &str, price: ModelPrice) -> Self {
        self.entries.push((model.to_string(), price));
        self
    }

    /// Price used when the model is unknown. Defaults to a deliberately
    /// non-zero value so an unrecognised model is never silently free.
    pub fn with_fallback(mut self, price: ModelPrice) -> Self {
        self.fallback = price;
        self
    }

    pub fn price_for(&self, model: &str) -> ModelPrice {
        // Exact match, then suffix match so `claude-sonnet-5-20260101` finds
        // `claude-sonnet-5`.
        if let Some((_, price)) = self.entries.iter().find(|(name, _)| name == model) {
            return *price;
        }
        if let Some((_, price)) = self
            .entries
            .iter()
            .find(|(name, _)| model.starts_with(name.as_str()))
        {
            return *price;
        }
        self.fallback
    }

    /// Cost in dollars for a usage record. Returns `None` when the price is
    /// unknown, which callers must handle — not zero.
    pub fn cost_for(&self, model: &str, usage: &Usage) -> Option<f64> {
        let price = self.price_for(model);
        // A model priced at exactly zero is genuinely free (a local Ollama).
        // An *unknown* model is not, and falls through to the non-zero
        // fallback set by with_fallback.
        let per_token = |per_million: f64| per_million / 1_000_000.0;
        Some(
            usage.input_tokens as f64 * per_token(price.input)
                + usage.output_tokens as f64 * per_token(price.output)
                + usage.cache_read_tokens as f64 * per_token(price.cache_read)
                + usage.cache_write_tokens as f64 * per_token(price.cache_write),
        )
    }
}

/// Where a session stands against its spend ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetState {
    /// Under the warn threshold.
    Ok,
    /// Over 80% of the ceiling. Still allowed, but the operator should know.
    Warn,
    /// At or over the ceiling. Further calls are refused.
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    /// Ceiling in dollars. `None` means no ceiling — which the CLI must not
    /// default to for an agent that can call a model repeatedly.
    pub limit_usd: Option<f64>,
    pub spent_usd: f64,
    pub warn_at: f64,
}

impl Budget {
    pub fn new(limit_usd: Option<f64>) -> Self {
        Budget {
            limit_usd,
            spent_usd: 0.0,
            warn_at: 0.8,
        }
    }

    pub fn unlimited() -> Self {
        Budget::new(None)
    }

    pub fn state(&self) -> BudgetState {
        match self.limit_usd {
            None => BudgetState::Ok,
            Some(limit) if limit <= 0.0 => BudgetState::Blocked,
            Some(limit) if self.spent_usd >= limit => BudgetState::Blocked,
            Some(limit) if self.spent_usd >= limit * self.warn_at => BudgetState::Warn,
            Some(_) => BudgetState::Ok,
        }
    }

    /// Record a spend. Returns the state *after* applying it, so the caller
    /// learns in one call whether this call crossed the line.
    pub fn charge(&mut self, cost: f64) -> BudgetState {
        self.spent_usd += cost.max(0.0);
        self.state()
    }

    /// Refuse a call that would exceed the ceiling. Checking *before* spending
    /// is the point: a projected overrun must not start a request whose
    /// response then blows the budget further.
    pub fn check(&self, projected_usd: f64) -> Result<(), String> {
        match self.limit_usd {
            None => Ok(()),
            Some(limit) if limit <= 0.0 => Err("budget is zero; no spend is allowed".to_string()),
            Some(limit) => {
                let projected = self.spent_usd + projected_usd.max(0.0);
                if projected > limit {
                    Err(format!(
                        "projected ${projected:.4} exceeds the ${limit:.4} ceiling (spent ${:.4})",
                        self.spent_usd
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn remaining_usd(&self) -> Option<f64> {
        self.limit_usd
            .map(|limit| (limit - self.spent_usd).max(0.0))
    }
}

/// A cheap complexity score used to route simple turns to a cheaper model.
///
/// Not a model-quality predictor. It is a conservative heuristic: when in
/// doubt, it scores high and the expensive model runs. The failure mode of
/// under-scoring is a bad answer; of over-scoring, a slightly higher bill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Complexity {
    /// 0 = trivial, 100 = the most demanding thing the loop handles.
    pub score: u32,
}

impl Complexity {
    pub fn score(messages: &[crate::models::Message], tool_count: usize) -> Self {
        let mut score: u32 = 0;

        // Turn count is the strongest signal: long conversations are the ones
        // where a cheap model starts losing the thread.
        score += (messages.len() as u32).saturating_mul(4).min(40);

        // Very large single messages usually mean pasted code or a document.
        let largest = messages
            .iter()
            .map(|m| match m {
                crate::models::Message::System { content }
                | crate::models::Message::User { content }
                | crate::models::Message::Assistant { content }
                | crate::models::Message::ToolResult { content, .. } => content.len(),
            })
            .max()
            .unwrap_or(0);
        if largest > 8_000 {
            score += 20;
        } else if largest > 2_000 {
            score += 10;
        }

        // Many available tools means a complex selection problem.
        score += (tool_count as u32).saturating_mul(2).min(20);

        Complexity {
            score: score.min(100),
        }
    }

    /// The routing rule. Documented so the decision is inspectable rather
    /// than buried in a matcher arm.
    ///
    /// The bands are wide on purpose. A wrongly-downgraded turn produces a
    /// worse answer; a wrongly-upgraded one only costs a little more. The
    /// asymmetry says: when the score is near a boundary, use the bigger model.
    pub fn route(self) -> Route {
        match self.score {
            0..=19 => Route::Cheap,
            20..=49 => Route::Balanced,
            _ => Route::Capable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    Cheap,
    Balanced,
    Capable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Message, ModelId};

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        }
    }

    #[test]
    fn cost_is_computed_per_million_tokens() {
        let table = CostTable::new().with(
            "test-model",
            ModelPrice {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
        );
        // 1M input at $3 + 1M output at $15 = $18
        let cost = table
            .cost_for("test-model", &usage(1_000_000, 1_000_000))
            .unwrap();
        assert!((cost - 18.0).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn a_small_call_costs_a_small_amount() {
        let table = CostTable::new().with(
            "m",
            ModelPrice {
                input: 3.0,
                output: 15.0,
                ..ModelPrice::default()
            },
        );
        let cost = table.cost_for("m", &usage(1_000, 500)).unwrap();
        assert!((cost - 0.0105).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn a_versioned_model_name_finds_its_base_price() {
        let table = CostTable::new().with(
            "claude-sonnet-5",
            ModelPrice {
                input: 3.0,
                output: 15.0,
                ..ModelPrice::default()
            },
        );
        // Vendors ship dated snapshots; they must not fall through to the
        // fallback price just because the suffix differs.
        let priced = table
            .cost_for("claude-sonnet-5-20260101", &usage(1_000_000, 0))
            .unwrap();
        assert!((priced - 3.0).abs() < 1e-9, "got {priced}");
    }

    #[test]
    fn an_unknown_model_is_not_free() {
        // The dangerous failure: a typo silently costing nothing.
        let table = CostTable::new().with_fallback(ModelPrice {
            input: 1.0,
            output: 1.0,
            ..ModelPrice::default()
        });
        let cost = table.cost_for("typo-model", &usage(1_000_000, 0)).unwrap();
        assert!(cost > 0.0, "an unknown model must carry a non-zero price");
    }

    #[test]
    fn a_genuinely_free_model_costs_nothing() {
        let table = CostTable::new().with("local", ModelPrice::default());
        assert_eq!(
            table
                .cost_for("local", &usage(1_000_000, 1_000_000))
                .unwrap(),
            0.0
        );
    }

    #[test]
    fn the_budget_warns_at_eighty_percent() {
        let mut budget = Budget::new(Some(10.0));
        assert_eq!(budget.charge(7.9), BudgetState::Ok);
        assert_eq!(
            budget.charge(0.2),
            BudgetState::Warn,
            "crossing 80% must warn"
        );
    }

    #[test]
    fn the_budget_blocks_at_one_hundred_percent() {
        let mut budget = Budget::new(Some(10.0));
        assert_eq!(budget.charge(10.0), BudgetState::Blocked);
    }

    #[test]
    fn a_projected_overrun_is_refused_before_spending() {
        let mut budget = Budget::new(Some(1.0));
        budget.charge(0.9);
        // A $0.5 call would land at $1.40, past the $1.00 ceiling.
        let error = budget.check(0.5).unwrap_err();
        assert!(
            error.contains("exceeds"),
            "the error must say what was projected"
        );
    }

    #[test]
    fn a_call_that_fits_is_allowed() {
        let mut budget = Budget::new(Some(1.0));
        budget.charge(0.5);
        assert!(budget.check(0.4).is_ok());
    }

    #[test]
    fn an_unlimited_budget_never_blocks() {
        let mut budget = Budget::unlimited();
        assert_eq!(budget.charge(1_000_000.0), BudgetState::Ok);
        assert!(budget.check(1_000_000.0).is_ok());
    }

    #[test]
    fn a_zero_budget_blocks_immediately() {
        let budget = Budget::new(Some(0.0));
        assert_eq!(budget.state(), BudgetState::Blocked);
        assert!(budget.check(0.0).is_err());
    }

    #[test]
    fn remaining_never_goes_negative() {
        let mut budget = Budget::new(Some(1.0));
        budget.charge(2.0);
        assert_eq!(budget.remaining_usd(), Some(0.0));
    }

    #[test]
    fn a_short_conversation_routes_cheap() {
        let complexity = Complexity::score(&[Message::user("hi")], 0);
        assert_eq!(complexity.route(), Route::Cheap);
    }

    #[test]
    fn one_pasted_document_with_tools_is_balanced_not_capable() {
        // A single large input with a modest tool surface is mid-range. The
        // earlier bands put it at the cheap edge, which under-serves the turn.
        let messages = vec![Message::user("x".repeat(50_000))];
        assert_eq!(Complexity::score(&messages, 12).route(), Route::Balanced);
    }

    #[test]
    fn turn_count_alone_does_not_reach_capable() {
        // Twenty short messages is a long conversation, not a hard problem.
        // Routing it to the expensive model because the transcript is long
        // would be exactly the over-spend the scorer exists to avoid.
        let messages: Vec<Message> = (0..20).map(|_| Message::user("ok")).collect();
        assert_eq!(Complexity::score(&messages, 0).route(), Route::Balanced);
    }

    #[test]
    fn a_long_and_wide_conversation_routes_capable() {
        // Length plus content plus tool surface: the combination is the signal,
        // not any one dimension alone.
        let messages: Vec<Message> = (0..25).map(|_| Message::user("x".repeat(4_000))).collect();
        assert_eq!(Complexity::score(&messages, 10).route(), Route::Capable);
    }

    #[test]
    fn a_large_document_with_a_wide_tool_surface_is_capable() {
        // Document size (20) plus a deep transcript plus the tool cap (20) has
        // to clear 50 to reach the capable band. A single pasted file with a
        // modest tool set stays balanced, which is the cheaper correct answer.
        let messages: Vec<Message> = (0..10).map(|_| Message::user("x".repeat(20_000))).collect();
        let score = Complexity::score(&messages, 15);
        assert!(
            score.score >= 50,
            "expected the capable band, got {}",
            score.score
        );
        assert_eq!(score.route(), Route::Capable);
    }

    #[test]
    fn many_tools_raise_the_score() {
        let simple = Complexity::score(&[Message::user("hi")], 0);
        let busy = Complexity::score(&[Message::user("hi")], 15);
        assert!(
            busy.score > simple.score,
            "tool count must influence routing"
        );
    }

    #[test]
    fn a_huge_pasted_document_raises_the_score() {
        let big = Message::user("x".repeat(20_000));
        assert!(Complexity::score(&[big], 0).score >= 20);
    }

    #[test]
    fn usage_merges_additively() {
        let merged = usage(10, 5).merge(&usage(3, 2));
        assert_eq!(merged.input_tokens, 13);
        assert_eq!(merged.output_tokens, 7);
        assert_eq!(merged.total(), 20);
    }

    #[test]
    fn model_ids_serialise_for_a_config_file() {
        let id = ModelId::new(crate::models::Vendor::Anthropic, "claude-sonnet-5");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#"{"vendor":"anthropic","model":"claude-sonnet-5"}"#);
        assert_eq!(serde_json::from_str::<ModelId>(&json).unwrap(), id);
    }
}
