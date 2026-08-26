# metoks

A local-first tool that aggregates token/credit usage across your AI coding
services (Claude Code, Codex CLI, OpenRouter), stores a unified time series,
renders a real-time dashboard, and **forecasts** end-of-window consumption so you
stay under your weekly limits.

Rust implementation of the spec in [`_handoff/DESIGN.md`](_handoff/DESIGN.md).
Runs entirely on your machine — **no usage data leaves the box**.

## Stack

| Concern | Crate |
|---|---|
| Async runtime | `tokio` |
| Web + SSE | `axum` |
| Store | `rusqlite` (bundled SQLite) + `r2d2` pool |
| File watching | `notify` |
| HTTP client | `reqwest` |
| Time / timezones | `chrono` + `chrono-tz` |
| Frontend | React + Vite + TypeScript + Recharts → built into `static/`, served by the backend |

## Install

```bash
# 1. build the frontend once → ./static (embedded into the binary at compile time)
cd frontend && npm install && npm run build && cd ..

# 2. install the `metoks` binary to ~/.cargo/bin (must be on your PATH)
cargo install --path .
```

The frontend is embedded into the binary, so the installed `metoks` serves the web
dashboard from any directory. Config, DB, and cache live in the standard per-user
dirs (see below), so no working-directory setup is needed. Re-run both steps to
upgrade (rebuild the frontend so the embedded copy is current).

### Build without installing

```bash
cargo build --release          # → target/release/metoks
cd frontend && npm run build    # refresh ./static (served from disk in dev)
```

In dev, an on-disk `static/` is served directly (so `npm run build` shows up
immediately); the embedded copy is only used when `static/` is absent.

## Run

```bash
# create the DB and print loaded config + pricing count
cargo run -- init

# one-shot backfill of local logs, print per-service totals
cargo run -- backfill

# print the current weekly forecast per enabled service
cargo run -- forecast

# start collectors + scheduler + API + dashboard (single process, single port)
cargo run --release -- run
# → open http://localhost:8787
```

## File locations

Config, data, and cache live in the OS-standard per-user directories (via the
`dirs` crate), each under an `metoks/` subdirectory:

| | macOS | Linux (XDG) |
|---|---|---|
| Config (`config.yaml`) | `~/Library/Application Support/metoks/` | `$XDG_CONFIG_HOME/metoks/` |
| Data (`metoks.db`) | `~/Library/Application Support/metoks/` | `$XDG_DATA_HOME/metoks/` |
| Cache (`pricing.json`) | `~/Library/Caches/metoks/` | `$XDG_CACHE_HOME/metoks/` |

`metoks init` scaffolds a starter `config.yaml` at the config path (if absent)
and prints every resolved location.

Each can be overridden — by flag (`--config <path>`, `--db <path>`) or env var
(`METOKS_CONFIG`, `METOKS_DB`, `METOKS_DATA_DIR`, `METOKS_CACHE_DIR`).
The repo's [`config.yaml`](config.yaml) is the embedded template `init` copies from.

## Configuration

Edit the scaffolded `config.yaml` in your config dir (`metoks init` prints the
path). Keys are read **only** from the env vars named in config — never inlined,
never written to the DB or logs. Enabling a service whose key env is missing fails
loudly on startup.

## Data sources

- **Claude Code** (`~/.claude/projects/**/*.jsonl`) — subscription; cost is an
  API-equivalent *estimate*. Deduped globally by `(message.id, requestId)`, which
  is the honest "count each real API call once" measure (this diverges from
  `ccusage`, which double-counts some cross-file duplicates — see
  `src/normalize.rs`).
- **Codex** (`${CODEX_HOME:-~/.codex}/sessions` + `archived_sessions`) —
  subscription/estimate. Uses per-turn `last_token_usage`; model resolved from the
  nearest `turn_context`. **Bonus:** Codex logs carry `rate_limits` with the real
  weekly `used_percent` and reset time, so the weekly gauge is `source="real"`.
- **OpenRouter** (`/credits`) — pay_per_token, **real** cost; polled on an interval
  and diffed to synthesize spend events. Off by default (needs `OPENROUTER_API_KEY`).

## API

`GET /api/health · /api/services · /api/timeseries · /api/forecast · /api/snapshot`,
`GET /api/stream` (SSE), `GET /api/breakdown`, `POST /api/limits`.

## Test

```bash
cargo test          # forecaster math, dedupe, delta logic, window resolution
```

## Status

Phases 1–7 of the design are implemented. Phase 8 (direct Anthropic/OpenAI
pollers, opencode collector, 5-hour session windows) is not yet built.
