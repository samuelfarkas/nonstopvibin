import { test } from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { QuotaMeter } from "../src/client/components.tsx";
import { activityBuckets } from "../src/client/activity-buckets.ts";
import type { UsageRecord } from "../src/shared/types.ts";
import { activityModels } from "../src/client/activity-models.ts";
import { ModelTokenChart } from "../src/client/ModelTokenChart.tsx";
import { mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { UsagePrices } from "../src/server/usage-pricing.ts";
import { ModelCatalog } from "../src/server/model-catalog.ts";

const row = (time: number): UsageRecord => ({
  id: String(time),
  profileId: "fixture",
  timestamp: new Date(time).toISOString(),
  provider: "codex",
  model: "fixture",
  account: "fixture",
  inputTokens: 0,
  cachedTokens: 0,
  cacheWriteTokens: 0,
  outputTokens: 0,
  reasoningTokens: 0,
  totalTokens: 0,
  failed: false,
  statusCode: 200,
  latencyMs: 1,
  stream: false,
});

test("charts include rolling-window partial buckets and local daylight-saving boundaries", () => {
  const old = process.env.TZ;
  process.env.TZ = "Europe/Prague";
  try {
    for (const date of [
      "2026-09-13T12:30:00",
      "2026-03-29T12:30:00",
      "2026-10-25T12:30:00",
    ]) {
      const now = new Date(date);
      for (const days of [1, 7, 30]) {
        const cutoff = now.getTime() - days * 86_400_000;
        const records = [
          row(cutoff),
          row(cutoff + 900_000),
          row(now.getTime()),
        ];
        const buckets = activityBuckets(records, days, now);
        assert.equal(
          buckets.reduce((total, bucket) => total + bucket.total, 0),
          records.length,
        );
        assert.equal(
          new Set(buckets.map((bucket) => bucket.key)).size,
          buckets.length,
        );
        assert.equal(
          activityBuckets([row(cutoff - 1)], days, now).reduce(
            (n, b) => n + b.total,
            0,
          ),
          0,
        );
      }
    }
    const oldRecord = row(new Date("2000-01-01").getTime());
    assert.equal(
      activityBuckets([oldRecord], 3650, new Date("2026-01-01")).reduce(
        (n, b) => n + b.total,
        0,
      ),
      1,
    );
  } finally {
    if (old === undefined) delete process.env.TZ;
    else process.env.TZ = old;
  }
});

test("model aggregation reuses reported totals, includes failures and separates prices", () => {
  assert.deepEqual(activityModels([]), []);
  const records = [
    { ...row(1), id: "a", model: "large", totalTokens: 500, inputTokens: 999 },
    { ...row(2), id: "b", model: "small", totalTokens: 20 },
    { ...row(3), id: "c", model: "small", totalTokens: 0, failed: true },
    { ...row(4), id: "d", model: "large", provider: "claude", totalTokens: 50 },
  ];
  const models = activityModels(records, { a: 1.25, b: 0, c: null });
  assert.deepEqual(
    models.map(([model, group]) => [model, group.tokens]),
    [
      ["large", 550],
      ["small", 20],
    ],
  );
  assert.deepEqual(models[0][1], {
    requests: 2,
    tokens: 550,
    failed: 0,
    latencies: [1, 1],
    usd: 1.25,
    priced: 1,
  });
  assert.equal(models[1][1].failed, 1);
  assert.equal(models[1][1].priced, 1);
  assert.equal(models[1][1].usd, 0);
  assert.equal(activityModels([records[0]])[0][1].tokens, 500);
});

test("model chart follows selected history periods, profile isolation and the 500-row cap", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-model-chart-"));
  const store = new Store(directory, fileKeyCodec(directory));
  const offline: typeof fetch = async () => new Response("", { status: 503 });
  const prices = new UsagePrices(offline);
  const catalog = new ModelCatalog(offline);
  try {
    const profile = store.createProfile("Synthetic", "forest");
    const other = store.createProfile("Other", "blue");
    const now = Date.now();
    const day = 86_400_000;
    store.addUsage(
      [0.5, 2, 10, 4000].map((age, index) => ({
        ...row(now - age * day),
        profileId: profile.id,
        model: `model-${index}`,
        totalTokens: (index + 1) * 100,
      })),
    );
    store.addUsage([
      {
        ...row(now),
        profileId: other.id,
        model: "other-profile",
        totalTokens: 9999,
      },
    ]);
    for (const [days, expected] of [
      [1, 100],
      [7, 300],
      [30, 600],
      [3650, 1000],
    ]) {
      const since =
        days === 3650 ? "" : new Date(now - days * day).toISOString();
      const history = await prices.history(store, profile.id, since, catalog);
      const models = activityModels(history.records, history.pricing.usd);
      assert.equal(
        models.reduce((sum, [, group]) => sum + group.tokens, 0),
        expected,
      );
      assert.equal(history.summary.totalTokens, expected);
      assert.ok(models.every(([model]) => model !== "other-profile"));
    }
    store.addUsage(
      Array.from({ length: 501 }, (_, index) => ({
        ...row(now - index),
        id: `capped-${index}`,
        profileId: profile.id,
        model: "capped",
        totalTokens: 1,
      })),
    );
    const history = await prices.history(store, profile.id, "", catalog);
    assert.equal(history.records.length, 500);
    assert.deepEqual(
      activityModels(history.records).map(([model, group]) => [
        model,
        group.tokens,
      ]),
      [["capped", 500]],
    );
    assert.equal(history.summary.totalTokens, 1501);
  } finally {
    await prices.close();
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("token chart renders exact accessible counts, magnitude order, zero and empty states", () => {
  const render = (records: UsageRecord[]) =>
    renderToStaticMarkup(
      createElement(ModelTokenChart, {
        models: activityModels(records),
        requests: records.length,
      }),
    );
  const empty = render([]);
  assert.match(empty, /No token usage in this period/);
  assert.doesNotMatch(empty, /<table/);
  const single = render([{ ...row(1), totalTokens: 1234 }]);
  assert.match(single, /1,234/);
  assert.match(single, /scope="row"/);
  assert.match(single, /scope="col"/);
  assert.match(single, /aria-label="fixture: 1,234 tokens, 1 requests"/);
  assert.match(single, /tabindex="0"/);
  assert.match(single, /width:100%/);
  assert.match(single, /token counts, not monetary costs/);
  const multiple = render([
    { ...row(1), model: "small", totalTokens: 25 },
    { ...row(2), model: "large", totalTokens: 100 },
    { ...row(3), model: "zero", totalTokens: 0 },
    { ...row(4), model: "<script>unsafe</script>", totalTokens: 10 },
  ]);
  assert.ok(multiple.indexOf(">large</th>") < multiple.indexOf(">small</th>"));
  assert.match(multiple, /width:25%/);
  assert.match(multiple, /width:0%/);
  assert.doesNotMatch(multiple, /NaN|Infinity|<script>/);
  assert.match(multiple, /&lt;script&gt;unsafe/);
  assert.match(render([{ ...row(1), totalTokens: 0 }]), /width:0%/);
});

test("quota tracks expose meter semantics only for known, named values", () => {
  for (const window of [
    undefined,
    { label: "Quota", remainingPercent: null, resetsAt: null },
  ]) {
    const markup = renderToStaticMarkup(
      createElement(QuotaMeter, { window, label: "not checked" }),
    );
    assert.doesNotMatch(markup, /role="meter"/);
    assert.match(markup, /aria-hidden="true"/);
  }
  const markup = renderToStaticMarkup(
    createElement(QuotaMeter, {
      label: "Session",
      window: { label: "Provider quota", remainingPercent: 0, resetsAt: null },
    }),
  );
  assert.match(markup, /role="meter"/);
  assert.match(markup, /aria-label="Session"/);
  assert.match(markup, /aria-valuenow="0"/);
});
