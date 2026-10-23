//! Fresh-input-equivalent weights for the stats report.
//!
//! The table lives in the machine-wide store config (`<store>/config.yaml`,
//! key `token_costs`) because `kanban stats` and the TUI Stats window have no
//! single project. Built-in rows are compiled in and apply when that key, or
//! the row being looked up, is absent. Fresh input is always weight 1. A
//! configured row overrides only the fields it sets.
//!
//! Lookup for one usage record, per field: exact model id, then provider,
//! then the user's `default` row, then the compiled provider row when the
//! user did not configure that provider, then the compiled `default` row.
//! Provider is the model id's prefix before the first slash, otherwise
//! inferred from the backend (`claude` → `anthropic`, `codex` → `openai`,
//! `grok` → `xai`). A negative or non-numeric value is ignored.

use std::collections::HashMap;

use serde_yaml_ng::{Mapping, Value};

use crate::core::telemetry::TokenBreakdown;

/// Weights that turn a token split into fresh-input-equivalent tokens.
/// Fresh input is always 1. `usd_per_mtok_input` is unset until a row
/// configures it; the report shows dollars only then.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostWeights {
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
    pub usd_per_mtok_input: Option<f64>,
}

impl CostWeights {
    pub fn equivalent(self, breakdown: &TokenBreakdown) -> f64 {
        breakdown.input as f64
            + breakdown.cache_read as f64 * self.cache_read
            + breakdown.cache_write as f64 * self.cache_write
            + breakdown.output as f64 * self.output
    }

