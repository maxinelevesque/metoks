//! SQLite store (DESIGN.md §4). Append-only events + config tables.
//! Uses an r2d2 connection pool over a bundled SQLite; all access is synchronous
//! and short-lived so we never hold a connection across an `.await`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

use crate::models::{Event, Unit};

pub type DbPool = Pool<SqliteConnectionManager>;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    event_uid             TEXT UNIQUE NOT NULL,
    service               TEXT NOT NULL,
    metered_mode          TEXT NOT NULL,
    ts                    TEXT NOT NULL,
    model                 TEXT,
    input_tokens          INTEGER DEFAULT 0,
    output_tokens         INTEGER DEFAULT 0,
    cache_read_tokens     INTEGER DEFAULT 0,
    cache_write_tokens    INTEGER DEFAULT 0,
    reasoning_tokens      INTEGER DEFAULT 0,
    cost_usd              REAL DEFAULT 0,
    cost_is_estimate      INTEGER DEFAULT 0,
    session_id            TEXT,
    project               TEXT,
    raw_source            TEXT,
    created_at            TEXT DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_events_service_ts ON events(service, ts);
CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);

CREATE TABLE IF NOT EXISTS limits (
    service        TEXT NOT NULL,
    window_kind    TEXT NOT NULL,
    limit_value    REAL,
    limit_unit     TEXT NOT NULL,
    limit_source   TEXT NOT NULL,
    window_reset   TEXT,
    window_anchor  TEXT,
    updated_at     TEXT DEFAULT (datetime('now')),
    PRIMARY KEY (service, window_kind)
);

CREATE TABLE IF NOT EXISTS cumulative_snapshots (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    service       TEXT NOT NULL,
    ts            TEXT NOT NULL,
    total_usage   REAL NOT NULL,
    total_limit   REAL,
    unit          TEXT NOT NULL,
    raw           TEXT
);
CREATE INDEX IF NOT EXISTS idx_snap_service_ts ON cumulative_snapshots(service, ts);

-- Per-file byte offsets for incremental tail collectors.
CREATE TABLE IF NOT EXISTS file_offsets (
    path       TEXT PRIMARY KEY,
    offset     INTEGER NOT NULL,
    updated_at TEXT DEFAULT (datetime('now'))
);

-- Generic small key/value store (e.g. per-file last-seen Codex model).
CREATE TABLE IF NOT EXISTS kv (
    k          TEXT PRIMARY KEY,
    v          TEXT NOT NULL,
    updated_at TEXT DEFAULT (datetime('now'))
);

-- Authoritative provider-reported window status (Codex rate_limits, etc).
-- This is the "real" limit signal: the provider tells us used_percent directly.
CREATE TABLE IF NOT EXISTS rate_limit_status (
    service        TEXT NOT NULL,
    window_kind    TEXT NOT NULL,      -- 'weekly' | 'session'
    used_percent   REAL,
    window_minutes INTEGER,
    resets_at      TEXT,               -- ISO-8601
    observed_at    TEXT NOT NULL,      -- ISO-8601 when we saw it
    PRIMARY KEY (service, window_kind)
);

