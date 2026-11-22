// Quotas (parseQuota and Quotas.refresh's upstream requests), account listings
// (Accounts.sync), Claude seat identity, Codex banked resets and API-key accounts.
// Upstream calls go through a stub core management API or a stubbed global fetch.
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { Accounts } from "../../../src/server/accounts.ts";
import {
  apiAccount,
  goModelProtocol,
} from "../../../src/server/api-accounts.ts";
import type { CorePool } from "../../../src/server/core.ts";
import { credentialDirectory } from "../../../src/server/data-directory.ts";
import {
  claudeIdentity,
  sameSeat,
  verifyClaudeIdentity,
} from "../../../src/server/identity.ts";
import { parseQuota, Quotas } from "../../../src/server/quota.ts";
import {
  CodexResets,
  resetCreditsSchema,
  resetResultSchema,
} from "../../../src/server/reset-credits.ts";
import { Store } from "../../../src/server/store.ts";
import { fileKeyCodec } from "../../../src/server/vault.ts";
import { type JsonObject, record, text } from "../../../src/server/json.ts";
import type { ApiAccountInput } from "../../../src/shared/api.ts";
import type { Json, SeatIdentity } from "../../../src/shared/types.ts";
import {
  type Case,
  fetchStub,
  outcome,
  Placeholders,
  scratch,
  stub,
} from "../lib.ts";

const now = Date.parse("2026-11-22T12:00:00.000Z");

