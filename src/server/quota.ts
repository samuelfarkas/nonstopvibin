import type { Json, Quota, QuotaWindow } from "../shared/types.ts";
import { number, record, text, unwrap, type JsonObject } from "./json.ts";

function remaining(used: Json | undefined): number | null {
  const n = number(used);
  return n === null ? null : Math.max(0, Math.min(100, 100 - n));
}
function timestamp(value: Json | undefined): string | null {
  if (value === null || value === undefined || value === "") return null;
  const n = number(value);
  const date =
    n === null ? new Date(String(value)) : new Date(n < 1e12 ? n * 1000 : n);
  return Number.isFinite(date.getTime()) ? date.toISOString() : null;
}
function reset(raw: JsonObject, now: number): string | null {
  const absolute = timestamp(
    raw.resets_at ?? raw.reset_at ?? raw.resetAt ?? raw.resetsAt,
  );
  if (absolute) return absolute;
  const seconds = number(
    raw.reset_after_seconds ??
      raw.resetInSec ??
      raw.resetInSeconds ??
      raw.reset_in_sec,
  );
  return seconds !== null && seconds >= 0
    ? new Date(now + seconds * 1000).toISOString()
    : null;
}
function durationLabel(value: Json | undefined, fallback: string): string {
  const seconds = number(value);
  if (seconds === 18_000) return "5-hour";
  if (seconds === 604_800) return "Weekly";
  if (seconds !== null && seconds >= 2_419_200 && seconds <= 2_764_800)
    return "Monthly";
  return seconds !== null && seconds > 0
    ? `${Math.round(seconds / 3600)}-hour`
    : fallback;
}
// Account-wide windows decide headroom; model windows are marked scoped.
const claudeWindows = [
  ["five_hour", "5-hour", false],
  ["seven_day", "Weekly", false],
  ["seven_day_opus", "Opus weekly", true],
  ["seven_day_sonnet", "Sonnet weekly", true],
  ["seven_day_oauth_apps", "OAuth weekly", true],
  ["seven_day_cowork", "Cowork weekly", true],
] as const;
export function parseQuota(
  provider: string,
  payload: Json | undefined,
  now = Date.now(),
): Quota {
  let body: JsonObject;
  try {
    body = record(unwrap(payload));
  } catch {
    throw new Error("Quota response was not valid JSON.");
  }
  const windows: QuotaWindow[] = [];
  if (provider === "codex") {
    const add = (value: Json | undefined, prefix = "") => {
      const rate = record(value);
      for (const [key, label] of [
        ["primary_window", "Session"],
        ["secondary_window", "Long window"],
      ]) {
        const w = record(rate[key]);
        if (!Object.keys(w).length) continue;
        windows.push({
          label: prefix + durationLabel(w.limit_window_seconds, label),
          remainingPercent: remaining(w.used_percent),
          resetsAt: reset(w, now),
        });
      }
    };
    add(body.rate_limit);
    add(body.code_review_rate_limit, "Code review · ");
    if (Array.isArray(body.additional_rate_limits))
      for (const item of body.additional_rate_limits) {
        const r = record(item);
        add(
          r.rate_limit,
          `${String(r.limit_name ?? r.metered_feature ?? "Additional")} · `,
        );
      }
  } else if (provider === "claude") {
    for (const [key, label, scoped] of claudeWindows) {
      const w = record(body[key]);
      if (!Object.keys(w).length) continue;
      const window: QuotaWindow = {
        label,
        remainingPercent: remaining(w.utilization),
        resetsAt: reset(w, now),
      };
      if (scoped) window.scoped = true;
      windows.push(window);
    }
    if (Array.isArray(body.limits))
      for (const item of body.limits) {
        const w = record(item);
        const model = text(record(record(w.scope).model).display_name)?.trim();
        if (w.kind !== "weekly_scoped" || !model) continue;
        const label = `${model} weekly`;
        if (windows.some((window) => window.label === label)) continue;
        windows.push({
          label,
          remainingPercent: remaining(w.percent),
          resetsAt: reset(w, now),
          scoped: true,
        });
      }
  } else if (provider === "opencode-go") {
    const usage = record(body.usage);
    for (const [key, label] of [
      ["rolling", "5-hour"],
      ["weekly", "Weekly"],
      ["monthly", "Monthly"],
    ]) {
      const w = record(usage[key]);
      if (!Object.keys(w).length) continue;
      const used =
        w.usagePercent ??
        w.usedPercent ??
        w.used_percent ??
        w.utilization ??
        w.percent;
      const limit = number(w.limit);
      const count = number(w.used);
      windows.push({
        label,
        remainingPercent: remaining(
          used ??
            (limit !== null && limit > 0 && count !== null
              ? (count / limit) * 100
              : null),
        ),
        resetsAt: reset(w, now),
      });
    }
  } else if (provider === "kimi") {
    const entries = [
      ...(Object.keys(record(body.usage)).length
        ? [{ ...record(body.usage), label: "Weekly" }]
        : []),
      ...(Array.isArray(body.limits) ? body.limits : []),
    ];
    for (const item of entries) {
      const r = record(item);
      const detail = Object.keys(record(r.detail)).length
        ? record(r.detail)
        : r;
      const limit = number(detail.limit);
      const left = number(detail.remaining);
      const used = number(detail.used);
      const pct =
        limit !== null && limit > 0
          ? left !== null
            ? (left / limit) * 100
            : used !== null
              ? 100 - (used / limit) * 100
              : null
          : null;
      windows.push({
        label: String(r.label ?? "Rate limit"),
        remainingPercent: pct === null ? null : Math.max(0, Math.min(100, pct)),
        resetsAt:
          timestamp(detail.resetTime ?? detail.reset_time ?? r.resetTime) ??
          reset(detail, now),
      });
    }
  } else if (provider === "antigravity") {
    const buckets = body.buckets ?? record(body.quotaSummary).buckets;
    if (Array.isArray(buckets))
      for (const item of buckets) {
        const w = record(item);
        const fraction = number(w.remainingFraction ?? w.remaining_fraction);
        windows.push({
          label: String(w.modelId ?? w.model_id ?? "Model quota"),
          remainingPercent:
            fraction === null
              ? null
              : Math.max(0, Math.min(100, fraction * 100)),
          resetsAt: timestamp(w.resetTime ?? w.reset_time),
        });
      }
  } else if (provider === "xai") {
    const payloads =
      body.weekly || body.monthly ? [body.weekly, body.monthly] : [body];
    for (const payload of payloads) {
      const raw = record(payload);
      const config = Object.keys(record(raw.config)).length
        ? record(raw.config)
        : raw;
      const period = record(config.current_period ?? config.currentPeriod);
      const usedPercent = number(
        config.credit_usage_percent ?? config.creditUsagePercent,
      );
      if (usedPercent !== null || String(period.type).includes("week"))
        windows.push({
          label: "Weekly",
          remainingPercent: remaining(usedPercent),
          resetsAt: timestamp(period.end),
        });
      const products = config.product_usage ?? config.productUsage;
      if (Array.isArray(products))
        for (const item of products) {
          const product = record(item);
          windows.push({
            label: String(product.product ?? "Product quota"),
            remainingPercent: remaining(
              product.usage_percent ?? product.usagePercent,
            ),
            resetsAt: timestamp(period.end),
          });
        }
      const limit = number(config.monthly_limit ?? config.monthlyLimit);
      const used = number(config.used);
      if (limit !== null)
        windows.push({
          label: "Monthly",
          remainingPercent:
            limit > 0 && used !== null ? remaining((used / limit) * 100) : null,
          resetsAt: timestamp(
            config.billing_period_end ?? config.billingPeriodEnd,
          ),
        });
    }
  }
  const plan = text(body.plan_type);
  const bankedResets =
    provider === "codex"
      ? number(record(body.rate_limit_reset_credits).available_count)
      : null;
  return {
    status: windows.length ? "available" : "unavailable",
    windows,
    checkedAt: new Date(now).toISOString(),
    ...(plan !== undefined && { plan }),
    ...(bankedResets !== null &&
      Number.isSafeInteger(bankedResets) &&
      bankedResets >= 0 && { bankedResets }),
  };
}