-- Fiducials: the user's raw, ground-truth utilization readings (the numbers that
-- actually determine spend). Append-only. Everything downstream rests on these:
-- the token cap and current % are calibrated to the latest reading, so only the
-- token delta *since* a reading comes from our approximate log counting.
CREATE TABLE IF NOT EXISTS fiducials (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    service             TEXT NOT NULL,
    window_kind         TEXT NOT NULL DEFAULT 'weekly',
    ts                  TEXT NOT NULL,   -- when the reading applies (ISO-8601)
    percent             REAL NOT NULL,   -- ground-truth % used, (0,100]
    resets_at           TEXT,            -- known reset, if provided
    window_start        TEXT NOT NULL,   -- window the cumulative was measured over
    measured_cumulative REAL NOT NULL,   -- our tokens in [window_start, ts] at reading time
    unit                TEXT NOT NULL,
    created_at          TEXT DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_fiducials_service_ts ON fiducials(service, ts);
"#;

/// Open (creating if needed) the SQLite DB and ensure the schema exists.
pub fn open(db_path: &str) -> Result<DbPool> {
    if let Some(parent) = std::path::Path::new(db_path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let manager = SqliteConnectionManager::file(db_path).with_init(|c| {
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")
    });
    let pool = Pool::builder()
        .max_size(8)
        // Open connections lazily so 8 fresh connections don't race on the
        // initial `journal_mode=WAL` switch (which needs brief exclusive access).
        .min_idle(Some(1))
        .build(manager)
        .with_context(|| format!("opening sqlite pool at {db_path}"))?;
    {
        let conn = pool.get()?;
        conn.execute_batch(SCHEMA).context("creating schema")?;
    }
    Ok(pool)
}

/// Insert an event idempotently (INSERT OR IGNORE on event_uid).
/// Returns true if a new row was inserted.
pub fn insert_event(conn: &rusqlite::Connection, e: &Event) -> Result<bool> {
    let changed = conn.execute(
        "INSERT OR IGNORE INTO events
          (event_uid, service, metered_mode, ts, model,
           input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
           reasoning_tokens, cost_usd, cost_is_estimate, session_id, project, raw_source)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            e.event_uid,
            e.service,
            e.metered_mode.as_str(),
            e.ts.to_rfc3339(),
            e.model,
            e.input_tokens,
            e.output_tokens,
            e.cache_read_tokens,
            e.cache_write_tokens,
            e.reasoning_tokens,
            e.cost_usd,
            e.cost_is_estimate as i64,
            e.session_id,
            e.project,
            e.raw_source,
        ],
    )?;
    Ok(changed > 0)
}

/// Bulk insert; returns count of newly-inserted rows.
pub fn insert_events(pool: &DbPool, events: &[Event]) -> Result<usize> {
    let mut conn = pool.get()?;
    let tx = conn.transaction()?;
    let mut inserted = 0usize;
    for e in events {
        if insert_event(&tx, e)? {
            inserted += 1;
        }
    }
    tx.commit()?;
    Ok(inserted)
}

pub fn get_file_offset(conn: &rusqlite::Connection, path: &str) -> Result<u64> {
    let off: Option<i64> = conn
        .query_row(
            "SELECT offset FROM file_offsets WHERE path=?1",
            params![path],
            |r| r.get(0),
        )
        .ok();
    Ok(off.unwrap_or(0).max(0) as u64)
}

pub fn set_file_offset(conn: &rusqlite::Connection, path: &str, offset: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO file_offsets(path, offset, updated_at) VALUES(?1, ?2, datetime('now'))
         ON CONFLICT(path) DO UPDATE SET offset=excluded.offset, updated_at=datetime('now')",
        params![path, offset as i64],
    )?;
    Ok(())
}

/// A row from the limits table. Some fields are part of the complete record but
/// not read by the current forecaster.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LimitRow {
    pub service: String,
    pub window_kind: String,
    pub limit_value: Option<f64>,
    pub limit_unit: Unit,
    pub limit_source: String,
    pub window_reset: Option<DateTime<Utc>>,
    pub window_anchor: Option<DateTime<Utc>>,
}

pub fn upsert_limit(
    conn: &rusqlite::Connection,
    service: &str,
    window_kind: &str,
    limit_value: Option<f64>,
    unit: Unit,
    source: &str,
    window_reset: Option<DateTime<Utc>>,
    window_anchor: Option<DateTime<Utc>>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO limits(service, window_kind, limit_value, limit_unit, limit_source, window_reset, window_anchor, updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,datetime('now'))
         ON CONFLICT(service, window_kind) DO UPDATE SET
            limit_value=excluded.limit_value,
            limit_unit=excluded.limit_unit,
            limit_source=excluded.limit_source,
            window_reset=excluded.window_reset,
            window_anchor=excluded.window_anchor,
            updated_at=datetime('now')",
        params![
            service,
            window_kind,
            limit_value,
            unit.as_str(),
            source,
            window_reset.map(|d| d.to_rfc3339()),
            window_anchor.map(|d| d.to_rfc3339()),
        ],
    )?;
    Ok(())
}

