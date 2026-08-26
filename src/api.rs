//! Backend API (DESIGN.md §12). All JSON. Serves the built frontend at `/`.

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::{header, StatusCode, Uri},
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use include_dir::{include_dir, Dir};
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::config::Config;
use crate::db::{self, DbPool};
use crate::forecast;
use crate::models::Unit;

#[derive(Clone)]
pub struct AppState {
    pub pool: DbPool,
    pub cfg: Arc<Config>,
    pub tx: broadcast::Sender<String>,
    pub started: DateTime<Utc>,
    /// Per-service cache of the (heavy) cumulative payload; invalidated when the
    /// service's event/fiducial counts change, or after a short TTL.
    pub cume_cache: Arc<std::sync::Mutex<std::collections::HashMap<String, CumeCacheEntry>>>,
}

#[derive(Clone)]
pub struct CumeCacheEntry {
    key: (i64, i64),
    at: std::time::Instant,
    payload: serde_json::Value,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/services", get(services))
        .route("/timeseries", get(timeseries))
        .route("/forecast", get(forecast_ep))
        .route("/cumulative", get(cumulative_ep))
        .route("/snapshot", get(snapshot_ep))
        .route("/stream", get(stream_ep))
        .route("/breakdown", get(breakdown_ep))
        .route("/fiducials", get(fiducials_ep))
        .route("/limits", post(set_limit))
        .route("/anchor", post(anchor_ep))
        .with_state(state.clone());

    let mut app = Router::new().nest("/api", api);

    // Frontend: prefer an on-disk `static/` (dev: picks up `npm run build`
    // instantly); otherwise fall back to the copy embedded in the binary, so an
    // installed `metoks` serves the dashboard from any working directory.
    let static_dir = std::path::Path::new("static");
    if static_dir.exists() {
        let index = static_dir.join("index.html");
        app = app.fallback_service(
            tower_http::services::ServeDir::new("static")
                .not_found_service(tower_http::services::ServeFile::new(index)),
        );
    } else {
        app = app.fallback(embedded_asset);
    }

    // No CORS layer on purpose: the API is bound to localhost and the frontend is
    // served same-origin (dev goes through vite's same-origin proxy), so no
    // cross-origin access is needed. Omitting it means other websites the user
    // visits can't read their usage data or POST readings to the local API.
    app
}

// ---- embedded frontend (for installed binaries) ---------------------------

/// The built frontend, embedded at compile time. Requires `frontend/` to have
/// been built into `static/` before `cargo build`.
static STATIC_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/static");

async fn embedded_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(file) = STATIC_DIR.get_file(path) {
        return serve_bytes(path, file.contents());
    }
    // SPA fallback: unknown paths render index.html.
    match STATIC_DIR.get_file("index.html") {
        Some(file) => serve_bytes("index.html", file.contents()),
        None => (StatusCode::NOT_FOUND, "frontend not embedded").into_response(),
    }
}

fn serve_bytes(path: &str, bytes: &'static [u8]) -> Response {
    ([(header::CONTENT_TYPE, content_type(path))], bytes).into_response()
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

// ---- /api/health ----------------------------------------------------------

async fn health(State(st): State<AppState>) -> impl IntoResponse {
    let db_ok = st.pool.get().is_ok();
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "db_ok": db_ok,
        "uptime_seconds": (Utc::now() - st.started).num_seconds(),
    }))
}

// ---- /api/services --------------------------------------------------------

async fn services(State(st): State<AppState>) -> impl IntoResponse {
    match build_services(&st) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(e),
    }
}

