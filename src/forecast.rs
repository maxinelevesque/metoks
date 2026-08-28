//! Forecasting engine (DESIGN.md §10–§11).
//!
//! Window resolution + two burn models: `linear` and `dow_weighted`. The pure
//! math functions take plain numbers so they can be unit-tested against
//! hand-computed values; `forecast_service` wires them to the DB.

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

use crate::config::Config;
use crate::db::{self, DbPool};
use crate::models::{Forecast, Unit};

pub const WINDOW_DAYS: i64 = 7;

/// The core numeric result of a forecast, independent of window/DB plumbing.
/// (Some fields are consumed only by unit tests / the rolling path; the main
/// forecaster reads `projected` and re-derives percentages from fiducials.)
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ForecastCore {
    pub projected: f64,
    pub pct_now: Option<f64>,
    pub pct_projected: Option<f64>,
    pub status: String,
    pub eta_to_limit: Option<DateTime<Utc>>,
    pub forecast_model: String,
}

fn status_for(pct_projected: Option<f64>, warn: f64, danger: f64) -> String {
    match pct_projected {
        None => "unknown".to_string(),
        Some(p) if p < warn => "green".to_string(),
        Some(p) if p < danger => "amber".to_string(),
        Some(_) => "red".to_string(),
    }
}

/// When would we hit `limit` at the current (linear) burn rate?
fn eta(consumed: f64, limit: Option<f64>, start: DateTime<Utc>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let limit = limit?;
    let elapsed = (now - start).num_seconds() as f64;
    if elapsed <= 0.0 || consumed <= 0.0 {
        return None;
    }
    let burn = consumed / elapsed; // per second
    if burn <= 0.0 {
        return None;
    }
    let secs_to_limit = limit / burn;
    start.checked_add_signed(Duration::seconds(secs_to_limit as i64))
}

/// Baseline linear burn-rate projection (DESIGN.md §11).
pub fn linear(
    consumed: f64,
    limit: Option<f64>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    now: DateTime<Utc>,
    warn: f64,
    danger: f64,
) -> ForecastCore {
    let elapsed = (now - start).num_seconds().max(0) as f64;
    let remaining = (end - now).num_seconds().max(0) as f64;
    let projected = if elapsed > 0.0 {
        let burn = consumed / elapsed;
        consumed + burn * remaining
    } else {
        consumed
    };
    let pct_now = limit.filter(|l| *l > 0.0).map(|l| consumed / l);
    let pct_projected = limit.filter(|l| *l > 0.0).map(|l| projected / l);
    ForecastCore {
        projected,
        pct_now,
        pct_projected,
        status: status_for(pct_projected, warn, danger),
        eta_to_limit: eta(consumed, limit, start, now),
        forecast_model: "linear".to_string(),
    }
}

/// A normalized (day-of-week, hour) activity grid summing to 1.0 over a week.
#[derive(Debug, Clone)]
pub struct DowGrid {
    /// weight[dow][hour], dow 0=Mon..6=Sun (config tz), summing to ~1.0
    pub weight: [[f64; 24]; 7],
    pub tz: Tz,
}

impl DowGrid {
    pub fn total(&self) -> f64 {
        self.weight.iter().flatten().sum()
    }

    /// Weight covered by the interval [a,b], apportioning partial hours. Iterates
    /// hour-by-hour (a 7-day window is ≤168 steps) so window alignment to
    /// calendar hours doesn't matter.
    pub fn weight_between(&self, a: DateTime<Utc>, b: DateTime<Utc>) -> f64 {
        if b <= a {
            return 0.0;
        }
        let mut acc = 0.0;
        let mut cur = a;
        while cur < b {
            let local = cur.with_timezone(&self.tz);
            // Start of this local hour, then next hour boundary.
            let hour_start_local = self
                .tz
                .with_ymd_and_hms(local.year(), local.month(), local.day(), local.hour(), 0, 0)
                .single()
                .unwrap_or(local);
            let hour_start = hour_start_local.with_timezone(&Utc);
            let next_hour = hour_start + Duration::hours(1);
            let seg_end = next_hour.min(b);
            let frac = (seg_end - cur).num_seconds() as f64 / 3600.0;
            let dow = local.weekday().num_days_from_monday() as usize; // 0=Mon
            let hour = local.hour() as usize;
            acc += self.weight[dow][hour] * frac;
            cur = seg_end;
        }
        acc
    }
}

/// Day-of-week + hour weighted projection (DESIGN.md §11). Superseded by the
/// trend model for live forecasts; retained as a building block and for tests.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub fn dow_weighted(
    consumed: f64,
    limit: Option<f64>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    now: DateTime<Utc>,
    grid: &DowGrid,
    warn: f64,
    danger: f64,
) -> ForecastCore {
    let total = grid.total();
    let elapsed_hours = (now - start).num_seconds() as f64 / 3600.0;
    // Cold-start guards → linear.
    if total <= 0.0 || elapsed_hours < 48.0 {
        return linear(consumed, limit, start, end, now, warn, danger);
    }
    let elapsed_w = grid.weight_between(start, now);
    let total_w = grid.weight_between(start, end);
    if total_w <= 0.0 {
        return linear(consumed, limit, start, end, now, warn, danger);
    }
    let expected_fraction_elapsed = elapsed_w / total_w;
    if expected_fraction_elapsed <= 1e-6 {
        return linear(consumed, limit, start, end, now, warn, danger);
    }
    let projected = consumed / expected_fraction_elapsed;
    let pct_now = limit.filter(|l| *l > 0.0).map(|l| consumed / l);
    let pct_projected = limit.filter(|l| *l > 0.0).map(|l| projected / l);
    ForecastCore {
        projected,
        pct_now,
        pct_projected,
        status: status_for(pct_projected, warn, danger),
        eta_to_limit: eta(consumed, limit, start, now),
        forecast_model: "dow_weighted".to_string(),
    }
}

