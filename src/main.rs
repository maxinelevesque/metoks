//! metoks — local-first unified AI usage & forecast dashboard.
//! `metoks init` creates the DB and prints loaded config + pricing count.
//! `metoks run`  starts collectors + scheduler + API on a single port.

mod api;
mod collectors;
mod config;
mod db;
mod forecast;
mod models;
mod normalize;
mod paths;
mod pricing;
mod scheduler;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use config::Config;

#[derive(Parser)]
#[command(name = "metoks", version, about = "Unified AI usage & forecast dashboard")]
struct Cli {
    /// Path to config.yaml (overrides METOKS_CONFIG and the standard config dir)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Path to the SQLite database (overrides METOKS_DB and the standard data dir)
    #[arg(long, global = true)]
    db: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create the DB, load config + pricing, print a summary.
    Init,
    /// Run all file-collectors once (backfill) and print per-service counts.
    Backfill,
    /// Print the current weekly forecast for each enabled service.
    Forecast,
    /// Compact one-shot status for every enabled service.
    Status,
    /// Live terminal dashboard of the weekly-budget plot.
    Tui,
    /// Log a ground-truth utilization reading (fiducial) and calibrate the cap
    /// from it (e.g. `metoks log claude_code 47`).
    #[command(visible_alias = "log")]
    Anchor {
        /// service: claude_code | codex | openrouter
        service: String,
        /// current percent used of the weekly plan, e.g. 47
        percent: f64,
        /// optional known reset time, ISO-8601 (e.g. 2026-08-28T00:00:00Z)
        #[arg(long)]
        resets_at: Option<String>,
    },
    /// List the raw ground-truth readings (fiducials) logged for a service.
    Fiducials {
        /// service: claude_code | codex | openrouter
        service: String,
    },
    /// Start collectors + scheduler + API.
    Run,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "metoks=info,warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(paths::config_file);
    let db_path = cli
        .db
        .clone()
        .unwrap_or_else(|| paths::db_file().to_string_lossy().into_owned());

    match cli.command {
        Commands::Init => cmd_init(&config_path, &db_path).await,
        Commands::Backfill => cmd_backfill(&config_path, &db_path).await,
        Commands::Forecast => cmd_forecast(&config_path, &db_path).await,
        Commands::Anchor {
            service,
            percent,
            resets_at,
        } => cmd_anchor(&config_path, &db_path, &service, percent, resets_at.as_deref()).await,
        Commands::Fiducials { service } => cmd_fiducials(&db_path, &service).await,
        Commands::Status => cmd_status(&config_path, &db_path).await,
        Commands::Tui => cmd_tui(&config_path, &db_path).await,
        Commands::Run => cmd_run(&config_path, &db_path).await,
    }
}

async fn cmd_init(config_path: &std::path::Path, db_path: &str) -> Result<()> {
    // Scaffold a starter config into the standard config dir on first run.
    if Config::scaffold_if_missing(config_path)? {
        println!("✓ wrote starter config to {}", config_path.display());
    }
    let cfg = Config::load_or_default(config_path)?;
    cfg.validate_keys()?;

    let _pool = db::open(db_path)?;
    println!("✓ database ready at {db_path}");

    let pricing = pricing::Pricing::load(&cfg.pricing_overrides).await?;
    println!(
        "✓ pricing loaded: {} models (cached at {})",
        pricing.len(),
        paths::pricing_cache().display()
    );

    println!("\nlocations:");
    println!("  config : {}", config_path.display());
    println!("  data   : {}", paths::data_dir().display());
    println!("  cache  : {}", paths::cache_dir().display());

    println!("\nloaded config:");
    println!("  timezone           : {}", cfg.timezone);
    println!("  poll_interval_secs : {}", cfg.poll_interval_seconds);
    println!("  port               : {}", cfg.port);
    println!("  forecast.model     : {}", cfg.forecast.model);
    println!("  services enabled   :");
    print_svc("claude_code", cfg.services.claude_code.enabled);
    print_svc("codex", cfg.services.codex.enabled);
    print_svc("openrouter", cfg.services.openrouter.enabled);
    print_svc("opencode", cfg.services.opencode.enabled);
    print_svc("anthropic_api", cfg.services.anthropic_api.enabled);
    print_svc("openai_api", cfg.services.openai_api.enabled);

    Ok(())
}

fn print_svc(name: &str, enabled: bool) {
    println!("    - {:<14} {}", name, if enabled { "on" } else { "off" });
}

async fn cmd_backfill(config_path: &std::path::Path, db_path: &str) -> Result<()> {
    let cfg = Config::load_or_default(config_path)?;
    cfg.validate_keys()?;
    let pool = db::open(db_path)?;
    let pricing = pricing::Pricing::load(&cfg.pricing_overrides).await?;

    if cfg.services.claude_code.enabled {
        let n = collectors::claude_code::backfill(&pool, &cfg, &pricing)?;
        println!("claude_code: +{n} new events");
    }
    if cfg.services.codex.enabled {
        let n = collectors::codex::backfill(&pool, &cfg, &pricing)?;
        println!("codex:       +{n} new events");
    }

    println!("\nper-service totals:");
    let conn = pool.get()?;
    for c in db::service_counts(&conn)? {
        println!(
            "  {:<14} events={:<7} tokens={:<14} cost_usd=${:.4}",
            c.service, c.events, c.tokens, c.cost_usd
        );
    }
    Ok(())
}

