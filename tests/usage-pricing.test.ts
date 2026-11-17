import { mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { usageSummary } from "./usage-summary.ts";
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  estimateUsd,
  normalizedTokens,
  UsagePrices,
} from "../src/server/usage-pricing.ts";
import { pricingTokens } from "../src/server/usage.ts";
import { ModelCatalog } from "../src/server/model-catalog.ts";
import type { UsageRecord } from "../src/shared/types.ts";

const record: UsageRecord = {
  id: "fixture",
  profileId: "work",
  timestamp: "2026-09-13T00:00:00Z",
  provider: "claude",
  model: "fixture-model",
  account: "fixture",
  inputTokens: 100,
  cachedTokens: 200,
  cacheWriteTokens: 50,
  outputTokens: 30,
  reasoningTokens: 10,
  totalTokens: 380,
  latencyMs: 10,
  failed: false,
  statusCode: 200,
  stream: true,
};
const cost = { input: 2, output: 10, cache_read: 0.2, cache_write: 2.5 };

test("pricing normalizes provider accounting without double counting, and rejects unknown data", () => {
  assert.equal(estimateUsd(record, cost), 0.000665);
  assert.equal(
    estimateUsd({ ...record, provider: "codex", inputTokens: 350 }, cost),
    0.000665,
  );
  assert.equal(
    estimateUsd(
      { ...record, provider: "gemini", inputTokens: 350, outputTokens: 20 },
      cost,
    ),
    0.000665,
  );
  assert.equal(estimateUsd({ ...record, provider: "custom" }, cost), null);
  assert.equal(estimateUsd({ ...record, totalTokens: 381 }, cost), null);
  assert.equal(estimateUsd({ ...record, pricingTokens: null }, cost), null);
  assert.equal(estimateUsd(record, undefined), null);
  assert.equal(estimateUsd(record, { input: 2, output: 10 }), null);
  assert.equal(estimateUsd(record, { ...cost, reasoning: 20 }), 0.000765);
  assert.equal(
    estimateUsd(record, { input: 0, output: 0, cache_read: 0, cache_write: 0 }),
    0,
  );
  assert.equal(
    estimateUsd({ ...record, failed: true }, cost),
    0.000665,
    "failed requests can still consume tokens",
  );
  assert.equal(
    estimateUsd(record, {
      ...cost,
      tiers: [{ ...cost, input: 4, tier: { type: "context", size: 350 } }],
    }),
    0.000665,
  );
  assert.equal(
    estimateUsd(record, {
      ...cost,
      tiers: [{ ...cost, input: 4, tier: { type: "context", size: 349 } }],
    }),
    0.000865,
  );
  assert.equal(
    estimateUsd(record, {
      ...cost,
      tiers: [{ input: 4, output: 10, tier: { type: "context", size: 349 } }],
    }),
    null,
  );
});

test("canonical counters are validated before persistence, including non-overlap and completeness", () => {
  const raw = {
    schema_version: 2,
    quality: "complete",
    total_tokens: 380,
    unclassified_tokens: 0,
    input: {
      total_tokens: 350,
      uncached_tokens: 100,
      cache_read_tokens: 200,
      cache_write_tokens: 50,
    },
    output: {
      total_tokens: 30,
      non_reasoning_tokens: 20,
      reasoning_tokens: 10,
    },
  };
  const tokens = pricingTokens(raw);
  assert.deepEqual(tokens, {
    input: 100,
    cached: 200,
    cacheWrite: 50,
    output: 20,
    reasoning: 10,
  });
  assert.equal(
    estimateUsd({ ...record, provider: "custom", pricingTokens: tokens }, cost),
    0.000665,
  );
  for (const invalid of [
    undefined,
    {},
    { ...raw, quality: "unclassified" },
    { ...raw, total_tokens: 381 },
    { ...raw, input: { ...raw.input, uncached_tokens: -1 } },
    { ...raw, output: { ...raw.output, reasoning_tokens: 11 } },
  ]) {
    assert.equal(pricingTokens(invalid), null);
  }
});

