import type { Account, Json, Quota, QuotaWindow } from "../shared/types.ts";
import {
  providerLabel,
  providers,
  type OAuthProvider,
} from "../shared/providers.ts";
import type { Accounts } from "./accounts.ts";
import { userAgent } from "./config.ts";
import type { CorePool } from "./core.ts";
import { AppError, errorMessage } from "./errors.ts";
import { claudeOAuthBeta } from "./identity.ts";
import {
  number,
  record,
  responseJson,
  text,
  unwrap,
  type JsonObject,
} from "./json.ts";
import type { Store } from "./store.ts";

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
function codexQuota(body: JsonObject, now: number): Read {
  const windows: QuotaWindow[] = [];
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
  const bankedResets = number(
    record(body.rate_limit_reset_credits).available_count,
  );
  return {
    windows,
    ...(bankedResets !== null &&
      Number.isSafeInteger(bankedResets) &&
      bankedResets >= 0 && { bankedResets }),
  };
}
function claudeQuota(body: JsonObject, now: number): Read {
  const windows: QuotaWindow[] = [];
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
  return { windows };
}
function opencodeQuota(body: JsonObject, now: number): Read {
  const windows: QuotaWindow[] = [];
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
  return { windows };
}
function kimiQuota(body: JsonObject, now: number): Read {
  const windows: QuotaWindow[] = [];
  const entries = [
    ...(Object.keys(record(body.usage)).length
      ? [{ ...record(body.usage), label: "Weekly" }]
      : []),
    ...(Array.isArray(body.limits) ? body.limits : []),
  ];
  for (const item of entries) {
    const r = record(item);
    const detail = Object.keys(record(r.detail)).length ? record(r.detail) : r;
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
  return { windows };
}
function antigravityQuota(body: JsonObject): Read {
  const windows: QuotaWindow[] = [];
  const buckets = body.buckets ?? record(body.quotaSummary).buckets;
  if (Array.isArray(buckets))
    for (const item of buckets) {
      const w = record(item);
      const fraction = number(w.remainingFraction ?? w.remaining_fraction);
      windows.push({
        label: String(w.modelId ?? w.model_id ?? "Model quota"),
        remainingPercent:
          fraction === null ? null : Math.max(0, Math.min(100, fraction * 100)),
        resetsAt: timestamp(w.resetTime ?? w.reset_time),
      });
    }
  return { windows };
}
function xaiQuota(body: JsonObject): Read {
  const windows: QuotaWindow[] = [];
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
  return { windows };
}

/** A request the core sends upstream, putting the account's token in place of `$TOKEN$`. */
export interface UpstreamRequest {
  method: "GET" | "POST";
  url: string;
  /** The core drops undefined values. */
  header: Record<string, string | undefined>;
  data?: string;
}
/** Sends `request` through the core's /api-call with an account's credential. */
export async function callUpstream(
  pool: CorePool,
  profileId: string,
  authIndex: string | undefined,
  request: UpstreamRequest,
): Promise<{ status: number | null; body: Json | undefined }> {
  const response = record(
    await pool.management(profileId, "/api-call", "POST", {
      auth_index: authIndex,
      ...request,
    }),
  );
  return { status: number(response.status_code), body: response.body };
}
const bearer = {
  Authorization: "Bearer $TOKEN$",
  "Content-Type": "application/json",
};
/** ChatGPT's backend for Codex subscriptions: quota and banked resets. */
export const codexApi = "https://chatgpt.com/backend-api/wham";
export const codexHeader = (chatgptAccountId: string | undefined) => ({
  ...bearer,
  "Chatgpt-Account-Id": chatgptAccountId,
  "User-Agent": userAgent,
});
const get = (
  url: string,
  header: Record<string, string> = {},
): UpstreamRequest => ({
  method: "GET",
  url,
  header: { ...bearer, ...header },
});
const xaiBilling = "https://cli-chat-proxy.grok.com/v1/billing";

type Read = Pick<Quota, "windows" | "bankedResets">;
interface Asking {
  /** Through the core with the account's token; throws `failure`, or a generic message, unless the answer is HTTP 200. */
  send(request: UpstreamRequest, failure?: string): Promise<Json | undefined>;
  chatgptAccountId: string | undefined;
  apiKey(): string;
}
interface QuotaSource {
  /** OAuth accounts are asked through the core; others with their own key. */
  oauth: boolean;
  ask(asking: Asking): Promise<Json | undefined>;
  read(body: JsonObject, now: number): Read;
}
// How to ask each provider for its quota, and how to read the answer.
const quotaSources = {
  claude: {
    oauth: true,
    ask: ({ send }) =>
      send(
        get("https://api.anthropic.com/api/oauth/usage", {
          "anthropic-beta": claudeOAuthBeta,
        }),
      ),
    read: claudeQuota,
  },
  codex: {
    oauth: true,
    ask: ({ send, chatgptAccountId }) =>
      send({
        method: "GET",
        url: `${codexApi}/usage`,
        header: codexHeader(chatgptAccountId),
      }),
    read: codexQuota,
  },
  kimi: {
    oauth: true,
    ask: ({ send }) => send(get("https://api.kimi.com/coding/v1/usages")),
    read: kimiQuota,
  },
  antigravity: {
    oauth: true,
    ask: ({ send }) =>
      send({
        method: "POST",
        url: "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
        header: bearer,
        data: "{}",
      }),
    read: antigravityQuota,
  },
  xai: {
    oauth: true,
    // The monthly limit and the weekly credits are two calls.
    ask: async ({ send }) => {
      const monthly = await send(get(xaiBilling));
      const weekly = await send(
        get(`${xaiBilling}?format=credits`),
        "Grok weekly quota check failed. Try again later.",
      );
      try {
        return { monthly: unwrap(monthly), weekly: unwrap(weekly) };
      } catch {
        throw new Error("Quota response was not valid JSON.");
      }
    },
    read: xaiQuota,
  },
  "opencode-go": {
    oauth: false,
    ask: async ({ apiKey }) => {
      const response = await fetch(
        `${providers["opencode-go"].preset.baseUrl}/usage`,
        {
          headers: {
            Authorization: `Bearer ${apiKey()}`,
            "User-Agent": userAgent,
          },
          signal: AbortSignal.timeout(15_000),
          redirect: "error",
        },
      );
      if (!response.ok)
        throw new Error(
          `OpenCode Go returned HTTP ${response.status}. Check your API key or try again later.`,
        );
      try {
        return await responseJson(response);
      } catch {
        throw new Error("Quota response was not valid JSON.");
      }
    },
    read: opencodeQuota,
  },
} satisfies Record<OAuthProvider | "opencode-go", QuotaSource>;
type QuotaProvider = keyof typeof quotaSources;
const hasQuota = (provider: string): provider is QuotaProvider =>
  Object.hasOwn(quotaSources, provider);
function quotaSource(account: Account): QuotaSource | undefined {
  const source = hasQuota(account.provider)
    ? quotaSources[account.provider]
    : undefined;
  return source && (!source.oauth || account.kind === "oauth")
    ? source
    : undefined;
}

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
  const { windows, bankedResets } = hasQuota(provider)
    ? quotaSources[provider].read(body, now)
    : { windows: [], bankedResets: undefined };
  const plan = text(body.plan_type);
  return {
    status: windows.length ? "available" : "unavailable",
    windows,
    checkedAt: new Date(now).toISOString(),
    ...(plan !== undefined && { plan }),
    ...(bankedResets !== undefined && { bankedResets }),
  };
}