fn build_services(st: &AppState) -> Result<serde_json::Value> {
    let conn = st.pool.get()?;
    let counts = db::service_counts(&conn)?;
    let cfg = &st.cfg;
    let mut out = Vec::new();
    let entries: [(&str, bool, &str); 3] = [
        (crate::models::SERVICE_CLAUDE_CODE, cfg.services.claude_code.enabled, "subscription"),
        (crate::models::SERVICE_CODEX, cfg.services.codex.enabled, "subscription"),
        (crate::models::SERVICE_OPENROUTER, cfg.services.openrouter.enabled, "pay_per_token"),
    ];
    for (svc, enabled, mode) in entries {
        let c = counts.iter().find(|c| c.service == svc);
        let unit = forecast::service_unit(cfg, svc);
        let rl_weekly = db::get_rate_limit(&conn, svc, "weekly")?;
        let rl_session = db::get_rate_limit(&conn, svc, "session")?;
        out.push(json!({
            "service": svc,
            "enabled": enabled,
            "metered_mode": mode,
            "unit": unit.as_str(),
            "events": c.map(|c| c.events).unwrap_or(0),
            "tokens": c.map(|c| c.tokens).unwrap_or(0),
            "cost_usd": c.map(|c| c.cost_usd).unwrap_or(0.0),
            "rate_limit_weekly": rl_weekly,
            "rate_limit_session": rl_session,
        }));
    }
    Ok(json!({ "services": out }))
}

// ---- /api/timeseries ------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TimeseriesQuery {
    service: Option<String>,
    from: Option<String>,
    to: Option<String>,
    #[serde(default)]
    bucket: Option<String>,
    /// group series by "service" (default) or "model"
    #[serde(default)]
    by: Option<String>,
}

#[derive(Debug, Serialize)]
struct BucketRow {
    ts: String,
    /// the series key: a service name or a model id, depending on `by`
    key: String,
    tokens: i64,
    cost_usd: f64,
}

async fn timeseries(
    State(st): State<AppState>,
    Query(q): Query<TimeseriesQuery>,
) -> impl IntoResponse {
    match build_timeseries(&st, &q) {
        Ok(v) => Json(json!({ "buckets": v })).into_response(),
        Err(e) => err(e),
    }
}

fn parse_dt_opt(s: &Option<String>, default: DateTime<Utc>) -> DateTime<Utc> {
    s.as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or(default)
}

/// Truncate a UTC instant to the start of its local hour/day, returned as UTC.
fn bucket_start(ts: DateTime<Utc>, tz: Tz, daily: bool) -> DateTime<Utc> {
    let l = ts.with_timezone(&tz);
    let truncated = if daily {
        tz.with_ymd_and_hms(l.year(), l.month(), l.day(), 0, 0, 0)
    } else {
        tz.with_ymd_and_hms(l.year(), l.month(), l.day(), l.hour(), 0, 0)
    };
    truncated.single().unwrap_or(l).with_timezone(&Utc)
}

fn build_timeseries(st: &AppState, q: &TimeseriesQuery) -> Result<Vec<BucketRow>> {
    let tz: Tz = st.cfg.timezone.parse().unwrap_or(chrono_tz::UTC);
    let now = Utc::now();
    let from = parse_dt_opt(&q.from, now - Duration::days(30));
    let to = parse_dt_opt(&q.to, now);
    let daily = q.bucket.as_deref() != Some("hour"); // default: day
    // Series key column: model or service (validated against a fixed allowlist so
    // it can be interpolated into SQL safely).
    let key_col = if q.by.as_deref() == Some("model") {
        "COALESCE(model,'(unknown)')"
    } else {
        "service"
    };
    let tok = "input_tokens+output_tokens+cache_read_tokens+cache_write_tokens+reasoning_tokens";

    let conn = st.pool.get()?;
    let (sql, has_filter) = match &q.service {
        Some(_) => (
            format!("SELECT ts, {key_col}, {tok}, cost_usd FROM events WHERE ts>=?1 AND ts<=?2 AND service=?3"),
            true,
        ),
        None => (
            format!("SELECT ts, {key_col}, {tok}, cost_usd FROM events WHERE ts>=?1 AND ts<=?2"),
            false,
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let map = |r: &rusqlite::Row| -> rusqlite::Result<(String, String, i64, f64)> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    };
    let rows: Vec<(String, String, i64, f64)> = if has_filter {
        stmt.query_map(
            rusqlite::params![from.to_rfc3339(), to.to_rfc3339(), q.service.as_ref().unwrap()],
            map,
        )?
        .collect::<std::result::Result<_, _>>()?
    } else {
        stmt.query_map(rusqlite::params![from.to_rfc3339(), to.to_rfc3339()], map)?
            .collect::<std::result::Result<_, _>>()?
    };

    use std::collections::BTreeMap;
    let mut agg: BTreeMap<(String, String), (i64, f64)> = BTreeMap::new();
    for (ts_s, key, tokens, cost) in rows {
        if let Ok(ts) = DateTime::parse_from_rfc3339(&ts_s) {
            let b = bucket_start(ts.with_timezone(&Utc), tz, daily).to_rfc3339();
            let e = agg.entry((b, key)).or_insert((0, 0.0));
            e.0 += tokens;
            e.1 += cost;
        }
    }
    Ok(agg
        .into_iter()
        .map(|((ts, key), (tokens, cost_usd))| BucketRow {
            ts,
            key,
            tokens,
            cost_usd,
        })
        .collect())
}

// ---- /api/forecast --------------------------------------------------------

async fn forecast_ep(State(st): State<AppState>) -> impl IntoResponse {
    match build_forecast(&st) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(e),
    }
}