/// Fixed 7-day window. If a reset time is known, the window ends there; otherwise
/// it's anchored to a deterministic weekly grid so `window_end` is always in the
/// future (required for a meaningful projection).
pub fn resolve_window(now: DateTime<Utc>, reset: Option<DateTime<Utc>>) -> (DateTime<Utc>, DateTime<Utc>) {
    let dur = Duration::days(WINDOW_DAYS);
    if let Some(r) = reset {
        // Roll forward if the stored reset is in the past.
        let mut end = r;
        while end <= now {
            end += dur;
        }
        return (end - dur, end);
    }
    // Deterministic weekly grid anchored at a fixed epoch.
    let anchor = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
    let elapsed = now - anchor;
    let idx = elapsed.num_seconds().div_euclid(dur.num_seconds());
    let start = anchor + Duration::seconds(idx * dur.num_seconds());
    (start, start + dur)
}

/// Native unit for a service's weekly window.
pub fn service_unit(cfg: &Config, service: &str) -> Unit {
    match service {
        crate::models::SERVICE_OPENROUTER => cfg
            .services
            .openrouter
            .weekly_budget
            .as_ref()
            .and_then(|l| l.unit_parsed().ok())
            .unwrap_or(Unit::Usd),
        crate::models::SERVICE_CODEX => cfg
            .services
            .codex
            .weekly_limit
            .as_ref()
            .and_then(|l| l.unit_parsed().ok())
            .unwrap_or(Unit::Tokens),
        _ => cfg
            .services
            .claude_code
            .weekly_limit
            .as_ref()
            .and_then(|l| l.unit_parsed().ok())
            .unwrap_or(Unit::Tokens),
    }
}

/// Build the DoW/hour grid from the last `weeks` of events for a service.
fn build_grid(
    conn: &rusqlite::Connection,
    service: &str,
    unit: Unit,
    tz: Tz,
    now: DateTime<Utc>,
    weeks: i64,
) -> Result<DowGrid> {
    let since = now - Duration::days(7 * weeks.max(1));
    let expr = match unit {
        Unit::Tokens => "input_tokens+output_tokens+cache_read_tokens+cache_write_tokens+reasoning_tokens",
        Unit::Usd => "cost_usd",
    };
    let sql = format!("SELECT ts, ({expr}) AS amt FROM events WHERE service=?1 AND ts>=?2");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params![service, since.to_rfc3339()],
        |r| {
            let ts: String = r.get(0)?;
            let amt: f64 = r.get(1)?;
            Ok((ts, amt))
        },
    )?;
    let mut weight = [[0.0f64; 24]; 7];
    for row in rows {
        let (ts_s, amt) = row?;
        if let Ok(ts) = DateTime::parse_from_rfc3339(&ts_s) {
            let local = ts.with_timezone(&tz);
            let dow = local.weekday().num_days_from_monday() as usize;
            let hour = local.hour() as usize;
            weight[dow][hour] += amt;
        }
    }
    // Normalize to sum 1.0 (if any activity).
    let total: f64 = weight.iter().flatten().sum();
    if total > 0.0 {
        for cell in weight.iter_mut().flatten() {
            *cell /= total;
        }
    }
    Ok(DowGrid { weight, tz })
}

/// The next reset time for a service's weekly window, if known: provider-real
/// (Codex rate_limit) first, then a stored limits-table reset. `None` → rolling.
fn resolve_reset(conn: &rusqlite::Connection, service: &str) -> Result<Option<DateTime<Utc>>> {
    if let Some(rl) = db::get_rate_limit(conn, service, "weekly")? {
        if rl.resets_at.is_some() {
            return Ok(rl.resets_at);
        }
    }
    if let Some(row) = db::get_limit(conn, service, "weekly")? {
        if row.window_reset.is_some() {
            return Ok(row.window_reset);
        }
    }
    Ok(None)
}

/// Resolve the token cap + its source for a service.
/// Precedence: provider-real percent (Codex) > limits table (anchored/configured)
/// > config file. `consumed` is used to derive a cap from a reported percent.
fn resolve_cap(
    conn: &rusqlite::Connection,
    cfg: &Config,
    service: &str,
    unit: Unit,
    consumed: f64,
) -> Result<(Option<f64>, Option<String>)> {
    // 1. Provider-reported percent (Codex) → derive a real cap so pct_now matches.
    if let Some(rl) = db::get_rate_limit(conn, service, "weekly")? {
        if let Some(pct) = rl.used_percent {
            if pct > 0.0 && consumed > 0.0 {
                return Ok((Some(consumed / (pct / 100.0)), Some("real".to_string())));
            }
        }
    }
    // 2. limits table (anchored / configured / real value set via API).
    if let Some(row) = db::get_limit(conn, service, "weekly")? {
        if row.limit_value.is_some() {
            return Ok((row.limit_value, Some(row.limit_source)));
        }
    }
    // 3. config file static limit.
    let cfg_limit = match service {
        crate::models::SERVICE_OPENROUTER => cfg.services.openrouter.weekly_budget.as_ref(),
        crate::models::SERVICE_CODEX => cfg.services.codex.weekly_limit.as_ref(),
        _ => cfg.services.claude_code.weekly_limit.as_ref(),
    };
    if let Some(l) = cfg_limit {
        if l.unit_parsed().ok() == Some(unit) {
            return Ok((Some(l.value), Some(l.source.clone())));
        }
    }
    Ok((None, None))
}

/// Recent daily burn (native unit per day) over the last `days`, for the rolling
/// sustained-pace projection.
fn recent_daily_burn(
    conn: &rusqlite::Connection,
    service: &str,
    unit: Unit,
    now: DateTime<Utc>,
    days: i64,
) -> Result<f64> {
    let start = now - Duration::days(days.max(1));
    let total = db::consumed_in_window(conn, service, unit, start, now)?;
    Ok(total / days.max(1) as f64)
}

// ---------------------------------------------------------------------------
// Trend model + projection cone
// ---------------------------------------------------------------------------

const PACE_LOOKBACK_DAYS: i64 = 21;
const PACE_HALF_LIFE_DAYS: f64 = 5.0;
const CONE_Z: f64 = 1.0; // ±1σ band
const CONE_STEPS: i64 = 32;

/// Band bounds relative to the level, so σ is never zero or absurd.
const SIGMA_MIN_FRAC: f64 = 0.10;
const SIGMA_MAX_FRAC: f64 = 0.60;

