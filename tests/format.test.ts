import { test } from "node:test";
import assert from "node:assert/strict";
import { groupAccountsByProvider } from "../src/client/format.ts";
import { count, dateLabel } from "../src/client/Activity.tsx";

test("reused formatters preserve number boundaries and local date output", () => {
  for (const [n, expected] of [
    [-1, "-1"],
    [0, "0"],
    [1.25, "1.3"],
    [99_999, "99,999"],
    [100_000, "100K"],
    [1_250_000, "1.3M"],
  ] as const)
    assert.equal(count(n), expected);
  // Prague moves its clocks from 02:00 to 03:00 at 01:00 UTC on 2026-03-29.
  for (const [locale, timeZone, before, after] of [
    ["cs-CZ", "Europe/Prague", "01:30:00", "03:30:00"],
    ["cs-CZ", "America/New_York", "20:30:00", "21:30:00"],
    ["en-US", "Europe/Prague", "01:30:00 AM", "03:30:00 AM"],
  ] as const) {
    const format = new Intl.DateTimeFormat(locale, {
      timeZone,
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
    // ICU may separate the day period with a narrow no-break space.
    const label = (value: string) =>
      dateLabel(value, format).replace(/\s/gu, " ");
    assert.equal(label("2026-03-29T00:30:00Z"), before);
    assert.equal(label("2026-03-29T01:30:00Z"), after);
    assert.equal(label("invalid"), "Invalid Date");
  }
});

test("provider groups gather interleaved subscriptions without changing priority order", () => {
  const account = {
    name: "Account",
    kind: "oauth",
    disabled: false,
    status: "Ready",
    priority: 0,
  } as const;
  const accounts = [
    { ...account, id: "a", provider: "claude" },
    { ...account, id: "b", provider: "codex" },
    { ...account, id: "c", provider: "claude", disabled: true },
    { ...account, id: "d", provider: "custom-provider" },
  ];
  const groups = groupAccountsByProvider(accounts);
  assert.deepEqual([...groups.keys()], ["claude", "codex", "custom-provider"]);
  assert.deepEqual(groups.get("claude"), [accounts[0], accounts[2]]);
  assert.equal([...groups.values()].flat().length, accounts.length);
  assert.deepEqual(
    accounts.map((a) => a.id),
    ["a", "b", "c", "d"],
  );
  assert.equal(groupAccountsByProvider([]).size, 0);
});
