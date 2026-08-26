//! Normalizers: map raw source records → the common `Event` shape (DESIGN.md §8).
//! Parsers are defensive — a line we can't classify is skipped, never fatal.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::models::{Event, MeteredMode};
use crate::pricing::Pricing;

/// Stable dedupe hash. See DESIGN.md §8. `extra` lets a source mix in a naturally
/// unique id (Claude's message id, Codex's turn id) for extra collision safety.
pub fn event_uid(
    service: &str,
    session_id: Option<&str>,
    ts: &DateTime<Utc>,
    input: i64,
    output: i64,
    model: Option<&str>,
    extra: Option<&str>,
) -> String {
    let mut h = Sha256::new();
    h.update(service.as_bytes());
    h.update(b"|");
    h.update(session_id.unwrap_or("").as_bytes());
    h.update(b"|");
    h.update(ts.to_rfc3339().as_bytes());
    h.update(b"|");
    h.update(input.to_le_bytes());
    h.update(b"|");
    h.update(output.to_le_bytes());
    h.update(b"|");
    h.update(model.unwrap_or("").as_bytes());
    if let Some(x) = extra {
        h.update(b"|");
        h.update(x.as_bytes());
    }
    format!("{:x}", h.finalize())
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Parse one Claude Code JSONL line into an Event, if it carries usage.
/// `fallback_session` comes from the file name, `fallback_project` from the dir.
pub fn claude_code_line(
    line: &str,
    fallback_session: Option<&str>,
    fallback_project: Option<&str>,
    raw_source: &str,
    pricing: &Pricing,
) -> Option<Event> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;

    // Only assistant message lines carry a usage object.
    let msg = v.get("message")?;
    let usage = msg.get("usage")?;

    let model = msg
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());
    // Early builds lack model metadata — skip rather than guess.
    let model = model?;
    // Synthetic bookkeeping models sometimes appear; skip the obvious ones.
    if model == "<synthetic>" {
        return None;
    }

    let g = |k: &str| usage.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let input = g("input_tokens");
    let output = g("output_tokens");
    let cache_write = g("cache_creation_input_tokens");
    let cache_read = g("cache_read_input_tokens");

    // A line with no tokens at all isn't useful.
    if input == 0 && output == 0 && cache_write == 0 && cache_read == 0 {
        return None;
    }

    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(parse_ts)?;

    let session_id = v
        .get("sessionId")
        .or_else(|| v.get("session_id"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .or_else(|| fallback_session.map(|s| s.to_string()));

    // Prefer the line's cwd basename for project; fall back to the encoded dir.
    let project = v
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(|c| basename(c).to_string())
        .or_else(|| fallback_project.map(|s| s.to_string()));

    // Dedupe by message identity, matching ccusage: the same (message.id,
    // requestId) can appear in multiple session files (resumed/compacted
    // sessions) and must collapse to one event. Fall back to the token-based
    // hash only when neither id is present.
    let message_id = msg.get("id").and_then(|x| x.as_str());
    let request_id = v.get("requestId").and_then(|x| x.as_str());
    let uid = match (message_id, request_id) {
        (None, None) => event_uid(
            crate::models::SERVICE_CLAUDE_CODE,
            session_id.as_deref(),
            &ts,
            input,
            output,
            Some(&model),
            None,
        ),
        (m, r) => {
            let mut h = Sha256::new();
            h.update(crate::models::SERVICE_CLAUDE_CODE.as_bytes());
            h.update(b"|");
            h.update(m.unwrap_or("").as_bytes());
            h.update(b"|");
            h.update(r.unwrap_or("").as_bytes());
            format!("{:x}", h.finalize())
        }
    };

    let (cost_usd, priced) = pricing.cost(Some(&model), input, output, cache_read, cache_write, 0);
    if !priced {
        tracing::debug!("unknown model for pricing: {model}");
    }

    Some(Event {
        event_uid: uid,
        service: crate::models::SERVICE_CLAUDE_CODE.to_string(),
        metered_mode: MeteredMode::Subscription,
        ts,
        model: Some(model),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: 0,
        cost_usd,
        cost_is_estimate: true,
        session_id,
        project,
        raw_source: Some(raw_source.to_string()),
    })
}

pub fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Codex CLI
// ---------------------------------------------------------------------------

/// Per-turn token usage extracted from a Codex `token_count` event.
#[derive(Debug, Clone)]
pub struct CodexUsage {
    /// billable (non-cached) input
    pub input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// output excluding reasoning (reasoning stored separately, summed correctly)
    pub output: i64,
    pub reasoning: i64,
}

/// One provider-reported rate-limit window.
#[derive(Debug, Clone)]
pub struct RateWindow {
    pub used_percent: f64,
    pub window_minutes: i64,
    pub resets_at: Option<DateTime<Utc>>,
}

/// Everything we might extract from a single Codex JSONL line.
#[derive(Debug, Clone, Default)]
pub struct CodexParsed {
    pub ts: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub usage: Option<CodexUsage>,
    pub rate_limits: Vec<RateWindow>,
}

/// Parse one Codex rollout line. A `token_count` line carries both usage and
/// rate_limits; a `turn_context` line carries the model.
pub fn parse_codex_line(line: &str) -> CodexParsed {
    let mut out = CodexParsed::default();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return out;
    };
    out.ts = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_ts);

    match v.get("type").and_then(|t| t.as_str()) {
        Some("turn_context") => {
            let payload = v.get("payload");
            out.model = payload
                .and_then(|p| p.get("model"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string());
            out.cwd = payload
                .and_then(|p| p.get("cwd"))
                .and_then(|c| c.as_str())
                .map(|c| basename(c).to_string());
        }
        Some("event_msg") => {
            let payload = v.get("payload");
            let is_token_count = payload
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str())
                == Some("token_count");
            if is_token_count {
                if let Some(info) = payload.and_then(|p| p.get("info")) {
                    // Prefer per-turn delta to avoid double counting the cumulative total.
                    if let Some(last) = info.get("last_token_usage") {
                        out.usage = codex_usage_from(last);
                    }
                }
                if let Some(rl) = payload.and_then(|p| p.get("rate_limits")) {
                    for key in ["primary", "secondary"] {
                        if let Some(w) = rl.get(key).and_then(rate_window_from) {
                            out.rate_limits.push(w);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    out
}

fn codex_usage_from(u: &serde_json::Value) -> Option<CodexUsage> {
    let g = |k: &str| u.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let input_total = g("input_tokens"); // includes cached
    let cached = g("cached_input_tokens");
    let cache_write = g("cache_write_input_tokens");
    let output_total = g("output_tokens"); // includes reasoning
    let reasoning = g("reasoning_output_tokens");
    // Split so the column sum equals the provider's total (no double counting).
    let input = (input_total - cached).max(0);
    let output = (output_total - reasoning).max(0);
    if input == 0 && output == 0 && cached == 0 && cache_write == 0 && reasoning == 0 {
        return None;
    }
    Some(CodexUsage {
        input,
        cache_read: cached,
        cache_write,
        output,
        reasoning,
    })
}

fn rate_window_from(w: &serde_json::Value) -> Option<RateWindow> {
    let used_percent = w.get("used_percent").and_then(|x| x.as_f64())?;
    let window_minutes = w.get("window_minutes").and_then(|x| x.as_i64()).unwrap_or(0);
    let resets_at = w
        .get("resets_at")
        .and_then(|x| x.as_i64())
        .and_then(|epoch| DateTime::<Utc>::from_timestamp(epoch, 0));
    Some(RateWindow {
        used_percent,
        window_minutes,
        resets_at,
    })
}

/// Build a Codex Event from resolved usage + model.
#[allow(clippy::too_many_arguments)]
pub fn codex_event(
    usage: &CodexUsage,
    model: Option<&str>,
    session_id: Option<&str>,
    project: Option<&str>,
    ts: DateTime<Utc>,
    raw_source: &str,
    pricing: &Pricing,
) -> Event {
    let (cost_usd, _priced) = pricing.cost(
        model,
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_write,
        usage.reasoning,
    );
    // Extra uniqueness: fold the full token shape in so two same-ts turns differ.
    let extra = format!(
        "{}-{}-{}-{}",
        usage.cache_read, usage.cache_write, usage.reasoning, usage.output
    );
    Event {
        event_uid: event_uid(
            crate::models::SERVICE_CODEX,
            session_id,
            &ts,
            usage.input,
            usage.output,
            model,
            Some(&extra),
        ),
        service: crate::models::SERVICE_CODEX.to_string(),
        metered_mode: MeteredMode::Subscription,
        ts,
        model: model.map(|s| s.to_string()),
        input_tokens: usage.input,
        output_tokens: usage.output,
        cache_read_tokens: usage.cache_read,
        cache_write_tokens: usage.cache_write,
        reasoning_tokens: usage.reasoning,
        cost_usd,
        cost_is_estimate: true,
        session_id: session_id.map(|s| s.to_string()),
        project: project.map(|s| s.to_string()),
        raw_source: Some(raw_source.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Redacted fixtures shaped like real log lines (DESIGN.md §15).
    const CLAUDE_LINE: &str = r#"{"message":{"model":"claude-opus-4-8","id":"msg_TEST","usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":5,"cache_read_input_tokens":100}},"requestId":"req_TEST","timestamp":"2026-08-20T19:10:28.376Z","sessionId":"sess-1","cwd":"/Users/x/code/myproj","type":"assistant"}"#;
    const CLAUDE_USER_LINE: &str = r#"{"type":"user","message":{"role":"user","content":"hi"},"timestamp":"2026-08-20T19:10:28.376Z"}"#;
    const CODEX_TOKENS: &str = r#"{"timestamp":"2026-08-20T19:10:28.376Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":8264,"cached_input_tokens":4864,"cache_write_input_tokens":0,"output_tokens":208,"reasoning_output_tokens":148,"total_tokens":8472}},"rate_limits":{"primary":{"used_percent":88.0,"window_minutes":10080,"resets_at":1787505234},"secondary":null}}}"#;
    const CODEX_TURN: &str = r#"{"timestamp":"2026-08-20T19:10:23.822Z","type":"turn_context","payload":{"model":"gpt-5.6-sol","cwd":"/Users/x/code/proj2"}}"#;

    #[test]
    fn claude_line_maps_tokens_and_project() {
        let e = claude_code_line(CLAUDE_LINE, None, None, "f.jsonl", &Pricing::default()).unwrap();
        assert_eq!(e.input_tokens, 10);
        assert_eq!(e.output_tokens, 20);
        assert_eq!(e.cache_write_tokens, 5); // cache_creation → cache_write
        assert_eq!(e.cache_read_tokens, 100); // cache_read_input → cache_read
        assert_eq!(e.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(e.project.as_deref(), Some("myproj")); // basename of cwd
        assert_eq!(e.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn claude_dedupe_uid_is_stable_by_message_identity() {
        let a = claude_code_line(CLAUDE_LINE, None, None, "a.jsonl", &Pricing::default()).unwrap();
        // Same message copied to a different file/session → same uid (dedupes).
        let b = claude_code_line(CLAUDE_LINE, Some("other-sess"), None, "b.jsonl", &Pricing::default()).unwrap();
        assert_eq!(a.event_uid, b.event_uid);
    }

    #[test]
    fn claude_skips_lines_without_usage() {
        assert!(claude_code_line(CLAUDE_USER_LINE, None, None, "f", &Pricing::default()).is_none());
    }

    #[test]
    fn codex_usage_splits_without_double_counting() {
        let p = parse_codex_line(CODEX_TOKENS);
        let u = p.usage.expect("usage");
        assert_eq!(u.input, 8264 - 4864); // billable non-cached input
        assert_eq!(u.cache_read, 4864);
        assert_eq!(u.cache_write, 0);
        assert_eq!(u.output, 208 - 148); // output excludes reasoning
        assert_eq!(u.reasoning, 148);
        // column sum must equal the provider's reported total (8472)
        assert_eq!(u.input + u.cache_read + u.cache_write + u.output + u.reasoning, 8472);
    }

    #[test]
    fn codex_parses_weekly_rate_limit_only() {
        let p = parse_codex_line(CODEX_TOKENS);
        assert_eq!(p.rate_limits.len(), 1); // secondary is null
        let w = &p.rate_limits[0];
        assert_eq!(w.used_percent, 88.0);
        assert_eq!(w.window_minutes, 10080);
        assert!(w.resets_at.is_some());
    }

    #[test]
    fn codex_turn_context_yields_model_and_cwd() {
        let p = parse_codex_line(CODEX_TURN);
        assert_eq!(p.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(p.cwd.as_deref(), Some("proj2"));
        assert!(p.usage.is_none());
    }
}
