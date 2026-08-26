import { useMemo, useState } from "react";
import {
  Area,
  ComposedChart,
  Line,
  ReferenceLine,
  ResponsiveContainer,
  Scatter,
  Tooltip,
  XAxis,
  YAxis,
  CartesianGrid,
} from "recharts";
import type { Cumulative } from "../types";
import {
  SERVICE_LABELS,
  STATUS_COLORS,
  colorForModel,
  isKnownModel,
  modelLabel,
  OTHER_KEY,
  orderedModelKeys,
} from "../types";
import { fmtTokens, fmtPct, fmtDateTime } from "../format";

const CAP_SOURCE_LABEL: Record<string, string> = {
  fiducial: "your readings",
  real: "provider",
  anchored: "your readings",
  configured: "config",
  autodetected: "auto-detected",
};

function projRange(c: Cumulative): { lo: number | null; hi: number | null } {
  if (c.cone_pct.length === 0) return { lo: null, hi: null };
  const last = c.cone_pct[c.cone_pct.length - 1];
  return { lo: last.lo / 100, hi: last.hi / 100 };
}

function headline(c: Cumulative): string {
  const name = SERVICE_LABELS[c.service] ?? c.service;
  if (c.cap == null) {
    return `No cap set for ${name}. Log your current % below to ground one.`;
  }
  if (c.low_confidence) {
    return `${name}: ${fmtPct(c.pct_now)} used so far. Too early in the window for a reliable projection.`;
  }
  const proj = fmtPct(c.pct_projected);
  const { lo, hi } = projRange(c);
  const band = lo != null && hi != null ? ` (likely ${fmtPct(lo)}–${fmtPct(hi)})` : "";
  const dest = c.mode === "rolling" ? "a sustained week lands near" : "on pace for";
  const tail =
    c.status === "red" && c.eta_to_limit
      ? ` — you'd hit the cap around ${fmtDateTime(c.eta_to_limit)}.`
      : ".";
  return `${name}: ${fmtPct(c.pct_now)} used; ${dest} ~${proj}${band}${tail}`;
}

interface Row {
  t: number;
  coneMid?: number;
  coneBand?: [number, number];
  [model: string]: number | [number, number] | undefined;
}

