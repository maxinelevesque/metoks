import { useEffect, useState } from "react";
import type { ProjectSeries } from "../types";
import { fmtTokens } from "../format";

/** Compact normalized sparkline (line + dots) in inline SVG. */
function SparkLine({ points, color }: { points: number[]; color: string }) {
  const w = 260;
  const h = 34;
  const pad = 3;
  const n = points.length;
  const max = Math.max(1e-9, ...points);
  const x = (i: number) => (n <= 1 ? pad : pad + (i * (w - 2 * pad)) / (n - 1));
  const y = (v: number) => h - pad - (v / max) * (h - 2 * pad);
  const path = points.map((v, i) => `${i === 0 ? "M" : "L"}${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(" ");
  return (
    <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} className="spark" preserveAspectRatio="none">
      <path d={path} fill="none" stroke={color} strokeWidth={1.5} />
      {points.map((v, i) => (
        <circle key={i} cx={x(i)} cy={y(v)} r={v > 0 ? 1.4 : 0} fill={color} />
      ))}
    </svg>
  );
}

const PALETTE = ["#3987e5", "#199e70", "#c98500", "#d55181", "#9085e9", "#d95926", "#e66767"];

export default function Projects({ tick }: { tick: string }) {
  const [projects, setProjects] = useState<ProjectSeries[]>([]);
  const [days, setDays] = useState(14);

  useEffect(() => {
    fetch(`/api/projects?days=${days}`)
      .then((r) => r.json())
      .then((j) => setProjects(j.projects ?? []))
      .catch(() => {});
  }, [days, tick]);

  return (
    <section className="card">
      <div className="card__head">
        <h2>Projects — tokens over the last {days} days (each normalized)</h2>
        <div className="toggle">
          {[7, 14, 30].map((d) => (
            <button
              key={d}
              className={"toggle__btn" + (d === days ? " toggle__btn--on" : "")}
              onClick={() => setDays(d)}
            >
              {d}d
            </button>
          ))}
        </div>
      </div>
      {projects.length === 0 ? (
        <p className="note">No project usage in this window.</p>
      ) : (
        <div className="projects">
          {projects.slice(0, 40).map((p, i) => (
            <div className="project-row" key={p.project}>
              <div className="project-row__label">
                <span className="project-row__name" style={{ color: PALETTE[i % PALETTE.length] }}>
                  {p.project}
                </span>
                <span className="gauge__muted">{fmtTokens(p.total)} tok</span>
              </div>
              <SparkLine points={p.points} color={PALETTE[i % PALETTE.length]} />
            </div>
          ))}
        </div>
      )}
    </section>
  );
}