/// Estimate the weekly volume `P` (tokens/week) and its 1σ from recent daily
/// usage: a robust (winsorized, EWMA-weighted toward recent days) mean daily
/// volume × 7. The day-of-week grid is intentionally *not* used here — it shapes
/// the projection cone, but de-seasonalizing the level lets one outlier day
/// distort every other day's estimate (a spike inflates the whole grid, which
/// then divides normal days by a too-small share). Winsorizing daily totals
/// directly keeps a 10× spike from running the pace away.
fn estimate_pace(
    conn: &rusqlite::Connection,
    service: &str,
    unit: Unit,
    now: DateTime<Utc>,
) -> Result<(f64, f64)> {
    let mut samples: Vec<(f64, f64)> = Vec::new(); // (daily volume, EWMA weight)
    for d in 0..PACE_LOOKBACK_DAYS {
        let end = now - Duration::days(d);
        let start = end - Duration::days(1);
        let total = db::consumed_in_window(conn, service, unit, start, end)?;
        let w = 0.5f64.powf(d as f64 / PACE_HALF_LIFE_DAYS);
        samples.push((total, w));
    }
    if samples.iter().map(|(_, w)| *w).sum::<f64>() <= 0.0 {
        let p = db::consumed_in_window(conn, service, unit, now - Duration::days(7), now)?;
        return Ok((p, 0.3 * p));
    }
    let (daily, daily_sigma) = robust_pace(&samples);
    Ok((daily * 7.0, daily_sigma * 7.0))
}

/// Weighted median of `(value, weight)` samples.
fn weighted_median(samples: &[(f64, f64)]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut s: Vec<(f64, f64)> = samples.to_vec();
    s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let total: f64 = s.iter().map(|(_, w)| *w).sum();
    let mut acc = 0.0;
    for (v, w) in &s {
        acc += *w;
        if acc >= total / 2.0 {
            return *v;
        }
    }
    s.last().map(|(v, _)| *v).unwrap_or(0.0)
}

/// Weighted mean and (population) standard deviation.
fn weighted_mean_sd(samples: &[(f64, f64)]) -> (f64, f64) {
    let sw: f64 = samples.iter().map(|(_, w)| *w).sum();
    if sw <= 0.0 {
        return (0.0, 0.0);
    }
    let mean = samples.iter().map(|(v, w)| v * w).sum::<f64>() / sw;
    let var = samples.iter().map(|(v, w)| w * (v - mean).powi(2)).sum::<f64>() / sw;
    (mean, var.max(0.0).sqrt())
}

/// Robust pace: winsorize samples to ±3 robust-σ (MAD) around the weighted
/// median, then take the weighted mean (unbiased for the total) and a σ bounded
/// to a sane fraction of the level. A single spike day can't whip the estimate.
fn robust_pace(samples: &[(f64, f64)]) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let m = weighted_median(samples);
    let devs: Vec<(f64, f64)> = samples.iter().map(|(v, w)| ((v - m).abs(), *w)).collect();
    let mad = weighted_median(&devs);
    let (raw_mean, _raw_sd) = weighted_mean_sd(samples);
    // A robust scale that never collapses to zero when a majority of days are
    // equal (MAD=0) — falls back to fractions of the median/mean.
    let robust_scale = (1.4826 * mad)
        .max(0.5 * m)
        .max(0.25 * raw_mean)
        .max(f64::MIN_POSITIVE);
    let hi = m + 3.0 * robust_scale; // clip spikes to a few robust-σ above typical
    let wins: Vec<(f64, f64)> = samples.iter().map(|(v, w)| (v.clamp(0.0, hi), *w)).collect();
    let (p, sd) = weighted_mean_sd(&wins);
    let sigma = if p > 0.0 {
        sd.clamp(SIGMA_MIN_FRAC * p, SIGMA_MAX_FRAC * p)
    } else {
        sd
    };
    (p, sigma)
}

/// Horizon the cone projects to: the reset for a fixed window, else a full week
/// ahead for a rolling window.
fn horizon_end(now: DateTime<Utc>, window_end: DateTime<Utc>, rolling: bool) -> DateTime<Utc> {
    if rolling {
        now + Duration::days(WINDOW_DAYS)
    } else {
        window_end.max(now)
    }
}

/// Pure cone-point math: cumulative low/mid/high at a future point, given the
/// weekly pace `p` (±`sigma`), the fraction of a week `w` elapsed since `now`,
/// and any rolled-off usage (rolling windows only).
fn cone_values(consumed: f64, p: f64, sigma: f64, z: f64, w: f64, rolloff: f64) -> (f64, f64, f64) {
    let mid = consumed + p * w - rolloff;
    let lo = consumed + (p - z * sigma).max(0.0) * w - rolloff;
    let hi = consumed + (p + z * sigma) * w - rolloff;
    (lo.max(0.0), mid.max(0.0), hi.max(0.0))
}

/// Build the projection fan from `now` to the horizon. For rolling windows the
/// oldest days fall out of the trailing window, so we subtract the actual usage
/// that rolls off — the rolling total can fall even at a steady pace.
fn build_cone(
    conn: &rusqlite::Connection,
    service: &str,
    unit: Unit,
    grid: &DowGrid,
    consumed: f64,
    now: DateTime<Utc>,
    window_end: DateTime<Utc>,
    rolling: bool,
    p: f64,
    sigma: f64,
) -> Result<Vec<ConePoint>> {
    let horizon = horizon_end(now, window_end, rolling);
    let span = (horizon - now).num_seconds();
    if span <= 0 {
        return Ok(Vec::new());
    }
    let roll_start = now - Duration::days(WINDOW_DAYS);
    let mut out = Vec::with_capacity(CONE_STEPS as usize + 1);
    for i in 0..=CONE_STEPS {
        let t = now + Duration::seconds(span * i / CONE_STEPS);
        let w = grid.weight_between(now, t); // fraction of a week in (now, t]
        let rolloff = if rolling {
            let roll_to = roll_start + (t - now);
            db::consumed_in_window(conn, service, unit, roll_start, roll_to)?
        } else {
            0.0
        };
        let (lo, mid, hi) = cone_values(consumed, p, sigma, CONE_Z, w, rolloff);
        out.push(ConePoint { ts: t, lo, mid, hi });
    }
    Ok(out)
}

