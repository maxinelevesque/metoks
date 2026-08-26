import { useEffect, useState } from "react";
import type { Breakdown as BreakdownData, BreakdownRow } from "../types";
import { SERVICE_COLORS, SERVICE_LABELS, modelLabel } from "../types";
import { fmtTokens } from "../format";

export default function Breakdown({ refreshKey }: { refreshKey: number }) {
  const [data, setData] = useState<BreakdownData | null>(null);
  const [tab, setTab] = useState<"by_model" | "by_project">("by_model");

  useEffect(() => {
    fetch("/api/breakdown?days=7")
      .then((r) => r.json())
      .then(setData)
      .catch(() => {});
  }, [refreshKey]);

  const rows: BreakdownRow[] = (data ? data[tab] : []).slice(0, 15);
  const maxTokens = rows.reduce((m, r) => Math.max(m, r.tokens), 0) || 1;

  return (
    <section className="card">
      <div className="card__head">
        <h2>Breakdown — last 7 days</h2>
        <div className="toggle">
          <button
            className={"toggle__btn" + (tab === "by_model" ? " toggle__btn--on" : "")}
            onClick={() => setTab("by_model")}
          >
            By model
          </button>
          <button
            className={"toggle__btn" + (tab === "by_project" ? " toggle__btn--on" : "")}
            onClick={() => setTab("by_project")}
          >
            By project
          </button>
        </div>
      </div>
      <table className="table">
        <thead>
          <tr>
            <th>Service</th>
            <th>{tab === "by_model" ? "Model" : "Project"}</th>
            <th className="num">Events</th>
            <th className="num">Tokens</th>
            <th className="bar-col">Share</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i}>
              <td>
                <span className="dot" style={{ background: SERVICE_COLORS[r.service] ?? "#888" }} />
                {SERVICE_LABELS[r.service] ?? r.service}
              </td>
              <td className="mono">{tab === "by_model" ? modelLabel(r.key) : r.key}</td>
              <td className="num">{r.events.toLocaleString()}</td>
              <td className="num">{fmtTokens(r.tokens)}</td>
              <td className="bar-col">
                <span
                  className="minibar"
                  style={{
                    width: (r.tokens / maxTokens) * 100 + "%",
                    background: SERVICE_COLORS[r.service] ?? "#888",
                  }}
                />
              </td>
            </tr>
          ))}
          {rows.length === 0 && (
            <tr>
              <td colSpan={5} className="empty">
                No usage in the last 7 days.
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </section>
  );
}
