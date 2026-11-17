import { useCallback, useEffect, useState } from "react";
import { LoaderCircle } from "lucide-react";
import type { AppState } from "../shared/types.ts";
import { api, initializeSession, refusedSession } from "./api.ts";
import { messageOf } from "./action.ts";
import { Logo } from "./components.tsx";

export function usePolling(
  poll: ((signal: AbortSignal) => Promise<void>) | undefined,
  ms: number,
  { immediately = false, whileHidden = false } = {},
) {
  useEffect(() => {
    if (!poll) return;
    const controller = new AbortController();
    let busy = false;
    const tick = async () => {
      if (busy) return;
      busy = true;
      try {
        await poll(controller.signal);
      } finally {
        busy = false;
      }
    };
    if (immediately) void tick();
    let timer =
      whileHidden || !document.hidden ? setInterval(tick, ms) : undefined;
    const onVisibilityChange = () => {
      clearInterval(timer);
      timer = undefined;
      if (!document.hidden) {
        void tick();
        timer = setInterval(tick, ms);
      }
    };
    if (!whileHidden)
      document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      controller.abort();
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [poll, ms, immediately, whileHidden]);
}

export function useAppState() {
  const [state, setState] = useState<AppState>();
  const [ready, setReady] = useState(false);
  const [pollError, setPollError] = useState("");
  const [failure, setFailure] = useState("");
  const [sessionRefused, setSessionRefused] = useState(false);
  // Mutating routes return the full state; pass it to skip a second /state fetch.
  const refresh = useCallback(async (next?: AppState) => {
    setState(next ?? (await api("GET /state")));
  }, []);
  useEffect(() => {
    let mounted = true;
    initializeSession()
      .then(async () => {
        await refresh();
        if (mounted) setReady(true);
      })
      .catch((e) => {
        if (!mounted) return;
        setFailure(e.message);
        setSessionRefused(e instanceof Error && refusedSession(e));
      });
    return () => {
      mounted = false;
    };
  }, [refresh]);
  const poll = useCallback(async () => {
    try {
      await refresh();
      setPollError("");
    } catch (e) {
      const message = messageOf(e, "The local service is unavailable.");
      // A restarted server issues a new session token; show the recovery
      // screen rather than a stale workspace.
      if (e instanceof Error && refusedSession(e)) {
        setReady(false);
        setState(undefined);
        setFailure(message);
        setSessionRefused(true);
      } else setPollError(message);
    }
  }, [refresh]);
  usePolling(ready ? poll : undefined, 4000);
  return { state, refresh, pollError, setPollError, failure, sessionRefused };
}

export function Opening({
  failure,
  sessionRefused,
}: {
  failure: string;
  sessionRefused: boolean;
}) {
  return (
    <div className="fatal">
      <Logo size={40} word={false} />
      <h1>NonstopVibin</h1>
      {failure ? (
        <>
          <p role="alert">{failure}</p>
          {/* A refused session's message already carries this guidance. */}
          {!sessionRefused && (
            <p className="muted">
              Open NonstopVibin from the desktop app. On a server, run{" "}
              <code>nonstopvibin url</code> for a new link.
            </p>
          )}
          <button className="button" onClick={() => location.reload()}>
            Try again
          </button>
        </>
      ) : (
        <p>
          <LoaderCircle size={15} className="spin" /> Opening your workspace…
        </p>
      )}
    </div>
  );
}