/// Produce the forecast for one service's weekly window. Fixed window when a reset
/// is known (Codex); rolling trailing-7-day otherwise (DESIGN choice).
pub fn forecast_service(
    pool: &DbPool,
    cfg: &Config,
    service: &str,
    now: DateTime<Utc>,
) -> Result<Forecast> {
    let tz: Tz = cfg.timezone.parse().unwrap_or(chrono_tz::UTC);
    let unit = service_unit(cfg, service);
    let conn = pool.get()?;
    let warn = cfg.forecast.warn_threshold;
    let danger = cfg.forecast.danger_threshold;

    let reset = resolve_reset(&conn, service)?;
    let rolling = reset.is_none();
    let (start, end) = if rolling {
        (now - Duration::days(WINDOW_DAYS), now)
    } else {
        resolve_window(now, reset)
    };
    let consumed = db::consumed_in_window(&conn, service, unit, start, now)?;

    // Projected *tokens* at the horizon, from the trend model: current weekly
    // pace (recent-weighted, weekday-shaped) applied along the future profile.
    let grid = build_grid(&conn, service, unit, tz, now, cfg.forecast.history_weeks)?;
    let (pace, _sigma) = estimate_pace(&conn, service, unit, now)?;
    let horizon = horizon_end(now, end, rolling);
    let projected_tokens = if grid.total() > 0.0 {
        let w = grid.weight_between(now, horizon);
        let rolloff = if rolling { consumed } else { 0.0 };
        (consumed + pace * w - rolloff).max(0.0)
    } else if rolling {
        pace
    } else {
        linear(consumed, None, start, end, now, warn, danger).projected
    };
    let model_name = "trend";

    // Level (cap + current %) — grounded on readings and stable across provider
    // quota resets (a reset zeroes the counter, not the weekly allowance).
    let (limit, limit_source, pct_now) =
        resolve_cap_and_pct(&conn, cfg, service, unit, now, start, consumed)?;

    let pct_projected = match (limit, pct_now) {
        (Some(cap), Some(pn)) if cap > 0.0 => Some(pn + (projected_tokens - consumed) / cap),
        _ => None,
    };

    // Just after a reset a fixed window has too little history to project from;
    // a rolling window is low-confidence only when there's no usage at all.
    let elapsed_hours = (now - start).num_hours();
    let low_confidence = if rolling {
        consumed <= 0.0
    } else {
        elapsed_hours < 24
    };
    // Don't raise an alarm on an unreliable early-window projection.
    let status = if low_confidence {
        "unknown".to_string()
    } else {
        status_for(pct_projected, warn, danger)
    };

    // ETA to 100%: tokens remaining to cap ÷ burn rate.
    let eta = match (limit, pct_now) {
        (Some(cap), Some(pn)) if cap > 0.0 && pn < 1.0 => {
            let tokens_left = cap * (1.0 - pn);
            let burn_per_sec = if rolling {
                recent_daily_burn(&conn, service, unit, now, 2)? / 86400.0
            } else {
                let elapsed = (now - start).num_seconds() as f64;
                if elapsed > 0.0 { consumed / elapsed } else { 0.0 }
            };
            if burn_per_sec > 0.0 {
                now.checked_add_signed(Duration::seconds((tokens_left / burn_per_sec) as i64))
            } else {
                None
            }
        }
        _ => None,
    };

    Ok(Forecast {
        service: service.to_string(),
        window_kind: "weekly".to_string(),
        unit,
        consumed,
        limit,
        limit_source,
        pct_now,
        projected: projected_tokens,
        pct_projected,
        status,
        eta_to_limit: eta,
        window_start: start,
        window_end: end,
        forecast_model: model_name.to_string(),
        low_confidence,
    })
}

/// Readings below this percent are too noisy to derive a cap from (e.g. a "1%"
/// reading right after a reset implies a wildly imprecise cap).
const CAP_MIN_PERCENT: f64 = 3.0;
/// How far back to look for a good reading to carry a cap across a reset.
const CAP_CARRY_DAYS: i64 = 21;

/// Plan cap (tokens per full window) from a set of `(measured, percent)` readings:
/// the median of `measured / (percent/100)` over readings with a usable percent.
/// The median is robust to a single off reading. Returns None if none qualify.
fn plan_cap(readings: &[(f64, f64)]) -> Option<f64> {
    let mut caps: Vec<f64> = readings
        .iter()
        .filter(|(m, p)| *p >= CAP_MIN_PERCENT && *m > 0.0)
        .map(|(m, p)| m / (p / 100.0))
        .collect();
    if caps.is_empty() {
        return None;
    }
    caps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(caps[caps.len() / 2])
}

/// Resolve the token cap and current fraction. The cap is the plan's weekly
/// allowance — it's stable, so it's estimated *robustly from many readings* (a
/// median), not from whatever the latest one implies. This survives provider
/// quota resets (a reset zeroes the counter, not the allowance) and shrugs off a
/// single garbage reading — e.g. a fresh post-reset "6%" when our own counting
/// for the new window hasn't caught up yet would imply a wildly small cap, but
/// it's outvoted by the mature readings. Precedence:
///   1. median cap over recent readings (≥floor percent);
///   2. provider rate-limit percent (Codex), for setups with no manual readings;
///   3. a persisted "observed" cap (last good value, across resets);
///   4. the configured cap.
/// `consumed` is this window's usage, so pct_now resets with the window. The cap
/// is a robust median (for projection); the current % is anchored to the user's
/// latest reading (so "I said 38%" shows 38%), plus the token delta since.
fn resolve_cap_and_pct(
    conn: &rusqlite::Connection,
    cfg: &Config,
    service: &str,
    unit: Unit,
    now: DateTime<Utc>,
    start: DateTime<Utc>,
    consumed: f64,
) -> Result<(Option<f64>, Option<String>, Option<f64>)> {
    let (cap, source) = resolve_cap_value(conn, cfg, service, unit, now, start, consumed)?;
    let pct_now = match cap {
        Some(c) if c > 0.0 => Some(anchored_pct_now(conn, service, unit, start, consumed, c)?),
        _ => None,
    };
    Ok((cap, source, pct_now))
}