pub fn get_limit(
    conn: &rusqlite::Connection,
    service: &str,
    window_kind: &str,
) -> Result<Option<LimitRow>> {
    let row = conn
        .query_row(
            "SELECT service, window_kind, limit_value, limit_unit, limit_source, window_reset, window_anchor
             FROM limits WHERE service=?1 AND window_kind=?2",
            params![service, window_kind],
            |r| {
                let unit_s: String = r.get(3)?;
                let reset_s: Option<String> = r.get(5)?;
                let anchor_s: Option<String> = r.get(6)?;
                Ok(LimitRow {
                    service: r.get(0)?,
                    window_kind: r.get(1)?,
                    limit_value: r.get(2)?,
                    limit_unit: Unit::parse(&unit_s).unwrap_or(Unit::Tokens),
                    limit_source: r.get(4)?,
                    window_reset: reset_s.and_then(|s| parse_dt(&s)),
                    window_anchor: anchor_s.and_then(|s| parse_dt(&s)),
                })
            },
        )
        .ok();
    Ok(row)
}

fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Insert a cumulative snapshot and return the previous total_usage (if any) for
/// the same service, so the caller can diff.
pub fn insert_snapshot(
    conn: &rusqlite::Connection,
    service: &str,
    ts: DateTime<Utc>,
    total_usage: f64,
    total_limit: Option<f64>,
    unit: Unit,
    raw: &str,
) -> Result<Option<f64>> {
    let prev: Option<f64> = conn
        .query_row(
            "SELECT total_usage FROM cumulative_snapshots WHERE service=?1 ORDER BY ts DESC LIMIT 1",
            params![service],
            |r| r.get(0),
        )
        .ok();
    conn.execute(
        "INSERT INTO cumulative_snapshots(service, ts, total_usage, total_limit, unit, raw)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            service,
            ts.to_rfc3339(),
            total_usage,
            total_limit,
            unit.as_str(),
            raw
        ],
    )?;
    Ok(prev)
}

