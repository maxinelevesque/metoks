import type { Forecast } from "../types";
import { SERVICE_LABELS, STATUS_COLORS } from "../types";
import { fmtTokens, fmtPct } from "../format";

const STATUS_WORD: Record<string, string> = {
  green: "on track",
  amber: "warning",
  red: "over pace",
  unknown: "no cap set",
};

function Gauge({ f, selected, onSelect }: { f: Forecast; selected: boolean; onSelect: () => void }) {
  const color = STATUS_COLORS[f.status] ?? STATUS_COLORS.unknown;
  const nowPct = Math.min(1, Math.max(0, f.pct_now ?? 0));
  const projPct = Math.min(1, Math.max(0, f.pct_projected ?? 0));
  const projExtra = f.low_confidence ? 0 : Math.max(0, projPct - nowPct);

  return (
    <button
      className={"gauge" + (selected ? " gauge--selected" : "")}
      onClick={onSelect}
      aria-pressed={selected}
    >
      <div className="gauge__head">
        <span className="gauge__name">{SERVICE_LABELS[f.service] ?? f.service}</span>
        <span className="gauge__status" style={{ color }}>
          ● {STATUS_WORD[f.status] ?? f.status}
        </span>
      </div>
      <div
        className="gauge__bar"
        role="img"
        aria-label={`${fmtPct(f.pct_now)} used${f.low_confidence ? "" : `, projected ${fmtPct(f.pct_projected)}`}`}
      >
        <div className="gauge__fill" style={{ width: nowPct * 100 + "%", background: color }} />
        <div
          className="gauge__fill gauge__fill--proj"
          style={{ width: projExtra * 100 + "%", background: color }}
        />
      </div>
      <div className="gauge__foot">
        <span>
          now <b>{fmtPct(f.pct_now)}</b>
        </span>
        <span className="gauge__muted">
          {f.low_confidence ? "proj — (early)" : <>proj <b>{fmtPct(f.pct_projected)}</b></>}
        </span>
        <span className="gauge__muted">
          {fmtTokens(f.consumed)}
          {f.limit != null ? " / " + fmtTokens(f.limit) : ""}
        </span>
      </div>
    </button>
  );
}

export default function StatusStrip({
  forecasts,
  selected,
  onSelect,
}: {
  forecasts: Forecast[];
  selected: string;
  onSelect: (s: string) => void;
}) {
  return (
    <div className="strip">
      {forecasts.map((f) => (
        <Gauge
          key={f.service}
          f={f}
          selected={f.service === selected}
          onSelect={() => onSelect(f.service)}
        />
      ))}
    </div>
  );
}
