export type Unit = "tokens" | "usd";

export interface RateLimitStatus {
  service: string;
  window_kind: string;
  used_percent: number | null;
  window_minutes: number | null;
  resets_at: string | null;
  observed_at: string;
}

export interface ServiceInfo {
  service: string;
  enabled: boolean;
  metered_mode: "subscription" | "pay_per_token";
  unit: Unit;
  events: number;
  tokens: number;
  cost_usd: number;
  rate_limit_weekly: RateLimitStatus | null;
  rate_limit_session: RateLimitStatus | null;
}

export interface Forecast {
  service: string;
  window_kind: string;
  unit: Unit;
  consumed: number;
  limit: number | null;
  limit_source: string | null;
  pct_now: number | null;
  projected: number;
  pct_projected: number | null;
  status: "green" | "amber" | "red" | "unknown";
  eta_to_limit: string | null;
  window_start: string;
  window_end: string;
  forecast_model: string;
  low_confidence: boolean;
}

export interface FiducialPoint {
  ts: string;
  percent: number;
}

export interface ConePoint {
  ts: string;
  lo: number;
  mid: number;
  hi: number;
}

export interface TokenCumPoint {
  ts: string;
  cum: number[]; // aligned to Cumulative.models
}

export interface Cumulative {
  service: string;
  unit: Unit;
  mode: "fixed" | "rolling";
  window_start: string;
  window_end: string;
  now: string;
  cap: number | null;
  cap_source: string | null;
  consumed: number;
  projected: number;
  pct_now: number | null;
  pct_projected: number | null;
  status: "green" | "amber" | "red" | "unknown";
  eta_to_limit: string | null;
  forecast_model: string;
  low_confidence: boolean;
  axis_cap: number | null;
  models: string[];
  token_points: TokenCumPoint[];
  cone_pct: ConePoint[];
  fiducials: FiducialPoint[];
  pace_weekly: number;
  pace_sigma: number;
  on_device_tokens: number;
  off_device_tokens: number;
  off_device_rate: number;
}

export interface ProjectSeries {
  project: string;
  total: number;
  points: number[];
}

export interface Bucket {
  ts: string;
  key: string; // service or model, depending on ?by=
  tokens: number;
  cost_usd: number;
}

export interface Snapshot {
  services: ServiceInfo[];
  forecast: {
    forecasts: Forecast[];
    cumulatives: Cumulative[];
    generated_at: string;
  };
  generated_at: string;
}

export interface BreakdownRow {
  service: string;
  key: string;
  tokens: number;
  cost_usd: number;
  events: number;
}
export interface Breakdown {
  days: number;
  by_model: BreakdownRow[];
  by_project: BreakdownRow[];
}

export const SERVICE_LABELS: Record<string, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
  openrouter: "OpenRouter",
};

export const SERVICE_COLORS: Record<string, string> = {
  claude_code: "#3987e5",
  codex: "#d95926",
  openrouter: "#199e70",
};

export const STATUS_COLORS: Record<string, string> = {
  green: "#1f9d57",
  amber: "#e0a100",
  red: "#e34948",
  unknown: "#6b6b66",
};

// Fixed model → color, in stack order. Consecutive dark-palette categorical slots
// (validated adjacent-pairs), so stacked neighbors stay distinguishable.
export const MODEL_COLORS: [string, string][] = [
  ["claude-opus-5", "#3987e5"], // blue
  ["claude-opus-4-8", "#d95926"], // orange
  ["claude-sonnet-5", "#199e70"], // aqua
  ["claude-fable-5", "#c98500"], // yellow
  ["claude-haiku-4-5-20251001", "#d55181"], // magenta
  ["gpt-5.6-sol", "#008300"], // green
  ["codex-auto-review", "#9085e9"], // violet
];
export const OTHER_COLOR = "#6b6b66";
export const OTHER_KEY = "Other";

const MODEL_SET = new Set(MODEL_COLORS.map(([m]) => m));
export function isKnownModel(m: string): boolean {
  return MODEL_SET.has(m);
}
export function colorForModel(m: string): string {
  if (m === OTHER_KEY) return OTHER_COLOR;
  const hit = MODEL_COLORS.find(([k]) => k === m);
  return hit ? hit[1] : OTHER_COLOR;
}
/** Stack order for the models actually present, unknowns folded into "Other". */
export function orderedModelKeys(present: Set<string>): string[] {
  const known = MODEL_COLORS.map(([m]) => m).filter((m) => present.has(m));
  const hasOther = [...present].some((m) => !isKnownModel(m));
  return hasOther ? [...known, OTHER_KEY] : known;
}
// short label for a model in the legend/tooltip
export function modelLabel(m: string): string {
  return m
    .replace(/^claude-/, "")
    .replace(/-\d{8}$/, "");
}