test("activity refreshes are nonblocking, coalesced, cached, and credential-free", async () => {
  let modelCalls = 0;
  let fxCalls = 0;
  let release: (() => void) | undefined;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const catalog = new ModelCatalog(
    Object.assign(async (_url: RequestInfo | URL, init?: RequestInit) => {
      modelCalls++;
      assert.equal(init?.headers, undefined);
      await gate;
      return Response.json({
        anthropic: {
          models: {
            "fixture-model": {
              name: "Fixture",
              limit: { context: 64000, output: 8000 },
              modalities: { input: ["text"], output: ["text"] },
              reasoning: true,
              tool_call: true,
              cost,
            },
          },
        },
      });
    }, fetch),
  );
  const prices = new UsagePrices(
    Object.assign(async (url: RequestInfo | URL, init?: RequestInit) => {
      fxCalls++;
      assert.equal(
        url,
        "https://api.frankfurter.dev/v2/providers/ecb/rate/usd/eur",
      );
      assert.equal(init?.headers, undefined);
      assert.equal(init?.redirect, "error");
      assert.ok(init?.signal);
      await gate;
      return Response.json({
        base: "USD",
        quote: "EUR",
        rate: 0.9,
        date: "2026-09-11",
      });
    }, fetch),
  );
  assert.equal(prices.snapshot([record], catalog).usd.fixture, null);
  prices.snapshot([record], catalog);
  assert.equal(modelCalls, 1);
  assert.equal(fxCalls, 1);
  release?.();
  await catalog.models([], []);
  await new Promise((resolve) => setTimeout(resolve, 0));
  const result = prices.snapshot(
    [record, { ...record, id: "unknown", provider: "custom" }],
    catalog,
  );
  assert.equal(result.usd.fixture, 0.000665);
  assert.equal(result.usd.unknown, null);
  assert.deepEqual(result.eur, { rate: 0.9, date: "2026-09-11" });
  assert.ok(result.fetchedAt);
  assert.equal(modelCalls, 1);
  assert.equal(fxCalls, 1);
  assert.deepEqual(
    Object.keys(prices.snapshot([{ ...record, id: "personal" }], catalog).usd),
    ["personal"],
    "prices contain only the requested profile's records",
  );
});

test("outages back off rather than retrying on every dashboard poll", async () => {
  let calls = 0;
  const failed = Object.assign(async () => {
    calls++;
    return new Response("down", { status: 503 });
  }, fetch);
  const catalog = new ModelCatalog(failed);
  const prices = new UsagePrices(failed);
  prices.snapshot([record], catalog);
  await new Promise((resolve) => setTimeout(resolve, 0));
  const result = prices.snapshot([record], catalog);
  assert.equal(calls, 2);
  assert.equal(result.usd.fixture, null);
  assert.equal(result.eur, null);
  assert.equal(result.refreshing, false);
});