async fn cmd_forecast(config_path: &std::path::Path, db_path: &str) -> Result<()> {
    let cfg = Config::load_or_default(config_path)?;
    let pool = db::open(db_path)?;
    let now = chrono::Utc::now();
    for svc in forecast::enabled_services(&cfg) {
        let f = forecast::forecast_service(&pool, &cfg, svc, now)?;
        let fmt_amt = |v: f64| match f.unit {
            models::Unit::Usd => format!("${v:.2}"),
            models::Unit::Tokens => format!("{v:.0} tok"),
        };
        println!("── {} [{}]", f.service, f.unit.as_str());
        println!(
            "   consumed  {}   limit {} ({})",
            fmt_amt(f.consumed),
            f.limit.map(fmt_amt).unwrap_or_else(|| "—".into()),
            f.limit_source.as_deref().unwrap_or("none"),
        );
        println!(
            "   now {}   projected {} ({})   status {}",
            f.pct_now.map(|p| format!("{:.0}%", p * 100.0)).unwrap_or_else(|| "—".into()),
            f.pct_projected.map(|p| format!("{:.0}%", p * 100.0)).unwrap_or_else(|| "—".into()),
            f.forecast_model,
            f.status,
        );
        println!(
            "   window {} → {}   eta_to_limit {}",
            f.window_start.format("%Y-%m-%d %H:%M"),
            f.window_end.format("%Y-%m-%d %H:%M"),
            f.eta_to_limit
                .map(|e| e.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "—".into()),
        );
    }
    Ok(())
}

async fn cmd_anchor(
    config_path: &std::path::Path,
    db_path: &str,
    service: &str,
    percent: f64,
    resets_at: Option<&str>,
) -> Result<()> {
    let cfg = Config::load_or_default(config_path)?;
    let pool = db::open(db_path)?;
    let now = chrono::Utc::now();
    let reset = resets_at
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc));
    if resets_at.is_some() && reset.is_none() {
        anyhow::bail!("could not parse --resets-at (use ISO-8601, e.g. 2026-08-28T00:00:00Z)");
    }
    let cap = forecast::apply_anchor(&pool, &cfg, service, percent, reset, now)?;
    println!(
        "✓ logged fiducial: {service} at {percent:.0}% ({})",
        now.format("%Y-%m-%d %H:%M")
    );
    println!("  → weekly cap now derived from your readings ≈ {cap:.0} tokens");
    // Show the resulting forecast line.
    let f = forecast::forecast_service(&pool, &cfg, service, now)?;
    println!(
        "  now {}   projected {}   status {}",
        f.pct_now.map(|p| format!("{:.0}%", p * 100.0)).unwrap_or_else(|| "—".into()),
        f.pct_projected.map(|p| format!("{:.0}%", p * 100.0)).unwrap_or_else(|| "—".into()),
        f.status,
    );
    Ok(())
}

async fn cmd_fiducials(db_path: &str, service: &str) -> Result<()> {
    let pool = db::open(db_path)?;
    let conn = pool.get()?;
    let rows = db::list_fiducials(&conn, service, 50)?;
    if rows.is_empty() {
        println!("no fiducials logged for {service} yet");
        return Ok(());
    }
    println!("ground-truth readings for {service} (newest first):");
    for f in rows {
        println!(
            "  {}   {:>5.1}%   measured {:.0} tok{}",
            f.ts.format("%Y-%m-%d %H:%M"),
            f.percent,
            f.measured_cumulative,
            f.resets_at
                .map(|r| format!("   resets {}", r.format("%Y-%m-%d %H:%M")))
                .unwrap_or_default(),
        );
    }
    Ok(())
}

async fn cmd_status(config_path: &std::path::Path, db_path: &str) -> Result<()> {
    let cfg = Config::load_or_default(config_path)?;
    let pool = db::open(db_path)?;
    let now = chrono::Utc::now();
    for svc in forecast::enabled_services(&cfg) {
        let f = forecast::forecast_service(&pool, &cfg, svc, now)?;
        let conn = pool.get()?;
        let last = db::list_fiducials(&conn, svc, 1)?.into_iter().next();
        let pctf = |v: Option<f64>| v.map(|p| format!("{:.0}%", p * 100.0)).unwrap_or_else(|| "—".into());
        println!(
            "{:<12} {:<6} now {:<5} → proj {:<5} {}",
            svc,
            f.status,
            pctf(f.pct_now),
            if f.low_confidence { "—".into() } else { pctf(f.pct_projected) },
            match &last {
                Some(fd) => format!("(last reading {:.0}% {})", fd.percent, fd.ts.format("%m-%d %H:%M")),
                None => "(no readings logged)".into(),
            },
        );
    }
    Ok(())
}

async fn cmd_tui(config_path: &std::path::Path, db_path: &str) -> Result<()> {
    let cfg = Config::load_or_default(config_path)?;
    let pool = db::open(db_path)?;
    tui::run(&pool, &cfg)
}

async fn cmd_run(config_path: &std::path::Path, db_path: &str) -> Result<()> {
    let cfg = Config::load_or_default(config_path)?;
    cfg.validate_keys()?;
    let pool = db::open(db_path)?;
    tracing::info!("database: {db_path}");
    let pricing = scheduler::load_pricing(&cfg).await?;
    tracing::info!("pricing loaded: {} models", pricing.len());

    let port = cfg.port;
    let (tx, _rx) = tokio::sync::broadcast::channel::<String>(64);
    let state = api::AppState {
        pool,
        cfg: std::sync::Arc::new(cfg),
        tx,
        started: chrono::Utc::now(),
        cume_cache: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };

    // Keep the watcher alive for the whole process.
    let _watcher = scheduler::spawn(state.clone(), pricing)?;

    let app = api::router(state);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("metoks listening on http://{addr}  (Ctrl-C to stop)");
    axum::serve(listener, app).await?;
    Ok(())
}
