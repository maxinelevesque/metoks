//! Normalized domain types shared across collectors, store, forecaster and API.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A source of usage.
pub const SERVICE_CLAUDE_CODE: &str = "claude_code";
pub const SERVICE_CODEX: &str = "codex";
pub const SERVICE_OPENROUTER: &str = "openrouter";

/// How a service is billed. `subscription` → dollar figures are API-equivalent
/// estimates; `pay_per_token` → real spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeteredMode {
    Subscription,
    PayPerToken,
}

impl MeteredMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            MeteredMode::Subscription => "subscription",
            MeteredMode::PayPerToken => "pay_per_token",
        }
    }
}

/// One normalized usage record. Append-only; deduped on `event_uid`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event_uid: String,
    pub service: String,
    pub metered_mode: MeteredMode,
    pub ts: DateTime<Utc>,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub cost_usd: f64,
    pub cost_is_estimate: bool,
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub raw_source: Option<String>,
}

/// The native unit a service's weekly window is tracked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    Tokens,
    Usd,
}

impl Unit {
    pub fn as_str(&self) -> &'static str {
        match self {
            Unit::Tokens => "tokens",
            Unit::Usd => "usd",
        }
    }
    pub fn parse(s: &str) -> Option<Unit> {
        match s {
            "tokens" => Some(Unit::Tokens),
            "usd" => Some(Unit::Usd),
            _ => None,
        }
    }
}

/// A forecast for one service+window, matching DESIGN.md §11.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forecast {
    pub service: String,
    pub window_kind: String,
    pub unit: Unit,
    pub consumed: f64,
    pub limit: Option<f64>,
    pub limit_source: Option<String>,
    pub pct_now: Option<f64>,
    pub projected: f64,
    pub pct_projected: Option<f64>,
    pub status: String,
    pub eta_to_limit: Option<DateTime<Utc>>,
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub forecast_model: String,
    /// True when too little of the window has elapsed for a trustworthy projection
    /// (e.g. just after a reset). The UI should de-emphasize `projected`.
    pub low_confidence: bool,
}