const quotaPayloads: Array<[string, string, Json | undefined]> = [
  [
    "claude",
    "every window",
    {
      five_hour: { utilization: 25, resets_at: "2026-11-22T14:00:00Z" },
      seven_day: { utilization: 60.5, resets_at: "2026-11-27T00:00:00+02:00" },
      seven_day_opus: { utilization: 90, resets_at: "2026-11-27T00:00:00Z" },
      seven_day_sonnet: { utilization: 0 },
      seven_day_oauth_apps: { utilization: 100, resets_at: 1_795_000_000 },
      seven_day_cowork: { utilization: 10, resets_at: 1_795_000_000_000 },
    },
  ],
  [
    "claude",
    "limits scoped and duplicates",
    {
      seven_day_opus: { utilization: 50 },
      limits: [
        {
          kind: "weekly_scoped",
          scope: { model: { display_name: "Opus" } },
          percent: 99,
          resets_at: "2026-11-27T00:00:00Z",
        },
        {
          kind: "weekly_scoped",
          scope: { model: { display_name: " Fable " } },
          percent: 5,
        },
        {
          kind: "weekly_scoped",
          scope: { model: { display_name: "Fable" } },
          percent: 50,
        },
        {
          kind: "weekly_scoped",
          scope: { model: { display_name: "" } },
          percent: 1,
        },
        { kind: "weekly_scoped", scope: { model: {} }, percent: 1 },
        {
          kind: "daily",
          scope: { model: { display_name: "Haiku" } },
          percent: 1,
        },
        "junk",
      ],
    },
  ],
  [
    "claude",
    "odd utilizations",
    {
      five_hour: { utilization: "40", reset_after_seconds: 3600 },
      seven_day: { utilization: 150, resetsAt: "not a date" },
      seven_day_opus: { utilization: -20, reset_at: "" },
      seven_day_sonnet: { utilization: null, resetInSec: -5 },
      seven_day_cowork: { utilization: "abc", resetInSeconds: "120" },
    },
  ],
  [
    "claude",
    "empty window objects skipped",
    { five_hour: {}, seven_day: null, seven_day_opus: [] },
  ],
  ["claude", "nothing", {}],
  [
    "claude",
    "wrapped json string",
    JSON.stringify({ five_hour: { utilization: 1 } }),
  ],
  ["claude", "invalid json string", "{not json"],
  ["claude", "null", null],
  ["claude", "array", [1, 2]],
  ["claude", "undefined", undefined],
  [
    "codex",
    "plus with banked resets",
    {
      plan_type: "plus",
      rate_limit: {
        primary_window: {
          used_percent: 40,
          limit_window_seconds: 18_000,
          reset_after_seconds: 3600,
        },
        secondary_window: {
          used_percent: 70,
          limit_window_seconds: 604_800,
          resets_at: 1_795_000_000,
        },
      },
      code_review_rate_limit: {
        primary_window: {
          used_percent: 0,
          limit_window_seconds: 604_800,
          reset_after_seconds: 86_400,
        },
      },
      rate_limit_reset_credits: { available_count: 2 },
    },
  ],
  [
    "codex",
    "window durations",
    {
      rate_limit: {
        primary_window: { used_percent: 1, limit_window_seconds: 2_592_000 },
        secondary_window: { used_percent: 2, limit_window_seconds: 7200 },
      },
      code_review_rate_limit: {
        primary_window: { used_percent: 3, limit_window_seconds: 0 },
        secondary_window: { used_percent: 4 },
      },
      additional_rate_limits: [
        {
          limit_name: "GPT-5-Codex-Mini",
          rate_limit: {
            primary_window: {
              used_percent: 5,
              limit_window_seconds: 2_419_200,
            },
          },
        },
        {
          metered_feature: "codex_other",
          rate_limit: {
            primary_window: {
              used_percent: 6,
              limit_window_seconds: 2_764_800,
            },
          },
        },
        {
          rate_limit: {
            secondary_window: {
              used_percent: 7,
              limit_window_seconds: 2_764_801,
            },
          },
        },
        { limit_name: "Empty", rate_limit: {} },
        "junk",
      ],
    },
  ],
  [
    "codex",
    "banked resets invalid",
    { rate_limit_reset_credits: { available_count: -1 }, plan_type: 5 },
  ],
  [
    "codex",
    "banked resets fraction",
    { rate_limit_reset_credits: { available_count: 1.5 } },
  ],
  [
    "codex",
    "banked resets string",
    { rate_limit_reset_credits: { available_count: "3" } },
  ],
  [
    "codex",
    "banked resets zero only",
    { rate_limit_reset_credits: { available_count: 0 } },
  ],
  [
    "kimi",
    "usage and limits",
    {
      usage: { limit: 100, remaining: 25, resetTime: "2026-11-29T00:00:00Z" },
      limits: [
        {
          label: "5-hour",
          detail: { limit: 50, used: 10, reset_time: "2026-11-22T17:00:00Z" },
        },
        { detail: { limit: 0, used: 1 } },
        { limit: 10, remaining: 20, resetTime: 1_795_000_000 },
        { label: "Seconds", detail: { limit: 10, reset_after_seconds: 60 } },
        "junk",
      ],
    },
  ],
  ["kimi", "empty", { usage: {}, limits: [] }],
  [
    "antigravity",
    "buckets",
    {
      buckets: [
        {
          modelId: "gemini-2.5-pro",
          remainingFraction: 0.75,
          resetTime: "2026-11-23T00:00:00Z",
        },
        { model_id: "claude", remaining_fraction: 1.5, reset_time: "bad" },
        { remainingFraction: "0.1" },
        {},
      ],
    },
  ],
  [
    "antigravity",
    "quota summary buckets",
    { quotaSummary: { buckets: [{ modelId: "m", remainingFraction: -0.5 }] } },
  ],
  ["antigravity", "buckets not an array", { buckets: { a: 1 } }],
  [
    "xai",
    "weekly and monthly",
    {
      weekly: {
        config: {
          credit_usage_percent: 30,
          current_period: { type: "weekly", end: "2026-11-29T00:00:00Z" },
          product_usage: [{ product: "grok-code", usage_percent: 12 }, {}],
        },
      },
      monthly: {
        monthly_limit: 100,
        used: 25,
        billing_period_end: "2026-12-01T00:00:00Z",
      },
    },
  ],
  [
    "xai",
    "flat body camelCase",
    {
      creditUsagePercent: "45",
      currentPeriod: { type: "week", end: 1_795_000_000 },
      productUsage: [{ product: "chat", usagePercent: 99 }],
      monthlyLimit: 0,
      used: 5,
      billingPeriodEnd: "2026-12-01",
    },
  ],
  ["xai", "monthly only no usage", { monthly: { monthly_limit: 50 } }],
  ["xai", "period without percent", { current_period: { type: "monthly" } }],
  [
    "xai",
    "weekly period without percent",
    { current_period: { type: "weekly", end: "2026-11-29T00:00:00Z" } },
  ],
  [
    "opencode-go",
    "rolling weekly monthly",
    {
      usage: {
        rolling: { usagePercent: 12, resetInSec: 3600 },
        weekly: { used: 30, limit: 120, resetInSec: 172_800 },
        monthly: { usedPercent: 55 },
      },
    },
  ],
  [
    "opencode-go",
    "percent field variants",
    {
      usage: {
        rolling: { used_percent: 1, utilization: 2 },
        weekly: { utilization: 3, percent: 4 },
        monthly: { percent: 5, used: 1, limit: 0 },
      },
    },
  ],
  [
    "opencode-go",
    "count without limit",
    { usage: { weekly: { used: 3 }, monthly: { used: 3, limit: "6" } } },
  ],
  ["gemini", "unknown provider", { five_hour: { utilization: 1 } }],
  ["custom", "unknown provider with plan", { plan_type: "team" }],
];