export default function CumulativePanel({
  c,
  onAnchor,
}: {
  c: Cumulative;
  onAnchor: () => void;
}) {
  const color = STATUS_COLORS[c.status] ?? STATUS_COLORS.unknown;

  const { rows, keys } = useMemo(() => {
    const present = new Set(c.models);
    const keys = orderedModelKeys(present); // known models present + "Other"
    const byT = new Map<number, Row>();
    // Left axis: cumulative tokens by model.
    for (const p of c.token_points) {
      const t = new Date(p.ts).getTime();
      const row = byT.get(t) ?? ({ t } as Row);
      c.models.forEach((m, i) => {
        const key = isKnownModel(m) ? m : OTHER_KEY;
        row[key] = ((row[key] as number) ?? 0) + p.cum[i];
      });
      byT.set(t, row);
    }
    // Right axis: projection cone (percent).
    for (const p of c.cone_pct) {
      const t = new Date(p.ts).getTime();
      const row = byT.get(t) ?? ({ t } as Row);
      row.coneMid = p.mid;
      row.coneBand = [p.lo, p.hi];
      byT.set(t, row);
    }
    const rows = Array.from(byT.values()).sort((a, b) => a.t - b.t);
    return { rows, keys };
  }, [c]);

  const fidData = useMemo(
    () => c.fiducials.map((f) => ({ t: new Date(f.ts).getTime(), percent: f.percent })),
    [c]
  );

  const xStart = new Date(c.window_start).getTime();
  const xEnd = new Date(c.window_end).getTime();
  const xDomain: [number, number] = [xStart, xEnd];
  const nowMs = new Date(c.now).getTime();

  // Clean daily ticks at local midnight across the window (DST-safe).
  const dayTicks = useMemo(() => {
    const ticks: number[] = [];
    const d = new Date(xStart);
    d.setHours(0, 0, 0, 0);
    if (d.getTime() < xStart) d.setDate(d.getDate() + 1);
    for (let t = d.getTime(); t <= xEnd; ) {
      ticks.push(t);
      const nd = new Date(t);
      nd.setDate(nd.getDate() + 1);
      t = nd.getTime();
    }
    return ticks;
  }, [xStart, xEnd]);
  // Left token axis is aligned to the right % axis: 100% = axis_cap (the local
  // token observation at the last reading ÷ its %), so the top (110%) = 1.1×cap.
  // Falls back to the observed max when no reading exists yet.
  const observedMax = c.token_points.reduce(
    (m, p) => Math.max(m, p.cum.reduce((a, b) => a + b, 0)),
    0
  );
  const tokenAxisMax = c.axis_cap != null ? c.axis_cap * 1.1 : observedMax * 1.08 || 1;

  return (
    <section className="card">
      <div className="card__head">
        <h2>Weekly window — {SERVICE_LABELS[c.service] ?? c.service}</h2>
        <span className="badge" style={{ borderColor: color, color }}>
          {c.status}
          {c.low_confidence ? " · early" : ""}
        </span>
      </div>

      <p className="plain" style={{ borderColor: color }}>
        {headline(c)}
      </p>

      <ResponsiveContainer width="100%" height={300}>
        <ComposedChart data={rows} margin={{ top: 8, right: 12, bottom: 4, left: 8 }}>
          <CartesianGrid stroke="var(--grid)" vertical={false} />
          <XAxis
            dataKey="t"
            type="number"
            scale="time"
            domain={xDomain}
            ticks={dayTicks}
            interval={0}
            allowDataOverflow
            tickFormatter={(t) =>
              new Date(t).toLocaleDateString(undefined, { weekday: "short", day: "numeric" })
            }
            stroke="var(--text-muted)"
            fontSize={12}
          />
          {/* left axis: observed local tokens */}
          <YAxis
            yAxisId="tok"
            stroke="var(--text-muted)"
            fontSize={12}
            width={54}
            domain={[0, tokenAxisMax]}
            allowDataOverflow
            tickFormatter={(v) => fmtTokens(v)}
          />
          {/* right axis: logged % + projection */}
          <YAxis
            yAxisId="pct"
            orientation="right"
            stroke="var(--text-muted)"
            fontSize={12}
            width={44}
            domain={[0, 110]}
            ticks={[0, 25, 50, 75, 100]}
            tickFormatter={(v) => `${v}%`}
          />
          <Tooltip
            contentStyle={{
              background: "var(--surface-2)",
              border: "1px solid var(--border)",
              borderRadius: 8,
              color: "var(--text-primary)",
            }}
            labelFormatter={(t) => fmtDateTime(new Date(Number(t)).toISOString())}
            formatter={(v: number | number[], name: string) => {
              if (name === "coneBand" && Array.isArray(v))
                return [`${v[0].toFixed(0)}–${v[1].toFixed(0)}%`, "projection ±1σ"];
              if (name === "coneMid") return [`${Number(v).toFixed(0)}%`, "projected"];
              if (name === "reading") return [`${Number(v).toFixed(0)}%`, "logged reading"];
              return [fmtTokens(Number(v)), modelLabel(name)];
            }}
          />
          {/* stacked cumulative tokens by model (left) */}
          {keys.map((k) => (
            <Area
              key={k}
              yAxisId="tok"
              type="linear"
              dataKey={k}
              stackId="tok"
              stroke={colorForModel(k)}
              fill={colorForModel(k)}
              fillOpacity={0.7}
              strokeWidth={0}
              isAnimationActive={false}
              connectNulls={false}
            />
          ))}
          {/* "now" divider between observed tokens and the projection */}
          <ReferenceLine
            yAxisId="pct"
            x={nowMs}
            stroke="var(--text-muted)"
            strokeDasharray="2 3"
            label={{ value: "now", fill: "var(--text-muted)", fontSize: 11, position: "top" }}
          />
          {/* cap line at 100% (right) */}
          <ReferenceLine
            yAxisId="pct"
            y={100}
            stroke={STATUS_COLORS.red}
            strokeDasharray="4 4"
            label={{ value: "cap 100%", fill: "var(--text-muted)", fontSize: 11, position: "insideTopRight" }}
          />
          {/* projection cone (right, %) */}
          <Area
            yAxisId="pct"
            type="linear"
            dataKey="coneBand"
            stroke="none"
            fill={color}
            fillOpacity={0.16}
            isAnimationActive={false}
            connectNulls
          />
          <Line
            yAxisId="pct"
            type="linear"
            dataKey="coneMid"
            stroke={color}
            strokeWidth={2}
            strokeDasharray="5 4"
            dot={false}
            isAnimationActive={false}
            connectNulls
          />
          {/* logged readings (right, %) */}
          <Scatter
            yAxisId="pct"
            data={fidData}
            dataKey="percent"
            name="reading"
            fill="var(--text-primary)"
            stroke="var(--surface-1)"
            strokeWidth={2}
            isAnimationActive={false}
          />
        </ComposedChart>
      </ResponsiveContainer>

      <p className="note">
        Left axis: observed local tokens (cumulative, by model). Right axis: logged % (● dots) and
        the ±1σ projection (shaded, dashed mid) toward the 100% cap. The axes are aligned — 100% =
        your local token count at the last reading ÷ its %, so the token area reaches each dot.
      </p>

      <div className="stats">
        <Stat label="Observed tokens" value={fmtTokens(c.consumed)} />
        <Stat
          label="Cap"
          value={c.cap != null ? fmtTokens(c.cap) : "—"}
          sub={c.cap_source ? CAP_SOURCE_LABEL[c.cap_source] ?? c.cap_source : "not set"}
        />
        <Stat label="Now" value={fmtPct(c.pct_now)} />
        <Stat
          label={c.mode === "rolling" ? "Sustained pace" : "Projected by reset"}
          value={c.low_confidence ? "—" : fmtPct(c.pct_projected)}
          sub={(() => {
            if (c.low_confidence) return "too early";
            const { lo, hi } = projRange(c);
            return lo != null && hi != null ? `±1σ ${fmtPct(lo)}–${fmtPct(hi)}` : c.forecast_model;
          })()}
        />
        <Stat label="ETA to cap" value={fmtDateTime(c.eta_to_limit)} />
        <Stat
          label={c.mode === "rolling" ? "Window" : "Resets"}
          value={c.mode === "rolling" ? "rolling 7-day" : fmtDateTime(c.window_end)}
        />
      </div>

      <AnchorForm service={c.service} onAnchor={onAnchor} />

      {c.fiducials.length > 0 && (
        <div className="readings">
          <div className="readings__label">Logged readings (ground truth)</div>
          <ul className="readings__list">
            {[...c.fiducials]
              .sort((a, b) => (a.ts < b.ts ? 1 : -1))
              .map((f, i) => (
                <li key={i}>
                  <b>{f.percent}%</b>
                  <span className="gauge__muted"> · {fmtDateTime(f.ts)}</span>
                </li>
              ))}
          </ul>
        </div>
      )}
    </section>
  );
}

