//! Config loading + validation (DESIGN.md §7).
//!
//! API keys are read ONLY from the env var named in config — never inlined,
//! never persisted. We validate on startup and fail loudly if an enabled
//! service's key env is missing.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

use crate::models::Unit;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub services: Services,
    #[serde(default)]
    pub forecast: ForecastConfig,
    /// Optional per-model pricing overrides: model_id -> {input,output,cache_read,cache_write}
    #[serde(default)]
    pub pricing_overrides: std::collections::HashMap<String, PriceOverride>,
}

fn default_poll_interval() -> u64 {
    60
}
fn default_timezone() -> String {
    "UTC".to_string()
}
fn default_port() -> u16 {
    8787
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Services {
    #[serde(default)]
    pub claude_code: ClaudeCodeCfg,
    #[serde(default)]
    pub codex: CodexCfg,
    #[serde(default)]
    pub openrouter: OpenRouterCfg,
    #[serde(default)]
    pub opencode: SimpleToggle,
    #[serde(default)]
    pub anthropic_api: ApiKeyCfg,
    #[serde(default)]
    pub openai_api: ApiKeyCfg,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeCodeCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_claude_globs")]
    pub log_globs: Vec<String>,
    pub weekly_limit: Option<LimitCfg>,
}

impl Default for ClaudeCodeCfg {
    fn default() -> Self {
        ClaudeCodeCfg {
            enabled: false,
            log_globs: default_claude_globs(),
            weekly_limit: None,
        }
    }
}

fn default_claude_globs() -> Vec<String> {
    vec!["~/.claude/projects/**/*.jsonl".to_string()]
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CodexCfg {
    #[serde(default)]
    pub enabled: bool,
    /// null → $CODEX_HOME or ~/.codex. May be a comma-separated list of roots.
    #[serde(default)]
    pub codex_home: Option<String>,
    pub weekly_limit: Option<LimitCfg>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenRouterCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_openrouter_env")]
    pub api_key_env: String,
    pub weekly_budget: Option<LimitCfg>,
}

fn default_openrouter_env() -> String {
    "OPENROUTER_API_KEY".to_string()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SimpleToggle {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiKeyCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub admin_key_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitCfg {
    pub value: f64,
    pub unit: String,
    #[serde(default = "default_limit_source")]
    pub source: String,
}

fn default_limit_source() -> String {
    "configured".to_string()
}

impl LimitCfg {
    pub fn unit_parsed(&self) -> Result<Unit> {
        Unit::parse(&self.unit).with_context(|| format!("invalid limit unit: {}", self.unit))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceOverride {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForecastConfig {
    #[serde(default = "default_forecast_model")]
    pub model: String,
    #[serde(default = "default_warn")]
    pub warn_threshold: f64,
    #[serde(default = "default_danger")]
    pub danger_threshold: f64,
}

impl Default for ForecastConfig {
    fn default() -> Self {
        ForecastConfig {
            model: default_forecast_model(),
            warn_threshold: default_warn(),
            danger_threshold: default_danger(),
        }
    }
}

fn default_forecast_model() -> String {
    "point_process".to_string()
}
fn default_warn() -> f64 {
    0.80
}
fn default_danger() -> f64 {
    1.00
}

impl Config {
    /// Starter config template, embedded at build time (single source of truth
    /// with the repo's `config.yaml`).
    pub const SAMPLE_YAML: &'static str = include_str!("../config.yaml");

    /// Write the starter config to `path` if it doesn't exist. Returns true if a
    /// file was created.
    pub fn scaffold_if_missing(path: &Path) -> Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        std::fs::write(path, Self::SAMPLE_YAML)
            .with_context(|| format!("writing starter config to {}", path.display()))?;
        Ok(true)
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let cfg: Config =
            serde_yaml_ng::from_str(&text).with_context(|| "parsing config yaml")?;
        Ok(cfg)
    }

    /// Load config if present, else a sensible default (claude_code enabled).
    pub fn load_or_default(path: &Path) -> Result<Config> {
        if path.exists() {
            Config::load(path)
        } else {
            Ok(Config::sample())
        }
    }

    /// Validate that every enabled service that needs a key has that env var set.
    /// Returns resolved key values keyed by env name (never logged/persisted).
    pub fn validate_keys(&self) -> Result<()> {
        if self.services.openrouter.enabled {
            let env = &self.services.openrouter.api_key_env;
            if std::env::var(env).map(|v| v.is_empty()).unwrap_or(true) {
                bail!(
                    "openrouter enabled but env var {env} is missing/empty (set it or disable the service)"
                );
            }
        }
        if self.services.anthropic_api.enabled {
            let env = self
                .services
                .anthropic_api
                .admin_key_env
                .as_deref()
                .unwrap_or("ANTHROPIC_ADMIN_KEY");
            if std::env::var(env).map(|v| v.is_empty()).unwrap_or(true) {
                bail!("anthropic_api enabled but env var {env} is missing/empty");
            }
        }
        if self.services.openai_api.enabled {
            let env = self
                .services
                .openai_api
                .api_key_env
                .as_deref()
                .unwrap_or("OPENAI_API_KEY");
            if std::env::var(env).map(|v| v.is_empty()).unwrap_or(true) {
                bail!("openai_api enabled but env var {env} is missing/empty");
            }
        }
        Ok(())
    }

    /// A reasonable default config used when no config.yaml exists.
    pub fn sample() -> Config {
        Config {
            poll_interval_seconds: 60,
            timezone: "America/Los_Angeles".to_string(),
            port: 8787,
            services: Services {
                claude_code: ClaudeCodeCfg {
                    enabled: true,
                    ..Default::default()
                },
                codex: CodexCfg {
                    enabled: true,
                    ..Default::default()
                },
                openrouter: OpenRouterCfg {
                    enabled: false,
                    ..Default::default()
                },
                ..Default::default()
            },
            forecast: ForecastConfig::default(),
            pricing_overrides: Default::default(),
        }
    }
}

/// Expand a leading `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}
