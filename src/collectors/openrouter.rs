//! OpenRouter poller (DESIGN.md §5.3) — pay_per_token, real cost.
//!
//! OpenRouter exposes cumulative totals, not a per-event feed. We poll on an
//! interval, store each response in `cumulative_snapshots`, and diff consecutive
//! `total_usage` values to synthesize spend deltas → derived events
//! (model unknown → NULL). The first snapshot only establishes a baseline.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::db::{self, DbPool};
use crate::models::{Event, MeteredMode, Unit};

const BASE: &str = "https://openrouter.ai/api/v1";

/// Parsed `/credits` response.
#[derive(Debug, Clone, Copy)]
pub struct Credits {
    pub total_credits: f64,
    pub total_usage: f64,
}

fn parse_credits(body: &serde_json::Value) -> Option<Credits> {
    let d = body.get("data")?;
    Some(Credits {
        total_credits: d.get("total_credits").and_then(|x| x.as_f64()).unwrap_or(0.0),
        total_usage: d.get("total_usage").and_then(|x| x.as_f64())?,
    })
}

/// Pure delta logic: given the previous cumulative usage (if any) and the current
/// value, produce a derived spend event when spend increased. Kept separate from
/// I/O so it can be unit-tested (DESIGN.md §15).
pub fn derive_delta_event(prev: Option<f64>, curr: f64, ts: DateTime<Utc>) -> Option<Event> {
    let prev = prev?; // first snapshot: baseline only, no event
    let delta = curr - prev;
    if delta <= 0.0 {
        return None;
    }
    let mut h = Sha256::new();
    h.update(crate::models::SERVICE_OPENROUTER.as_bytes());
    h.update(b"|");
    h.update(ts.to_rfc3339().as_bytes());
    h.update(b"|");
    h.update(curr.to_le_bytes());
    let uid = format!("{:x}", h.finalize());
    Some(Event {
        event_uid: uid,
        service: crate::models::SERVICE_OPENROUTER.to_string(),
        metered_mode: MeteredMode::PayPerToken,
        ts,
        model: None,
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cost_usd: delta,
        cost_is_estimate: false,
        session_id: None,
        project: None,
        raw_source: Some(format!("{BASE}/credits")),
    })
}

/// Poll `/credits` once: store a snapshot, diff, and insert a derived event.
/// Returns the derived event if spend increased. `api_key` is read from env by
/// the caller and never persisted.
pub async fn poll_once(
    pool: &DbPool,
    client: &reqwest::Client,
    api_key: &str,
) -> Result<Option<Event>> {
    let resp = client
        .get(format!("{BASE}/credits"))
        .bearer_auth(api_key)
        .send()
        .await
        .context("GET /credits")?
        .error_for_status()?;
    let body: serde_json::Value = resp.json().await.context("parsing /credits json")?;
    let credits = parse_credits(&body).context("unexpected /credits shape")?;
    let ts = Utc::now();

    // Store snapshot with the raw body (contains no secrets) and get prev usage.
    let conn = pool.get()?;
    let prev = db::insert_snapshot(
        &conn,
        crate::models::SERVICE_OPENROUTER,
        ts,
        credits.total_usage,
        Some(credits.total_credits),
        Unit::Usd,
        &body.to_string(),
    )?;
    drop(conn);

    let event = derive_delta_event(prev, credits.total_usage, ts);
    if let Some(ev) = &event {
        db::insert_events(pool, std::slice::from_ref(ev))?;
    }
    Ok(event)
}

pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_snapshot_is_baseline_only() {
        let ev = derive_delta_event(None, 10.0, Utc::now());
        assert!(ev.is_none());
    }

    #[test]
    fn positive_delta_emits_event_with_cost() {
        let ev = derive_delta_event(Some(10.0), 12.5, Utc::now()).unwrap();
        assert!((ev.cost_usd - 2.5).abs() < 1e-9);
        assert_eq!(ev.metered_mode, MeteredMode::PayPerToken);
        assert!(!ev.cost_is_estimate);
        assert_eq!(ev.service, crate::models::SERVICE_OPENROUTER);
    }

    #[test]
    fn zero_or_negative_delta_emits_nothing() {
        assert!(derive_delta_event(Some(10.0), 10.0, Utc::now()).is_none());
        assert!(derive_delta_event(Some(10.0), 9.0, Utc::now()).is_none());
    }

    #[test]
    fn parse_credits_shape() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"data":{"total_credits":40.0,"total_usage":3.5}}"#).unwrap();
        let c = parse_credits(&v).unwrap();
        assert_eq!(c.total_credits, 40.0);
        assert_eq!(c.total_usage, 3.5);
    }
}