/// Current utilization fraction, anchored to the latest reading in *this* window:
/// `latest.percent + (consumed_now − tokens_at_reading) / cap`. With no in-window
/// reading (e.g. right after a reset) it falls back to `consumed / cap`.
fn anchored_pct_now(
    conn: &rusqlite::Connection,
    service: &str,
    unit: Unit,
    start: DateTime<Utc>,
    consumed: f64,
    cap: f64,
) -> Result<f64> {
    let in_window = db::fiducials_since(conn, service, start)?;
    if let Some(last) = in_window.last() {
        let measured = db::consumed_in_window(conn, service, unit, start, last.ts)?;
        return Ok(last.percent / 100.0 + (consumed - measured) / cap);
    }
    Ok(consumed / cap)
}

/// A reading whose implied cap is below this fraction of the robust median is
/// treated as logged while counting was behind, and ignored for the cap.
const CAP_OUTLIER_FRAC: f64 = 0.4;

/// The plan's weekly token cap. In normal use it comes from your *latest
/// current-window reading* (so the chart curve, the reading dot, and the gauge
/// all agree), with each reading's tokens recomputed live. A robust median of
/// recent readings backs it up: it carries the cap across a provider quota reset
/// (a reset zeroes the counter, not the allowance) and guards against a single
/// reading logged before the collector caught up. Precedence: latest in-window
/// reading (guarded) → carry-median → provider percent → observed → configured.
fn resolve_cap_value(
    conn: &rusqlite::Connection,
    cfg: &Config,
    service: &str,
    unit: Unit,
    now: DateTime<Utc>,
    start: DateTime<Utc>,
    consumed: f64,
) -> Result<(Option<f64>, Option<String>)> {
    // Recent readings, each recomputed live over its own window, and their median
    // cap (used both as a fallback and as an outlier guard).
    let recent = db::fiducials_since(conn, service, now - Duration::days(CAP_CARRY_DAYS))?;
    let mut readings: Vec<(f64, f64)> = Vec::with_capacity(recent.len());
    for f in &recent {
        let measured = db::consumed_in_window(conn, service, unit, f.window_start, f.ts)?;
        readings.push((measured, f.percent));
    }
    let median = plan_cap(&readings);

    // 1. Latest current-window reading (≥floor) → cap from its live tokens, unless
    //    it's a severe under-outlier vs the median (counting was behind).
    if let Some(last) = recent
        .iter()
        .rev()
        .find(|f| f.percent >= CAP_MIN_PERCENT && f.ts >= start)
    {
        let measured = db::consumed_in_window(conn, service, unit, start, last.ts)?;
        if measured > 0.0 {
            let cap = measured / (last.percent / 100.0);
            if median.map_or(true, |m| cap >= CAP_OUTLIER_FRAC * m) {
                persist_observed_cap(conn, service, cap)?;
                return Ok((Some(cap), Some("fiducial".into())));
            }
        }
    }

    // 2. Carry the plan cap across a reset: robust median of recent readings.
    if let Some(cap) = median {
        persist_observed_cap(conn, service, cap)?;
        return Ok((Some(cap), Some("carried".into())));
    }

    // 3. Provider-reported percent (Codex), for setups with no manual readings.
    if let Some(rl) = db::get_rate_limit(conn, service, "weekly")? {
        if let Some(p) = rl.used_percent {
            if p >= CAP_MIN_PERCENT && consumed > 0.0 {
                let cap = consumed / (p / 100.0);
                persist_observed_cap(conn, service, cap)?;
                return Ok((Some(cap), Some("real".into())));
            }
        }
    }

    // 4. Persisted observed cap (last good value, carried across resets).
    if let Some(v) = db::get_kv(conn, &observed_cap_key(service))? {
        if let Ok(cap) = v.parse::<f64>() {
            if cap > 0.0 {
                return Ok((Some(cap), Some("carried".into())));
            }
        }
    }

    // 5. Configured cap.
    resolve_cap(conn, cfg, service, unit, consumed)
}

fn observed_cap_key(service: &str) -> String {
    format!("observed_cap:{service}")
}

fn persist_observed_cap(conn: &rusqlite::Connection, service: &str, cap: f64) -> Result<()> {
    db::set_kv(conn, &observed_cap_key(service), &format!("{cap}"))
}

// ---------------------------------------------------------------------------
// Cumulative view + anchoring
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConePoint {
    pub ts: DateTime<Utc>,
    pub lo: f64,
    pub mid: f64,
    pub hi: f64,
}

/// A ground-truth reading, on the percent (right) axis.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FiducialPoint {
    pub ts: DateTime<Utc>,
    pub percent: f64,
}

/// One time point of cumulative observed tokens, split per model (aligned to
/// `Cumulative.models`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenCumPoint {
    pub ts: DateTime<Utc>,
    pub cum: Vec<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Cumulative {
    pub service: String,
    pub unit: Unit,
    pub mode: String, // "fixed" | "rolling"
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub now: DateTime<Utc>,
    pub cap: Option<f64>,
    pub cap_source: Option<String>,
    pub consumed: f64,
    pub projected: f64,
    pub pct_now: Option<f64>,
    pub pct_projected: Option<f64>,
    pub status: String,
    pub eta_to_limit: Option<DateTime<Utc>>,
    pub forecast_model: String,
    pub low_confidence: bool,
    /// Token value that maps to 100% on the aligned axes: the local token
    /// observation at the last logged reading ÷ that reading's percent. `None`
    /// until a reading exists. The left token axis is scaled to 1.1×this.
    pub axis_cap: Option<f64>,
    // ---- left axis: observed local tokens, cumulative, broken out by model ----
    pub models: Vec<String>,
    pub token_points: Vec<TokenCumPoint>,
    // ---- right axis (0–110%): logged readings + projection ----
    /// projection fan in PERCENT of cap, now → window_end
    pub cone_pct: Vec<ConePoint>,
    /// the raw ground-truth readings in this window (percent)
    pub fiducials: Vec<FiducialPoint>,
    /// estimated current weekly volume (tokens) and its 1σ, for labelling
    pub pace_weekly: f64,
    pub pace_sigma: f64,
}

