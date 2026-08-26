import { useEffect, useMemo, useState } from "react";
import {
  Bar,
  BarChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
  CartesianGrid,
  Legend,
} from "recharts";
import type { Bucket } from "../types";
import {
  colorForModel,
  isKnownModel,
  modelLabel,
  OTHER_KEY,
  orderedModelKeys,
} from "../types";
import { fmtTokens } from "../format";

type BucketSize = "day" | "hour";
interface Row {
  t: number;
  [model: string]: number;
}

export default function UsageChart({ tick }: { tick: string }) {
  const [bucket, setBucket] = useState<BucketSize>("day");
  const [data, setData] = useState<Bucket[]>([]);

  useEffect(() => {
    const to = new Date();
    const from = new Date(
      to.getTime() - (bucket === "hour" ? 3 : 30) * 24 * 3600 * 1000
    );
    const url = `/api/timeseries?by=model&bucket=${bucket}&from=${from.toISOString()}&to=${to.toISOString()}`;
    fetch(url)
      .then((r) => r.json())
      .then((j) => setData(j.buckets ?? []))
      .catch(() => {});
  }, [bucket, tick]);

  const { rows, keys } = useMemo(() => {
    const present = new Set<string>();
    const byT = new Map<number, Row>();
    for (const b of data) {
      const t = new Date(b.ts).getTime();
      const key = isKnownModel(b.key) ? b.key : OTHER_KEY;
      present.add(b.key);
      const row = byT.get(t) ?? ({ t } as Row);
      row[key] = (row[key] ?? 0) + b.tokens;
      byT.set(t, row);
    }
    const keys = orderedModelKeys(present);
    const rows = Array.from(byT.values()).sort((a, b) => a.t - b.t);
    return { rows, keys };
  }, [data]);

  const fmtTick = (t: number) => {
    const d = new Date(t);
    if (bucket === "hour") {
      return d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric" });
    }
    return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
  };

  return (
    <section className="card">
      <div className="card__head">
        <h2>Tokens over time — by model</h2>
        <div className="toggle">
          {(["day", "hour"] as BucketSize[]).map((b) => (
            <button
              key={b}
              className={"toggle__btn" + (b === bucket ? " toggle__btn--on" : "")}
              onClick={() => setBucket(b)}
            >
              {b === "day" ? "Day" : "Hour"}
            </button>
          ))}
        </div>
      </div>
      <ResponsiveContainer width="100%" height={320}>
        <BarChart data={rows} margin={{ top: 8, right: 16, bottom: 4, left: 8 }} barCategoryGap="12%">
          <CartesianGrid stroke="var(--grid)" vertical={false} />
          <XAxis
            dataKey="t"
            tickFormatter={fmtTick}
            stroke="var(--text-muted)"
            fontSize={12}
            minTickGap={40}
          />
          <YAxis
            stroke="var(--text-muted)"
            fontSize={12}
            width={54}
            tickFormatter={(v) => fmtTokens(v)}
          />
          <Tooltip
            cursor={{ fill: "var(--surface-2)", opacity: 0.4 }}
            contentStyle={{
              background: "var(--surface-2)",
              border: "1px solid var(--border)",
              borderRadius: 8,
              color: "var(--text-primary)",
            }}
            labelFormatter={(t) => fmtTick(Number(t))}
            formatter={(v: number, name: string) => [fmtTokens(v), modelLabel(name)]}
          />
          <Legend formatter={(v) => modelLabel(v)} wrapperStyle={{ fontSize: 12 }} />
          {keys.map((k) => (
            <Bar
              key={k}
              dataKey={k}
              stackId="tok"
              fill={colorForModel(k)}
              isAnimationActive={false}
            />
          ))}
        </BarChart>
      </ResponsiveContainer>
    </section>
  );
}
