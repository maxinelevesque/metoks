//! Standard per-user locations for config, data, and cache.
//!
//! Resolves to the OS conventions via the `dirs` crate, under an `metoks/`
//! subdirectory:
//!   - macOS:  ~/Library/Application Support/metoks (config + data),
//!             ~/Library/Caches/metoks (cache)
//!   - Linux:  $XDG_CONFIG_HOME/metoks, $XDG_DATA_HOME/metoks,
//!             $XDG_CACHE_HOME/metoks
//!
//! Every location can be overridden by an env var so nothing is ever forced onto
//! the standard dirs when a user wants otherwise.

use std::path::PathBuf;

const APP: &str = "metoks";

/// `<base>/metoks`, falling back to `~/.metoks` if the OS dir is unknown.
fn app_dir(base: Option<PathBuf>) -> PathBuf {
    base.map(|d| d.join(APP)).unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".metoks")
    })
}

/// Config file: `$METOKS_CONFIG` → `<config_dir>/metoks/config.yaml`.
pub fn config_file() -> PathBuf {
    if let Ok(p) = std::env::var("METOKS_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    app_dir(dirs::config_dir()).join("config.yaml")
}

/// Data directory: `$METOKS_DATA_DIR` → `<data_dir>/metoks`.
pub fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("METOKS_DATA_DIR") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    app_dir(dirs::data_dir())
}

/// Database file: `$METOKS_DB` → `<data_dir>/metoks/metoks.db`.
pub fn db_file() -> PathBuf {
    if let Ok(p) = std::env::var("METOKS_DB") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    data_dir().join("metoks.db")
}

/// Cache directory: `$METOKS_CACHE_DIR` → `<cache_dir>/metoks`.
pub fn cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("METOKS_CACHE_DIR") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    app_dir(dirs::cache_dir())
}

/// Cached LiteLLM pricing dataset.
pub fn pricing_cache() -> PathBuf {
    cache_dir().join("pricing.json")
}
