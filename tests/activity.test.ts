import { test } from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { QuotaMeter } from "../src/client/components.tsx";
import { activityBuckets } from "../src/client/activity-buckets.ts";
import type { UsageRecord } from "../src/shared/types.ts";

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
    // Prague changes its clocks on 2026-03-29 and 2026-10-25: windows ending on and after those days.
    for (const date of [
      "2026-09-13T12:30:00",
      "2026-03-29T12:30:00",
      "2026-03-30T12:30:00",
      "2026-10-25T12:30:00",
      "2026-10-26T12:30:00",
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