/// Sum consumed tokens (or usd) for a service in [start,end].
pub fn consumed_in_window(
    conn: &rusqlite::Connection,
    service: &str,
    unit: Unit,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<f64> {
    let expr = match unit {
        Unit::Tokens => {
            "COALESCE(SUM(input_tokens+output_tokens+cache_read_tokens+cache_write_tokens+reasoning_tokens),0)"
        }
        Unit::Usd => "COALESCE(SUM(cost_usd),0)",
    };
    let sql = format!(
        "SELECT {expr} FROM events WHERE service=?1 AND ts>=?2 AND ts<=?3"
    );
    let v: f64 = conn.query_row(
        &sql,
        params![service, start.to_rfc3339(), end.to_rfc3339()],
        |r| r.get(0),
    )?;
    Ok(v)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CountsSummary {
    pub service: String,
    pub events: i64,
    pub cost_usd: f64,
    pub tokens: i64,
}

pub fn service_counts(conn: &rusqlite::Connection) -> Result<Vec<CountsSummary>> {
    let mut stmt = conn.prepare(
        "SELECT service, COUNT(*), COALESCE(SUM(cost_usd),0),
                COALESCE(SUM(input_tokens+output_tokens+cache_read_tokens+cache_write_tokens+reasoning_tokens),0)
         FROM events GROUP BY service ORDER BY service",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(CountsSummary {
                service: r.get(0)?,
                events: r.get(1)?,
                cost_usd: r.get(2)?,
                tokens: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// A ground-truth utilization reading.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Fiducial {
    pub id: i64,
    pub service: String,
    pub window_kind: String,
    pub ts: DateTime<Utc>,
    pub percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub window_start: DateTime<Utc>,
    pub measured_cumulative: f64,
    pub unit: Unit,
}

#[allow(clippy::too_many_arguments)]
pub fn insert_fiducial(
    conn: &rusqlite::Connection,
    service: &str,
    ts: DateTime<Utc>,
    percent: f64,
    resets_at: Option<DateTime<Utc>>,
    window_start: DateTime<Utc>,
    measured_cumulative: f64,
    unit: Unit,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO fiducials(service, window_kind, ts, percent, resets_at, window_start, measured_cumulative, unit)
         VALUES(?1,'weekly',?2,?3,?4,?5,?6,?7)",
        params![
            service,
            ts.to_rfc3339(),
            percent,
            resets_at.map(|d| d.to_rfc3339()),
            window_start.to_rfc3339(),
            measured_cumulative,
            unit.as_str(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Fiducials for a service with `ts >= since`, oldest first.
pub fn fiducials_since(
    conn: &rusqlite::Connection,
    service: &str,
    since: DateTime<Utc>,
) -> Result<Vec<Fiducial>> {
    let mut stmt = conn.prepare(
        "SELECT id, service, window_kind, ts, resets_at, window_start, percent, unit, measured_cumulative
         FROM fiducials WHERE service=?1 AND ts>=?2 ORDER BY ts ASC",
    )?;
    let rows = stmt
        .query_map(params![service, since.to_rfc3339()], |r| {
            let ts: String = r.get(3)?;
            let resets: Option<String> = r.get(4)?;
            let ws: String = r.get(5)?;
            let unit_s: String = r.get(7)?;
            Ok(Fiducial {
                id: r.get(0)?,
                service: r.get(1)?,
                window_kind: r.get(2)?,
                ts: parse_dt(&ts).unwrap_or_else(Utc::now),
                percent: r.get(6)?,
                resets_at: resets.and_then(|s| parse_dt(&s)),
                window_start: parse_dt(&ws).unwrap_or_else(Utc::now),
                measured_cumulative: r.get(8)?,
                unit: Unit::parse(&unit_s).unwrap_or(Unit::Tokens),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Most recent `limit` fiducials for a service, newest first.
pub fn list_fiducials(
    conn: &rusqlite::Connection,
    service: &str,
    limit: i64,
) -> Result<Vec<Fiducial>> {
    let mut stmt = conn.prepare(
        "SELECT id, service, window_kind, ts, resets_at, window_start, percent, unit, measured_cumulative
         FROM fiducials WHERE service=?1 ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![service, limit], |r| {
            let ts: String = r.get(3)?;
            let resets: Option<String> = r.get(4)?;
            let ws: String = r.get(5)?;
            let unit_s: String = r.get(7)?;
            Ok(Fiducial {
                id: r.get(0)?,
                service: r.get(1)?,
                window_kind: r.get(2)?,
                ts: parse_dt(&ts).unwrap_or_else(Utc::now),
                percent: r.get(6)?,
                resets_at: resets.and_then(|s| parse_dt(&s)),
                window_start: parse_dt(&ws).unwrap_or_else(Utc::now),
                measured_cumulative: r.get(8)?,
                unit: Unit::parse(&unit_s).unwrap_or(Unit::Tokens),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// A per-project token time series (fixed-length buckets), for sparklines.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectSeries {
    pub project: String,
    pub total: i64,
    pub points: Vec<f64>, // tokens per bucket, oldest → newest
}

/// Per-project token totals bucketed over `[since, now]` (aggregated across
/// services), for the sparkline views. `bucket_hours` sets the resolution.
pub fn project_series(
    conn: &rusqlite::Connection,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
    bucket_hours: i64,
) -> Result<Vec<ProjectSeries>> {
    let bucket_secs = (bucket_hours.max(1)) * 3600;
    let span = (now - since).num_seconds().max(bucket_secs);
    let n = (span / bucket_secs) as usize + 1;
    let mut stmt = conn.prepare(
        "SELECT COALESCE(project,'(unknown)') AS p, ts,
                input_tokens+output_tokens+cache_read_tokens+cache_write_tokens+reasoning_tokens
         FROM events WHERE ts>=?1",
    )?;
    let rows = stmt.query_map(params![since.to_rfc3339()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
    })?;
    use std::collections::HashMap;
    let mut map: HashMap<String, Vec<f64>> = HashMap::new();
    let since_s = since.timestamp();
    for row in rows {
        let (proj, ts_s, tok) = row?;
        if let Ok(ts) = DateTime::parse_from_rfc3339(&ts_s) {
            let idx = ((ts.with_timezone(&Utc).timestamp() - since_s) / bucket_secs) as usize;
            if idx < n {
                let v = map.entry(proj).or_insert_with(|| vec![0.0; n]);
                v[idx] += tok as f64;
            }
        }
    }
    let mut out: Vec<ProjectSeries> = map
        .into_iter()
        .map(|(project, points)| {
            let total = points.iter().sum::<f64>() as i64;
            ProjectSeries { project, total, points }
        })
        .collect();
    out.sort_by(|a, b| b.total.cmp(&a.total));
    Ok(out)
}

pub fn get_kv(conn: &rusqlite::Connection, k: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT v FROM kv WHERE k=?1", params![k], |r| r.get(0))
        .ok())
}

pub fn set_kv(conn: &rusqlite::Connection, k: &str, v: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO kv(k, v, updated_at) VALUES(?1,?2,datetime('now'))
         ON CONFLICT(k) DO UPDATE SET v=excluded.v, updated_at=datetime('now')",
        params![k, v],
    )?;
    Ok(())
}

/// Provider-reported window status (e.g. Codex weekly used_percent).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RateLimitStatus {
    pub service: String,
    pub window_kind: String,
    pub used_percent: Option<f64>,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
}

pub fn upsert_rate_limit(
    conn: &rusqlite::Connection,
    service: &str,
    window_kind: &str,
    used_percent: Option<f64>,
    window_minutes: Option<i64>,
    resets_at: Option<DateTime<Utc>>,
    observed_at: DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO rate_limit_status(service, window_kind, used_percent, window_minutes, resets_at, observed_at)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(service, window_kind) DO UPDATE SET
            used_percent=excluded.used_percent,
            window_minutes=excluded.window_minutes,
            resets_at=excluded.resets_at,
            observed_at=excluded.observed_at
         WHERE excluded.observed_at >= rate_limit_status.observed_at",
        params![
            service,
            window_kind,
            used_percent,
            window_minutes,
            resets_at.map(|d| d.to_rfc3339()),
            observed_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_rate_limit(
    conn: &rusqlite::Connection,
    service: &str,
    window_kind: &str,
) -> Result<Option<RateLimitStatus>> {
    Ok(conn
        .query_row(
            "SELECT service, window_kind, used_percent, window_minutes, resets_at, observed_at
             FROM rate_limit_status WHERE service=?1 AND window_kind=?2",
            params![service, window_kind],
            |r| {
                let resets: Option<String> = r.get(4)?;
                let observed: String = r.get(5)?;
                Ok(RateLimitStatus {
                    service: r.get(0)?,
                    window_kind: r.get(1)?,
                    used_percent: r.get(2)?,
                    window_minutes: r.get(3)?,
                    resets_at: resets.and_then(|s| parse_dt(&s)),
                    observed_at: parse_dt(&observed).unwrap_or_else(Utc::now),
                })
            },
        )
        .ok())
}
