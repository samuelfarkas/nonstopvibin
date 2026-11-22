// Usage-queue pages into UsageRecords (through Usage.collect and the Store), the
// normalised tokens and USD of each record, totals through UsagePrices.history, and
// models.dev enrichment.
import type { Accounts } from "../../../src/server/accounts.ts";
import type { CorePool } from "../../../src/server/core.ts";
import {
  enrichModels,
  ModelCatalog,
} from "../../../src/server/model-catalog.ts";
import type { Quotas } from "../../../src/server/quota.ts";
import { type ApiAccount, Store } from "../../../src/server/store.ts";
import { Usage } from "../../../src/server/usage.ts";
import {
  estimateUsd,
  normalizedTokens,
  UsagePrices,
} from "../../../src/server/usage-pricing.ts";
import { fileKeyCodec } from "../../../src/server/vault.ts";
import { record, text } from "../../../src/server/json.ts";
import type { Json, Model, UsageRecord } from "../../../src/shared/types.ts";
import {
  type Case,
  fetchStub,
  modelsDev,
  modelsDevFetch,
  outcome,
  Placeholders,
  scratch,
  stub,
} from "../lib.ts";

const breakdown = (
  uncached: number,
  cacheRead: number,
  cacheWrite: number,
  plain: number,
  reasoning: number,
) => ({
  schema_version: 2,
  quality: "complete",
  total_tokens: uncached + cacheRead + cacheWrite + plain + reasoning,
  unclassified_tokens: 0,
  input: {
    total_tokens: uncached + cacheRead + cacheWrite,
    uncached_tokens: uncached,
    cache_read_tokens: cacheRead,
    cache_write_tokens: cacheWrite,
  },
  output: {
    total_tokens: plain + reasoning,
    non_reasoning_tokens: plain,
    reasoning_tokens: reasoning,
  },
});

