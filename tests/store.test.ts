import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  Store,
  clientKeyPattern,
  secretIds,
  type UsageCursor,
} from "../src/server/store.ts";
import type { SecretCodec } from "../src/server/vault.ts";
import type { UsageRecord } from "../src/shared/types.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { usageSummary } from "./usage-summary.ts";
import { Application } from "../src/server/server.ts";

test("existing databases never receive a replacement vault key", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-missing-key-"));
  const store = new Store(directory, fileKeyCodec(directory));
  try {
    const profile = store.createProfile("Fixture");
    const secret = store.clientKey(profile.id);
    const path = join(directory, "vault.key");
    const original = await readFile(path);
    await rm(path);
    assert.throws(() => fileKeyCodec(directory), /vault\.key is missing/);
    await assert.rejects(stat(path), { code: "ENOENT" });
    await writeFile(path, original, { mode: 0o600 });
    const encrypted = store.db
      .prepare("SELECT encrypted FROM secrets WHERE id=?")
      .get(secretIds.client(profile.id));
    assert.equal(
      fileKeyCodec(directory).decrypt(String(encrypted?.encrypted)),
      secret,
    );
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("profile writes refresh the cached profile names the tray reads", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-generation-"));
  const store = new Store(directory, fileKeyCodec(directory));
  try {
    const empty = store.profileNames();
    const profile = store.createProfile("Fixture");
    const names = store.profileNames();
    assert.notEqual(names, empty);
    assert.deepEqual([...names], [[profile.id, "Fixture"]]);
    store.profiles();
    store.clientKey(profile.id);
    assert.equal(store.profileNames(), names, "reads keep the cache");
    store.saveProfile({ ...profile, name: "Renamed" });
    assert.deepEqual([...store.profileNames()], [[profile.id, "Renamed"]]);
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("desktop secrets persist using an owner-only local key", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-vault-"));
  const options = {
    directory,
    binary: join(directory, "unused-core"),
    allowMissingCore: true,
    clientDirectory: directory,
    agentHome: join(directory, "agent-home"),
    port: 0,
    desktop: true,
  };
  let app = await Application.create(options);
  try {
    const profile = app.store.createProfile("Fixture");
    const id = secretIds.client(profile.id);
    const key = app.store.clientKey(profile.id);
    const row = app.store.db
      .prepare("SELECT encrypted FROM secrets WHERE id=?")
      .get(id);
    assert.ok(row);
    assert.notEqual(row.encrypted, key);
    assert.equal(fileKeyCodec(directory).decrypt(String(row.encrypted)), key);
    assert.equal((await stat(directory)).mode & 0o777, 0o700);
    for (const file of ["vault.key", "nonstopvibin.sqlite"])
      assert.equal((await stat(join(directory, file))).mode & 0o777, 0o600);
    await app.close();
    app = await Application.create(options);
    assert.equal(app.store.clientKey(profile.id), key);
    await app.close();
    await rm(directory, { recursive: true });
    app = await Application.create(options);
    assert.ok(app.store.profiles().every((p) => p.id !== profile.id));
    assert.equal(
      app.store.db.prepare("SELECT id FROM secrets WHERE id=?").get(id),
      undefined,
    );
  } finally {
    await app.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("moving summaries match stored history through late inserts, changing periods, rollback and retention", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-periods-"));
  let store = new Store(directory, fileKeyCodec(directory));
  try {
    const profiles = [
      store.createProfile("First"),
      store.createProfile("Second"),
    ];
    const now = Date.now();
    const day = 86_400_000;
    const cutoff = (age: number) => new Date(now - age * day).toISOString();
    const check = (id: string, since: string) => {
      const rows = store.usage(id, since, 10000);
      const sum = (
        field:
          | "inputTokens"
          | "outputTokens"
          | "cachedTokens"
          | "reasoningTokens"
          | "totalTokens",
      ) => rows.reduce((n, row) => n + row[field], 0);
      assert.deepEqual(usageSummary(store, id, since), {
        requests: rows.length,
        failed: rows.filter((row) => row.failed).length,
        inputTokens: sum("inputTokens"),
        outputTokens: sum("outputTokens"),
        cachedTokens: sum("cachedTokens"),
        reasoningTokens: sum("reasoningTokens"),
        totalTokens: sum("totalTokens"),
      });
    };
    store.db.exec(`CREATE TRIGGER reject_fixture BEFORE INSERT ON usage
      WHEN NEW.id='reject' BEGIN SELECT RAISE(ABORT,'synthetic rejection'); END`);
    for (let i = 0; i < 80; i++) {
      const row: UsageRecord = {
        id: `row-${i}`,
        profileId: profiles[i % 2].id,
        // Include late arrivals and records exactly on either side of the cutoff.
        timestamp: new Date(now - (i % 40) * day + (i % 3) - 1).toISOString(),
        model: "fixture",
        provider: "custom",
        account: "fixture",
        inputTokens: i,
        outputTokens: 3,
        cachedTokens: 2,
        reasoningTokens: 1,
        cacheWriteTokens: 0,
        totalTokens: i + 3,
        latencyMs: 1,
        failed: i % 3 === 0,
        statusCode: i % 3 === 0 ? 429 : 200,
        stream: false,
      };
      store.addUsage([row, row]);
      if (i % 10 === 0)
        assert.throws(
          () =>
            store.addUsage([
              { ...row, id: `rollback-${i}` },
              { ...row, id: "reject" },
            ]),
          /synthetic rejection/,
        );
      if (i === 40) store.setUsageRetentionDays(30);
      for (const profile of profiles) {
        check(profile.id, "");
        for (const days of [7, 7 - i / day, 30, 1, 1, 40, 7])
          check(profile.id, cutoff(days));
      }
    }
    store.pruneUsage(now + 35 * day);
    for (const profile of profiles) {
      check(profile.id, "");
      check(profile.id, cutoff(40));
    }
    store.close();
    store = new Store(directory, fileKeyCodec(directory));
    for (const profile of profiles) {
      check(profile.id, "");
      check(profile.id, cutoff(40));
    }
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("history cleanup is opt-in, persists, and is reflected in each affected summary", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-retention-"));
  let store = new Store(directory, fileKeyCodec(directory));
  try {
    const profiles = [
      store.createProfile("First"),
      store.createProfile("Second"),
    ];
    const now = Date.now();
    const records: UsageRecord[] = profiles.flatMap((profile) =>
      [5, 40].map((age) => ({
        id: `${profile.id}:${age}`,
        profileId: profile.id,
        timestamp: new Date(now - age * 86_400_000).toISOString(),
        model: "fixture",
        provider: "custom",
        account: "synthetic",
        inputTokens: 1,
        outputTokens: 2,
        totalTokens: 3,
        cachedTokens: 0,
        reasoningTokens: 0,
        cacheWriteTokens: 0,
        latencyMs: 1,
        failed: false,
        statusCode: 200,
        stream: false,
      })),
    );
    for (const p of profiles)
      assert.equal(usageSummary(store, p.id).requests, 0);
    // Each profile holds one 5-day-old and one 40-day-old record.
    const both = {
      requests: 2,
      failed: 0,
      inputTokens: 2,
      outputTokens: 4,
      cachedTokens: 0,
      reasoningTokens: 0,
      totalTokens: 6,
    };
    store.addUsage(records);
    for (const p of profiles) assert.deepEqual(usageSummary(store, p.id), both);
    store.addUsage(records);
    for (const p of profiles)
      assert.equal(usageSummary(store, p.id).requests, 2);
    store.db.exec(`CREATE TRIGGER reject_fixture BEFORE INSERT ON usage
      WHEN NEW.id='reject' BEGIN SELECT RAISE(ABORT,'synthetic rejection'); END`);
    assert.throws(
      () =>
        store.addUsage([
          { ...records[0], id: "rolled-back" },
          { ...records[0], id: "reject" },
        ]),
      /synthetic rejection/,
    );
    assert.equal(store.usage(profiles[0].id, "", 300).length, 2);
    for (const p of profiles) assert.deepEqual(usageSummary(store, p.id), both);
    store.pruneUsage(now);
    for (const p of profiles)
      assert.equal(usageSummary(store, p.id).requests, 2);
    assert.throws(() => store.setUsageRetentionDays(-1), /Unsupported/);
    assert.equal(store.usageRetentionDays(), 0);
    store.setUsageRetentionDays(30);
    for (const p of profiles) {
      assert.equal(store.usage(p.id, "", 300).length, 1);
      assert.equal(usageSummary(store, p.id).totalTokens, 3);
    }
    store.close();
    store = new Store(directory, fileKeyCodec(directory));
    assert.equal(store.usageRetentionDays(), 30);
    store.pruneUsage(now + 31 * 86_400_000);
    for (const p of profiles)
      assert.equal(usageSummary(store, p.id).requests, 0);
    const changes = () =>
      store.db.prepare("SELECT total_changes() AS count").get()?.count;
    store.saveAccountMetadata(profiles[0].id, []);
    const before = changes();
    store.saveAccountMetadata(profiles[0].id, []);
    assert.equal(
      changes(),
      before,
      "unchanged account snapshots do not write to SQLite",
    );
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("saveApiAccount rolls back the account when encryption fails", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-store-"));
  let rejectEncryption = false;
  const codec: SecretCodec = {
    encrypt(value) {
      if (rejectEncryption) throw new Error("encryption failed");
      return value;
    },
    decrypt: (value) => value,
  };
  const store = new Store(directory, codec);
  try {
    const profile = store.createProfile("Fixture");
    rejectEncryption = true;
    assert.throws(
      () =>
        store.saveApiAccount(
          profile.id,
          {
            id: "fixture-account",
            name: "Fixture",
            provider: "openai",
            baseUrl: "https://example.invalid/v1",
            prefix: "",
            models: [],
            disabled: false,
          },
          "synthetic-key",
        ),
      /encryption failed/,
    );
    assert.deepEqual(store.apiAccounts(profile.id), []);
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("profile slug boundaries remain valid after truncation and duplicate suffixes", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-slugs-"));
  const store = new Store(directory, fileKeyCodec(directory));
  try {
    const name = "a".repeat(39) + " b";
    assert.equal(store.createProfile(name).slug, "a".repeat(39));
    assert.equal(store.createProfile(name).slug, "a".repeat(39) + "-2");
    assert.equal(store.createProfile(" ".repeat(41)).slug, "profile");
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("time pagination seeks through tied timestamps without losing rows or crossing snapshot bounds", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-time-pages-"));
  const store = new Store(directory, fileKeyCodec(directory));
  try {
    const profile = store.createProfile("Pagination");
    const other = store.createProfile("Other");
    // Minimal synthetic rows exercise storage ordering independently of provider accounting.
    const insert = store.db.prepare("INSERT INTO usage VALUES (?,?,?,?)");
    for (let i = 0; i < 2003; i++) {
      const timestamp = i < 1501 ? "2026-01-02" : "2026-01-03";
      insert.run(
        String(i),
        profile.id,
        timestamp,
        JSON.stringify({ id: String(i), timestamp }),
      );
    }
    insert.run(
      "before",
      profile.id,
      "2026-01-01",
      JSON.stringify({ id: "before", timestamp: "2026-01-01" }),
    );
    insert.run(
      "other",
      other.id,
      "2026-01-02",
      JSON.stringify({ id: "other", timestamp: "2026-01-02" }),
    );
    const through = store.usageSequence();
    insert.run(
      "later",
      profile.id,
      "2026-01-02",
      JSON.stringify({ id: "later", timestamp: "2026-01-02" }),
    );
    let from: UsageCursor | undefined;
    const ids: string[] = [];
    for (;;) {
      const page = store.usageBetween(
        profile.id,
        { since: "2026-01-02", before: "2026-01-04", through },
        from,
      );
      ids.push(...page.records.map((record) => record.id));
      if (!page.next) break;
      assert.equal(page.records.length, 500);
      from = page.next;
    }
    assert.deepEqual(
      ids,
      Array.from({ length: 2003 }, (_, i) => String(i)),
    );
    assert.equal(
      store.usageBetween(profile.id, {
        since: "2026-01-02",
        before: "2026-01-02",
        through,
      }).records.length,
      0,
    );
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("secret and setting rows keep the IDs stored databases already use", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-row-ids-"));
  const store = new Store(directory, fileKeyCodec(directory));
  try {
    const profile = store.createProfile("Fixture");
    store.saveApiAccount(
      profile.id,
      {
        id: "account",
        name: "Key",
        provider: "custom",
        baseUrl: "https://api.example.test/v1",
        prefix: "",
        models: [{ id: "model", protocol: "openai" }],
        disabled: false,
      },
      "secret",
    );
    store.saveAccountMetadata(profile.id, []);
    store.resetRequestId("identity", "credit");
    store.setUsageRetentionDays(30);
    assert.deepEqual(
      store.db
        .prepare("SELECT id FROM secrets ORDER BY id")
        .all()
        .map((row) => String(row.id)),
      [
        `${profile.id}:client`,
        `${profile.id}:core`,
        `${profile.id}:management`,
        "account:api",
      ].sort(),
    );
    assert.deepEqual(
      store.db
        .prepare("SELECT id FROM settings ORDER BY id")
        .all()
        .map((row) => String(row.id)),
      [
        `accounts:${profile.id}`,
        'codex-reset:["identity","credit"]',
        "usage-retention-days",
      ],
    );
    assert.equal(store.apiKey("account"), "secret");
    assert.match(store.clientKey(profile.id), clientKeyPattern);
  } finally {
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});
