import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ModelCatalog,
  agentModels,
  enrichModels,
} from "../src/server/model-catalog.ts";
import { modelsFor } from "../src/shared/providers.ts";
import { AppError } from "../src/server/errors.ts";
import type { ApiAccount } from "../src/shared/types.ts";

const entry = {
  name: "Synthetic model",
  limit: { context: 64000, output: 8000 },
  modalities: { input: ["text", "image"], output: ["text"] },
  reasoning: true,
  tool_call: true,
  cost: { input: 1.25, output: 5, cache_read: 0.125, cache_write: 1.5 },
};
const catalog = {
  anthropic: {
    models: {
      "fixture-a": entry,
      "fixture-b": {
        ...entry,
        limit: { context: 128000, output: 16000 },
        cost: { ...entry.cost, input: 2.5 },
      },
    },
  },
};
const account: ApiAccount = {
  id: "account",
  name: "Fixture",
  provider: "anthropic",
  baseUrl: "https://api.anthropic.com/v1",
  prefix: "work",
  disabled: false,
  models: [{ id: "fixture-a", protocol: "anthropic" }],
};

test("catalog intersects exact provider routes, preserves every live ID and exports per-model limits and prices", () => {
  const live = [
    { id: "fixture-a", owned_by: "claude" },
    { id: "fixture-b", owned_by: "claude" },
  ];
  const models = enrichModels(live, [], catalog, "2026-09-06T00:00:00Z");
  assert.deepEqual(
    models.map((m) => m.id),
    live.map((m) => m.id),
  );
  assert.equal(
    models[0].metadata?.source,
    "https://models.dev/api.json#anthropic/fixture-a",
  );
  assert.equal(models[0].metadata?.fetchedAt, "2026-09-06T00:00:00Z");
  const pi = agentModels(models, "pi");
  assert.equal(
    pi[0].api,
    "anthropic-messages",
    "Claude models keep their native protocol",
  );
  assert.equal(pi[0].contextWindow, 64000);
  assert.equal(pi[1].contextWindow, 128000);
  assert.equal(pi[1].cost.input, 2.5);
  assert.deepEqual(pi[0].input, ["text", "image"]);
  assert.equal(pi[0].reasoning, true);
  assert.equal(agentModels(models, "opencode")[0].limit?.output, 8000);
  const prefixed = enrichModels(
    [{ id: "work/fixture-a", owned_by: "openai" }],
    [account],
    catalog,
    "today",
  );
  assert.equal(prefixed[0].metadata?.cost?.input, 1.25);
  assert.equal(agentModels(prefixed, "pi")[0].api, "openai-responses");
  for (const [models, accounts] of [
    [[{ id: "fixture-a", owned_by: "custom" }], []],
    [[{ id: "fixture-a-latest", owned_by: "claude" }], []],
    [
      [{ id: "work/fixture-a", owned_by: "openai" }],
      [{ ...account, disabled: true }],
    ],
    [
      [{ id: "work/fixture-a", owned_by: "openai" }],
      [account, { ...account, provider: "custom" }],
    ],
  ] as const) {
    assert.equal(
      enrichModels([...models], [...accounts], catalog, "today")[0].metadata,
      undefined,
    );
  }
  const unknown = enrichModels(
    [...live, { id: "missing", owned_by: "claude" }],
    [],
    catalog,
    "today",
  );
  assert.equal(unknown.length, 3);
  assert.deepEqual(
    agentModels(unknown, "pi").map((model) => model.id),
    ["fixture-a", "fixture-b"],
    "models without limits and prices are left out, the rest stay usable",
  );
  assert.throws(
    () => agentModels([{ id: "missing", owned_by: "claude" }], "pi"),
    /missing/,
  );
  const invalid = enrichModels(
    live,
    [],
    {
      anthropic: {
        models: { "fixture-a": { ...entry, cost: { input: -1, output: 2 } } },
      },
    },
    "today",
  );
  assert.throws(() => agentModels(invalid, "opencode"), /fixture-a, fixture-b/);
});

test("models.dev fetching is credential-free, bounded, cached, coalesced and retries after failure", async () => {
  let calls = 0;
  let fail = true;
  const fetcher: typeof fetch = Object.assign(
    async (url: RequestInfo | URL, init?: RequestInit) => {
      calls++;
      assert.equal(url, "https://models.dev/api.json");
      assert.equal(init?.headers, undefined);
      assert.equal(init?.redirect, "error");
      assert.ok(init?.signal);
      await Promise.resolve();
      return fail
        ? new Response("unavailable", { status: 503 })
        : Response.json(catalog);
    },
    fetch,
  );
  const service = new ModelCatalog(fetcher);
  await assert.rejects(service.models([], []), /Could not refresh/);
  fail = false;
  const [one, two] = await Promise.all([
    service.models([{ id: "fixture-a", owned_by: "claude" }], []),
    service.models([{ id: "fixture-b", owned_by: "claude" }], []),
  ]);
  assert.equal(calls, 2);
  assert.equal(one[0].id, "fixture-a");
  assert.equal(two[0].id, "fixture-b");
  await service.models([], []);
  assert.equal(calls, 2);
});

test("concurrent callers of a failed models.dev fetch all get the 502 error", async () => {
  let calls = 0;
  const fetcher: typeof fetch = Object.assign(async () => {
    calls++;
    await Promise.resolve();
    return new Response("unavailable", { status: 503 });
  }, fetch);
  const service = new ModelCatalog(fetcher);
  const results = await Promise.allSettled([
    service.models([], []),
    service.models([], []),
  ]);
  assert.equal(calls, 1);
  for (const result of results) {
    assert.equal(result.status, "rejected");
    assert.ok(result.reason instanceof AppError);
    assert.equal(result.reason.status, 502);
  }
});

test("pricing tiers retain their published thresholds and missing cache prices are not fabricated", () => {
  const models = enrichModels(
    [{ id: "tiered", owned_by: "claude" }],
    [],
    {
      anthropic: {
        models: {
          tiered: {
            ...entry,
            cost: {
              input: 2,
              output: 8,
              tiers: [
                {
                  input: 4,
                  output: 12,
                  tier: { type: "context", size: 272000 },
                },
              ],
            },
          },
        },
      },
    },
    "today",
  );
  const pi = agentModels(models, "pi")[0];
  const tier = pi.cost.tiers?.[0];
  assert.ok(tier && "inputTokensAbove" in tier);
  assert.equal(tier.inputTokensAbove, 272000);
  assert.equal(pi.cost.tiers?.[0].input, 4);
  assert.equal("cacheRead" in pi.cost, false);
  assert.match(pi.name, /cache pricing unavailable/);
  assert.match(agentModels(models, "opencode")[0].name, /base rate/);
});

test("modelsFor keeps only the agent's native vendor unless other providers are offered", () => {
  const models = [
    { id: "claude-opus", owned_by: "anthropic" },
    { id: "gpt-5", owned_by: "openai" },
    { id: "key-model", owned_by: "codex-0123abcd" },
    { id: "gemini", owned_by: "google" },
    { id: "unknown" },
  ];
  const ids = (list: { id: string }[]) => list.map((model) => model.id);
  assert.deepEqual(ids(modelsFor("claude", models, false)), ["claude-opus"]);
  assert.deepEqual(ids(modelsFor("codex", models, false)), [
    "gpt-5",
    "key-model",
  ]);
  assert.deepEqual(ids(modelsFor("codex", models, true)), ids(models));
  assert.deepEqual(ids(modelsFor("pi", models, false)), ids(models));
});
