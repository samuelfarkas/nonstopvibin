import { test } from "node:test";
import assert from "node:assert/strict";
import { coreConfiguration } from "../src/server/config.ts";
import type { Profile } from "../src/shared/types.ts";
import type { ApiAccount } from "../src/server/store.ts";

const profile: Profile = {
  id: "fixture",
  name: "Fixture",
  slug: "fixture",
  strategy: "round-robin",
  sessionAffinity: false,
  enabled: false,
  createdAt: "2026-09-05",
};
test("credentials never cross endpoints whose URLs have the same suffix", () => {
  const accounts: ApiAccount[] = ["first", "other"].map((id) => ({
    id,
    name: id,
    provider: "custom",
    prefix: "",
    baseUrl: `https://${id}.example/openai/v1`,
    disabled: false,
    models: [{ id: "model-a", protocol: "openai" }],
  }));
  accounts.push({
    ...accounts[0],
    id: "same-endpoint",
    name: "second account",
  });
  accounts.push({
    ...accounts[0],
    id: "other-model",
    models: [{ id: "model-b", protocol: "openai" }],
  });
  const entries = coreConfiguration(
    profile,
    1,
    "/tmp/fixture",
    { core: "fixture", management: "fixture" },
    accounts,
    (id) => `test-${id}`,
  )["openai-compatibility"];
  assert.equal(entries.length, 3);
  assert.equal(new Set(entries.map((e) => e.name)).size, 3);
  for (const account of accounts) {
    const entry = entries.find((e) =>
      e["api-key-entries"].some((k) => k["api-key"] === `test-${account.id}`),
    );
    assert.equal(entry?.["base-url"], account.baseUrl);
  }
});