test("full-period totals are incremental, isolated, tier-aware and invalidated by retention and new rates", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-full-prices-"));
  const store = new Store(directory, fileKeyCodec(directory));
  let rate = {
    ...cost,
    tiers: [
      { ...cost, input: 4, tier: { type: "context" as const, size: 350 } },
    ],
  };
  const catalog = new ModelCatalog(
    Object.assign(
      async () =>
        Response.json({
          anthropic: {
            models: {
              "fixture-model": {
                name: "Fixture",
                limit: { context: 64000, output: 8000 },
                modalities: { input: ["text"], output: ["text"] },
                reasoning: true,
                tool_call: true,
                cost: rate,
              },
            },
          },
        }),
      fetch,
    ),
  );
  const prices = new UsagePrices(
    Object.assign(async () => new Response(null, { status: 503 }), fetch),
  );
  const originalNow = Date.now;
  try {
    const first = store.createProfile("First");
    const second = store.createProfile("Second");
    const now = Date.now();
    const since = (days: number) =>
      new Date(now - days * 86_400_000).toISOString();
    const records = Array.from({ length: 1201 }, (_, i): UsageRecord => {
      const entry: UsageRecord = {
        ...record,
        id: `history-${i}`,
        profileId: first.id,
        timestamp: since(i < 600 ? 40 : 1),
      };
      if (i === 1200) entry.pricingTokens = null;
      if (i === 1199) {
        entry.inputTokens = 101;
        entry.totalTokens = 381;
      }
      return entry;
    });
    store.addUsage(records);
    store.addUsage([{ ...record, id: "other", profileId: second.id }]);
    await catalog.models([], []);
    let readRows = 0;
    const bySequence = store.usageAfter.bind(store);
    const byTime = store.usageBetween.bind(store);
    store.usageAfter = (...args) => {
      const page = bySequence(...args);
      readRows += page.records.length;
      return page;
    };
    store.usageBetween = (...args) => {
      const page = byTime(...args);
      readRows += page.records.length;
      return page;
    };
    const get = async (id: string, cutoff = "") => {
      const { pricing: result, summary } = await prices.history(
        store,
        id,
        cutoff,
        catalog,
      );
      assert.deepEqual(summary, usageSummary(store, id, cutoff));
      const rows = store.usage(id, cutoff, 10000);
      const expected = rows.map((r) => estimateUsd(r, rate));
      assert.equal(result.totals?.requests, rows.length);
      assert.equal(
        result.totals?.pricedRequests,
        expected.filter((n) => n !== null).length,
      );
      assert.ok(
        Math.abs(
          (result.totals?.usd ?? 0) -
            expected.reduce<number>((sum, n) => sum + (n ?? 0), 0),
        ) < 1e-10,
      );
      assert.equal(
        result.totals?.normalizedRequests,
        rows.filter((r) => normalizedTokens(r)).length,
      );
      for (const field of [
        "input",
        "cached",
        "cacheWrite",
        "output",
        "reasoning",
      ] as const)
        assert.equal(
          result.totals?.tokens[field],
          rows.reduce((sum, r) => sum + (normalizedTokens(r)?.[field] ?? 0), 0),
        );
      return result;
    };
    const initial = await get(first.id);
    assert.equal(initial.totals?.pricedRequests, 1200);
    assert.equal(initial.totals?.tokens.input, 120001);
    assert.equal(initial.totals?.tokens.output, 24000);
    assert.equal(readRows, 1201);
    readRows = 0;
    await get(first.id);
    assert.equal(readRows, 0, "warm refresh reads no historical rows");
    await get(second.id);
    store.addUsage([
      { ...record, id: "late", profileId: first.id, timestamp: since(2) },
      records[0],
    ]);
    readRows = 0;
    await get(first.id);
    assert.equal(readRows, 1, "only committed new records are visited");
    readRows = 0;
    await get(first.id, since(7));
    assert.equal(readRows, 600, "moving cutoff subtracts only expired rows");
    await Promise.all([
      get(first.id, since(30)),
      get(first.id),
      get(second.id),
    ]);
    store.db.exec(
      `CREATE TRIGGER reject_usage BEFORE INSERT ON usage WHEN NEW.id='reject' BEGIN SELECT RAISE(ABORT,'rejected'); END`,
    );
    assert.throws(
      () =>
        store.addUsage([
          { ...records[0], id: "rollback" },
          { ...records[0], id: "reject" },
        ]),
      /rejected/,
    );
    await get(first.id);
    store.setUsageRetentionDays(30);
    await get(first.id);
    // Catalog changes reprice every retained request, not just newly arrived records.
    rate = { ...cost, input: 4, tiers: [] };
    Date.now = () => now + 3_700_000;
    await catalog.models([], []);
    readRows = 0;
    await get(first.id);
    assert.equal(readRows, 602);
    Date.now = originalNow;
    store.pruneUsage(now + 50 * 86_400_000);
    const empty = await get(first.id);
    assert.equal(empty.totals?.usd, 0);
  } finally {
    Date.now = originalNow;
    await prices.close();
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("history calculations restart after concurrent retention and stop before storage closes", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-price-lifecycle-"));
  const store = new Store(directory, fileKeyCodec(directory));
  let networkCalls = 0;
  const unavailable = Object.assign(async () => {
    networkCalls++;
    return new Response(null, { status: 503 });
  }, fetch);
  const catalog = new ModelCatalog(unavailable);
  const prices = new UsagePrices(unavailable);
  try {
    const profile = store.createProfile("Lifecycle");
    const empty = await prices.history(store, profile.id, "", catalog);
    assert.equal(empty.pricing.totals?.requests, 0);
    assert.equal(
      networkCalls,
      0,
      "empty history does not fetch prices or exchange rates",
    );
    const timestamp = new Date(Date.now() - 40 * 86_400_000).toISOString();
    store.addUsage(
      Array.from({ length: 1201 }, (_, i) => ({
        ...record,
        profileId: profile.id,
        id: `old-${i}`,
        timestamp,
      })),
    );
    const calculation = prices.history(store, profile.id, "", catalog);
    store.setUsageRetentionDays(30);
    const {
      pricing: retained,
      records: retainedRecords,
      summary: retainedSummary,
    } = await calculation;
    assert.equal(retainedRecords.length, 0);
    assert.equal(retainedSummary.requests, 0);
    assert.equal(
      retained.totals?.requests,
      0,
      "deletions during a yielded batch invalidate the partial total",
    );
    store.addUsage(
      Array.from({ length: 1201 }, (_, i) => ({
        ...record,
        profileId: profile.id,
        id: `recent-${i}`,
        timestamp: new Date().toISOString(),
      })),
    );
    const interrupted = prices.history(store, profile.id, "", catalog);
    await Promise.all([
      assert.rejects(interrupted, /shutting down/),
      prices.close(),
    ]);
  } finally {
    await prices.close();
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("queued history responses keep records, summary and prices in one insertion snapshot", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-history-snapshot-"));
  const store = new Store(directory, fileKeyCodec(directory));
  const unavailable = Object.assign(
    async () => new Response(null, { status: 503 }),
    fetch,
  );
  const prices = new UsagePrices(unavailable);
  const catalog = new ModelCatalog(unavailable);
  try {
    const profile = store.createProfile("Snapshot");
    store.addUsage(
      Array.from({ length: 1201 }, (_, i) => ({
        ...record,
        id: `snapshot-${i}`,
        profileId: profile.id,
      })),
    );
    const first = prices.history(store, profile.id, "", catalog);
    const queued = prices.history(store, profile.id, "", catalog);
    store.addUsage([
      {
        ...record,
        id: "new",
        profileId: profile.id,
        timestamp: "2026-09-14T00:00:00Z",
      },
    ]);
    const snapshots = await Promise.all([first, queued]);
    for (const [i, result] of snapshots.entries()) {
      assert.equal(result.summary.requests, 1201 + i);
      assert.equal(result.pricing.totals?.requests, result.summary.requests);
      assert.equal(result.summary.totalTokens, (1201 + i) * record.totalTokens);
      assert.equal(
        Object.values(result.pricing.totals!.tokens).reduce(
          (sum, n) => sum + n,
          0,
        ),
        result.summary.totalTokens,
      );
      assert.equal(
        result.records.some((r) => r.id === "new"),
        i === 1,
      );
    }
  } finally {
    await prices.close();
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("a queued history caller runs its own request after the caller ahead of it fails", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-history-failure-"));
  const store = new Store(directory, fileKeyCodec(directory));
  const unavailable = Object.assign(
    async () => new Response(null, { status: 503 }),
    fetch,
  );
  const prices = new UsagePrices(unavailable);
  const catalog = new ModelCatalog(unavailable);
  const usage = store.usage.bind(store);
  let failures = 1;
  store.usage = (...args) => {
    if (failures-- > 0) throw new Error("synthetic storage failure");
    return usage(...args);
  };
  try {
    const profile = store.createProfile("Queued failure");
    store.addUsage([{ ...record, profileId: profile.id }]);
    const first = prices.history(store, profile.id, "", catalog);
    const queued = prices.history(store, profile.id, "", catalog);
    await assert.rejects(first, /synthetic storage failure/);
    assert.equal((await queued).summary.requests, 1);
  } finally {
    await prices.close();
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});
