import { useEffect, useState } from "react";
import { useSnapshot } from "./useSnapshot";
import StatusStrip from "./components/StatusStrip";
import UsageChart from "./components/UsageChart";
import CumulativePanel from "./components/CumulativePanel";
import Breakdown from "./components/Breakdown";

export default function App() {
  const { snapshot, conn, refresh } = useSnapshot();
  const [selected, setSelected] = useState<string>("claude_code");
  const [refreshKey, setRefreshKey] = useState(0);

  const forecasts = snapshot?.forecast.forecasts ?? [];
  const cumulatives = snapshot?.forecast.cumulatives ?? [];

  useEffect(() => {
    if (forecasts.length && !forecasts.some((f) => f.service === selected)) {
      setSelected(forecasts[0].service);
    }
  }, [forecasts, selected]);

  const selCumulative = cumulatives.find((c) => c.service === selected);

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="brand__mark">◈</span> metoks
        </div>
        <div className="topbar__right">
          <span className={"conn conn--" + conn} title={"connection: " + conn}>
            {conn === "sse" ? "live" : conn === "polling" ? "polling" : "…"}
          </span>
        </div>
      </header>

      {!snapshot ? (
        <div className="loading">Connecting to metoks…</div>
      ) : (
        <main className="grid">
          <StatusStrip forecasts={forecasts} selected={selected} onSelect={setSelected} />

          {selCumulative && (
            <CumulativePanel
              c={selCumulative}
              onAnchor={() => {
                refresh(); // pull the new cap/%/dots immediately
                setRefreshKey((k) => k + 1);
              }}
            />
          )}

          <UsageChart tick={snapshot.generated_at} />

          <Breakdown refreshKey={refreshKey} />
        </main>
      )}

      <footer className="foot">
        local-first · no usage data leaves this machine ·{" "}
        {snapshot && (
          <span className="gauge__muted">
            updated {new Date(snapshot.generated_at).toLocaleTimeString()}
          </span>
        )}
      </footer>
    </div>
  );
}
