import { useCallback, useEffect, useRef, useState } from "react";
import type { Snapshot } from "./types";

type Conn = "sse" | "polling" | "connecting";

/**
 * Live snapshot feed: connect to /api/stream (SSE); if it drops or errors,
 * fall back to polling /api/snapshot every 5s (DESIGN.md §12). `refresh()` forces
 * an immediate fetch (e.g. right after logging a reading).
 */
export function useSnapshot(): {
  snapshot: Snapshot | null;
  conn: Conn;
  refresh: () => void;
} {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [conn, setConn] = useState<Conn>("connecting");
  const pollTimer = useRef<number | null>(null);

  const refresh = useCallback(async () => {
    try {
      const r = await fetch("/api/snapshot");
      if (r.ok) setSnapshot(await r.json());
    } catch {
      /* ignore */
    }
  }, []);

  useEffect(() => {
    let es: EventSource | null = null;
    let stopped = false;

    const startPolling = () => {
      if (pollTimer.current != null) return;
      setConn("polling");
      const tick = async () => {
        try {
          const r = await fetch("/api/snapshot");
          if (r.ok) setSnapshot(await r.json());
        } catch {
          /* keep trying */
        }
      };
      tick();
      pollTimer.current = window.setInterval(tick, 5000);
    };

    const stopPolling = () => {
      if (pollTimer.current != null) {
        clearInterval(pollTimer.current);
        pollTimer.current = null;
      }
    };

    const connect = () => {
      try {
        es = new EventSource("/api/stream");
        es.addEventListener("snapshot", (e) => {
          if (stopped) return;
          stopPolling();
          setConn("sse");
          try {
            setSnapshot(JSON.parse((e as MessageEvent).data));
          } catch {
            /* ignore malformed */
          }
        });
        es.onerror = () => {
          // Browser auto-reconnects SSE; meanwhile poll so data stays fresh.
          startPolling();
        };
      } catch {
        startPolling();
      }
    };

    connect();

    return () => {
      stopped = true;
      es?.close();
      stopPolling();
    };
  }, []);

  return { snapshot, conn, refresh };
}
