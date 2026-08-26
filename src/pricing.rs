//! Pricing / cost computation (DESIGN.md §9).
//!
//! Source of truth is LiteLLM's public model-pricing dataset (the same dataset
//! ccusage uses). Fetched on startup, cached to `./cache/pricing.json`, refreshed
//! daily; falls back to cache offline.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::config::PriceOverride;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const REFRESH_AFTER: Duration = Duration::from_secs(24 * 3600);

/// Per-token prices for one model.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Pricing {
    table: HashMap<String, ModelPrice>,
    overrides: HashMap<String, ModelPrice>,
}

impl Pricing {
    pub fn len(&self) -> usize {
        self.table.len()
    }

    /// Load pricing: use cache if fresh, else fetch and rewrite cache, else fall
    /// back to a stale cache if the network is unavailable.
    pub async fn load(overrides: &HashMap<String, PriceOverride>) -> Result<Pricing> {
        let raw = load_raw_json().await?;
        let table = parse_litellm(&raw);
        let overrides = overrides
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    ModelPrice {
                        input: v.input,
                        output: v.output,
                        cache_read: v.cache_read,
                        cache_write: v.cache_write,
                    },
                )
            })
            .collect();
        Ok(Pricing { table, overrides })
    }

    /// Look up a model's price, honoring overrides and a few fuzzy fallbacks.
    pub fn lookup(&self, model: &str) -> Option<ModelPrice> {
        if let Some(p) = self.overrides.get(model) {
            return Some(*p);
        }
        if let Some(p) = self.table.get(model) {
            return Some(*p);
        }
        // Try stripping a provider prefix like "anthropic/" or "openrouter/".
        if let Some((_, rest)) = model.split_once('/') {
            if let Some(p) = self.table.get(rest) {
                return Some(*p);
            }
        }
        // No loose/fuzzy matching: guessing a model's price is worse than
        // reporting zero. Unknown → None; the caller warns so the user can add a
        // `pricing_overrides` entry (DESIGN.md §9).
        None
    }

    /// Compute cost for an event's token breakdown.
    pub fn cost(
        &self,
        model: Option<&str>,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
        reasoning: i64,
    ) -> (f64, bool) {
        let Some(model) = model else {
            return (0.0, false);
        };
        let Some(p) = self.lookup(model) else {
            return (0.0, false); // unknown model → 0 cost (warned by caller)
        };
        let cost = input as f64 * p.input
            + output as f64 * p.output
            + cache_read as f64 * p.cache_read
            + cache_write as f64 * p.cache_write
            + reasoning as f64 * p.output; // reasoning billed at output rate
        (cost, true)
    }
}

async fn load_raw_json() -> Result<String> {
    let cache_path = crate::paths::pricing_cache();
    let cache = cache_path.as_path();
    // Use fresh cache if young enough.
    if let Ok(meta) = std::fs::metadata(cache) {
        if let Ok(modified) = meta.modified() {
            if SystemTime::now()
                .duration_since(modified)
                .map(|age| age < REFRESH_AFTER)
                .unwrap_or(false)
            {
                if let Ok(txt) = std::fs::read_to_string(cache) {
                    return Ok(txt);
                }
            }
        }
    }
    // Otherwise fetch.
    match fetch_remote().await {
        Ok(txt) => {
            if let Some(parent) = cache.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(cache, &txt).ok();
            Ok(txt)
        }
        Err(e) => {
            // Fall back to a stale cache if present.
            if let Ok(txt) = std::fs::read_to_string(cache) {
                tracing::warn!("pricing fetch failed ({e}); using stale cache");
                Ok(txt)
            } else {
                Err(e).context("no pricing available (fetch failed and no cache)")
            }
        }
    }
}

async fn fetch_remote() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let resp = client.get(LITELLM_URL).send().await?.error_for_status()?;
    Ok(resp.text().await?)
}

fn parse_litellm(raw: &str) -> HashMap<String, ModelPrice> {
    let mut out = HashMap::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return out;
    };
    let Some(map) = v.as_object() else {
        return out;
    };
    for (name, entry) in map {
        if name == "sample_spec" {
            continue;
        }
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let g = |k: &str| obj.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let price = ModelPrice {
            input: g("input_cost_per_token"),
            output: g("output_cost_per_token"),
            cache_read: g("cache_read_input_token_cost"),
            cache_write: g("cache_creation_input_token_cost"),
        };
        out.insert(name.clone(), price);
    }
    out
}