/** Accounts a stub core lists at /auth-files. */
const listing: Json[] = [
  {
    name: "claude-org.json",
    provider: "claude",
    type: "claude",
    account_type: "oauth",
    email: "ada@example.com",
    note: "Work Claude",
    priority: 2,
    disabled: false,
    status: "active",
    auth_index: "1",
  },
  {
    name: "claude-personal.json",
    type: "claude",
    email: "ada@example.com",
    disabled: true,
    status: "disabled",
    auth_index: "2",
  },
  {
    name: "claude-upper.json",
    provider: "claude",
    note: "",
    priority: "3",
    status: "active",
    unavailable: true,
    auth_index: 4,
  },
  {
    name: "claude-bad-identity.json",
    provider: "claude",
    status: "error",
    status_message: "unauthorized",
    auth_index: "5",
  },
  {
    name: "claude-missing-file.json",
    provider: "claude",
    status: "error",
    status_message: "token expired",
    auth_index: "6",
  },
  {
    name: "claude-invalid-json.json",
    provider: "claude",
    status: "active",
    priority: "x",
    auth_index: "7",
  },
  {
    name: "codex-pro.json",
    provider: "codex",
    email: "ada@example.com",
    status: "error",
    status_message: "Invalid Grant (retrying)",
    unavailable: true,
    auth_index: "8",
    id_token: { chatgpt_account_id: "11111111-2222-4333-8444-555555555555" },
  },
  {
    name: "codex-no-id.json",
    provider: "codex",
    status: "active",
    auth_index: "9",
    id_token: "not an object",
  },
  { name: "kimi.json", provider: "kimi", status: "active" },
  { name: "xai.json", provider: "xai", status: "active", auth_index: "10" },
  {
    name: "antigravity.json",
    provider: "antigravity",
    status: "active",
    auth_index: "11",
  },
  {
    name: "mystery.json",
    provider: "mystery-provider",
    status: "active",
    auth_index: "12",
  },
  { id: "id-only.json", type: "codex", status: "active", auth_index: "13" },
  { name: "runtime.json", provider: "claude", runtime_only: true },
  {
    name: "claude-api-key-1",
    provider: "claude",
    account_type: "api_key",
    auth_index: "14",
  },
  { provider: "claude", status: "active" },
  "junk",
];
const credentialFiles = {
  "claude-org.json": JSON.stringify({
    type: "claude",
    account_uuid: "aaaaaaaa-0000-4000-8000-000000000001",
    organization_uuid: "bbbbbbbb-0000-4000-8000-000000000001",
    organization_name: "Acme Research",
  }),
  "claude-personal.json": JSON.stringify({
    type: "claude",
    account_uuid: "aaaaaaaa-0000-4000-8000-000000000002",
  }),
  "claude-upper.json": JSON.stringify({
    type: "claude",
    account_uuid: " CCCCCCCC-0000-4000-8000-000000000003 ",
    organization_uuid: "DDDDDDDD-0000-4000-8000-000000000003",
    organization_name: "  Café Ltd  ",
  }),
  "claude-bad-identity.json": JSON.stringify({
    type: "claude",
    account_uuid: "not-a-uuid",
    organization_uuid: 7,
    organization_name: "Bad\u0007Name",
  }),
  "claude-invalid-json.json": "{oops",
  "codex-pro.json": JSON.stringify({ type: "codex" }),
} satisfies Record<string, string>;

const identityCases: Array<[string, Json]> = [
  [
    "full",
    {
      account_uuid: "AAAAAAAA-0000-4000-8000-000000000001",
      organization_uuid: "bbbbbbbb-0000-4000-8000-000000000001",
      organization_name: "Acme",
    },
  ],
  [
    "whitespace trimmed",
    {
      account_uuid: "  aaaaaaaa-0000-4000-8000-000000000001\n",
      organization_name: "  Acme  ",
    },
  ],
  [
    "not uuids",
    {
      account_uuid: "aaaaaaaa00004000800000000000001",
      organization_uuid: "{bbbbbbbb-0000-4000-8000-000000000001}",
    },
  ],
  ["nil uuid", { account_uuid: "00000000-0000-0000-0000-000000000000" }],
  ["v7 uuid", { account_uuid: "01890a5d-ac96-774b-bcce-b302099a8057" }],
  ["name too long", { organization_name: "x".repeat(201) }],
  ["name at limit", { organization_name: "x".repeat(200) }],
  ["name with control", { organization_name: "a\tb" }],
  ["name with format char", { organization_name: "a​b" }],
  ["name empty after trim", { organization_name: "   " }],
  ["not an object", "text"],
  ["null", null],
];
const seatPairs: Array<[string, SeatIdentity, SeatIdentity]> = [
  [
    "same seat",
    { accountUuid: "a", organizationUuid: "o" },
    { accountUuid: "a", organizationUuid: "o", organizationName: "x" },
  ],
  [
    "different org",
    { accountUuid: "a", organizationUuid: "o" },
    { accountUuid: "a", organizationUuid: "p" },
  ],
  ["missing org on both", { accountUuid: "a" }, { accountUuid: "a" }],
  ["empty", {}, {}],
];
const profileResponses: Array<
  [string, { status: number; body: string } | "network", JsonObject]
