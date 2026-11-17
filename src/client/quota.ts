// Quota health: an account's floor, whether its reading is current, its level,
// how each window words it, and a provider's headroom. Level colours are the
// --level-* custom properties in styles.css.
import type { Account, Quota } from "../shared/types.ts";
import { quotaStaleMs } from "../shared/vocabulary.ts";

/** Below this share left, a quota is low. */
const lowPercent = 20;

/** Lowest remaining account-wide window: the limit a request hits first.
    Model-scoped windows (Opus, Fable…) never mark the whole account out. */
export function floor(account: Account): number | null {
  const windows = account.quota?.windows ?? [];
  const shared = windows.filter((w) => !w.scoped);
  const values = (shared.length ? shared : windows)
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
/** The class that colours a share left: "unknown", "empty", "low" or none. */
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

/** The tag beside an account on the Subscriptions page, most urgent first. */
export function accountTag(
  a: Account,
): { text: string; tone: "out" | "bad" | "warn" | "" } | null {
  const level = meterLevel(floor(a));
  return a.disabled
    ? { text: "Paused", tone: "out" }
    : a.status === "Reconnect needed"
      ? { text: "Reconnect needed", tone: "bad" }
      : a.quota?.status === "error"
        ? { text: "Check failed", tone: "bad" }
        : level === "empty"
          ? { text: "Out", tone: "out" }
          : level === "low"
            ? { text: "Low", tone: "warn" }
            : isQuotaStale(a.quota)
              ? { text: "Stale", tone: "" }
              : null;
}
/** The tray's line under an account when its reading is not a current one. */
export function trayNotice(a: Account): string | null {
  const stale = isQuotaStale(a.quota);
  return a.disabled
    ? "Disabled"
    : a.quota?.status === "error"
      ? "Check failed · last reading"
      : stale
        ? "Stale reading"
        : a.quota?.status === "unavailable"
          ? "Quota unavailable"
          : null;
}
