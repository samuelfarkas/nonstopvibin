import test from "node:test";
import assert from "node:assert/strict";
import { csvCell, resetIn } from "../src/client/format.ts";
import { isQuotaStale } from "../src/client/quota.ts";
import type { Quota } from "../src/shared/types.ts";

const now = Date.parse("2026-09-05T12:00:00Z");
test("quota freshness preserves failed readings and the five-minute boundary", () => {
  const quota: Quota = {
    status: "available",
    windows: [{ label: "5-hour", remainingPercent: 75, resetsAt: null }],
    checkedAt: "2026-09-05T12:00:00.000Z",
  };
  assert.equal(isQuotaStale(undefined, now), false);
  assert.equal(isQuotaStale(quota, now), false);
  assert.equal(isQuotaStale(quota, now + 300_000), false);
  assert.equal(isQuotaStale(quota, now + 300_001), true);
  assert.equal(isQuotaStale({ ...quota, status: "error" }, now), true);
});

test("an elapsed reset asks for a fresh observation", () => {
  assert.equal(resetIn("2026-09-05T11:59:59Z", now), "reset due · refresh");
});

test("CSV cells neutralize whitespace-prefixed formulas and escape quotes", () => {
  assert.equal(csvCell(" =SUM(A1)"), `" '=SUM(A1)"`);
  assert.equal(csvCell("\t=cmd"), `"\t'=cmd"`);
  assert.equal(csvCell("plain text"), `"plain text"`);
  assert.equal(csvCell(`say "hello"`), `"say ""hello"""`);
});