/// Cumulative observed tokens over [start, upto], split per model, sampled at
/// hourly boundaries *and* at each marker time (the fiducial timestamps) so the
/// curve passes exactly through the logged-reading points — no bucketing offset.
fn token_cumulative_by_model(
    conn: &rusqlite::Connection,
    service: &str,
    start: DateTime<Utc>,
    upto: DateTime<Utc>,
    markers: &[DateTime<Utc>],
) -> Result<(Vec<String>, Vec<TokenCumPoint>)> {
    let sql = "SELECT ts, COALESCE(model,'(unknown)'),
                      input_tokens+output_tokens+cache_read_tokens+cache_write_tokens+reasoning_tokens
               FROM events WHERE service=?1 AND ts>=?2 AND ts<=?3 ORDER BY ts";
    let mut stmt = conn.prepare(sql)?;
    let rows_iter = stmt.query_map(
        rusqlite::params![service, start.to_rfc3339(), upto.to_rfc3339()],
        |r| {
            let ts: String = r.get(0)?;
            let model: String = r.get(1)?;
            let amt: i64 = r.get(2)?;
            Ok((ts, model, amt as f64))
        },
    )?;

    use std::collections::{BTreeMap, BTreeSet};
    let mut rows: Vec<(i64, String, f64)> = Vec::new();
    let mut models_set: BTreeSet<String> = BTreeSet::new();
    for row in rows_iter {
        let (ts_s, model, amt) = row?;
        if let Ok(ts) = DateTime::parse_from_rfc3339(&ts_s) {
            rows.push((ts.with_timezone(&Utc).timestamp(), model.clone(), amt));
            models_set.insert(model);
        }
    }
    let models: Vec<String> = models_set.into_iter().collect();
    let idx: BTreeMap<&str, usize> =
        models.iter().enumerate().map(|(i, m)| (m.as_str(), i)).collect();

    // Sample times: hourly boundaries in (start, upto], plus each marker, plus upto.
    let start_s = start.timestamp();
    let upto_s = upto.timestamp();
    let mut samples: BTreeSet<i64> = BTreeSet::new();
    let first_hour = start_s - start_s.rem_euclid(3600) + 3600;
    let mut h = first_hour;
    while h < upto_s {
        samples.insert(h);
        h += 3600;
    }
    for m in markers {
        let s = m.timestamp();
        if s > start_s && s <= upto_s {
            samples.insert(s);
        }
    }
    samples.insert(upto_s);

    // One pass over events, snapshotting running cumulative at each sample time.
    let mut points = Vec::with_capacity(samples.len() + 1);
    let mut running = vec![0.0f64; models.len()];
    points.push(TokenCumPoint { ts: start, cum: running.clone() }); // anchor at 0
    let mut ei = 0usize;
    for st in samples {
        while ei < rows.len() && rows[ei].0 <= st {
            if let Some(i) = idx.get(rows[ei].1.as_str()) {
                running[*i] += rows[ei].2;
            }
            ei += 1;
        }
        if let Some(ts) = DateTime::<Utc>::from_timestamp(st, 0) {
            points.push(TokenCumPoint { ts, cum: running.clone() });
        }
    }
    Ok((models, points))
}

/// Full cumulative view for the dashboard's forecast chart. Left axis: observed
/// local tokens (cumulative, by model). Right axis: logged % readings + the
/// projection cone in percent of cap, over the fixed weekly window.
pub fn cumulative_view(
    pool: &DbPool,
    cfg: &Config,
    service: &str,
    now: DateTime<Utc>,
) -> Result<Cumulative> {
    let f = forecast_service(pool, cfg, service, now)?;
    let conn = pool.get()?;
    let unit = f.unit;
    let rolling = resolve_reset(&conn, service)?.is_none();
    let mode = if rolling { "rolling" } else { "fixed" };

    // Right axis: the logged readings (percent). The token axis is calibrated to
    // the *same* cap used for pct/cone (from the last reading), so 100% in tokens
    // lines up with 100% on the right axis and the token area meets the dots.
    let axis_cap = f.limit;
    let raw_fids = db::fiducials_since(&conn, service, f.window_start)?;
    let markers: Vec<DateTime<Utc>> = raw_fids.iter().map(|fd| fd.ts).collect();
    let fiducials: Vec<FiducialPoint> = raw_fids
        .iter()
        .map(|fd| FiducialPoint { ts: fd.ts, percent: fd.percent })
        .collect();

    // Left axis: cumulative observed tokens by model, sampled through the readings.
    let (models, token_points) =
        token_cumulative_by_model(&conn, service, f.window_start, now, &markers)?;

    // Projection cone (tokens) → percent of cap, anchored at the fiducial-grounded
    // current %, over now → window_end.
    let tz: Tz = cfg.timezone.parse().unwrap_or(chrono_tz::UTC);
    let grid = build_grid(&conn, service, unit, tz, now, cfg.forecast.history_weeks)?;
    let (pace_weekly, pace_sigma) = estimate_pace(&conn, service, unit, now)?;
    let token_cone = build_cone(
        &conn, service, unit, &grid, f.consumed, now, f.window_end, rolling, pace_weekly,
        pace_sigma,
    )?;
    let cone_pct = match (f.limit, f.pct_now) {
        (Some(cap), Some(pn)) if cap > 0.0 => token_cone
            .iter()
            .map(|c| ConePoint {
                ts: c.ts,
                lo: (pn + (c.lo - f.consumed) / cap) * 100.0,
                mid: (pn + (c.mid - f.consumed) / cap) * 100.0,
                hi: (pn + (c.hi - f.consumed) / cap) * 100.0,
            })
            .collect(),
        _ => Vec::new(),
    };

    Ok(Cumulative {
        service: service.to_string(),
        unit,
        mode: mode.to_string(),
        window_start: f.window_start,
        window_end: f.window_end,
        now,
        cap: f.limit,
        cap_source: f.limit_source,
        consumed: f.consumed,
        projected: f.projected,
        pct_now: f.pct_now,
        pct_projected: f.pct_projected,
        status: f.status,
        eta_to_limit: f.eta_to_limit,
        forecast_model: f.forecast_model,
        low_confidence: f.low_confidence,
        axis_cap,
        models,
        token_points,
        cone_pct,
        fiducials,
        pace_weekly,
        pace_sigma,
    })
}

