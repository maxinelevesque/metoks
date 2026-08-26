//! Codex CLI collector — backfill + tail of `${CODEX_HOME}/sessions` and
//! `archived_sessions`. `CODEX_HOME` may be a comma-separated list of roots.
//!
//! Token usage lives in `token_count` events (`last_token_usage` = per-turn
//! delta). The active model comes from the nearest preceding `turn_context`,
//! which we persist per-file so incremental tailing resolves it across reads.
//! The same events also carry `rate_limits` — the provider's real weekly/session
//! used_percent — which we record as an authoritative window status.

use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::collectors::read_appended_lines;
use crate::config::{expand_tilde, Config};
use crate::db::{self, DbPool};
use crate::normalize::{codex_event, parse_codex_line};
use crate::pricing::Pricing;

/// Resolve Codex home roots: config override → $CODEX_HOME (comma list) → ~/.codex.
pub fn codex_roots(cfg: &Config) -> Vec<PathBuf> {
    let raw = cfg
        .services
        .codex
        .codex_home
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("CODEX_HOME").ok())
        .unwrap_or_else(|| "~/.codex".to_string());
    raw.split(',')
        .map(|s| PathBuf::from(expand_tilde(s.trim())))
        .filter(|p| p.exists())
        .collect()
}

/// Directories under each root that hold rollout files.
fn session_dirs(cfg: &Config) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for root in codex_roots(cfg) {
        for sub in ["sessions", "archived_sessions"] {
            let d = root.join(sub);
            if d.exists() {
                dirs.push(d);
            }
        }
    }
    dirs
}

pub fn watch_roots(cfg: &Config) -> Vec<PathBuf> {
    session_dirs(cfg)
}

fn session_id_from(path: &Path) -> Option<String> {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .map(|s| s.strip_prefix("rollout-").map(|x| x.to_string()).unwrap_or(s))
}

/// Scan one rollout file incrementally: resolve models, emit token events, and
/// capture the latest rate-limit status. Returns events inserted.
pub fn scan_one(pool: &DbPool, pricing: &Pricing, path: &Path) -> Result<usize> {
    let path_s = path.to_string_lossy().into_owned();
    let session = session_id_from(path);

    let conn = pool.get()?;
    let start = db::get_file_offset(&conn, &path_s)?;
    let mut model = db::get_kv(&conn, &format!("codex_model:{path_s}"))?;
    let mut project = db::get_kv(&conn, &format!("codex_proj:{path_s}"))?;
    drop(conn);

    let (lines, new_offset) = read_appended_lines(&path_s, start)?;

    let mut events = Vec::new();
    let mut latest_rl: Option<(chrono::DateTime<Utc>, Vec<crate::normalize::RateWindow>)> = None;

    for line in &lines {
        let p = parse_codex_line(line);
        if p.model.is_some() {
            model = p.model.clone();
        }
        if p.cwd.is_some() {
            project = p.cwd.clone();
        }
        if let Some(usage) = &p.usage {
            let ts = p.ts.unwrap_or_else(Utc::now);
            // Skip usage we can't attribute to a model (design §5.2).
            if let Some(m) = &model {
                events.push(codex_event(
                    usage,
                    Some(m),
                    session.as_deref(),
                    project.as_deref(),
                    ts,
                    &path_s,
                    pricing,
                ));
            } else {
                tracing::debug!("codex token_count with no resolvable model in {path_s}; skipping");
            }
        }
        if !p.rate_limits.is_empty() {
            let ts = p.ts.unwrap_or_else(Utc::now);
            latest_rl = Some((ts, p.rate_limits.clone()));
        }
    }

    let inserted = db::insert_events(pool, &events)?;

    let conn = pool.get()?;
    db::set_file_offset(&conn, &path_s, new_offset)?;
    if let Some(m) = &model {
        db::set_kv(&conn, &format!("codex_model:{path_s}"), m)?;
    }
    if let Some(pr) = &project {
        db::set_kv(&conn, &format!("codex_proj:{path_s}"), pr)?;
    }
    if let Some((ts, windows)) = latest_rl {
        for w in windows {
            // >= ~1 day → weekly; otherwise the rolling session window.
            let kind = if w.window_minutes >= 1440 {
                "weekly"
            } else {
                "session"
            };
            db::upsert_rate_limit(
                &conn,
                crate::models::SERVICE_CODEX,
                kind,
                Some(w.used_percent),
                Some(w.window_minutes),
                w.resets_at,
                ts,
            )?;
        }
    }

    Ok(inserted)
}

/// One-time backfill across all rollout files under every root.
pub fn backfill(pool: &DbPool, cfg: &Config, pricing: &Pricing) -> Result<usize> {
    let mut total = 0usize;
    for dir in session_dirs(cfg) {
        let pattern = format!("{}/**/rollout-*.jsonl", dir.to_string_lossy());
        for entry in glob::glob(&pattern).into_iter().flatten().flatten() {
            match scan_one(pool, pricing, &entry) {
                Ok(n) => total += n,
                Err(e) => tracing::warn!("codex backfill {}: {e}", entry.display()),
            }
        }
    }
    Ok(total)
}

pub fn on_change(pool: &DbPool, pricing: &Arc<Pricing>, path: &Path) -> Result<usize> {
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned());
    let is_rollout = name
        .map(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
        .unwrap_or(false);
    if is_rollout {
        scan_one(pool, pricing, path)
    } else {
        Ok(0)
    }
}