> = [
  [
    "verified",
    {
      status: 200,
      body: JSON.stringify({
        account: {
          uuid: "AAAAAAAA-0000-4000-8000-000000000009",
          email: "grace@example.com",
        },
        organization: {
          uuid: "bbbbbbbb-0000-4000-8000-000000000009",
          name: "Org",
        },
      }),
    },
    {
      access_token: "synthetic-access",
      email: "old@example.com",
      organization_name: "Imported",
    },
  ],
  [
    "no email keeps the old one",
    {
      status: 200,
      body: JSON.stringify({
        account: { uuid: "aaaaaaaa-0000-4000-8000-000000000009" },
        organization: { uuid: "bbbbbbbb-0000-4000-8000-000000000009" },
      }),
    },
    {
      access_token: "synthetic-access",
      email: "old@example.com",
      organization_name: "Imported",
    },
  ],
  [
    "missing organization",
    {
      status: 200,
      body: JSON.stringify({
        account: { uuid: "aaaaaaaa-0000-4000-8000-000000000009" },
      }),
    },
    { access_token: "synthetic-access" },
  ],
  [
    "http 401",
    { status: 401, body: "{}" },
    { access_token: "synthetic-access" },
  ],
  [
    "unreadable",
    { status: 200, body: "<html>" },
    { access_token: "synthetic-access" },
  ],
  ["network failure", "network", { access_token: "synthetic-access" }],
  [
    "no token",
    { status: 200, body: "{}" },
    { refresh_token: "synthetic-refresh" },
  ],
];