    /// Dollars for `fresh_equivalent` tokens, when a price was configured.
    pub fn usd(self, fresh_equivalent: f64) -> Option<f64> {
        self.usd_per_mtok_input
            .map(|rate| fresh_equivalent * rate / 1_000_000.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Rate {
    CacheRead,
    CacheWrite,
    Output,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct PartialCost {
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    output: Option<f64>,
    usd_per_mtok_input: Option<f64>,
}

impl PartialCost {
    fn from_mapping(fields: &Mapping) -> Self {
        Self {
            cache_read: field(fields, "cache_read"),
            cache_write: field(fields, "cache_write"),
            output: field(fields, "output"),
            usd_per_mtok_input: field(fields, "usd_per_mtok_input"),
        }
    }

    fn rate(&self, rate: Rate) -> Option<f64> {
        match rate {
            Rate::CacheRead => self.cache_read,
            Rate::CacheWrite => self.cache_write,
            Rate::Output => self.output,
        }
    }
}

/// Parsed `token_costs` table. Missing keys use [`TokenCosts::builtin`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenCosts {
    user: HashMap<String, PartialCost>,
}

impl TokenCosts {
    pub fn builtin() -> Self {
        Self::default()
    }

    pub fn from_mapping(map: &Mapping) -> Self {
        let mut user = HashMap::new();
        for (key, value) in map {
            let Some(name) = key.as_str().map(str::trim).filter(|name| !name.is_empty()) else {
                continue;
            };
            let Some(fields) = value.as_mapping() else {
                continue;
            };
            user.insert(name.to_string(), PartialCost::from_mapping(fields));
        }
        Self { user }
    }

    /// Weights for one usage record. See the module comment for the order.
    pub fn weights_for(&self, backend: Option<&str>, model: Option<&str>) -> CostWeights {
        let provider = model_provider(model).or_else(|| infer_provider(backend));
        let model = model.map(str::trim).filter(|model| !model.is_empty());
        CostWeights {
            cache_read: self.rate(model, provider.as_deref(), Rate::CacheRead),
            cache_write: self.rate(model, provider.as_deref(), Rate::CacheWrite),
            output: self.rate(model, provider.as_deref(), Rate::Output),
            usd_per_mtok_input: self.usd_rate(model, provider.as_deref()),
        }
    }

    pub fn equivalent(&self, breakdown: &TokenBreakdown, weights: CostWeights) -> f64 {
        weights.equivalent(breakdown)
    }

    fn rate(&self, model: Option<&str>, provider: Option<&str>, rate: Rate) -> f64 {
        if let Some(model) = model
            && let Some(value) = self.field(model, |row| row.rate(rate))
        {
            return value;
        }
        if let Some(provider) = provider {
            if let Some(value) = self.field(provider, |row| row.rate(rate)) {
                return value;
            }
            if !self.has_entry(provider) && is_builtin_provider(provider) {
                return builtin_rate(provider, rate);
            }
        }
        if let Some(value) = self.field("default", |row| row.rate(rate)) {
            return value;
        }
        builtin_rate("default", rate)
    }

    fn usd_rate(&self, model: Option<&str>, provider: Option<&str>) -> Option<f64> {
        if let Some(model) = model
            && let Some(value) = self.field(model, |row| row.usd_per_mtok_input)
        {
            return Some(value);
        }
        if let Some(provider) = provider
            && let Some(value) = self.field(provider, |row| row.usd_per_mtok_input)
        {
            return Some(value);
        }
        self.field("default", |row| row.usd_per_mtok_input)
    }

    fn field(&self, key: &str, get: impl Fn(&PartialCost) -> Option<f64>) -> Option<f64> {
        self.user
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .and_then(|(_, row)| get(row))
    }

    fn has_entry(&self, key: &str) -> bool {
        self.user.keys().any(|name| name.eq_ignore_ascii_case(key))
    }
}

fn is_builtin_provider(name: &str) -> bool {
    matches!(name, "anthropic" | "openai")
}

fn builtin_rate(name: &str, rate: Rate) -> f64 {
    match (name, rate) {
        ("openai", Rate::CacheWrite) => 1.0,
        ("openai", Rate::Output) => 8.0,
        (_, Rate::CacheRead) => 0.1,
        (_, Rate::CacheWrite) => 1.25,
        (_, Rate::Output) => 5.0,
    }
}

fn model_provider(model: Option<&str>) -> Option<String> {
    let (provider, _) = model?.trim().split_once('/')?;
    (!provider.is_empty()).then(|| provider.to_ascii_lowercase())
}

fn infer_provider(backend: Option<&str>) -> Option<String> {
    match backend.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "claude" => Some("anthropic".to_string()),
        "grok" => Some("xai".to_string()),
        "codex" => Some("openai".to_string()),
        _ => None,
    }
}

fn field(map: &Mapping, name: &str) -> Option<f64> {
    map.get(Value::String(name.to_string()))
        .and_then(nonneg_f64)
}

fn nonneg_f64(value: &Value) -> Option<f64> {
    let parsed = match value {
        Value::Number(number) => number
            .as_f64()
            .or_else(|| number.as_i64().map(|value| value as f64))
            .or_else(|| number.as_u64().map(|value| value as f64)),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    parsed.filter(|value| value.is_finite() && *value >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> TokenCosts {
        let value: Value = serde_yaml_ng::from_str(yaml).unwrap();
        TokenCosts::from_mapping(value.as_mapping().unwrap())
    }

    #[test]
    fn builtin_rows_cover_anthropic_openai_and_the_rest() {
        let costs = TokenCosts::builtin();
        let anthropic = costs.weights_for(Some("claude"), Some("opus"));
        assert_eq!(
            (
                anthropic.cache_read,
                anthropic.cache_write,
                anthropic.output
            ),
            (0.1, 1.25, 5.0)
        );
        assert!(anthropic.usd_per_mtok_input.is_none());
        let openai = costs.weights_for(Some("codex"), None);
        assert_eq!(
            (openai.cache_read, openai.cache_write, openai.output),
            (0.1, 1.0, 8.0)
        );
        let other = costs.weights_for(Some("opencode"), Some("zai/glm"));
        assert_eq!(
            (other.cache_read, other.cache_write, other.output),
            (0.1, 1.25, 5.0)
        );
        let eq = costs.equivalent(
            &TokenBreakdown {
                input: 100,
                output: 10,
                cache_read: 1000,
                cache_write: 10,
            },
            anthropic,
        );
        assert!((eq - 262.5).abs() < 1e-9, "{eq}");
    }

    #[test]
    fn partial_rows_fall_back_per_field_and_lookup_order() {
        let costs = parse(
            r#"
            default:
              cache_write: 2.0
            anthropic:
              cache_read: 0.2
            "anthropic/opus":
              output: 3.0
              usd_per_mtok_input: 3.0
            openai:
              output: nope
              cache_read: -1
            "#,
        );
        let model = costs.weights_for(Some("claude"), Some("anthropic/opus"));
        assert!((model.output - 3.0).abs() < 1e-12);
        assert!((model.cache_read - 0.2).abs() < 1e-12);
        assert!((model.cache_write - 2.0).abs() < 1e-12);
        assert_eq!(model.usd_per_mtok_input, Some(3.0));

        let provider = costs.weights_for(None, Some("anthropic/other"));
        assert!((provider.cache_read - 0.2).abs() < 1e-12);
        assert!((provider.output - 5.0).abs() < 1e-12);
        assert!(provider.usd_per_mtok_input.is_none());

        // The model prefix wins over the backend inference.
        let prefixed = costs.weights_for(Some("claude"), Some("openai/gpt-5.5"));
        assert!((prefixed.cache_write - 2.0).abs() < 1e-12, "user default");
        assert!(
            (prefixed.output - 5.0).abs() < 1e-12,
            "invalid openai output"
        );

        // An absent openai row still uses the compiled openai output.
        let compiled = TokenCosts::builtin().weights_for(Some("codex"), Some("gpt"));
        assert!((compiled.output - 8.0).abs() < 1e-12);
    }
}
