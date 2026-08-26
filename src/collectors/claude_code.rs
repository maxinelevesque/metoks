//! Claude Code collector — backfill + watchdog tail of `~/.claude/projects/**`.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::collectors::scan_file;
use crate::config::{expand_tilde, Config};
use crate::db::DbPool;
use crate::normalize::claude_code_line;
use crate::pricing::Pricing;

/// Session id = file stem; project fallback = parent dir name (line's cwd wins).
fn file_context(path: &Path) -> (Option<String>, Option<String>) {
    let session = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned());
    let project = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned());
    (session, project)
}

fn scan_one(pool: &DbPool, pricing: &Pricing, path: &Path) -> Result<usize> {
    let (session, project) = file_context(path);
    let raw = path.to_string_lossy().into_owned();
    let path_s = raw.clone();
    scan_file(pool, &path_s, |line| {
        claude_code_line(line, session.as_deref(), project.as_deref(), &raw, pricing)
    })
}

/// One-time backfill across all configured globs. Returns events inserted.
pub fn backfill(pool: &DbPool, cfg: &Config, pricing: &Pricing) -> Result<usize> {
    let mut total = 0usize;
    for glob_pat in &cfg.services.claude_code.log_globs {
        let expanded = expand_tilde(glob_pat);
        for entry in glob::glob(&expanded).into_iter().flatten().flatten() {
            match scan_one(pool, pricing, &entry) {
                Ok(n) => total += n,
                Err(e) => tracing::warn!("claude_code backfill {}: {e}", entry.display()),
            }
        }
    }
    Ok(total)
}

/// The longest leading path of a glob pattern with no wildcard component.
pub fn glob_root(pattern: &str) -> PathBuf {
    let expanded = expand_tilde(pattern);
    let mut root = PathBuf::new();
    for comp in Path::new(&expanded).components() {
        let s = comp.as_os_str().to_string_lossy();
        if s.contains('*') || s.contains('?') || s.contains('[') {
            break;
        }
        root.push(comp);
    }
    root
}

/// Watch roots for the file watcher (deduped).
pub fn watch_roots(cfg: &Config) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = cfg
        .services
        .claude_code
        .log_globs
        .iter()
        .map(|g| glob_root(g))
        .filter(|p| p.exists())
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Handle a changed path reported by the watcher.
pub fn on_change(pool: &DbPool, pricing: &Arc<Pricing>, path: &Path) -> Result<usize> {
    if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
        scan_one(pool, pricing, path)
    } else {
        Ok(0)
    }
}
