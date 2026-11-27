import type { Account } from "../shared/types.ts";

export function groupAccountsByProvider(
  accounts: Account[],
): Map<string, Account[]> {
  const groups = new Map<string, Account[]>();
  for (const account of accounts) {
    const group = groups.get(account.provider);
    if (group) group.push(account);
    else groups.set(account.provider, [account]);
  }
  return groups;
}

const plans = new Map([
  ["plus", "Plus"],
  ["pro", "Pro"],
  ["team", "Team"],
  ["business", "Business"],
  ["enterprise", "Enterprise"],
  ["edu", "Edu"],
  ["free", "Free"],
]);
export function planLabel(plan: string): string {
  const text = plans.get(plan) ?? plan.replace(/[_-]+/g, " ").trim();
  return text.charAt(0).toUpperCase() + text.slice(1);
}

export function resetIn(value: string | null, now = Date.now()): string {
  if (!value) return "no reset time";
  const seconds = Math.ceil((new Date(value).getTime() - now) / 1000);
  if (!Number.isFinite(seconds)) return "no reset time";
  if (seconds <= 0) return "reset due · refresh";
  const minutes = Math.ceil(seconds / 60);
  const hours = Math.floor(minutes / 60);
  const days = Math.floor(hours / 24);
  return days > 0
    ? `resets ${days}d ${hours % 24}h`
    : hours > 0
      ? `resets ${hours}h ${minutes % 60}m`
      : `resets ${minutes}m`;
}
export function exactTime(value: string | null): string {
  return value
    ? new Date(value).toLocaleString(undefined, {
        weekday: "short",
        month: "short",
        day: "numeric",
        hour: "numeric",
        minute: "2-digit",
        timeZoneName: "short",
      })
    : "The provider has not reported a reset time.";
}
export function age(value?: string): string {
  if (!value) return "not checked yet";
  const mins = Math.floor((Date.now() - new Date(value).getTime()) / 60_000);
  return mins < 1
    ? "checked just now"
    : `checked ${mins < 60 ? `${mins}m` : `${Math.floor(mins / 60)}h`} ago`;
}

const standardCount = new Intl.NumberFormat("en", { maximumFractionDigits: 1 });
const compactCount = new Intl.NumberFormat("en", {
  notation: "compact",
  maximumFractionDigits: 1,
});
export function count(n: number): string {
  return (n >= 100_000 ? compactCount : standardCount).format(n);
}
export function dateLabel(value: string, format: Intl.DateTimeFormat): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "Invalid Date" : format.format(date);
}
export function csvCell(value: string): string {
  return `"${value
    .replace(/^(\s*)([=+@\t\r-])/, "$1'$2")
    .replaceAll('"', '""')}"`;
}