/** Single queue entries; each is collected as a one-entry page. */
const entries: Array<[string, Json]> = [
  [
    "complete claude entry",
    {
      request_id: "req-1",
      timestamp: "2026-11-16T09:15:00.000Z",
      provider: "claude",
      model: "claude-sonnet-4-5-20250929",
      alias: "claude-sonnet-4-5",
      auth_index: "0000000000000001",
      tokens: {
        input_tokens: 1200,
        output_tokens: 300,
        cache_read_tokens: 800,
        cache_creation_tokens: 100,
        total_tokens: 1500,
      },
      token_breakdown: breakdown(300, 800, 100, 300, 0),
      latency_ms: 2150,
      stream: true,
    },
  ],
  [
    "json string wrapped",
    JSON.stringify({
      request_id: "req-wrapped",
      timestamp: "2026-11-16T10:00:00.000Z",
      provider: "codex",
      model: "gpt-5",
      tokens: { input_tokens: 1, total_tokens: 1 },
    }),
  ],
  [
    "no request id hashes the entry",
    {
      timestamp: "2026-11-16T11:00:00.000Z",
      provider: "openai-compatibility",
      model: "qwen",
      alias: "local/qwen",
      tokens: { input_tokens: 7, total_tokens: 7 },
    },
  ],
  [
    "empty request id hashes the entry",
    {
      request_id: "",
      timestamp: "2026-11-16T11:00:01.000Z",
      provider: "claude",
      model: "m",
    },
  ],
  [
    "numeric request id",
    {
      request_id: 42,
      timestamp: "2026-11-16T11:00:02.000Z",
      provider: "claude",
      model: "m",
    },
  ],
  [
    "only alias",
    {
      request_id: "req-alias",
      timestamp: "2026-11-16T12:00:00.000Z",
      provider: "claude",
      alias: "anthropic/claude-sonnet-4-5",
      tokens: { input_tokens: 64, output_tokens: 16, total_tokens: 80 },
    },
  ],
  [
    "empty alias falls back to model",
    {
      request_id: "req-empty-alias",
      timestamp: "2026-11-16T12:00:01.000Z",
      provider: "claude",
      model: "claude-opus-4-1",
      alias: "",
    },
  ],
  [
    "no model and no alias",
    { request_id: "req-unknown", timestamp: "2026-11-16T12:00:02.000Z" },
  ],
  [
    "non-string model",
    {
      request_id: "req-model-number",
      timestamp: "2026-11-16T12:00:03.000Z",
      provider: "claude",
      model: 5,
    },
  ],
  [
    "provider null",
    {
      request_id: "req-provider-null",
      timestamp: "2026-11-16T12:00:04.000Z",
      provider: null,
      model: "m",
    },
  ],
  [
    "numeric strings and negatives",
    {
      request_id: "req-strings",
      timestamp: "2026-11-16T13:00:00.000Z",
      provider: "codex",
      model: "gpt-5",
      tokens: {
        input_tokens: "100",
        output_tokens: "-5",
        cached_tokens: "20",
        reasoning_tokens: "",
        total_tokens: "1e2",
      },
      latency_ms: "-3",
    },
  ],
  [
    "non-finite and junk counters",
    {
      request_id: "req-junk",
      timestamp: "2026-11-16T13:00:01.000Z",
      provider: "codex",
      model: "gpt-5",
      tokens: {
        input_tokens: "abc",
        output_tokens: true,
        cached_tokens: null,
        total_tokens: "Infinity",
      },
      latency_ms: [1],
    },
  ],
  [
    "fractional counters",
    {
      request_id: "req-fraction",
      timestamp: "2026-11-16T13:00:02.000Z",
      provider: "codex",
      model: "gpt-5",
      tokens: { input_tokens: 1.5, output_tokens: 2.25, total_tokens: 3.75 },
      latency_ms: 0.5,
    },
  ],
  [
    "cache_read_tokens zero beats cached_tokens",
    {
      request_id: "req-cache-zero",
      timestamp: "2026-11-16T13:00:03.000Z",
      provider: "claude",
      model: "m",
      tokens: { cache_read_tokens: 0, cached_tokens: 50 },
    },
  ],
  [
    "negative cache_read_tokens",
    {
      request_id: "req-cache-negative",
      timestamp: "2026-11-16T13:00:04.000Z",
      provider: "claude",
      model: "m",
      tokens: { cache_read_tokens: -10, cached_tokens: 50 },
    },
  ],
  [
    "failed with status",
    {
      request_id: "req-429",
      timestamp: "2026-11-16T14:00:00.000Z",
      provider: "codex",
      model: "gpt-5",
      failed: true,
      fail: { status_code: 429 },
    },
  ],
  [
    "failed without status",
    {
      request_id: "req-502",
      timestamp: "2026-11-16T14:00:01.000Z",
      provider: "codex",
      model: "gpt-5",
      failed: true,
    },
  ],
  [
    "failed truthy string",
    {
      request_id: "req-failed-string",
      timestamp: "2026-11-16T14:00:02.000Z",
      provider: "codex",
      model: "gpt-5",
      failed: "true",
    },
  ],
  [
    "status zero",
    {
      request_id: "req-status-zero",
      timestamp: "2026-11-16T14:00:03.000Z",
      provider: "codex",
      model: "gpt-5",
      fail: { status_code: 0 },
    },
  ],
  [
    "status string",
    {
      request_id: "req-status-string",
      timestamp: "2026-11-16T14:00:04.000Z",
      provider: "codex",
      model: "gpt-5",
      fail: { status_code: "503" },
    },
  ],
  [
    "stream truthy non-boolean",
    {
      request_id: "req-stream",
      timestamp: "2026-11-16T14:00:05.000Z",
      provider: "codex",
      model: "gpt-5",
      stream: 1,
    },
  ],
  [
    "breakdown disagreeing total",
    {
      request_id: "req-bd-total",
      timestamp: "2026-11-16T15:00:00.000Z",
      provider: "claude",
      model: "m",
      tokens: { total_tokens: 6200 },
      token_breakdown: {
        ...breakdown(5000, 0, 0, 1200, 0),
        total_tokens: 9999,
      },
    },
  ],
  [
    "breakdown wrong schema version",
    {
      request_id: "req-bd-v1",
      timestamp: "2026-11-16T15:00:01.000Z",
      provider: "claude",
      model: "m",
      token_breakdown: { ...breakdown(1, 0, 0, 1, 0), schema_version: 1 },
    },
  ],
  [
    "breakdown partial quality",
    {
      request_id: "req-bd-partial",
      timestamp: "2026-11-16T15:00:02.000Z",
      provider: "claude",
      model: "m",
      token_breakdown: { ...breakdown(1, 0, 0, 1, 0), quality: "partial" },
    },
  ],
  [
    "breakdown unclassified",
    {
      request_id: "req-bd-unclassified",
      timestamp: "2026-11-16T15:00:03.000Z",
      provider: "claude",
      model: "m",
      token_breakdown: { ...breakdown(1, 0, 0, 1, 0), unclassified_tokens: 1 },
    },
  ],
  [
    "breakdown input parts disagree",
    {
      request_id: "req-bd-input",
      timestamp: "2026-11-16T15:00:04.000Z",
      provider: "claude",
      model: "m",
      token_breakdown: {
        ...breakdown(1, 0, 0, 1, 0),
        input: {
          total_tokens: 5,
          uncached_tokens: 1,
          cache_read_tokens: 1,
          cache_write_tokens: 1,
        },
      },
    },
  ],
  [
    "breakdown negative",
    {
      request_id: "req-bd-negative",
      timestamp: "2026-11-16T15:00:05.000Z",
      provider: "claude",
      model: "m",
      token_breakdown: breakdown(-1, 1, 0, 1, 0),
    },
  ],
  [
    "breakdown fractional",
    {
      request_id: "req-bd-fraction",
      timestamp: "2026-11-16T15:00:06.000Z",
      provider: "claude",
      model: "m",
      token_breakdown: breakdown(0.5, 0.5, 0, 1, 0),
    },
  ],
  [
    "breakdown string counters",
    {
      request_id: "req-bd-string",
      timestamp: "2026-11-16T15:00:07.000Z",
      provider: "claude",
      model: "m",
      token_breakdown: { ...breakdown(1, 0, 0, 1, 0), total_tokens: "2" },
    },
  ],
  [
    "breakdown extra fields",
    {
      request_id: "req-bd-extra",
      timestamp: "2026-11-16T15:00:08.000Z",
      provider: "claude",
      model: "m",
      tokens: { total_tokens: 10 },
      token_breakdown: { ...breakdown(2, 3, 1, 3, 1), extra: true },
    },
  ],
  [
    "breakdown not an object",
    {
      request_id: "req-bd-string-value",
      timestamp: "2026-11-16T15:00:09.000Z",
      provider: "claude",
      model: "m",
      token_breakdown: "complete",
    },
  ],
  [
    "nanosecond timestamp",
    {
      request_id: "req-nanos",
      timestamp: "2026-11-20T10:00:00.123456789Z",
      provider: "claude",
      model: "m",
    },
  ],
  [
    "numeric auth index",
    {
      request_id: "req-auth-number",
      timestamp: "2026-11-16T16:00:00.000Z",
      provider: "claude",
      model: "m",
      auth_index: 7,
    },
  ],
  [
    "tokens not an object",
    {
      request_id: "req-tokens-array",
      timestamp: "2026-11-16T16:00:01.000Z",
      provider: "claude",
      model: "m",
      tokens: [1, 2],
    },
  ],
  [
    "unknown fields dropped",
    {
      request_id: "req-extra",
      timestamp: "2026-11-16T16:00:02.000Z",
      provider: "claude",
      model: "m",
      prompt: "secret prompt",
      api_key: "should not be stored",
      error_body: "upstream said",
    },
  ],
];