function AnchorForm({ service, onAnchor }: { service: string; onAnchor: () => void }) {
  const [pct, setPct] = useState("");
  const [resets, setResets] = useState("");
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);

  const submit = async () => {
    const p = parseFloat(pct);
    if (!isFinite(p) || p <= 0 || p > 100) {
      setMsg("Enter a percent in (0, 100].");
      return;
    }
    setBusy(true);
    setMsg(null);
    try {
      const body: Record<string, unknown> = { service, percent: p };
      if (resets) body.resets_at = new Date(resets).toISOString();
      const r = await fetch("/api/anchor", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      const j = await r.json();
      if (r.ok) {
        setMsg(`Logged. Cap now ≈ ${fmtTokens(j.cap)} tokens/week (from your readings).`);
        setPct("");
        onAnchor();
      } else {
        setMsg(j.error ?? "Failed to log reading.");
      }
    } catch {
      setMsg("Request failed.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="anchor">
      <div className="anchor__row">
        <label>
          Current % (from your plan)
          <input
            type="number"
            min={0}
            max={100}
            placeholder="e.g. 47"
            value={pct}
            onChange={(e) => setPct(e.target.value)}
          />
        </label>
        <label>
          Reset time (optional)
          <input type="datetime-local" value={resets} onChange={(e) => setResets(e.target.value)} />
        </label>
        <button className="link-btn" onClick={submit} disabled={busy}>
          {busy ? "…" : "Log reading"}
        </button>
      </div>
      <p className="anchor__hint">
        Log the % your plan reports right now — saved as a raw ground-truth reading. The cap and
        projection calibrate to your readings; two or more let metoks cancel drift in its own token
        counting.
      </p>
      {msg && <p className="anchor__msg">{msg}</p>}
    </div>
  );
}

function Stat({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className="stat">
      <div className="stat__label">{label}</div>
      <div className="stat__value">{value}</div>
      {sub && <div className="stat__sub">{sub}</div>}
    </div>
  );
}