/// Record a ground-truth utilization reading (fiducial) at `now`. The reading is
/// logged raw and append-only; the token cap and current % are then re-derived
/// from the fiducial history on every forecast. Returns the currently-derived cap.
pub fn apply_anchor(
    pool: &DbPool,
    cfg: &Config,
    service: &str,
    percent: f64,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<f64> {
    if !(0.0..=100.0).contains(&percent) || percent <= 0.0 {
        anyhow::bail!("percent must be in (0, 100]");
    }
    let unit = service_unit(cfg, service);
    let conn = pool.get()?;
    // Window the reading is measured against: known reset (fixed) else rolling.
    let reset = resets_at.or(resolve_reset(&conn, service)?);
    let (start, _end) = if reset.is_some() {
        resolve_window(now, reset)
    } else {
        (now - Duration::days(WINDOW_DAYS), now)
    };
    let consumed = db::consumed_in_window(&conn, service, unit, start, now)?;

    // Log the raw fiducial (never overwritten).
    db::insert_fiducial(&conn, service, now, percent, resets_at, start, consumed, unit)?;

    // If the user told us a reset time, persist it so future windows are fixed.
    if let Some(r) = resets_at {
        db::upsert_limit(&conn, service, "weekly", None, unit, "fiducial", Some(r), None)?;
    }

    // Report the cap now derived from the reading history.
    let (cap, _src, _pct) = resolve_cap_and_pct(&conn, cfg, service, unit, now, start, consumed)?;
    cap.ok_or_else(|| anyhow::anyhow!("recorded, but no usable reading yet to derive a cap from"))
}

/// Enabled services in a stable order.
pub fn enabled_services(cfg: &Config) -> Vec<&'static str> {
    let mut v = Vec::new();
    if cfg.services.claude_code.enabled {
        v.push(crate::models::SERVICE_CLAUDE_CODE);
    }
    if cfg.services.codex.enabled {
        v.push(crate::models::SERVICE_CODEX);
    }
    if cfg.services.openrouter.enabled {
        v.push(crate::models::SERVICE_OPENROUTER);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn linear_halfway_doubles() {
        // Window is 10 days; 5 days elapsed; consumed 50 → projected 100.
        let start = dt("2026-08-10T00:00:00Z");
        let end = dt("2026-08-20T00:00:00Z");
        let now = dt("2026-08-15T00:00:00Z");
        let f = linear(50.0, Some(100.0), start, end, now, 0.8, 1.0);
        assert!((f.projected - 100.0).abs() < 1e-6, "projected={}", f.projected);
        assert!((f.pct_now.unwrap() - 0.5).abs() < 1e-6);
        assert!((f.pct_projected.unwrap() - 1.0).abs() < 1e-6);
        // pct_projected == danger(1.0) → not < danger → red
        assert_eq!(f.status, "red");
    }

    #[test]
    fn linear_status_bands() {
        let start = dt("2026-08-10T00:00:00Z");
        let end = dt("2026-08-17T00:00:00Z");
        let now = dt("2026-08-13T12:00:00Z"); // exactly half
        // consumed 30 → projected 60 → 60% → green
        let g = linear(30.0, Some(100.0), start, end, now, 0.8, 1.0);
        assert_eq!(g.status, "green");
        // consumed 45 → projected 90 → 90% → amber
        let a = linear(45.0, Some(100.0), start, end, now, 0.8, 1.0);
        assert_eq!(a.status, "amber");
    }

    #[test]
    fn eta_to_limit_matches_formula() {
        // burn = 100/5d; limit 100 → hits limit at start+5d.
        let start = dt("2026-08-10T00:00:00Z");
        let end = dt("2026-08-24T00:00:00Z");
        let now = dt("2026-08-15T00:00:00Z"); // 5 days elapsed
        let f = linear(100.0, Some(100.0), start, end, now, 0.8, 1.0);
        let eta = f.eta_to_limit.unwrap();
        assert_eq!(eta, dt("2026-08-15T00:00:00Z"));
    }

    #[test]
    fn no_limit_is_unknown() {
        let start = dt("2026-08-10T00:00:00Z");
        let end = dt("2026-08-17T00:00:00Z");
        let now = dt("2026-08-13T00:00:00Z");
        let f = linear(10.0, None, start, end, now, 0.8, 1.0);
        assert_eq!(f.status, "unknown");
        assert!(f.pct_now.is_none());
        assert!(f.eta_to_limit.is_none());
    }

    #[test]
    fn dow_cold_start_falls_back_to_linear() {
        let start = dt("2026-08-10T00:00:00Z");
        let end = dt("2026-08-17T00:00:00Z");
        let now = dt("2026-08-11T00:00:00Z"); // only 1 day elapsed (<48h)
        let grid = DowGrid {
            weight: [[1.0 / 168.0; 24]; 7],
            tz: chrono_tz::UTC,
        };
        let f = dow_weighted(10.0, Some(100.0), start, end, now, &grid, 0.8, 1.0);
        assert_eq!(f.forecast_model, "linear");
    }

    #[test]
    fn dow_uniform_grid_matches_linear() {
        // A uniform grid should behave like linear: half elapsed → double.
        let start = dt("2026-08-10T00:00:00Z");
        let end = dt("2026-08-17T00:00:00Z");
        let now = dt("2026-08-13T12:00:00Z"); // exactly half of 7 days
        let grid = DowGrid {
            weight: [[1.0 / 168.0; 24]; 7],
            tz: chrono_tz::UTC,
        };
        let f = dow_weighted(50.0, Some(100.0), start, end, now, &grid, 0.8, 1.0);
        assert_eq!(f.forecast_model, "dow_weighted");
        assert!((f.projected - 100.0).abs() < 0.5, "projected={}", f.projected);
    }

    #[test]
    fn dow_weighted_front_loaded_history() {
        // History says all activity happens in the first half of the window, so
        // by the midpoint we've done ~100% of expected → projected ≈ consumed.
        let start = dt("2026-08-10T00:00:00Z"); // Mon 00:00 UTC
        let end = dt("2026-08-17T00:00:00Z");
        let now = dt("2026-08-13T12:00:00Z"); // exactly half
        let mut weight = [[0.0f64; 24]; 7];
        // Put all weight on Mon–Wed (dow 0,1,2), the first ~3.5 days.
        for d in 0..3 {
            for h in 0..24 {
                weight[d][h] = 1.0;
            }
        }
        // normalize
        let total: f64 = weight.iter().flatten().sum();
        for c in weight.iter_mut().flatten() {
            *c /= total;
        }
        let grid = DowGrid { weight, tz: chrono_tz::UTC };
        let f = dow_weighted(80.0, Some(100.0), start, end, now, &grid, 0.8, 1.0);
        // expected_fraction_elapsed is high → projected only modestly above consumed.
        assert!(f.projected < 100.0, "projected={}", f.projected);
        assert!(f.projected >= 80.0);
    }

    #[test]
    fn resolve_window_rolls_past_reset_forward() {
        let now = dt("2026-08-24T00:00:00Z");
        let past_reset = dt("2026-08-20T00:00:00Z");
        let (start, end) = resolve_window(now, Some(past_reset));
        assert!(end > now);
        assert_eq!(end - start, Duration::days(7));
        // 2026-08-20 + 7d = 2026-08-27
        assert_eq!(end, dt("2026-08-27T00:00:00Z"));
    }

    #[test]
    fn plan_cap_is_median_of_readings() {
        // caps: 100/.5=200, 69/.05=1380, 200/.09≈2222 → median 1380.
        let cap = plan_cap(&[(100.0, 50.0), (69.0, 5.0), (200.0, 9.0)]).unwrap();
        assert!((cap - 1380.0).abs() < 1.0, "cap={cap}");
    }

    #[test]
    fn plan_cap_ignores_noisy_low_readings() {
        // A "1%" reading right after a reset (1.6 tokens) would imply cap 160 —
        // it's below the floor and must be ignored; the 20% reading wins.
        let cap = plan_cap(&[(1.6, 1.0), (315.0, 20.0)]).unwrap();
        assert!((cap - 1575.0).abs() < 1.0, "cap={cap}");
        // If every reading is sub-floor, there's no usable cap.
        assert!(plan_cap(&[(1.6, 1.0)]).is_none());
        assert!(plan_cap(&[]).is_none());
    }

    #[test]
    fn cone_starts_tight_and_widens() {
        // At w=0 the fan is a point at `consumed`.
        let (lo0, mid0, hi0) = cone_values(1000.0, 500.0, 100.0, 1.0, 0.0, 0.0);
        assert!((lo0 - 1000.0).abs() < 1e-9 && (mid0 - 1000.0).abs() < 1e-9 && (hi0 - 1000.0).abs() < 1e-9);
        // Halfway through the week: mid adds p*w, band is ±z*sigma*w.
        let (lo, mid, hi) = cone_values(1000.0, 500.0, 100.0, 1.0, 0.5, 0.0);
        assert!((mid - 1250.0).abs() < 1e-9, "mid={mid}");
        assert!((lo - 1200.0).abs() < 1e-9, "lo={lo}");
        assert!((hi - 1300.0).abs() < 1e-9, "hi={hi}");
        // Band widens with w.
        let (_, _, hi_full) = cone_values(1000.0, 500.0, 100.0, 1.0, 1.0, 0.0);
        let (_, _, hi_half) = cone_values(1000.0, 500.0, 100.0, 1.0, 0.5, 0.0);
        assert!(hi_full - 1000.0 > hi_half - 1000.0);
    }

    #[test]
    fn robust_pace_shrugs_off_a_single_spike() {
        // Six ordinary days near 100, one 50× spike day.
        let mut s: Vec<(f64, f64)> = (0..6).map(|_| (100.0, 1.0)).collect();
        s.push((5000.0, 1.0));
        let (p, sigma) = robust_pace(&s);
        // The spike is winsorized, so the level stays near typical, not the raw
        // mean of ~800.
        assert!(p < 300.0, "p={p} should stay near typical usage");
        assert!(p > 80.0, "p={p} shouldn't collapse");
        // σ is bounded to a sane fraction of the level.
        assert!(sigma <= 0.60 * p + 1e-6 && sigma >= 0.10 * p - 1e-6, "sigma={sigma}");
    }

    #[test]
    fn robust_pace_flat_series_has_floor_sigma() {
        let s: Vec<(f64, f64)> = (0..10).map(|_| (200.0, 1.0)).collect();
        let (p, sigma) = robust_pace(&s);
        assert!((p - 200.0).abs() < 1e-6);
        // No raw spread, but σ is floored so the cone isn't a zero-width line.
        assert!((sigma - 0.10 * 200.0).abs() < 1e-6, "sigma={sigma}");
    }

    #[test]
    fn robust_pace_empty_is_zero() {
        assert_eq!(robust_pace(&[]), (0.0, 0.0));
    }

    #[test]
    fn cone_rolloff_can_lower_the_total() {
        // Rolling: heavy usage rolling off can pull the projected total below now.
        let (_, mid, _) = cone_values(1000.0, 200.0, 0.0, 1.0, 1.0, 800.0);
        assert!((mid - 400.0).abs() < 1e-9, "mid={mid}"); // 1000 + 200 - 800
    }

    #[test]
    fn resolve_window_future_reset_used_directly() {
        let now = dt("2026-08-24T00:00:00Z");
        let reset = dt("2026-08-31T00:00:00Z");
        let (start, end) = resolve_window(now, Some(reset));
        assert_eq!(end, reset);
        assert_eq!(start, dt("2026-08-24T00:00:00Z"));
    }
}