const apiInputs: Array<[string, ApiAccountInput]> = [
  [
    "custom https with models",
    {
      name: "C",
      provider: "custom",
      baseUrl: "https://llm.example.com/v1/",
      apiKey: "k",
      protocol: "openai",
      prefix: "c",
      models: [{ id: "m", protocol: "anthropic" }],
    },
  ],
  [
    "custom http localhost",
    {
      name: "C",
      provider: "custom",
      baseUrl: "http://localhost:11434/v1",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [{ id: "m", protocol: "openai" }],
    },
  ],
  [
    "custom http ipv6 loopback",
    {
      name: "C",
      provider: "custom",
      baseUrl: "http://[::1]:8080",
      apiKey: "k",
      protocol: "responses",
      prefix: "",
      models: [{ id: "m", protocol: "responses" }],
    },
  ],
  [
    "custom http remote",
    {
      name: "C",
      provider: "custom",
      baseUrl: "http://llm.example.com/v1",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [{ id: "m", protocol: "openai" }],
    },
  ],
  [
    "custom http 127.0.0.2",
    {
      name: "C",
      provider: "custom",
      baseUrl: "http://127.0.0.2/v1",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [{ id: "m", protocol: "openai" }],
    },
  ],
  [
    "credentials in url",
    {
      name: "C",
      provider: "custom",
      baseUrl: "https://user:pass@llm.example.com/v1",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [{ id: "m", protocol: "openai" }],
    },
  ],
  [
    "query in url",
    {
      name: "C",
      provider: "custom",
      baseUrl: "https://llm.example.com/v1?x=1",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [{ id: "m", protocol: "openai" }],
    },
  ],
  [
    "hash in url",
    {
      name: "C",
      provider: "custom",
      baseUrl: "https://llm.example.com/v1#x",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [{ id: "m", protocol: "openai" }],
    },
  ],
  [
    "not a url",
    {
      name: "C",
      provider: "custom",
      baseUrl: "llm.example.com",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [{ id: "m", protocol: "openai" }],
    },
  ],
  [
    "ftp url",
    {
      name: "C",
      provider: "custom",
      baseUrl: "ftp://llm.example.com",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [{ id: "m", protocol: "openai" }],
    },
  ],
  [
    "opencode go official",
    {
      name: "Go",
      provider: "opencode-go",
      baseUrl: "https://opencode.ai/zen/go/v1/",
      apiKey: "k",
      protocol: "anthropic",
      prefix: "go",
      models: [
        { id: "glm-4.6", protocol: "anthropic" },
        { id: "gpt-5", protocol: "openai" },
        { id: "qwen3", protocol: "openai" },
        { id: "minimax-m2", protocol: "openai" },
        { id: "grok-4", protocol: "openai" },
        { id: "muse-x", protocol: "openai" },
      ],
    },
  ],
  [
    "opencode go other endpoint",
    {
      name: "Go",
      provider: "opencode-go",
      baseUrl: "https://opencode.ai/zen/v1",
      apiKey: "k",
      protocol: "openai",
      prefix: "go",
      models: [{ id: "glm-4.6", protocol: "openai" }],
    },
  ],
  [
    "openai preset other endpoint allowed",
    {
      name: "O",
      provider: "openai",
      baseUrl: "https://proxy.example.com/v1",
      apiKey: "k",
      protocol: "openai",
      prefix: "oai",
      models: [{ id: "gpt-5", protocol: "openai" }],
    },
  ],
  [
    "discovery openai",
    {
      name: "D",
      provider: "custom",
      baseUrl: "https://discover.example.com/v1/",
      apiKey: "synthetic-discovery-key",
      protocol: "responses",
      prefix: "",
      models: [],
    },
  ],
  [
    "discovery anthropic preset",
    {
      name: "A",
      provider: "anthropic",
      baseUrl: "https://discover.example.com/anthropic",
      apiKey: "synthetic-discovery-key",
      protocol: "openai",
      prefix: "a",
      models: [],
    },
  ],
  [
    "discovery anthropic protocol",
    {
      name: "A",
      provider: "custom",
      baseUrl: "https://discover.example.com/anthropic",
      apiKey: "synthetic-discovery-key",
      protocol: "anthropic",
      prefix: "",
      models: [],
    },
  ],
  [
    "discovery opencode go endpoints",
    {
      name: "Go",
      provider: "opencode-go",
      baseUrl: "https://opencode.ai/zen/go/v1",
      apiKey: "synthetic-discovery-key",
      protocol: "openai",
      prefix: "go",
      models: [],
    },
  ],
  [
    "discovery empty list",
    {
      name: "E",
      provider: "custom",
      baseUrl: "https://empty.example.com/v1",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [],
    },
  ],
  [
    "discovery http 403",
    {
      name: "F",
      provider: "custom",
      baseUrl: "https://forbidden.example.com/v1",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [],
    },
  ],
  [
    "discovery data not an array",
    {
      name: "N",
      provider: "custom",
      baseUrl: "https://notarray.example.com/v1",
      apiKey: "k",
      protocol: "openai",
      prefix: "",
      models: [],
    },
  ],
];
const discovery = new Map<string, { status: number; body: Json }>(
  Object.entries({
    "https://discover.example.com/v1/models": {
      status: 200,
      body: {
        data: [
          { id: "a" },
          { id: 5 },
          { name: "no id" },
          { id: "b", endpoint: "/v1/messages" },
        ],
      },
    },
    "https://discover.example.com/anthropic/models": {
      status: 200,
      body: { data: [{ id: "claude-x" }] },
    },
    "https://opencode.ai/zen/go/v1/models": {
      status: 200,
      body: {
        data: [
          { id: "glm-4.6" },
          { id: "kimi-k2", endpoint: "https://opencode.ai/zen/go/v1/messages" },
          { id: "gpt-5", endpoint: "/v1/chat/completions" },
          { id: "x", endpoint: "/v1/responses" },
          { id: "qwen3-coder" },
        ],
      },
    },
    "https://empty.example.com/v1/models": { status: 200, body: { data: [] } },
    "https://forbidden.example.com/v1/models": { status: 403, body: {} },
    "https://notarray.example.com/v1/models": {
      status: 200,
      body: { data: {} },
    },
  }),
);

/** Replaces global fetch while `run` runs; records each request. */
async function withFetch<T>(
  answer: (url: string, init: RequestInit | undefined) => Response,
  run: () => Promise<T>,
): Promise<{ result: T | undefined; error?: unknown; requests: Json[] }> {
  const original = globalThis.fetch;
  const requests: Json[] = [];
  globalThis.fetch = fetchStub((url, init) => {
    requests.push({
      url,
      method: init?.method ?? "GET",
      headers: Object.fromEntries(new Headers(init?.headers).entries()),
      redirect: init?.redirect ?? null,
    });
    return answer(url, init);
  });
  try {
    return { result: await run(), requests };
  } catch (error) {
    return { result: undefined, error, requests };
  } finally {
    globalThis.fetch = original;
  }
}

