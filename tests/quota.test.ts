import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Cores } from "../src/server/cores.ts";
import { parseQuota } from "../src/server/quota.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { resetIn } from "../src/client/format.ts";
import { csvCell } from "../src/client/Activity.tsx";
import { isQuotaStale } from "../src/client/quota.ts";
import { goModelProtocol } from "../src/server/api-accounts.ts";

const now = Date.parse("2026-09-05T12:00:00Z");
test("quota freshness preserves failed readings and the five-minute boundary", () => {
  const quota = parseQuota("claude", { five_hour: { utilization: 25 } }, now);
  assert.equal(isQuotaStale(undefined, now), false);
  assert.equal(isQuotaStale(quota, now), false);
  assert.equal(isQuotaStale(quota, now + 300_000), false);
  assert.equal(isQuotaStale(quota, now + 300_001), true);
  assert.equal(isQuotaStale({ ...quota, status: "error" }, now), true);
  assert.equal(quota.windows[0].remainingPercent, 75);
});

test("Codex labels real durations, keeps missing values unknown, and preserves absolute resets", () => {
  const quota = parseQuota(
    "codex",
    {
      plan_type: "team",
      rate_limit: {
        primary_window: {
          used_percent: 31,
          limit_window_seconds: 18000,
          reset_at: 1788616800,
        },
        secondary_window: { used_percent: null, limit_window_seconds: 2592000 },
      },
    },
    now,
  );
  assert.equal(quota.windows[0].remainingPercent, 69);
  assert.equal(quota.windows[0].label, "5-hour");
  assert.equal(quota.windows[1].label, "Monthly");
  assert.equal(quota.windows[1].remainingPercent, null);
  assert.equal(quota.windows[1].resetsAt, null);
  assert.equal(quota.plan, "team");
});
test("Claude keeps provider windows distinct and never converts a null reading to 100%", () => {
  const q = parseQuota(
    "claude",
    {
      five_hour: { utilization: null },
      seven_day: { utilization: 0, resets_at: "2026-09-07T12:00:00Z" },
      seven_day_opus: { utilization: 87.5 },
    },
    now,
  );
  assert.equal(q.windows[0].remainingPercent, null);
  assert.equal(q.windows[1].remainingPercent, 100);
  assert.equal(q.windows[2].remainingPercent, 12.5);
  // Account-wide windows decide headroom; model windows are marked scoped.
  assert.equal(q.windows[1].scoped, undefined);
  assert.equal(q.windows[2].scoped, true);
});
test("Claude adds Fable scoped weekly usage with its own reset, even without a model ID", () => {
  const q = parseQuota(
    "claude",
    JSON.stringify({
      five_hour: { utilization: 25 },
      seven_day: { utilization: 40, resets_at: "2026-09-07T12:00:00Z" },
      seven_day_opus: { utilization: 20 },
      seven_day_sonnet: { utilization: 10 },
      limits: [
        { kind: "weekly_all", percent: 40 },
        {
          kind: "weekly_scoped",
          percent: 87.5,
          resets_at: "2026-09-08T12:00:00Z",
          scope: { model: { id: null, display_name: "Fable" } },
          is_active: false,
        },
        {
          kind: "weekly_scoped",
          percent: 20,
          scope: { model: { display_name: "Opus" } },
        },
      ],
    }),
    now,
  );
  assert.equal(q.status, "available");
  assert.equal(q.windows.length, 5);
  assert.equal(q.windows[1].remainingPercent, 60);
  assert.deepEqual(q.windows[4], {
    label: "Fable weekly",
    remainingPercent: 12.5,
    resetsAt: "2026-09-08T12:00:00.000Z",
    scoped: true,
  });
  assert.notEqual(q.windows[1].resetsAt, q.windows[4].resetsAt);
});
test("Claude scoped windows preserve unknown readings and ignore malformed scopes", () => {
  for (const [percent, expected] of [
    [null, null],
    ["invalid", null],
    [0, 100],
    [0.5, 99.5],
    [100, 0],
    [120, 0],
    [-1, 100],
  ]) {
    const q = parseQuota("claude", {
      limits: [
        null,
        { kind: "weekly_scoped", scope: null },
        { kind: "weekly_scoped", scope: { model: { display_name: 5 } } },
        { kind: "weekly_scoped", scope: { model: { display_name: " " } } },
        {
          kind: "weekly_scoped",
          scope: { model: { display_name: " Fable " } },
          percent,
          resets_at: "invalid",
        },
      ],
    });
    assert.deepEqual(q.windows, [
      {
        label: "Fable weekly",
        remainingPercent: expected,
        resetsAt: null,
        scoped: true,
      },
    ]);
  }
  for (const limits of [undefined, null, {}, []])
    assert.equal(parseQuota("claude", { limits }).status, "unavailable");
});
test("OpenCode Go API percentages below one are percentages, not fractions", () => {
  const q = parseQuota(
    "opencode-go",
    {
      usage: {
        rolling: { usagePercent: 0.5, resetInSec: 1800 },
        weekly: { usagePercent: 63, resetInSec: 86400 },
        monthly: { usagePercent: 100, resetInSec: 90000 },
      },
    },
    now,
  );
  assert.equal(q.windows[0].remainingPercent, 99.5);
  assert.equal(q.windows[0].resetsAt, "2026-09-05T12:30:00.000Z");
  assert.equal(q.windows[2].remainingPercent, 0);
});
test("No windows is unavailable and an elapsed reset asks for a fresh observation", () => {
  assert.equal(parseQuota("unknown", {}).status, "unavailable");
  assert.equal(resetIn("2026-09-05T11:59:59Z", now), "reset due · refresh");
});
test("CSV cells neutralize whitespace-prefixed formulas and escape quotes", () => {
  assert.equal(csvCell(" =SUM(A1)"), `" '=SUM(A1)"`);
  assert.equal(csvCell("\t=cmd"), `"\t'=cmd"`);
  assert.equal(csvCell("plain text"), `"plain text"`);
  assert.equal(csvCell(`say "hello"`), `"say ""hello"""`);
});
test("Antigravity fractions and Grok weekly/monthly limits preserve independent resets", () => {
  const google = parseQuota(
    "antigravity",
    {
      buckets: [
        {
          model_id: "model",
          remaining_fraction: 0.25,
          reset_time: "2026-09-06T12:00:00Z",
        },
      ],
    },
    now,
  );
  assert.equal(google.windows[0].remainingPercent, 25);
  const grok = parseQuota(
    "xai",
    {
      weekly: {
        config: {
          credit_usage_percent: 20,
          current_period: { type: "weekly", end: "2026-09-06T12:00:00Z" },
        },
      },
      monthly: {
        monthly_limit: 100,
        used: 60,
        billing_period_end: "2026-10-01T12:00:00Z",
      },
    },
    now,
  );
  assert.deepEqual(
    grok.windows.map((w) => [w.label, w.remainingPercent]),
    [
      ["Weekly", 80],
      ["Monthly", 40],
    ],
  );
  assert.notEqual(grok.windows[0].resetsAt, grok.windows[1].resetsAt);
  assert.equal(
    parseQuota("xai", { monthly_limit: 0, used: 0 }, now).windows[0]
      .remainingPercent,
    null,
  );
});
test("OpenCode Go quota errors do not persist invalid response excerpts", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-quota-"));
  const store = new Store(directory, fileKeyCodec(directory));
  const { quotas } = new Cores(store, "unused", { report: () => {} });
  const profile = store.createProfile("Fixture");
  const accountId = "opencode-go-fixture";
  store.saveApiAccount(
    profile.id,
    {
      id: accountId,
      name: "OpenCode Go",
      provider: "opencode-go",
      baseUrl: "https://opencode.ai/zen/go/v1",
      prefix: "",
      models: [],
      disabled: false,
    },
    "synthetic-key",
  );
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () =>
    new Response('{"secret":"SENTINEL_9f3a"', { status: 200 });
  try {
    const quota = await quotas.refresh(profile.id, accountId);
    assert.equal(quota.status, "error");
    assert.equal(quota.error, "Quota response was not valid JSON.");
    assert.equal(quota.error.includes("SENTINEL_9f3a"), false);
    assert.equal(
      JSON.stringify(store.quota(profile.id, accountId)).includes(
        "SENTINEL_9f3a",
      ),
      false,
    );
  } finally {
    globalThis.fetch = originalFetch;
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("OpenCode Go selects the wire protocol for published model families and live metadata", () => {
  assert.equal(goModelProtocol("gpt-5.6-luna"), "responses");
  assert.equal(goModelProtocol("minimax-m3"), "anthropic");
  assert.equal(goModelProtocol("glm-5.3"), "openai");
  assert.equal(goModelProtocol("new-model", "/v1/messages"), "anthropic");
});

test("Codex banked reset counts distinguish zero, unavailable, and invalid readings", () => {
  for (const count of [0, 1, 3]) {
    const payload = { rate_limit_reset_credits: { available_count: count } };
    assert.equal(parseQuota("codex", payload, now).bankedResets, count);
    assert.equal(parseQuota("claude", payload, now).bankedResets, undefined);
  }
  for (const count of [null, -1, 1.5, "invalid", Number.MAX_SAFE_INTEGER + 1]) {
    assert.equal(
      parseQuota(
        "codex",
        { rate_limit_reset_credits: { available_count: count } },
        now,
      ).bankedResets,
      undefined,
    );
  }
  assert.equal(parseQuota("codex", {}, now).bankedResets, undefined);
});
