//! Scheduler: startup backfill, file watchers (Claude Code + Codex), the
//! OpenRouter poller, and periodic snapshot broadcasts over SSE.

use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::api::{build_snapshot, AppState};
use crate::collectors::{claude_code, codex, openrouter};
use crate::config::Config;
use crate::pricing::Pricing;

/// Kick off all background work. Returns the notify watcher, which the caller
/// must keep alive for the process lifetime.
pub fn spawn(state: AppState, pricing: Arc<Pricing>) -> Result<Box<dyn Watcher + Send>> {
    let cfg = state.cfg.clone();

    // 1. One-time backfill (blocking) before watching.
    {
        let pool = state.pool.clone();
        let cfg_b = cfg.clone();
        let pricing_b = pricing.clone();
        let mut n = 0usize;
        if cfg_b.services.claude_code.enabled {
            match claude_code::backfill(&pool, &cfg_b, &pricing_b) {
                Ok(c) => n += c,
                Err(e) => tracing::warn!("claude_code backfill: {e}"),
            }
        }
        if cfg_b.services.codex.enabled {
            match codex::backfill(&pool, &cfg_b, &pricing_b) {
                Ok(c) => n += c,
                Err(e) => tracing::warn!("codex backfill: {e}"),
            }
        }
        tracing::info!("backfill inserted {n} new events");
        broadcast(&state);
    }

    // 2. File watcher → incremental scans on change.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            for p in ev.paths {
                let _ = tx.send(p);
            }
        }
    })?;

    let mut roots: Vec<PathBuf> = Vec::new();
    if cfg.services.claude_code.enabled {
        roots.extend(claude_code::watch_roots(&cfg));
    }
    if cfg.services.codex.enabled {
        roots.extend(codex::watch_roots(&cfg));
    }
    roots.sort();
    roots.dedup();
    for root in &roots {
        if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
            tracing::warn!("watch {}: {e}", root.display());
        } else {
            tracing::info!("watching {}", root.display());
        }
    }

    // 3. Task that drains file events and scans.
    {
        let state = state.clone();
        let pricing = pricing.clone();
        tokio::spawn(async move {
            while let Some(path) = rx.recv().await {
                let mut inserted = 0usize;
                if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                    if let Ok(n) = claude_code::on_change(&state.pool, &pricing, &path) {
                        inserted += n;
                    }
                    if let Ok(n) = codex::on_change(&state.pool, &pricing, &path) {
                        inserted += n;
                    }
                }
                if inserted > 0 {
                    broadcast(&state);
                }
            }
        });
    }

    // 4. OpenRouter poller.
    if cfg.services.openrouter.enabled {
        if let Ok(client) = openrouter::client() {
            let env = cfg.services.openrouter.api_key_env.clone();
            let interval = cfg.poll_interval_seconds.max(5);
            let state = state.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(interval));
                loop {
                    tick.tick().await;
                    let key = std::env::var(&env).unwrap_or_default();
                    if key.is_empty() {
                        continue;
                    }
                    match openrouter::poll_once(&state.pool, &client, &key).await {
                        Ok(Some(_ev)) => broadcast(&state),
                        Ok(None) => {}
                        Err(e) => tracing::warn!("openrouter poll: {e}"),
                    }
                }
            });
        }
    }

    // 5. Periodic snapshot broadcast (keeps SSE clients fresh as time advances).
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tick.tick().await;
                broadcast(&state);
            }
        });
    }

    Ok(Box::new(watcher))
}

fn broadcast(state: &AppState) {
    // Only build/send if someone is listening.
    if state.tx.receiver_count() == 0 {
        return;
    }
    match build_snapshot(state) {
        Ok(snap) => {
            let _ = state.tx.send(snap.to_string());
        }
        Err(e) => tracing::warn!("snapshot build for broadcast: {e}"),
    }
}

/// Convenience for `cmd_run`: load pricing.
pub async fn load_pricing(cfg: &Config) -> Result<Arc<Pricing>> {
    Ok(Arc::new(Pricing::load(&cfg.pricing_overrides).await?))
}