export async function recordAccounts(): Promise<Case[]> {
  const cases: Case[] = [];
  for (const [provider, name, payload] of quotaPayloads)
    cases.push(
      await outcome(
        `parseQuota ${provider} ${name}`,
        {
          provider,
          payload: payload ?? null,
          payloadAbsent: payload === undefined,
          now: "2026-11-22T12:00:00.000Z",
        },
        () => parseQuota(provider, payload, now),
      ),
    );

  // Accounts.sync on a listing, with credential files on disk for identity.
  const directory = scratch("accounts");
  const store = new Store(directory, fileKeyCodec(directory));
  const profile = store.createProfile("Accounts");
  const placeholders = new Placeholders()
    .add(directory, "<data>")
    .add(profile.id, "<profile>");
  const auth = credentialDirectory(directory, profile.id);
  mkdirSync(auth, { recursive: true });
  for (const [name, content] of Object.entries(credentialFiles))
    writeFileSync(join(auth, name), content, { mode: 0o644 });
  let managementAnswer: (
    path: string,
    method: string,
    body: Json | undefined,
  ) => Json = () => ({ files: listing });
  const managementCalls: Array<{ path: string; method: string; body: Json }> =
    [];
  // Accounts, Quotas and CodexResets read only usable, runtimes and management of the pool.
  const pool = stub<CorePool>({
    usable: () => true,
    runtimes: new Map(),
    management: async (
      _profileId: string,
      path: string,
      method = "GET",
      body?: Json,
    ) => {
      managementCalls.push({ path, method, body: body ?? null });
      return managementAnswer(path, method, body);
    },
  });
  const accounts = new Accounts(pool, store);
  await accounts.sync(profile.id);
  const ids = listing.flatMap((f) =>
    f instanceof Object && !Array.isArray(f)
      ? [String(f.name ?? f.id ?? "")]
      : [],
  );
  cases.push({
    name: "sync listing",
    input: { listing, credentialFiles },
    output: {
      metadata: store.accountMetadata(profile.id),
      chatgptAccountIds: Object.fromEntries(
        ids.map((id) => [
          id,
          accounts.chatgptAccountId(profile.id, id) ?? null,
        ]),
      ),
    },
  });
  const syncAnswers: Array<[string, Json]> = [
    ["sync files not an array", { files: {} }],
    ["sync no files key", {}],
    ["sync empty listing", { files: [] }],
  ];
  for (const [name, answer] of syncAnswers) {
    managementAnswer = () => answer;
    cases.push(
      await outcome(name, answer, async () => {
        await accounts.sync(profile.id);
        return store.accountMetadata(profile.id).map((a) => a.id);
      }),
    );
  }
  managementAnswer = () => ({ files: listing });
  await accounts.sync(profile.id);
  for (const name of [
    "claude-org.json",
    "a/b.json",
    "a\\b.json",
    ".json",
    "x.txt",
    "../x.json",
    "x.JSON",
  ])
    cases.push(
      await outcome(`credentialPath ${name}`, name, () =>
        accounts.credentialPath(profile.id, name),
      ),
    );

  // Quotas.refresh for each account, with the upstream answers a stub core returns.
  store.saveApiAccount(
    profile.id,
    {
      id: "go-account",
      name: "Go",
      provider: "opencode-go",
      baseUrl: "https://opencode.ai/zen/go/v1",
      prefix: "go",
      models: [{ id: "glm-4.6", protocol: "openai" }],
      disabled: false,
    },
    "synthetic go key",
  );
  store.saveApiAccount(
    profile.id,
    {
      id: "custom-account",
      name: "Custom",
      provider: "custom",
      baseUrl: "https://x.example/v1",
      prefix: "",
      models: [{ id: "m", protocol: "openai" }],
      disabled: true,
    },
    "k",
  );
  cases.push({
    name: "list with api accounts",
    input: "after sync listing",
    output: accounts.list(profile.id),
  });
  const quotas = new Quotas(pool, accounts);
  const upstream: Array<
    [string, string, Array<{ status_code: number; body: Json }>]
  > = [
    [
      "claude-org.json",
      "claude ok",
      [
        {
          status_code: 200,
          body: {
            five_hour: { utilization: 10, resets_at: "2026-11-22T14:00:00Z" },
          },
        },
      ],
    ],
    ["claude-org.json", "claude 401", [{ status_code: 401, body: "" }]],
    [
      "claude-upper.json",
      "claude 500 without a prior quota",
      [{ status_code: 500, body: "" }],
    ],
    [
      "codex-pro.json",
      "codex ok",
      [
        {
          status_code: 200,
          body: JSON.stringify({
            plan_type: "pro",
            rate_limit: {
              primary_window: {
                used_percent: 10,
                limit_window_seconds: 18_000,
                reset_after_seconds: 60,
              },
            },
            rate_limit_reset_credits: { available_count: 1 },
          }),
        },
      ],
    ],
    [
      "codex-no-id.json",
      "codex without chatgpt id",
      [{ status_code: 200, body: {} }],
    ],
    [
      "kimi.json",
      "kimi ok",
      [{ status_code: 200, body: { usage: { limit: 10, remaining: 5 } } }],
    ],
    [
      "antigravity.json",
      "antigravity ok",
      [
        {
          status_code: 200,
          body: { buckets: [{ modelId: "m", remainingFraction: 0.5 }] },
        },
      ],
    ],
    [
      "xai.json",
      "xai two calls",
      [
        {
          status_code: 200,
          body: JSON.stringify({ monthly_limit: 100, used: 10 }),
        },
        { status_code: 200, body: { credit_usage_percent: 20 } },
      ],
    ],
    [
      "xai.json",
      "xai weekly fails",
      [
        { status_code: 200, body: {} },
        { status_code: 429, body: "" },
      ],
    ],
    [
      "xai.json",
      "xai unreadable",
      [
        { status_code: 200, body: "{bad" },
        { status_code: 200, body: {} },
      ],
    ],
    ["mystery.json", "unknown provider", []],
    ["custom-account", "api account without quota", []],
    ["missing.json", "unknown account", []],
  ];
  for (const [accountId, name, answers] of upstream) {
    managementCalls.length = 0;
    const queue = [...answers];
    managementAnswer = () => queue.shift() ?? { status_code: 599, body: "" };
    cases.push(
      await outcome(`refresh ${name}`, { accountId, answers }, async () => ({
        quota: await quotas.refresh(profile.id, accountId),
        upstream: [...managementCalls],
      })),
    );
  }
  for (const [name, response] of [
    [
      "opencode go ok",
      new Response(JSON.stringify({ usage: { rolling: { usagePercent: 5 } } })),
    ],
    ["opencode go 401", new Response("", { status: 401 })],
    ["opencode go unreadable", new Response("<html>")],
  ] as const) {
    const recorded = await withFetch(
      () => response.clone(),
      () => quotas.refresh(profile.id, "go-account"),
    );
    cases.push({
      name: `refresh ${name}`,
      input: { accountId: "go-account", status: response.status },
      output: { quota: recorded.result, requests: recorded.requests },
    });
  }

  // Codex banked resets through the stub core.
  const resets = new CodexResets(
    pool,
    accounts,
    stub<Quotas>({
      refresh: async () => ({
        status: "available",
        windows: [],
        checkedAt: "2026-11-22T12:00:00.000Z",
      }),
    }),
  );
  const resetAnswers: Array<
    [
      string,
      "credits" | "consume",
      { status_code: number; body: Json },
      string?,
    ]
  > = [
    [
      "credits ok",
      "credits",
      {
        status_code: 200,
        body: {
          available_count: 1,
          credits: [
            {
              id: "credit-1",
              reset_type: "rate_limit",
              status: "available",
              expires_at: "2026-12-01T00:00:00+00:00",
              title: null,
            },
          ],
        },
      },
    ],
    [
      "credits wrapped string",
      "credits",
      {
        status_code: 200,
        body: JSON.stringify({ available_count: 0, credits: [] }),
      },
    ],
    [
      "credits bad schema",
      "credits",
      { status_code: 200, body: { available_count: -1, credits: [] } },
    ],
    ["credits http 403", "credits", { status_code: 403, body: "" }],
    ["credits unreadable", "credits", { status_code: 200, body: "{bad" }],
    [
      "consume reset",
      "consume",
      { status_code: 200, body: { code: "reset" } },
      "credit-1",
    ],
    [
      "consume nothing to reset clears the request",
      "consume",
      { status_code: 200, body: { code: "nothing_to_reset" } },
      "credit-2",
    ],
    [
      "consume unknown code",
      "consume",
      { status_code: 200, body: { code: "maybe" } },
      "credit-3",
    ],
  ];
  for (const [name, kind, answer, creditId] of resetAnswers) {
    managementCalls.length = 0;
    managementAnswer = () => answer;
    cases.push(
      await outcome(
        `reset ${name}`,
        { accountId: "codex-pro.json", creditId: creditId ?? null, answer },
        async () => {
          const result =
            kind === "credits"
              ? await resets.credits(profile.id, "codex-pro.json")
              : await resets.consume(
                  profile.id,
                  "codex-pro.json",
                  creditId ?? "",
                );
          const pending = creditId
            ? store.db
                .prepare(
                  "SELECT count(*) AS n FROM settings WHERE id LIKE 'codex-reset:%' AND id LIKE ?",
                )
                .get(`%${creditId}%`)
            : undefined;
          for (const call of managementCalls) {
            const data = text(record(call.body).data);
            if (data) {
              const sent: { redeem_request_id: string } = JSON.parse(data);
              placeholders.add(sent.redeem_request_id, "<uuid>");
            }
          }
          return {
            result,
            upstream: [...managementCalls],
            requestStillPending: pending ? Number(pending.n) > 0 : null,
          };
        },
      ),
    );
  }
  for (const [name, accountId] of [
    ["reset not codex", "claude-org.json"],
    ["reset unknown account", "nope.json"],
    ["reset without chatgpt id", "codex-no-id.json"],
  ])
    cases.push(
      await outcome(name, { accountId }, () =>
        resets.credits(profile.id, accountId),
      ),
    );
  for (const [name, value] of [
    ["credits minimal", { available_count: 0, credits: [] }],
    [
      "credits expires null",
      {
        available_count: 1,
        credits: [{ id: "c", reset_type: "x", status: "y", expires_at: null }],
      },
    ],
    [
      "credits expires no offset",
      {
        available_count: 1,
        credits: [
          {
            id: "c",
            reset_type: "x",
            status: "y",
            expires_at: "2026-12-01T00:00:00",
          },
        ],
      },
    ],
    [
      "credits empty id",
      {
        available_count: 1,
        credits: [{ id: "", reset_type: "x", status: "y", expires_at: null }],
      },
    ],
    ["credits fractional count", { available_count: 1.5, credits: [] }],
  ] as const)
    cases.push(
      await outcome(`schema ${name}`, value, () =>
        resetCreditsSchema.parse(value),
      ),
    );
  for (const code of [
    "reset",
    "already_redeemed",
    "nothing_to_reset",
    "no_credit",
    "other",
  ])
    cases.push(
      await outcome(`schema result ${code}`, { code }, () =>
        resetResultSchema.parse({ code }),
      ),
    );

  // Claude identity.
  for (const [name, value] of identityCases)
    cases.push({
      name: `claudeIdentity ${name}`,
      input: value,
      output: claudeIdentity(value),
    });
  for (const [name, a, b] of seatPairs)
    cases.push({
      name: `sameSeat ${name}`,
      input: [a, b],
      output: sameSeat(a, b),
    });
  for (const [name, response, raw] of profileResponses) {
    const recorded = await withFetch(
      () => {
        if (response === "network") throw new TypeError("fetch failed");
        return new Response(response.body, { status: response.status });
      },
      () => verifyClaudeIdentity(raw),
    );
    cases.push(
      recorded.error === undefined
        ? {
            name: `verifyClaudeIdentity ${name}`,
            input: { raw, response },
            output: {
              credential: recorded.result,
              requests: recorded.requests,
            },
          }
        : await outcome(
            `verifyClaudeIdentity ${name}`,
            { raw, response },
            () => {
              throw recorded.error;
            },
          ),
    );
  }

  // API-key accounts: validation, protocol rules and discovery.
  for (const [id, endpoint] of [
    ["gpt-5", undefined],
    ["grok-4", undefined],
    ["muse-1", undefined],
    ["minimax-m2", undefined],
    ["qwen3-coder", undefined],
    ["glm-4.6", undefined],
    ["kimi-k2", "/v1/messages"],
    ["gpt-5", "/v1/chat/completions"],
    ["glm", "/v1/responses"],
    ["qwen", "/v1/other"],
    ["GPT-5", undefined],
  ] as const)
    cases.push({
      name: `goModelProtocol ${id} ${endpoint ?? "none"}`,
      input: { id, endpoint: endpoint ?? null },
      output: goModelProtocol(id, endpoint),
    });
  for (const [name, input] of apiInputs) {
    const recorded = await withFetch(
      (url) => {
        const answer = discovery.get(url);
        if (!answer) throw new TypeError(`unexpected ${url}`);
        return new Response(JSON.stringify(answer.body), {
          status: answer.status,
        });
      },
      () => apiAccount(input),
    );
    const output = recorded.result && { ...recorded.result, id: "<uuid>" };
    cases.push(
      recorded.error === undefined
        ? {
            name: `apiAccount ${name}`,
            input,
            output: { account: output, requests: recorded.requests },
          }
        : await outcome(`apiAccount ${name}`, input, () => {
            throw recorded.error;
          }),
    );
  }
  store.close();
  return placeholders.apply(cases);
}
