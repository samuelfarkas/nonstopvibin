import { useCallback, useEffect, useRef, useState } from "react";
import { RefreshCw } from "lucide-react";
import type { ResetCredits as Credits } from "../shared/types.ts";
import { api } from "./api.ts";
import { exactTime } from "./format.ts";
import "./reset-credits.css";

export function ResetCredits({
  profileId,
  accountId,
  running,
  refresh,
}: {
  profileId: string;
  accountId: string;
  running: boolean;
  refresh(): Promise<void>;
}) {
  const [credits, setCredits] = useState<Credits>();
  const [selected, setSelected] = useState<Credits["credits"][number]>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [message, setMessage] = useState("");
  const restoreFocus = useRef(false);
  const cancelButton = useRef<HTMLButtonElement>(null);
  const refreshButton = useRef<HTMLButtonElement>(null);
  const path = `/profiles/${profileId}/accounts/${encodeURIComponent(accountId)}/reset-credits`;
  const load = useCallback(
    async (signal?: AbortSignal) => {
      setBusy(true);
      setError("");
      try {
        const result = await api<Credits>(path, "GET", undefined, signal);
        if (!signal?.aborted) setCredits(result);
      } catch (e) {
        if (!signal?.aborted)
          setError(e instanceof Error ? e.message : "Could not check resets.");
      } finally {
        if (!signal?.aborted) setBusy(false);
      }
    },
    [path],
  );
  useEffect(() => {
    if (!running) return;
    const controller = new AbortController();
    void load(controller.signal);
    return () => controller.abort();
  }, [load, running]);
  useEffect(() => {
    if (busy) return;
    if (selected) cancelButton.current?.focus();
    else if (restoreFocus.current) {
      refreshButton.current?.focus();
      restoreFocus.current = false;
    }
  }, [selected, busy]);
  async function consume() {
    if (!selected) return;
    setBusy(true);
    setError("");
    setMessage("");
    try {
      const result = await api<{
        code: "reset" | "already_redeemed" | "nothing_to_reset" | "no_credit";
        quotaRefreshed: boolean;
      }>(path, "POST", { creditId: selected.id });
      setMessage(
        {
          reset:
            "Reset applied. If requests still report exhausted quota, quit and reopen NonstopVibin to restart the profile proxy and clear its saved cooldown.",
          already_redeemed:
            "This reset was already applied. If requests still report exhausted quota, quit and reopen NonstopVibin to restart the profile proxy.",
          nothing_to_reset:
            "There are no eligible usage windows to reset. No reset was used.",
          no_credit: "This reset is no longer available. No reset was used.",
        }[result.code] +
          (result.quotaRefreshed
            ? ""
            : " Quotas could not be refreshed; check them again."),
      );
      setSelected(undefined);
      setCredits(undefined);
      await refresh();
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : "Reset outcome is unknown. Retry this same reset.",
      );
    } finally {
      setBusy(false);
    }
  }
  return (
    <section
      className="reset-credits"
      aria-label="Banked Codex resets"
      aria-busy={busy}
    >
      <div className="reset-credits-heading">
        <div>
          <h3>Banked resets</h3>
          <p className="reset-credits-caption" role="status">
            {!running
              ? "Start this profile to check availability."
              : busy && !selected
                ? "Checking availability…"
                : credits
                  ? credits.available_count === 0
                    ? "No resets available"
                    : `${credits.available_count} ${credits.available_count === 1 ? "reset" : "resets"} available`
                  : error
                    ? "Availability couldn’t be checked"
                    : "Saved resets for this subscription"}
          </p>
        </div>
        <button
          ref={refreshButton}
          type="button"
          className="text-button"
          aria-label="Refresh banked resets"
          disabled={busy || !running || !!selected}
          onClick={() => void load()}
        >
          <RefreshCw
            size={14}
            aria-hidden="true"
            className={busy && !selected ? "spin" : undefined}
          />
          Refresh
        </button>
      </div>
      {credits &&
        !selected &&
        credits.credits
          .filter((credit) => credit.status === "available")
          .map((credit) => (
            <div key={credit.id} className="reset-credit-row">
              <div className="reset-credit-copy">
                <strong>{credit.title || credit.reset_type}</strong>
                {credit.description && (
                  <p className="reset-credits-caption">{credit.description}</p>
                )}
                <p className="reset-credits-caption">
                  {credit.expires_at
                    ? `Expires ${exactTime(credit.expires_at)}`
                    : "No expiration provided"}
                </p>
              </div>
              <button
                type="button"
                className="button small"
                disabled={
                  busy ||
                  !running ||
                  (!!credit.expires_at &&
                    Date.parse(credit.expires_at) <= Date.now())
                }
                onClick={() => {
                  restoreFocus.current = true;
                  setSelected(credit);
                  setMessage("");
                }}
              >
                Review reset
              </button>
            </div>
          ))}
      {selected && (
        <div className="reset-credit-confirm">
          <h4>Use this reset?</h4>
          <strong>{selected.title || selected.reset_type}</strong>
          <p className="reset-credits-caption">
            {selected.expires_at
              ? `Expires ${exactTime(selected.expires_at)}`
              : "No expiration provided"}
          </p>
          <p>
            This uses one banked reset for this subscription. Your scheduled
            reset dates may change.
          </p>
          <div className="reset-credit-actions">
            <button
              ref={cancelButton}
              type="button"
              className="button small"
              disabled={busy}
              onClick={() => setSelected(undefined)}
            >
              Cancel
            </button>
            <button
              type="button"
              className="button small primary"
              disabled={busy || !running}
              onClick={() => void consume()}
            >
              {busy ? "Applying…" : "Use 1 reset"}
            </button>
          </div>
        </div>
      )}
      {message && (
        <p className="reset-credit-feedback" role="status">
          {message}
        </p>
      )}
      {error && (
        <p role="alert" className="inline-error reset-credit-feedback">
          {error}
        </p>
      )}
    </section>
  );
}