/** Whole pages, as the core returns them. */
const pages: Array<[string, Json]> = [
  ["empty page", []],
  ["object instead of array", { items: [] }],
  ["string page", "[]"],
  ["null page", null],
  ["page with scalar items", [1, "x", null, true]],
  [
    "page with duplicate request ids keeps the first",
    [
      {
        request_id: "dup",
        timestamp: "2026-11-17T00:00:00.000Z",
        provider: "claude",
        model: "first",
      },
      {
        request_id: "dup",
        timestamp: "2026-11-17T00:00:01.000Z",
        provider: "claude",
        model: "second",
      },
    ],
  ],
];

/** Records the pricing cases run on; tokens chosen to hit every branch. */
const pricingRecords: Array<[string, Partial<UsageRecord>]> = [
  [
    "claude breakdown with cache",
    {
      provider: "claude",
      model: "claude-sonnet-4-5",
      upstreamModel: "claude-sonnet-4-5",
      totalTokens: 1500,
      pricingTokens: {
        input: 300,
        cached: 800,
        cacheWrite: 100,
        output: 300,
        reasoning: 0,
      },
    },
  ],
  [
    "claude above the tier threshold",
    {
      provider: "claude",
      model: "claude-sonnet-4-5",
      upstreamModel: "claude-sonnet-4-5",
      totalTokens: 250_500,
      pricingTokens: {
        input: 150_000,
        cached: 50_000,
        cacheWrite: 50_000,
        output: 400,
        reasoning: 100,
      },
    },
  ],
  [
    "claude exactly at the tier threshold",
    {
      provider: "claude",
      model: "claude-sonnet-4-5",
      upstreamModel: "claude-sonnet-4-5",
      totalTokens: 200_001,
      pricingTokens: {
        input: 200_000,
        cached: 0,
        cacheWrite: 0,
        output: 1,
        reasoning: 0,
      },
    },
  ],
  [
    "claude legacy output includes reasoning",
    {
      provider: "claude",
      model: "claude-opus-4-1",
      upstreamModel: "claude-opus-4-1",
      inputTokens: 1000,
      outputTokens: 500,
      cachedTokens: 200,
      reasoningTokens: 100,
      cacheWriteTokens: 50,
      totalTokens: 1750,
    },
  ],
  [
    "anthropic legacy does not add up",
    {
      provider: "anthropic",
      model: "claude-opus-4-1",
      inputTokens: 1000,
      outputTokens: 500,
      reasoningTokens: 100,
      totalTokens: 1500,
    },
  ],
  [
    "codex legacy subtracts cache and reasoning",
    {
      provider: "codex",
      model: "gpt-5",
      upstreamModel: "gpt-5",
      inputTokens: 4000,
      outputTokens: 900,
      cachedTokens: 2500,
      reasoningTokens: 400,
      cacheWriteTokens: 0,
      totalTokens: 4900,
    },
  ],
  [
    "openai legacy negative after subtracting",
    {
      provider: "openai",
      model: "gpt-5",
      inputTokens: 10,
      outputTokens: 5,
      cachedTokens: 20,
      totalTokens: 15,
    },
  ],
  [
    "gemini legacy subtracts cache",
    {
      provider: "gemini",
      model: "gemini-2.5-pro",
      upstreamModel: "gemini-2.5-pro",
      inputTokens: 400,
      outputTokens: 100,
      cachedTokens: 100,
      reasoningTokens: 50,
      totalTokens: 550,
    },
  ],
  [
    "antigravity legacy",
    {
      provider: "antigravity",
      model: "gemini-2.5-pro",
      inputTokens: 400,
      outputTokens: 100,
      cachedTokens: 0,
      totalTokens: 500,
    },
  ],
  [
    "kimi legacy free model",
    {
      provider: "kimi",
      model: "kimi-k2",
      upstreamModel: "kimi-k2",
      inputTokens: 100,
      outputTokens: 10,
      totalTokens: 110,
    },
  ],
  [
    "unknown provider legacy",
    {
      provider: "openai-compatibility",
      model: "local/qwen",
      inputTokens: 1,
      totalTokens: 1,
    },
  ],
  [
    "pricingTokens null",
    {
      provider: "claude",
      model: "claude-sonnet-4-5",
      upstreamModel: "claude-sonnet-4-5",
      totalTokens: 6200,
      pricingTokens: null,
    },
  ],
  [
    "pricingTokens sum mismatch",
    {
      provider: "claude",
      model: "claude-sonnet-4-5",
      totalTokens: 10,
      pricingTokens: {
        input: 1,
        cached: 1,
        cacheWrite: 1,
        output: 1,
        reasoning: 1,
      },
    },
  ],
  [
    "cached tokens without a cache price",
    {
      provider: "codex",
      model: "gpt-5-codex",
      upstreamModel: "gpt-5-codex",
      totalTokens: 300,
      pricingTokens: {
        input: 100,
        cached: 100,
        cacheWrite: 0,
        output: 100,
        reasoning: 0,
      },
    },
  ],
  [
    "cache write without a cache write price",
    {
      provider: "codex",
      model: "gpt-5",
      upstreamModel: "gpt-5",
      totalTokens: 300,
      pricingTokens: {
        input: 100,
        cached: 0,
        cacheWrite: 100,
        output: 100,
        reasoning: 0,
      },
    },
  ],
  [
    "reasoning priced separately",
    {
      provider: "codex",
      model: "gpt-5",
      upstreamModel: "gpt-5",
      totalTokens: 1_000_000,
      pricingTokens: {
        input: 250_000,
        cached: 250_000,
        cacheWrite: 0,
        output: 250_000,
        reasoning: 250_000,
      },
    },
  ],
  [
    "model without cost",
    {
      provider: "gemini",
      model: "gemini-free",
      upstreamModel: "gemini-free",
      totalTokens: 2,
      pricingTokens: {
        input: 1,
        cached: 0,
        cacheWrite: 0,
        output: 1,
        reasoning: 0,
      },
    },
  ],
  [
    "model unknown to the catalog",
    {
      provider: "claude",
      model: "claude-unknown",
      upstreamModel: "claude-unknown",
      totalTokens: 2,
      pricingTokens: {
        input: 1,
        cached: 0,
        cacheWrite: 0,
        output: 1,
        reasoning: 0,
      },
    },
  ],
  [
    "alias without upstream model",
    {
      provider: "claude",
      model: "claude-sonnet-4-5",
      totalTokens: 2,
      pricingTokens: {
        input: 1,
        cached: 0,
        cacheWrite: 0,
        output: 1,
        reasoning: 0,
      },
    },
  ],
  [
    "zero tokens",
    {
      provider: "claude",
      model: "claude-sonnet-4-5",
      upstreamModel: "claude-sonnet-4-5",
      totalTokens: 0,
      pricingTokens: {
        input: 0,
        cached: 0,
        cacheWrite: 0,
        output: 0,
        reasoning: 0,
      },
    },
  ],
  [
    "failed request still counted",
    {
      provider: "codex",
      model: "gpt-5",
      upstreamModel: "gpt-5",
      failed: true,
      statusCode: 429,
      totalTokens: 10,
      pricingTokens: {
        input: 10,
        cached: 0,
        cacheWrite: 0,
        output: 0,
        reasoning: 0,
      },
    },
  ],
];
function pricingRecord(
  index: number,
  fields: Partial<UsageRecord>,
): UsageRecord {
  return {
    id: `p:req-${index}`,
    profileId: "p",
    timestamp: `2026-11-${String(10 + (index % 12)).padStart(2, "0")}T12:00:00.000Z`,
    provider: "claude",
    model: "m",
    account: "1",
    inputTokens: 0,
    outputTokens: 0,
    cachedTokens: 0,
    reasoningTokens: 0,
    cacheWriteTokens: 0,
    totalTokens: 0,
    latencyMs: 100,
    failed: false,
    statusCode: 200,
    stream: false,
    ...fields,
  };
}

