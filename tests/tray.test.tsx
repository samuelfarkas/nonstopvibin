import { test } from "node:test";
import assert from "node:assert/strict";
import { renderToStaticMarkup } from "react-dom/server";
import { TrayView } from "../src/client/Tray.tsx";
import type { AppState, Account } from "../src/shared/types.ts";

test("tray disclosures preserve quota details and surface stale, disabled, and unknown accounts", () => {
  const account: Account = {
    id: "a",
    name: "Claude",
    email: "person@example.test",
    provider: "claude",
    kind: "oauth",
    disabled: false,
    status: "Ready",
    priority: 0,
    quota: {
      status: "error",
      checkedAt: new Date().toISOString(),
      windows: [
        { label: "Weekly", remainingPercent: 59, resetsAt: null },
        {
          label: "Fable weekly",
          remainingPercent: 0,
          resetsAt: null,
          scoped: true,
        },
      ],
    },
  };
  const state: AppState = {
    profiles: [
      {
        id: "p",
        slug: "work",
        name: "Work",
        strategy: "round-robin",
        sessionAffinity: false,
        enabled: true,
        createdAt: "",
        runtime: "starting",
        accounts: [
          account,
          { ...account, id: "b", disabled: true, quota: undefined },
        ],
        endpoint: "",
      },
    ],
    coreVersion: "",
    gateway: "",
    usageRetentionDays: 0,
    version: "",
    errors: [],
  };
  const html = renderToStaticMarkup(
    <TrayView state={state} refresh={async () => {}} />,
  );
  assert.equal((html.match(/<details /g) ?? []).length, 3);
  assert.equal((html.match(/<summary /g) ?? []).length, 3);
  assert.doesNotMatch(html, /<details[^>]*\bopen/);
  assert.match(html, /<strong>59%<\/strong>/);
  assert.match(html, /Fable weekly/);
  assert.match(html, /Check failed · last reading/);
  assert.match(html, /Disabled/);
  assert.match(html, /<strong>—<\/strong>/);
  assert.match(html, />starting</);
  assert.match(html, /No current quota/);
  assert.match(html, /0\/1 current readings/);

  const profile = state.profiles[0]!;
  const fresh = {
    ...account,
    quota: { ...account.quota!, status: "available" as const },
  };
  profile.accounts = [
    fresh,
    {
      ...fresh,
      id: "zero",
      quota: {
        ...fresh.quota,
        windows: [{ label: "Weekly", remainingPercent: 0, resetsAt: null }],
      },
    },
    { ...fresh, id: "disabled", disabled: true },
    { ...fresh, id: "unknown", quota: undefined },
    {
      ...fresh,
      id: "stale",
      quota: { ...fresh.quota, checkedAt: "2000-01-01T00:00:00Z" },
    },
  ];
  const summary = () =>
    renderToStaticMarkup(
      <TrayView state={state} refresh={async () => {}} />,
    ).split('<div class="tray-profile-detail">')[0]!;
  assert.match(summary(), /<strong>30%<\/strong>/);
  assert.match(summary(), /left on average/);
  assert.match(summary(), /4 enabled/);
  assert.match(summary(), /2\/4 current readings/);
  assert.doesNotMatch(summary(), /person@example/);
  profile.accounts.push({
    ...fresh,
    id: "codex",
    provider: "codex",
    quota: {
      ...fresh.quota,
      windows: [{ label: "Weekly", remainingPercent: 90, resetsAt: null }],
    },
  });
  const grouped = summary();
  assert.match(grouped, /<strong>Claude<\/strong>/);
  assert.match(grouped, /<strong>Codex<\/strong>/);
  assert.match(grouped, /<strong>30%<\/strong>/);
  assert.match(grouped, /<strong>90%<\/strong>/);
  assert.equal((grouped.match(/class="tray-provider"/g) ?? []).length, 2);
  assert.doesNotMatch(grouped, /<strong>50%<\/strong>/);
  profile.accounts = [profile.accounts[1]!];
  assert.match(summary(), /<strong>0%<\/strong>/);
  profile.accounts = [];
  assert.match(summary(), /No current quota/);
  assert.doesNotMatch(summary(), /NaN/);
});