/** Each account's quota, asked of its provider and kept in the store. */
export class Quotas {
  private readonly pool: CorePool;
  private readonly store: Store;
  private readonly accounts: Accounts;
  private readonly refreshing = new Set<string>();
  constructor(pool: CorePool, accounts: Accounts) {
    this.pool = pool;
    this.store = accounts.store;
    this.accounts = accounts;
  }
  /** Re-reads a profile's accounts from its core, then refreshes the quota of each account `wanted` picks, in order. */
  async refreshAccounts(
    profileId: string,
    wanted: (account: Account) => boolean,
  ): Promise<void> {
    await this.accounts.sync(profileId);
    for (const account of this.accounts.list(profileId))
      if (wanted(account)) await this.refresh(profileId, account.id);
  }
  async refresh(profileId: string, accountId: string): Promise<Quota> {
    const account = this.accounts
      .list(profileId)
      .find((a) => a.id === accountId);
    if (!account) throw new AppError("Account not found.", 404);
    const key = `${profileId}:${accountId}`;
    const prior = this.store.quota(profileId, accountId);
    if (this.refreshing.has(key) && prior) return prior;
    if (this.refreshing.has(key))
      throw new AppError("Quota refresh is already in progress.", 409);
    this.refreshing.add(key);
    let quota: Quota;
    try {
      const source = quotaSource(account);
      if (!source) {
        quota = {
          status: "unavailable",
          windows: [],
          checkedAt: new Date().toISOString(),
        };
        this.store.saveQuota(profileId, accountId, quota);
        return quota;
      }
      const payload = await source.ask({
        send: async (request, failure) => {
          const { status, body } = await callUpstream(
            this.pool,
            profileId,
            account.authIndex,
            request,
          );
          if (status !== 200)
            throw new Error(
              failure ??
                `${providerLabel(account.provider)} returned HTTP ${status ?? "unknown"}. ${status === 401 || status === 403 ? "Reconnect this subscription." : "Try again later."}`,
            );
          return body;
        },
        chatgptAccountId: this.accounts.chatgptAccountId(profileId, accountId),
        apiKey: () => this.store.apiKey(accountId),
      });
      quota = parseQuota(account.provider, payload);
    } catch (error) {
      quota = {
        status: "error",
        windows: prior?.windows ?? [],
        checkedAt: prior?.checkedAt ?? new Date().toISOString(),
        error: errorMessage(error),
      };
    } finally {
      this.refreshing.delete(key);
    }
    this.store.saveQuota(profileId, accountId, quota);
    return quota;
  }
}
