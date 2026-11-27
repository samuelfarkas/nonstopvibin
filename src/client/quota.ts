// Level colours are the --level-* custom properties in styles.css.
import type { Account, Quota } from "../shared/types.ts";
import { quotaStaleMs } from "../shared/vocabulary.ts";

/** Below this share left, a quota is low. */
const lowPercent = 20;

/** Lowest remaining window: the limit a request hits first. */
export function floor(account: Account): number | null {
  const values = (account.quota?.windows ?? [])
    .map((w) => w.remainingPercent)
    .filter((v): v is number => v !== null);
  return values.length ? Math.min(...values) : null;
}
export function isQuotaStale(quota?: Quota, now = Date.now()): boolean {
  return Boolean(
    quota &&
    (quota.status === "error" ||
      now - new Date(quota.checkedAt).getTime() > quotaStaleMs),
  );
}
export const meterLevel = (pct: number | null | undefined) =>
  pct === null || pct === undefined
    ? "unknown"
    : pct <= 0
      ? "empty"
      : pct < lowPercent
        ? "low"
        : "";

const average = (values: number[]) =>
  values.length ? values.reduce((sum, v) => sum + v, 0) / values.length : null;

/** Subscriptions page: the profile's routing pool. A paused account adds no
    headroom, and the last reading counts even when it is stale. */
export function poolHeadroom(accounts: Account[]) {
  const values = accounts.map((a) => (a.disabled ? 0 : floor(a)));
  const mean = average(values.filter((v): v is number => v !== null));
  const counts = { ready: 0, low: 0, out: 0 };
  for (const a of accounts) {
    const level = meterLevel(floor(a));
    if (a.disabled || level === "empty") counts.out++;
    else if (level === "low") counts.low++;
    else counts.ready++;
  }
  return { average: mean === null ? null : Math.round(mean), values, counts };
}
/** Tray: a glance at what is usable now. Only enabled plans with a current
    reading count, and the caption says how many readings that was. */
export function currentHeadroom(accounts: Account[]) {
  const enabled = accounts.filter((a) => !a.disabled);
  const readings = enabled
    .filter((a) => a.quota?.status === "available" && !isQuotaStale(a.quota))
    .map(floor)
    .filter((v): v is number => v !== null);
  return {
    remaining: average(readings),
    enabled: enabled.length,
    readings: readings.length,
  };
}

/** What both windows call an account's quota state. */
export const quotaState = {
  paused: "Paused",
  failed: "Check failed",
  stale: "Stale",
} as const;
export function accountTag(
  a: Account,
): { text: string; tone: "out" | "bad" | "warn" | "" } | null {
  const level = meterLevel(floor(a));
  return a.disabled
    ? { text: quotaState.paused, tone: "out" }
    : a.status === "Reconnect needed"
      ? { text: "Reconnect needed", tone: "bad" }
      : a.quota?.status === "error"
        ? { text: quotaState.failed, tone: "bad" }
        : level === "empty"
          ? { text: "Out", tone: "out" }
          : level === "low"
            ? { text: "Low", tone: "warn" }
            : isQuotaStale(a.quota)
              ? { text: quotaState.stale, tone: "" }
              : null;
}
/** The tray's line under an account: the shared term, then what the numbers below it are. */
export function trayNotice(a: Account): string | null {
  return a.disabled
    ? quotaState.paused
    : a.quota?.status === "error"
      ? a.quota.windows.length
        ? `${quotaState.failed} · last reading`
        : quotaState.failed
      : isQuotaStale(a.quota)
        ? quotaState.stale
        : a.quota?.status === "unavailable"
          ? "Quota unavailable"
          : null;
}