const enrichModelsCases: Array<
  [string, { models: Model[]; accounts: ApiAccount[] }]
> = [
  [
    "oauth models by owner",
    {
      models: [
        { id: "claude-sonnet-4-5", owned_by: "claude" },
        { id: "gpt-5", owned_by: "codex" },
        { id: "kimi-k2", owned_by: "kimi" },
        { id: "gemini-2.5-pro", owned_by: "gemini" },
      ],
      accounts: [],
    },
  ],
  [
    "owner not in catalog or unknown",
    {
      models: [
        { id: "gpt-5", owned_by: "openai-compatible-abc" },
        { id: "gpt-5" },
        { id: "missing", owned_by: "claude" },
      ],
      accounts: [],
    },
  ],
  [
    "image output and no-limit models left plain",
    {
      models: [
        { id: "gpt-image-1", owned_by: "codex" },
        { id: "claude-haiku-legacy", owned_by: "claude" },
        { id: "bad-price", owned_by: "opencode-go" },
      ],
      accounts: [],
    },
  ],
  [
    "cost absent stays without cost",
    { models: [{ id: "gemini-free", owned_by: "gemini" }], accounts: [] },
  ],
  [
    "api account routes by prefix",
    {
      models: [
        { id: "go/glm-4.6", owned_by: "openai-compatible-123" },
        { id: "oai/gpt-5", owned_by: "openai-compatible-456" },
        { id: "glm-4.6", owned_by: "openai-compatible-123" },
      ],
      accounts: [
        {
          id: "a",
          name: "Go",
          provider: "opencode-go",
          baseUrl: "https://opencode.ai/zen/go/v1",
          prefix: "go",
          models: [{ id: "glm-4.6", protocol: "openai" }],
          disabled: false,
        },
        {
          id: "b",
          name: "OpenAI",
          provider: "openai",
          baseUrl: "https://api.openai.com/v1",
          prefix: "oai",
          models: [{ id: "gpt-5", protocol: "openai" }],
          disabled: false,
        },
      ],
    },
  ],
  [
    "disabled account ignored and owner used",
    {
      models: [{ id: "gpt-5", owned_by: "openai" }],
      accounts: [
        {
          id: "b",
          name: "OpenAI",
          provider: "anthropic",
          baseUrl: "https://api.anthropic.com/v1",
          prefix: "",
          models: [{ id: "gpt-5", protocol: "anthropic" }],
          disabled: true,
        },
      ],
    },
  ],
  [
    "two routes to different providers are ambiguous",
    {
      models: [{ id: "gpt-5", owned_by: "openai" }],
      accounts: [
        {
          id: "a",
          name: "A",
          provider: "openai",
          baseUrl: "https://api.openai.com/v1",
          prefix: "",
          models: [{ id: "gpt-5", protocol: "openai" }],
          disabled: false,
        },
        {
          id: "b",
          name: "B",
          provider: "custom",
          baseUrl: "https://x.example/v1",
          prefix: "",
          models: [{ id: "gpt-5", protocol: "openai" }],
          disabled: false,
        },
      ],
    },
  ],
  [
    "two routes to the same provider agree",
    {
      models: [{ id: "gpt-5" }],
      accounts: [
        {
          id: "a",
          name: "A",
          provider: "openai",
          baseUrl: "https://api.openai.com/v1",
          prefix: "",
          models: [{ id: "gpt-5", protocol: "openai" }],
          disabled: false,
        },
        {
          id: "b",
          name: "B",
          provider: "openai",
          baseUrl: "https://api.openai.com/v1",
          prefix: "",
          models: [{ id: "gpt-5", protocol: "responses" }],
          disabled: false,
        },
      ],
    },
  ],
  [
    "provider catalog id with slash is encoded",
    { models: [{ id: "kimi-k2", owned_by: "kimi" }], accounts: [] },
  ],
];