fn build_forecast(st: &AppState) -> Result<serde_json::Value> {
    let now = Utc::now();
    let mut forecasts = Vec::new();
    let mut cumulatives = Vec::new();
    for svc in forecast::enabled_services(&st.cfg) {
        forecasts.push(forecast::forecast_service(&st.pool, &st.cfg, svc, now)?);
        cumulatives.push(cumulative_cached(st, svc, now)?);
    }
    Ok(json!({
        "forecasts": forecasts,
        "cumulatives": cumulatives,
        "generated_at": now.to_rfc3339(),
    }))
}

/// Cumulative payload with caching: recomputed only when the service's event or
/// fiducial counts change, or after a 60s TTL.
fn cumulative_cached(st: &AppState, service: &str, now: DateTime<Utc>) -> Result<serde_json::Value> {
    let conn = st.pool.get()?;
    let ev: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE service=?1",
        [service],
        |r| r.get(0),
    )?;
    let fd: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fiducials WHERE service=?1",
        [service],
        |r| r.get(0),
    )?;
    drop(conn);
    let key = (ev, fd);

    if let Ok(cache) = st.cume_cache.lock() {
        if let Some(e) = cache.get(service) {
            if e.key == key && e.at.elapsed() < std::time::Duration::from_secs(60) {
                return Ok(e.payload.clone());
            }
        }
    }
    let c = forecast::cumulative_view(&st.pool, &st.cfg, service, now)?;
    let payload = serde_json::to_value(&c)?;
    if let Ok(mut cache) = st.cume_cache.lock() {
        cache.insert(
            service.to_string(),
            CumeCacheEntry { key, at: std::time::Instant::now(), payload: payload.clone() },
        );
    }
    Ok(payload)
}

// ---- /api/cumulative ------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CumulativeQuery {
    service: String,
}

async fn cumulative_ep(
    State(st): State<AppState>,
    Query(q): Query<CumulativeQuery>,
) -> impl IntoResponse {
    match cumulative_cached(&st, &q.service, Utc::now()) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(e),
    }
}

// ---- /api/fiducials -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FiducialsQuery {
    service: String,
    #[serde(default)]
    limit: Option<i64>,
}

async fn fiducials_ep(
    State(st): State<AppState>,
    Query(q): Query<FiducialsQuery>,
) -> impl IntoResponse {
    let conn = match st.pool.get() {
        Ok(c) => c,
        Err(e) => return err(e.into()),
    };
    match db::list_fiducials(&conn, &q.service, q.limit.unwrap_or(50)) {
        Ok(rows) => Json(json!({ "fiducials": rows })).into_response(),
        Err(e) => err(e),
    }
}

// ---- POST /api/anchor -----------------------------------------------------

#[derive(Debug, Deserialize)]
struct AnchorBody {
    service: String,
    /// current percent-used of the weekly plan, in (0, 100]
    percent: f64,
    /// optional known reset time (ISO-8601)
    #[serde(default)]
    resets_at: Option<String>,
}

