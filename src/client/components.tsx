import type { ReactNode } from "react";
import { Activity, Bot } from "lucide-react";
import { useState } from "react";
import type { Account, ProfileState, QuotaWindow } from "../shared/types.ts";
import { providerEntry } from "../shared/providers.ts";
import { exactTime, resetIn } from "./format.ts";
import { meterLevel } from "./quota.ts";

// Display widgets both windows draw with. Main-window controls are in controls.tsx.
export function AnimatedLogo({
  state,
}: {
  state: "running" | "paused" | "stopped";
}) {
  const [replay, setReplay] = useState(0);
  return (
    <button
      type="button"
      className="nv-logo-button"
      aria-label="NonstopVibin — replay logo animation"
      onClick={() => setReplay((value) => value + 1)}
    >
      <Logo key={replay} state={state} />
    </button>
  );
}

export function logoState(
  profiles: Pick<ProfileState, "runtime">[],
): "running" | "paused" | "stopped" {
  return profiles.some((p) => p.runtime === "running")
    ? "running"
    : profiles.some((p) => p.runtime === "starting")
      ? "paused"
      : "stopped";
}
export function Logo({
  state = "stopped",
  size = 24,
  word = true,
}: {
  state?: "running" | "paused" | "stopped";
  size?: number;
  word?: boolean;
}) {
  return (
    <span className="nv-logo">
      <span className="nv-ball-wrap" aria-hidden="true">
        <img
          className="nv-ball"
          data-state={state}
          src={`/tray/${state === "stopped" ? "stopped" : "ball"}.svg`}
          width={size}
          height={size}
          alt=""
        />
      </span>
      {word && (
        <span className="nv-text">
          <span className="nv-word">NONSTOP</span>
          <span className="nv-sub">VIBIN</span>
        </span>
      )}
    </span>
  );
}
export function ProviderIcon({ provider }: { provider: string }) {
  const asset = providerEntry(provider)?.icon;
  return (
    <span className="provider-icon">
      {asset ? (
        <img alt="" src={`/providers/${asset}.svg`} />
      ) : (
        <Bot size={25} />
      )}
    </span>
  );
}
type StatusValue = ProfileState["runtime"] | Account["status"];
/** Every runtime and account status, so tsc asks for a badge when the server adds one. */
const statuses: Record<
  StatusValue,
  { label: string; tone: "good" | "bad" | "" }
> = {
  stopped: { label: "Stopped", tone: "" },
  starting: { label: "Starting…", tone: "" },
  running: { label: "Running", tone: "good" },
  error: { label: "Error", tone: "bad" },
  Ready: { label: "Ready", tone: "good" },
  "Needs attention": { label: "Needs attention", tone: "bad" },
  "Reconnect needed": { label: "Reconnect needed", tone: "bad" },
  "Cooling down": { label: "Cooling down", tone: "" },
  Paused: { label: "Paused", tone: "" },
  Configured: { label: "Configured", tone: "" },
};
export function Status({ value }: { value: StatusValue }) {
  const { label, tone } = statuses[value];
  return (
    <span className={`status ${tone}`}>
      <span className="status-dot" />
      {label}
    </span>
  );
}
export function QuotaMeter({
  window,
  stale = false,
  unavailable = "not reported",
  label,
}: {
  window?: QuotaWindow;
  stale?: boolean;
  /** A row without a window still holds a meter's place; these say why it is empty. */
  unavailable?: string;
  label?: string;
}) {
  const pct = window?.remainingPercent;
  const level = meterLevel(pct);
  return (
    <div className={`quota-meter ${stale ? "stale" : ""} ${level}`}>
      <span className="label" title={label ?? window?.label}>
        {label ?? window?.label ?? "limit"}
      </span>
      <div className={`quota-value ${level}`}>
        {pct === null || pct === undefined ? (
          <>
            — <small>{unavailable}</small>
          </>
        ) : (
          `${Math.round(pct)}%`
        )}
      </div>
      <div
        className={`meter-track ${level}`}
        role={pct == null ? undefined : "meter"}
        aria-hidden={pct == null || undefined}
        aria-label={label ?? window?.label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={pct ?? undefined}
      >
        <div style={{ width: `${pct ?? 0}%` }} />
      </div>
      <div className="reset" title={exactTime(window?.resetsAt ?? null)}>
        {!window
          ? "—"
          : window.resetsAt || pct !== 100
            ? resetIn(window.resetsAt)
            : "nothing used yet"}
      </div>
    </div>
  );
}
export function AccountSummary({ account }: { account: Account }) {
  return (
    <div className="account-identity">
      <ProviderIcon provider={account.provider} />
      <div>
        <strong title={account.name}>{account.name}</strong>
        <div className="account-email">
          {account.email ||
            (account.kind === "api-key"
              ? `${account.prefix ? `${account.prefix}/ · ` : ""}${account.modelCount} model${account.modelCount === 1 ? "" : "s"}`
              : "OAuth subscription")}
        </div>
      </div>
    </div>
  );
}
export function Empty({
  icon,
  title,
  children,
}: {
  icon?: ReactNode;
  title: string;
  children: ReactNode;
}) {
  return (
    <div className="empty-state">
      <div className="empty-icon">{icon || <Activity size={22} />}</div>
      <h2>{title}</h2>
      {children}
    </div>
  );
}