export async function recordUsage(): Promise<Case[]> {
  const directory = scratch("usage");
  const store = new Store(directory, fileKeyCodec(directory));
  const profile = store.createProfile("Usage");
  const placeholders = new Placeholders().add(profile.id, "<profile>");
  let queue: Json = [];
  // Usage.collect reads only pool.management; the constructor reads accounts.store.
  const pool = stub<CorePool>({ management: async () => queue });
  const usage = new Usage(pool, stub<Accounts>({ store }), stub<Quotas>({}), {
    signingIn: () => false,
    report: () => undefined,
  });
  const collect = async (page: Json) => {
    queue = page;
    const before = store.usageSequence();
    const count = await usage.collect(profile.id);
    const rows = store.db
      .prepare("SELECT body FROM usage WHERE rowid>? ORDER BY rowid")
      .all(before)
      .map((row): Json => JSON.parse(String(row.body)));
    return { collected: count, stored: rows };
  };
  const cases: Case[] = [];
  for (const [name, entry] of entries)
    cases.push(
      await outcome(`queue entry ${name}`, [entry], () => collect([entry])),
    );
  for (const [name, page] of pages)
    cases.push(await outcome(`queue page ${name}`, page, () => collect(page)));
  // A timestamp-less entry gets the wall clock; only its shape is stable.
  const clockless = await collect([
    { request_id: "req-no-time", provider: "claude", model: "m" },
  ]);
  const stored = record(clockless.stored[0]);
  const storedAt = text(stored.timestamp) ?? "";
  cases.push({
    name: "queue entry without timestamp uses now",
    input: [{ request_id: "req-no-time", provider: "claude", model: "m" }],
    output: {
      collected: clockless.collected,
      stored: [
        {
          ...stored,
          timestamp: /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/.test(storedAt)
            ? "<now>"
            : storedAt,
        },
      ],
    },
  });

  // Pricing per record against the fixed models.dev catalog.
  const catalog = new ModelCatalog(modelsDevFetch);
  await catalog.models([], []);
  const fetchedAt = catalog.snapshot([]).fetchedAt ?? "";
  placeholders.add(fetchedAt, "<fetched-at>");
  const records = pricingRecords.map(([, fields], index) =>
    pricingRecord(index, fields),
  );
  const enrich = catalog.snapshot([]).enrich;
  for (const [index, [name]] of pricingRecords.entries()) {
    const usage = records[index];
    const [model] = enrich([
      { id: usage.upstreamModel || usage.model, owned_by: usage.provider },
    ]);
    cases.push({
      name: `price ${name}`,
      input: { record: usage, catalog: "modelsDev" },
      output: {
        normalized: normalizedTokens(usage),
        usd: estimateUsd(usage, model?.metadata?.cost),
      },
    });
  }
  // Totals through UsagePrices.history over a stored record set.
  const pricedProfile = store.createProfile("Priced");
  placeholders.add(pricedProfile.id, "<priced-profile>");
  store.addUsage(
    records.map((r) => ({
      ...r,
      id: `${pricedProfile.id}:${r.id}`,
      profileId: pricedProfile.id,
    })),
  );
  const prices = new UsagePrices(
    fetchStub(() => new Response("{}", { status: 503 })),
  );
  for (const since of [
    "2026-11-01T00:00:00.000Z",
    "2026-11-15T00:00:00.000Z",
    "2026-12-01T00:00:00.000Z",
  ]) {
    const history = await prices.history(
      store,
      pricedProfile.id,
      since,
      catalog,
    );
    cases.push({
      name: `totals since ${since}`,
      input: { records: `price cases, timestamps 2026-11-10..21`, since },
      output: {
        summary: history.summary,
        totals: history.pricing.totals,
        usd: history.pricing.usd,
        count: history.records.length,
        fetchedAt: history.pricing.fetchedAt,
      },
    });
  }
  await prices.close();

  // models.dev enrichment.
  cases.push({ name: "catalog payload", input: null, output: modelsDev });
  for (const [name, input] of enrichModelsCases)
    cases.push({
      name: `enrich ${name}`,
      input: {
        ...input,
        catalog: "modelsDev",
        fetchedAt: "2026-11-22T12:00:00.000Z",
      },
      output: enrichModels(
        input.models,
        input.accounts,
        modelsDev,
        "2026-11-22T12:00:00.000Z",
      ),
    });
  // ModelCatalog rejects a payload whose shape is wrong.
  const broken = new ModelCatalog(
    fetchStub(
      () => new Response(JSON.stringify({ anthropic: { models: "nope" } })),
    ),
  );
  cases.push(
    await outcome(
      "catalog malformed payload",
      { anthropic: { models: "nope" } },
      () => broken.models([{ id: "x" }], []),
    ),
  );
  store.close();
  return placeholders.apply(cases);
}