async fn anchor_ep(State(st): State<AppState>, Json(body): Json<AnchorBody>) -> impl IntoResponse {
    let reset = body
        .resets_at
        .as_ref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    match forecast::apply_anchor(&st.pool, &st.cfg, &body.service, body.percent, reset, Utc::now()) {
        Ok(cap) => {
            if let Ok(snap) = build_snapshot(&st) {
                let _ = st.tx.send(snap.to_string());
            }
            Json(json!({ "ok": true, "cap": cap })).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

// ---- /api/snapshot --------------------------------------------------------

async fn snapshot_ep(State(st): State<AppState>) -> impl IntoResponse {
    match build_snapshot(&st) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(e),
    }
}

/// One object powering the whole dashboard.
pub fn build_snapshot(st: &AppState) -> Result<serde_json::Value> {
    let services = build_services(st)?;
    let forecast = build_forecast(st)?;
    Ok(json!({
        "services": services.get("services").cloned().unwrap_or(json!([])),
        "forecast": forecast,
        "generated_at": Utc::now().to_rfc3339(),
    }))
}

// ---- /api/stream (SSE) ----------------------------------------------------

async fn stream_ep(
    State(st): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = st.tx.subscribe();
    // Send an initial snapshot immediately, then stream broadcast updates.
    let initial = build_snapshot(&st).unwrap_or_else(|_| json!({}));
    let init_stream = tokio_stream::once(Ok(SseEvent::default()
        .event("snapshot")
        .data(initial.to_string())));

    let updates = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(data) => Some(Ok(SseEvent::default().event("snapshot").data(data))),
        Err(_) => None, // lagged; skip
    });

    let stream = init_stream.chain(updates);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---- /api/breakdown -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BreakdownQuery {
    #[serde(default)]
    days: Option<i64>,
}

async fn breakdown_ep(
    State(st): State<AppState>,
    Query(q): Query<BreakdownQuery>,
) -> impl IntoResponse {
    match build_breakdown(&st, q.days.unwrap_or(7)) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(e),
    }
}

fn group_sum(
    conn: &rusqlite::Connection,
    col: &str,
    since: &str,
) -> Result<Vec<serde_json::Value>> {
    let sql = format!(
        "SELECT service, COALESCE({col},'(unknown)') AS k,
                COALESCE(SUM(input_tokens+output_tokens+cache_read_tokens+cache_write_tokens+reasoning_tokens),0) AS tokens,
                COALESCE(SUM(cost_usd),0) AS cost, COUNT(*) AS events
         FROM events WHERE ts>=?1
         GROUP BY service, k ORDER BY tokens DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params![since], |r| {
            Ok(json!({
                "service": r.get::<_, String>(0)?,
                "key": r.get::<_, String>(1)?,
                "tokens": r.get::<_, i64>(2)?,
                "cost_usd": r.get::<_, f64>(3)?,
                "events": r.get::<_, i64>(4)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn build_breakdown(st: &AppState, days: i64) -> Result<serde_json::Value> {
    let since = (Utc::now() - Duration::days(days.max(1))).to_rfc3339();
    let conn = st.pool.get()?;
    Ok(json!({
        "days": days,
        "by_model": group_sum(&conn, "model", &since)?,
        "by_project": group_sum(&conn, "project", &since)?,
    }))
}

// ---- POST /api/limits -----------------------------------------------------

#[derive(Debug, Deserialize)]
struct SetLimitBody {
    service: String,
    window_kind: String,
    value: f64,
    unit: String,
    #[serde(default = "default_source")]
    source: String,
}
fn default_source() -> String {
    "configured".to_string()
}

async fn set_limit(
    State(st): State<AppState>,
    Json(body): Json<SetLimitBody>,
) -> impl IntoResponse {
    let unit = match Unit::parse(&body.unit) {
        Some(u) => u,
        None => return (StatusCode::BAD_REQUEST, "invalid unit").into_response(),
    };
    let conn = match st.pool.get() {
        Ok(c) => c,
        Err(e) => return err(e.into()),
    };
    if let Err(e) = db::upsert_limit(
        &conn,
        &body.service,
        &body.window_kind,
        Some(body.value),
        unit,
        &body.source,
        None,
        None,
    ) {
        return err(e);
    }
    // push a refreshed snapshot to any listeners
    if let Ok(snap) = build_snapshot(&st) {
        let _ = st.tx.send(snap.to_string());
    }
    Json(json!({ "ok": true })).into_response()
}

// ---- helpers --------------------------------------------------------------

fn err(e: anyhow::Error) -> axum::response::Response {
    tracing::error!("api error: {e:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}
