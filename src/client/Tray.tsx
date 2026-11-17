import { useState } from "react";
import { ArrowRight, ChevronRight, RefreshCw } from "lucide-react";
import type { AppState } from "../shared/types.ts";
import { providerLabel } from "../shared/providers.ts";
import { api } from "./api.ts";
import { Empty, Logo, ProviderIcon, meterLevel } from "./components.tsx";
import {
  accountLabel,
  age,
  exactTime,
  floor,
  groupAccountsByProvider,
  isQuotaStale,
  resetIn,
} from "./format.ts";
import "./tray.css";

export function TrayView({
  state,
  refresh,
}: {
  state: AppState;
  refresh(next?: AppState): Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const subscriptions = state.profiles.reduce(
    (sum, p) => sum + p.accounts.length,
    0,
  );
  return (
    <div className="tray-view">
      <header>
        <Logo
          state={
            state.profiles.some((p) => p.runtime === "running")
              ? "running"
              : state.profiles.some((p) => p.runtime === "starting")
                ? "paused"
                : "stopped"
          }
        />
        <div className="tray-heading">
          <strong>Headroom</strong>
          <span>What’s left to vibe with</span>
        </div>
        <button
          className="icon-button"
          aria-label="Refresh all quotas"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            setError("");
            try {
              let next: AppState | undefined;
              for (const p of state.profiles.filter(
                (p) => p.runtime === "running",
              ))
                next = await api<AppState>(`/profiles/${p.id}/refresh`, "POST");
              await refresh(next);
            } catch (e) {
              setError(e instanceof Error ? e.message : "Refresh failed.");
            } finally {
              setBusy(false);
            }
          }}
        >
          <RefreshCw size={15} className={busy ? "spin" : ""} />
        </button>
      </header>
      {error && (
        <p className="inline-error" role="alert">
          {error}
        </p>
      )}
      <div className="tray-scroll">
        {!state.profiles.length && (
          <Empty title="Your subscriptions at a glance">
            Create your first profile in the app to see its quotas here.
          </Empty>
        )}
        {state.profiles.map((p) => {
          return (
            <details className="tray-profile-group" key={p.id}>
              <summary className="tray-profile">
                <span className="tray-profile-title">
                  <strong title={p.name}>{p.name}</strong>
                  <span
                    className="tray-runtime"
                    data-running={p.runtime === "running"}
                  >
                    {p.runtime}
                  </span>
                  <ChevronRight
                    className="tray-disclosure"
                    size={14}
                    aria-hidden="true"
                  />
                </span>
                {[...groupAccountsByProvider(p.accounts)].map(
                  ([provider, accounts]) => {
                    const enabled = accounts.filter((a) => !a.disabled);
                    const readings = enabled
                      .filter(
                        (a) =>
                          a.quota?.status === "available" &&
                          !isQuotaStale(a.quota),
                      )
                      .map(floor)
                      .filter((v): v is number => v !== null);
                    const remaining = readings.length
                      ? readings.reduce((sum, value) => sum + value, 0) /
                        readings.length
                      : null;
                    const partial = readings.length < enabled.length;
                    return (
                      <span className="tray-provider" key={provider}>
                        <span className="tray-provider-name">
                          <ProviderIcon provider={provider} />
                          <strong>{providerLabel(provider)}</strong>
                        </span>
                        <span
                          className={`tray-profile-headroom tray-remaining ${meterLevel(remaining)}`}
                        >
                          <span className="tray-profile-metric">
                            <strong>
                              {remaining === null
                                ? "—"
                                : `${Math.round(remaining)}%`}
                            </strong>
                            <span>
                              {remaining === null
                                ? "No current quota"
                                : "left on average"}
                            </span>
                          </span>
                          <span className="meter-track" aria-hidden="true">
                            <span style={{ width: `${remaining ?? 0}%` }} />
                          </span>
                        </span>
                        <span className="tray-profile-caption">
                          {enabled.length} enabled ·{" "}
                          {partial
                            ? `${readings.length}/${enabled.length} current readings`
                            : `${readings.length} current reading${readings.length === 1 ? "" : "s"}`}
                        </span>
                      </span>
                    );
                  },
                )}
                {!p.accounts.length && (
                  <span className="tray-profile-caption">
                    No subscriptions yet · No current quota
                  </span>
                )}
              </summary>
              <div className="tray-profile-detail">
                <p className="tray-average-note">
                  Per-provider average of each enabled plan’s lowest quota.
                  Current readings only; plans count equally.
                </p>
                <button
                  className="button small tray-manage"
                  onClick={() => window.nonstopvibin?.showWindow(p.id)}
                >
                  Open profile <ArrowRight size={13} aria-hidden="true" />
                </button>
                {p.accounts.length ? (
                  p.accounts.map((a) => {
                    const remaining = floor(a);
                    const stale = isQuotaStale(a.quota);
                    return (
                      <details
                        className={`tray-account ${isQuotaStale(a.quota) ? "stale" : ""} ${a.disabled ? "disabled" : ""}`}
                        key={a.id}
                      >
                        <summary className="tray-account-summary">
                          <ProviderIcon provider={a.provider} />
                          <span className="tray-account-name">
                            <strong title={accountLabel(a)}>
                              {accountLabel(a)}
                            </strong>
                            <small title={a.email || a.status}>
                              {a.email || a.status}
                            </small>
                            {(a.disabled ||
                              stale ||
                              a.quota?.status === "unavailable") && (
                              <small className="tray-account-notice">
                                {a.disabled
                                  ? "Disabled"
                                  : a.quota?.status === "error"
                                    ? "Check failed · last reading"
                                    : stale
                                      ? "Stale reading"
                                      : "Quota unavailable"}
                              </small>
                            )}
                          </span>
                          <span
                            className={`tray-remaining ${meterLevel(remaining)}`}
                          >
                            <strong>
                              {remaining === null
                                ? "—"
                                : `${Math.round(remaining)}%`}
                            </strong>
                            <span
                              className={`meter-track ${meterLevel(remaining)}`}
                              aria-hidden="true"
                            >
                              <span style={{ width: `${remaining ?? 0}%` }} />
                            </span>
                          </span>
                          <ChevronRight
                            className="tray-disclosure"
                            size={14}
                            aria-hidden="true"
                          />
                        </summary>
                        <div className="tray-account-detail">
                          <p className="tray-detail-heading">Quota remaining</p>
                          {a.quota?.windows.length ? (
                            a.quota.windows.map((w) => (
                              <div className="tray-window" key={w.label}>
                                <span>{w.label}</span>
                                <div
                                  className={`meter-track ${meterLevel(w.remainingPercent)}`}
                                >
                                  <div
                                    style={{
                                      width: `${w.remainingPercent ?? 0}%`,
                                    }}
                                  />
                                </div>
                                <strong
                                  className={meterLevel(w.remainingPercent)}
                                >
                                  {w.remainingPercent === null
                                    ? "—"
                                    : `${Math.round(w.remainingPercent)}%`}
                                </strong>
                                <small title={exactTime(w.resetsAt)}>
                                  {resetIn(w.resetsAt)}
                                </small>
                              </div>
                            ))
                          ) : (
                            <small className="tray-unavailable">
                              {a.quota?.status === "unavailable"
                                ? "Quota unavailable for this provider"
                                : "Quota not checked"}
                            </small>
                          )}
                          {a.quota?.status === "error" && (
                            <small className="tray-error text-bad">
                              Check failed · showing last reading
                            </small>
                          )}
                          <small className="tray-age">
                            {isQuotaStale(a.quota) && a.quota ? "stale · " : ""}
                            {age(a.quota?.checkedAt)}
                          </small>
                        </div>
                      </details>
                    );
                  })
                ) : (
                  <p className="tray-empty">No subscriptions yet.</p>
                )}
              </div>
            </details>
          );
        })}
      </div>
      <footer>
        <span>
          {subscriptions} subscription{subscriptions === 1 ? "" : "s"}
        </span>
        <button
          className="text-button"
          onClick={() => window.nonstopvibin?.showWindow()}
        >
          Open app <ArrowRight size={13} />
        </button>
      </footer>
    </div>
  );
}
