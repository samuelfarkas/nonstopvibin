import { ArrowRight, ChevronRight, RefreshCw } from "lucide-react";
import type { AppState } from "../shared/types.ts";
import { providerLabel } from "../shared/providers.ts";
import { api, desktop } from "./api.ts";
import { useAction } from "./action.ts";
import { Opening, useAppState } from "./feed.tsx";
import {
  Empty,
  Logo,
  ProviderIcon,
  QuotaMeter,
  logoState,
} from "./components.tsx";
import { accountLabel, age, groupAccountsByProvider } from "./format.ts";
import {
  currentHeadroom,
  floor,
  isQuotaStale,
  meterLevel,
  quotaState,
  trayNotice,
} from "./quota.ts";
import "./tray.css";

/** It never guards a close: the panel must stay closable. */
export function TrayWindow() {
  const { state, refresh, failure, sessionRefused } = useAppState();
  return state ? (
    <TrayView state={state} refresh={refresh} />
  ) : (
    <Opening failure={failure} sessionRefused={sessionRefused} />
  );
}

export function TrayView({
  state,
  refresh,
}: {
  state: AppState;
  refresh(next?: AppState): Promise<void>;
}) {
  const { busy, error, run } = useAction();
  const subscriptions = state.profiles.reduce(
    (sum, p) => sum + p.accounts.length,
    0,
  );
  return (
    <div className="tray-view">
      <header>
        <Logo state={logoState(state.profiles)} />
        <div className="tray-heading">
          <strong>Headroom</strong>
          <span>What’s left to vibe with</span>
        </div>
        <button
          className="icon-button"
          aria-label="Refresh all quotas"
          disabled={busy}
          onClick={() =>
            run(async () => {
              let next: AppState | undefined;
              for (const p of state.profiles.filter(
                (p) => p.runtime === "running",
              ))
                next = await api("POST /profiles/:profileId/refresh", {
                  params: { profileId: p.id },
                });
              await refresh(next);
            }, "Refresh failed.")
          }
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
            <p>Create your first profile in the app to see its quotas here.</p>
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
                    const { remaining, enabled, readings } =
                      currentHeadroom(accounts);
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
                          {enabled} enabled ·{" "}
                          {readings < enabled
                            ? `${readings}/${enabled} current readings`
                            : `${readings} current reading${readings === 1 ? "" : "s"}`}
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
                  onClick={() => desktop?.showWindow(p.id)}
                >
                  Open profile <ArrowRight size={13} aria-hidden="true" />
                </button>
                {p.accounts.length ? (
                  p.accounts.map((a) => {
                    const remaining = floor(a);
                    const stale = isQuotaStale(a.quota);
                    const notice = trayNotice(a);
                    return (
                      <details
                        className={`tray-account ${stale ? "stale" : ""} ${a.disabled ? "disabled" : ""}`}
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
                            {notice && (
                              <small className="tray-account-notice">
                                {notice}
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
                              <QuotaMeter
                                key={w.label}
                                window={w}
                                stale={stale}
                              />
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
                              {quotaState.failed} · showing last reading
                            </small>
                          )}
                          <small className="tray-age">
                            {stale ? "stale · " : ""}
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
        <button className="text-button" onClick={() => desktop?.showWindow()}>
          Open app <ArrowRight size={13} />
        </button>
      </footer>
    </div>
  );
}
