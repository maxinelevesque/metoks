# DESIGN.md — Unified AI Usage & Forecast Dashboard

> **Working name:** `agentmeter`
> **Purpose:** A local-first tool that aggregates token/credit usage across all of your AI
> coding services (Claude Code, Codex CLI, OpenRouter, opencode, and optionally direct
> Anthropic/OpenAI API keys), stores a unified time series, renders a real-time dashboard of
> usage over time, and **forecasts** end-of-window consumption so you stay under your weekly
> limits.

---

## 0. How to use this document (note to the implementing agent)

This spec is opinionated on stack and structure so you can move fast, but **three data
formats below are described from public docs, not from a machine you control.** Before you
write a parser against any of them, **verify the real shape empirically**:

```bash
# Claude Code — inspect one real session line
find ~/.claude/projects -name '*.jsonl' | head -1 | xargs -I{} sh -c 'head -c 4000 "{}"; echo'

# Codex CLI — inspect one real rollout line (respect CODEX_HOME if set)
find "${CODEX_HOME:-$HOME/.codex}/sessions" -name 'rollout-*.jsonl' | head -1 | xargs -I{} sh -c 'head -c 4000 "{}"; echo'
```

Treat the JSON shapes in §5 as a **starting hypothesis**, and write the parsers defensively
(skip lines you can't classify rather than crashing). Format versions have already changed
multiple times for both tools.

Build in the phase order given in §14. Each phase has acceptance criteria; a phase is "done"
when a human can run it and see the stated output. Do not start the frontend before the
collectors produce real rows in the DB.

**Security:** never read, log, print, or transmit `~/.codex/auth.json`, `~/.claude/.credentials`,
or any API key value. Keys are read from env/config only and are never written to the DB or
to logs.

---

## 1. Goals, scope, non-goals

### Goals
- One local dashboard showing **usage over time** across every configured service.
- Per-service and combined **weekly-window tracking** with a **burn-rate forecast**:
  "at your current pace you'll hit ~87% of your Claude weekly cap by Thursday 6pm."
- Works whether a service is **subscription-metered** (flat rate, tokens only) or
  **pay-per-token** (real dollar spend).
- Runs entirely on the user's machine; no usage data leaves the box.

### Non-goals (v1)
- No team/multi-user aggregation. Single user, single machine.
- No attempt to *bypass or raise* provider limits — read-only visibility.
- No historical backfill of data the sources never recorded (e.g. Codex sessions before
  token events existed).
- No mobile app. Responsive web is enough.

---

## 2. Glossary & key concepts

| Term | Meaning |
|---|---|
| **Service** | A source of usage: `claude_code`, `codex`, `openrouter`, `opencode`, `anthropic_api`, `openai_api`. |
| **Metered mode** | `subscription` (tokens only, dollar figures are API-equivalent *estimates*) or `pay_per_token` (real spend). |
| **Event** | One normalized usage record (see schema §4). |
| **Window** | A rolling limit period. Two kinds matter: `weekly` (7-day) and `session` (5-hour rolling, Claude/Codex subscriptions). v1 focuses on `weekly`; model `session` too if cheap. |
| **Limit** | The cap for a window. `source = "real"` if read from the service, `"configured"` if the user set a target manually. |
| **Burn rate** | Consumption per unit time within the current window. |

**Critical framing the agent must respect:** for subscription services (Claude Max, ChatGPT/Codex),
**there is no public API that returns "you've used X% of your weekly cap."** The CLIs surface it
interactively (`/usage`, `/status`) but reset timestamps and true caps largely live on the
provider's web account page. So the dashboard's weekly tracking for those services is built by
**reconstructing consumption from local logs** and comparing against a **limit the user configures
or that we auto-detect** (see §10). Pay-per-token services (OpenRouter) expose real numbers via API.

---

## 3. Architecture

```
                       ┌─────────────────────────────────────────────┐
                       │                 agentmeter                   │
                       │                                              │
  local JSONL logs     │   ┌──────────────┐      ┌────────────────┐   │
  ~/.claude/projects ──┼──▶│  Collectors  │      │                │   │
  ~/.codex/sessions  ──┼──▶│  (file-tail  │─────▶│   Normalizer   │   │
                       │   │   + pollers) │      │ (→ Event rows) │   │
  hosted usage APIs    │   │              │      │                │   │
  OpenRouter /credits ─┼──▶│              │      └───────┬────────┘   │
  Anthropic/OpenAI ────┼──▶└──────────────┘              │            │
                       │                                 ▼            │
                       │   ┌──────────────┐      ┌────────────────┐   │
                       │   │  Forecaster  │◀─────│  SQLite store  │   │
                       │   │ (burn rate)  │      │  (events,      │   │
                       │   └──────┬───────┘      │   windows,     │   │
                       │          │              │   limits)      │   │
                       │          ▼              └───────┬────────┘   │
                       │   ┌──────────────────────────────────────┐  │
                       │   │      FastAPI  (REST + SSE)            │  │
                       │   └───────────────────┬──────────────────┘  │
                       └───────────────────────┼─────────────────────┘
                                                ▼
                                   React SPA dashboard (Recharts)
                                   usage-over-time + forecast gauges
```

**Five components:**
1. **Collectors** — one per source; file-tailers for local logs, interval pollers for APIs.
2. **Normalizer** — maps every raw record to the common `Event` shape; attaches cost from the pricing table.
3. **Store** — SQLite. Append-only events + config tables.
4. **Forecaster** — reads current-window events, projects end-of-window totals.
5. **API + frontend** — FastAPI serves REST snapshots (and an SSE stream) to a React dashboard.

---

## 4. Data model (SQLite)

Use plain `sqlite3` (stdlib) or SQLModel. Schema:

```sql
-- One row per usage event. Append-only. Dedupe on event_uid.
CREATE TABLE IF NOT EXISTS events (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    event_uid             TEXT UNIQUE NOT NULL,   -- stable hash; see dedupe §8
    service               TEXT NOT NULL,          -- 'claude_code' | 'codex' | 'openrouter' | ...
    metered_mode          TEXT NOT NULL,          -- 'subscription' | 'pay_per_token'
    ts                    TEXT NOT NULL,          -- ISO-8601 UTC event time
    model                 TEXT,                   -- e.g. 'claude-opus-4-8', 'gpt-x'
    input_tokens          INTEGER DEFAULT 0,
    output_tokens         INTEGER DEFAULT 0,
    cache_read_tokens     INTEGER DEFAULT 0,
    cache_write_tokens    INTEGER DEFAULT 0,
    reasoning_tokens      INTEGER DEFAULT 0,
    cost_usd              REAL DEFAULT 0,          -- real for pay_per_token; est. for subscription
    cost_is_estimate      INTEGER DEFAULT 0,      -- 1 if API-equivalent estimate
    session_id            TEXT,                   -- source session/rollout id if available
    project               TEXT,                   -- repo/project if derivable from path
    raw_source            TEXT,                   -- file path or api endpoint, for debugging
    created_at            TEXT DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_events_service_ts ON events(service, ts);
CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);

-- Per-service window/limit config. Weekly focus in v1.
CREATE TABLE IF NOT EXISTS limits (
    service        TEXT NOT NULL,
    window_kind    TEXT NOT NULL,        -- 'weekly' | 'session'
    limit_value    REAL,                 -- in the unit below
    limit_unit     TEXT NOT NULL,        -- 'tokens' | 'usd'
    limit_source   TEXT NOT NULL,        -- 'real' | 'configured' | 'autodetected'
    window_reset   TEXT,                 -- ISO-8601 of next reset if known, else NULL (rolling)
    window_anchor  TEXT,                 -- start of current window if fixed, else NULL
    updated_at     TEXT DEFAULT (datetime('now')),
    PRIMARY KEY (service, window_kind)
);

-- Snapshots from pollers that report cumulative totals (OpenRouter credits, etc).
-- We diff consecutive rows to derive per-interval spend when no per-event data exists.
CREATE TABLE IF NOT EXISTS cumulative_snapshots (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    service       TEXT NOT NULL,
    ts            TEXT NOT NULL,
    total_usage   REAL NOT NULL,         -- cumulative usd (or tokens) reported by the API
    total_limit   REAL,                  -- cumulative credits/limit if reported
    unit          TEXT NOT NULL,         -- 'usd' | 'tokens'
    raw           TEXT                   -- json blob of the raw response (no secrets)
);
CREATE INDEX IF NOT EXISTS idx_snap_service_ts ON cumulative_snapshots(service, ts);
```

**Normalized `Event` (Python dataclass / Pydantic):**

```python
class Event(BaseModel):
    event_uid: str
    service: str
    metered_mode: Literal["subscription", "pay_per_token"]
    ts: datetime                      # UTC
    model: str | None = None
    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_write_tokens: int = 0
    reasoning_tokens: int = 0
    cost_usd: float = 0.0
    cost_is_estimate: bool = False
    session_id: str | None = None
    project: str | None = None
    raw_source: str | None = None
```

---

## 5. Data sources (verify shapes per §0)

### 5.1 Claude Code — local JSONL  *(metered_mode: subscription; cost = estimate)*
- **Location:** `~/.claude/projects/<encoded-project-path>/<session-id>.jsonl`
- **Format:** one JSON object per line; the assistant/message lines carry a `usage` object with
  `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, and a
  `model` field. Timestamps present per line.
- **Extract:** map `cache_creation_input_tokens → cache_write_tokens`,
  `cache_read_input_tokens → cache_read_tokens`. Derive `project` from the directory name.
  `session_id` from the file name. `ts` from the line's timestamp.
- **Cost:** compute from pricing table (§9); mark `cost_is_estimate=True`.
- **Gotchas:** lines without `usage` (user turns, tool results) are skipped. Some early builds
  lack model metadata — skip those events (don't guess the model).

### 5.2 Codex CLI — local JSONL  *(metered_mode: subscription; cost = estimate)*
- **Location:** `${CODEX_HOME:-~/.codex}/sessions/YYYY/MM/DD/rollout-<session-id>.jsonl`
  and `${CODEX_HOME:-~/.codex}/archived_sessions/...`. `CODEX_HOME` may be a comma-separated
  list of roots — handle each independently.
- **Format:** JSONL event stream. Token usage appears in `token_count` events; the active
  `turn_context` specifies the model for subsequently counted usage. Token events only exist in
  logs from ~Sept 2025 onward — older sessions have no usage to read (expected; skip silently).
- **Extract:** per-turn input/output (and cached/reasoning if present) token counts; resolve
  model from the nearest preceding `turn_context`. Skip `token_count` events with no resolvable
  model (a few Sept-2025 builds emitted these) rather than mispricing them.
- **Cost:** pricing table (§9); `cost_is_estimate=True`.
- **Note:** never touch `~/.codex/auth.json`.

### 5.3 OpenRouter — hosted API  *(metered_mode: pay_per_token; cost = real)*
- **Auth:** `Authorization: Bearer <OPENROUTER_API_KEY>` from config/env.
- **Endpoints (base `https://openrouter.ai/api/v1`):**
  - `GET /credits` → `{ data: { total_credits, total_usage } }`. Cumulative dollars.
  - `GET /key` → key status: `limit`, `usage`, `limit_remaining`, `rate_limit`, `is_free_tier`.
- **Strategy:** OpenRouter does not expose a per-event feed to us without proxying, so **poll on
  an interval (default 60s)**, write each response into `cumulative_snapshots`, and **diff
  consecutive `total_usage` values** to synthesize spend deltas → derived events (model unknown
  → leave `model=NULL`). This yields an accurate spend-over-time series.
- **Optional richer path:** if the user routes requests through their own code, they can add
  `usage: { include: true }` to each request and log real per-generation model+cost; out of
  scope for v1 but note the hook.

### 5.4 opencode — routes through a provider
- opencode calls an underlying provider. **If it's OpenRouter, its usage is already captured in
  5.3** — do not double count. If opencode is configured against a direct provider key, capture
  at that provider. Also check for a local session store (verify path empirically, likely under
  `~/.local/share/opencode/` or `~/.config/opencode/`); if it writes usage-bearing JSONL, add a
  collector modeled on 5.1/5.2. **v1: treat opencode usage as "whatever provider it uses" and
  surface a config note; only add a dedicated collector if a local usage log is confirmed.**

### 5.5 (Optional) Direct Anthropic / OpenAI API keys  *(pay_per_token; real)*
- **Anthropic Admin API:** organization usage & cost report endpoints (requires an **admin** key,
  distinct from a normal API key). Poll daily; write real cost.
- **OpenAI:** organization usage/costs endpoints. Poll daily.
- Gate both behind config flags; off by default. Verify current endpoint paths at build time.

---

## 6. "Weeklies" mean different things — normalize them

- **Claude Code (subscription):** real rolling 7-day cap exists but isn't in an API. Track
  reconstructed token consumption vs a configured/auto-detected weekly token limit.
- **Codex (subscription):** same shape; `/status` shows weekly % interactively but not via API.
- **OpenRouter (pay_per_token):** no inherent weekly cap; the "weekly" is a **user-defined weekly
  budget in USD**. Track real spend vs that budget.

So the dashboard's unit differs per service (`tokens` vs `usd`). Keep them in native units, and
present each service's weekly gauge in its own unit; only combine into a single "total spend"
view using USD (estimates + real, clearly labeled).

---

## 7. Configuration

`config.yaml` (path: `./config.yaml`, override via `AGENTMETER_CONFIG`):

```yaml
poll_interval_seconds: 60
timezone: "America/Los_Angeles"        # for window boundaries & day-of-week weighting

services:
  claude_code:
    enabled: true
    log_globs: ["~/.claude/projects/**/*.jsonl"]
    weekly_limit: { value: 300000000, unit: tokens, source: configured }  # example

  codex:
    enabled: true
    codex_home: null                    # null → $CODEX_HOME or ~/.codex
    weekly_limit: { value: 150000000, unit: tokens, source: configured }

  openrouter:
    enabled: true
    api_key_env: OPENROUTER_API_KEY     # read from env; never inline the key
    weekly_budget: { value: 40.00, unit: usd, source: configured }

  opencode:
    enabled: false

  anthropic_api:
    enabled: false
    admin_key_env: ANTHROPIC_ADMIN_KEY

  openai_api:
    enabled: false
    api_key_env: OPENAI_API_KEY

forecast:
  model: "dow_weighted"                 # 'linear' | 'dow_weighted'
  warn_threshold: 0.80                  # amber at projected 80% of limit
  danger_threshold: 1.00                # red at projected ≥100%
```

Keys come **only** from env vars named in config. Validate on startup and fail loudly if an
enabled service's key env is missing.

---

## 8. Collectors & normalization

### File-tail collectors (Claude Code, Codex)
- Use `watchdog` to watch the log dirs; on modify, read **only new bytes** (track per-file
  byte offset in a small state file / table) and parse appended lines.
- On startup, do a **one-time backfill**: walk all existing logs, parse, insert (dedupe handles
  reruns).
- **Dedupe:** `event_uid = sha256(f"{service}|{session_id}|{ts}|{input_tokens}|{output_tokens}|{model}")`.
  Insert with `INSERT OR IGNORE`. This makes backfill + tail idempotent.

### Poll collectors (OpenRouter, direct APIs)
- APScheduler job every `poll_interval_seconds`.
- Write raw cumulative response to `cumulative_snapshots`.
- Compute delta vs previous snapshot for the same service; if delta > 0, insert a derived event
  (`event_uid = sha256(service|ts|total_usage)`), `model=NULL`, `cost_usd=delta`,
  `metered_mode=pay_per_token`, `cost_is_estimate=False`.
- First snapshot establishes a baseline (no event emitted).

### Normalizer
- Single function `to_events(raw, service) -> list[Event]` per source.
- Attaches cost via pricing table for token-based sources.
- All timestamps converted to UTC on write; window math converts to config `timezone`.

---

## 9. Pricing / cost computation

- Source of truth: **LiteLLM's public model-pricing dataset** (the same dataset ccusage uses).
  Fetch on startup, cache to `./cache/pricing.json`, refresh daily; fall back to cache offline.
- Cost formula per event:

```
cost = input_tokens      * price.input
     + output_tokens     * price.output
     + cache_read_tokens * price.cache_read
     + cache_write_tokens* price.cache_write
     + reasoning_tokens  * price.output      # unless a distinct reasoning rate exists
```

- If a model id isn't in the dataset, record tokens with `cost_usd=0` and log a warning listing
  the unknown model (so the user can add an override in `config.yaml → pricing_overrides`).

---

## 10. Weekly window & limit resolution

For each enabled service, on each forecast run:

1. **Window bounds.** v1 uses a **rolling 7-day window** ending now (`window_start = now - 7d`),
   in the config timezone. (If/when a service exposes a fixed reset time, store it in
   `limits.window_reset` and use fixed bounds instead.)
2. **Consumed so far.** Sum events in `[window_start, now]` for that service, in the service's
   native unit (tokens or usd).
3. **Limit.** From `limits` table: prefer `real` > `autodetected` > `configured`.
   - *Auto-detection (optional, Claude/Codex):* infer a plausible cap by taking the max weekly
     consumption observed historically and rounding up — mirror the "custom mode" approach used
     by Claude-Code-Usage-Monitor. Mark `limit_source='autodetected'` and let the user override.
4. Persist current consumed/limit for the API layer.

---

## 11. Forecasting engine

Given `consumed` so far and the window, project end-of-window total.

### Baseline — linear burn rate
```
elapsed   = now - window_start           # seconds
remaining = window_end - now             # seconds
burn      = consumed / elapsed           # units per second
projected = consumed + burn * remaining
pct_now       = consumed / limit
pct_projected = projected / limit
eta_to_limit  = window_start + (limit / burn)   # when we'd hit 100% at current pace
```
Status: `green` if `pct_projected < warn_threshold`, `amber` if `< danger_threshold`, else `red`.

### Refinement — day-of-week + hour weighting (`dow_weighted`, default)
Naive linear over-forecasts on a Monday for weekday-heavy users. Instead of assuming uniform
future pace, distribute the remaining window by the user's historical activity profile:

1. From the last N weeks (default 4) of events, build a normalized **(day-of-week, hour) weight
   grid** summing to 1.0 over a week.
2. `expected_fraction_elapsed` = sum of grid weights for the portion of the current window that
   has already passed.
3. `projected = consumed / expected_fraction_elapsed` (guard against divide-by-zero / cold start:
   fall back to `linear` when < ~2 days of history in window).

Expose which model produced the number in the API so the UI can label it.

**Forecast output object (per service, per window):**
```json
{
  "service": "claude_code",
  "window_kind": "weekly",
  "unit": "tokens",
  "consumed": 182340000,
  "limit": 300000000,
  "limit_source": "configured",
  "pct_now": 0.61,
  "projected": 271900000,
  "pct_projected": 0.91,
  "status": "amber",
  "eta_to_limit": "2026-08-27T18:12:00Z",
  "window_start": "2026-08-17T00:00:00Z",
  "window_end": "2026-08-24T00:00:00Z",
  "forecast_model": "dow_weighted"
}
```

---

## 12. Backend API (FastAPI)

All JSON. Serve the built frontend as static files at `/`.

| Method | Path | Returns |
|---|---|---|
| `GET` | `/api/health` | `{status, version, db_ok, collectors: [{service, last_run, ok}]}` |
| `GET` | `/api/services` | configured services + enabled/mode/limit summary |
| `GET` | `/api/timeseries?service=&from=&to=&bucket=hour\|day` | usage-over-time buckets: `[{ts, service, tokens, cost_usd}]` |
| `GET` | `/api/forecast?window=weekly` | list of forecast objects (§11) for every enabled service **+ a combined USD forecast** |
| `GET` | `/api/snapshot` | one call powering the whole dashboard: `{services, timeseries, forecast, generated_at}` |
| `GET` | `/api/stream` | **SSE**: pushes a fresh `snapshot` whenever collectors write new data (or every 5s) |
| `POST` | `/api/limits` | set/override a limit: `{service, window_kind, value, unit, source:"configured"}` |

Frontend real-time strategy: connect to `/api/stream` (SSE); if it drops, fall back to polling
`/api/snapshot` every 5s. (SSE chosen over WebSocket — one-way push is all we need and it's
simpler.)

---

## 13. Frontend dashboard

**Stack:** React + Vite + TypeScript + Recharts. Single page. Built to `dist/`, served by FastAPI.

**Layout (top → bottom):**
1. **Header status strip.** One compact gauge per service: current % of weekly limit with the
   burn-rate projection overlaid (e.g. a bar with a "now" fill and a lighter "projected" fill),
   colored green/amber/red. This is the "am I on track" at-a-glance row.
2. **Usage over time.** Stacked area/bar chart (Recharts) of tokens *or* USD (toggle), stacked by
   service, with a day/week/hour bucket switch. Overlay a dashed **projection line** to the
   window end for the selected service.
3. **Weekly forecast detail.** For the selected service: consumed vs limit, projected end-of-week,
   ETA-to-limit, and which forecast model was used. Show a plain-language line:
   *"On pace for ~91% of your Claude weekly cap; you'd hit 100% around Thu 6:12pm."*
4. **Breakdown table.** Per model / per project (from `project` field) totals for the window.

**Conventions:** clearly badge subscription-service dollar figures as *estimated (API-equivalent)*
vs OpenRouter's *real spend*, since mixing them silently would mislead. Keep everything in the
config timezone.

For visual/aesthetic direction on the SPA, read `/mnt/skills/public/frontend-design/SKILL.md`
(if present in the build environment) before styling.

---

## 14. Build milestones (execute in order)

**Phase 1 — Skeleton + store.**
Repo scaffold, `config.yaml` loader with env-key validation, SQLite schema created on startup,
pricing fetcher with cache.
*Acceptance:* `python -m agentmeter init` creates the DB and prints loaded config + pricing count.

**Phase 2 — Claude Code collector.**
Backfill + watchdog tail of `~/.claude/projects/**`. Normalizer + dedupe + cost.
*Acceptance:* after a real Claude Code session, `SELECT count(*), sum(cost_usd) FROM events WHERE service='claude_code'` returns sane, non-zero numbers matching `ccusage` roughly.

**Phase 3 — Codex collector.** Same for `~/.codex/sessions` (+ archived, + CODEX_HOME list).
*Acceptance:* Codex events land; totals roughly track `ccusage codex` / `/usage`.

**Phase 4 — OpenRouter poller.** `/credits` + `/key` snapshots, delta → derived events.
*Acceptance:* leaving it running across a few OpenRouter calls produces spend events whose sum equals the observed `total_usage` delta.

**Phase 5 — Forecaster.** Window resolution, `linear` then `dow_weighted`, limits table + override.
*Acceptance:* `GET /api/forecast` returns correct math on a seeded fixture DB (unit-tested, §15).

**Phase 6 — API + SSE.** All endpoints in §12, snapshot assembly, SSE push.
*Acceptance:* `curl /api/snapshot` returns a complete object; `/api/stream` emits on new data.

**Phase 7 — Dashboard.** The four sections in §13, wired to `/api/stream` with polling fallback.
*Acceptance:* opening `http://localhost:<port>` shows live gauges + chart that update within ~5s of new usage.

**Phase 8 (optional) — Direct Anthropic/OpenAI pollers, opencode local collector, session (5h) windows.**

---

## 15. Testing

- **Unit:** normalizers against **captured real fixture lines** (commit a handful of redacted
  Claude/Codex JSONL lines under `tests/fixtures/`), pricing math, dedupe idempotency
  (parse same file twice → same row count), delta logic for cumulative snapshots.
- **Forecaster:** seed a DB with a known event distribution; assert `linear` and `dow_weighted`
  projections and `eta_to_limit` against hand-computed expected values. Include a cold-start case
  (falls back to linear) and a divide-by-zero guard.
- **API:** contract tests on each endpoint's JSON shape.
- **End-to-end smoke:** run collectors against `tests/fixtures/` logs, hit `/api/snapshot`,
  assert non-empty services + a valid forecast per service.

---

## 16. Suggested repo structure

```
agentmeter/
├── DESIGN.md
├── config.yaml
├── pyproject.toml
├── agentmeter/
│   ├── __main__.py            # `python -m agentmeter [init|run]`
│   ├── config.py              # load + validate config, resolve env keys
│   ├── db.py                  # schema, connection, upsert helpers
│   ├── pricing.py             # LiteLLM fetch/cache + cost() 
│   ├── models.py              # Event, Forecast pydantic models
│   ├── normalize.py           # to_events() per service
│   ├── collectors/
│   │   ├── base.py            # Collector interface, offset state
│   │   ├── claude_code.py     # file-tail
│   │   ├── codex.py           # file-tail (CODEX_HOME aware)
│   │   ├── openrouter.py      # poller + delta
│   │   ├── anthropic_api.py   # optional poller
│   │   └── openai_api.py      # optional poller
│   ├── forecast.py            # window resolution + linear/dow_weighted
│   ├── api.py                 # FastAPI app, routes, SSE, static serve
│   └── scheduler.py           # APScheduler wiring, watchdog wiring
├── frontend/                  # Vite + React + Recharts → builds to agentmeter/static/
└── tests/
    ├── fixtures/              # redacted real JSONL + seeded DBs
    └── ...
```

**Run:** `python -m agentmeter run` starts collectors + scheduler + API; frontend is pre-built
into `agentmeter/static/`. Single process, single port (default `:8787`).

---

## 17. Dependencies

**Backend (Python 3.11+):** `fastapi`, `uvicorn`, `apscheduler`, `watchdog`, `httpx`,
`pydantic>=2`, `pyyaml`. SQLite via stdlib. (Optional `sqlmodel` if you prefer an ORM.)
**Frontend:** `react`, `react-dom`, `recharts`, `vite`, `typescript`.

---

## 18. Open questions / future

- opencode local usage log path — confirm and add a first-class collector if present.
- Reading true reset timestamps: consider optionally parsing `/status` / `/usage` output by
  scripting the CLI, to upgrade Claude/Codex weekly limits from `configured` to `real`.
- Session (5-hour rolling) window tracking for a "safe to start a long run?" indicator.
- Existing tools to evaluate before/instead of building: **ccusage** (Claude+Codex log parsing,
  the reference implementation for §5.1/§5.2 and pricing), **Claude-Code-Usage-Monitor**
  (burn-rate + limit auto-detection prior art for §10/§11), and **CodexBar** (multi-service
  menu-bar aggregation). This project's differentiator is the **unified web dashboard + weekly
  forecast across all services in one place**.
```
