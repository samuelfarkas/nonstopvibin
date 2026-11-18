// The scheduler starts again a profile the app keeps running (it ran before and has a
// subscription) whose core is not running; Refresh starts such a profile before asking quota.
import { test } from "node:test";
import assert from "node:assert/strict";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { Management } from "../src/server/core.ts";
import { Cores } from "../src/server/cores.ts";
import { usageTiming } from "../src/server/usage.ts";
import type { AppState } from "../src/shared/types.ts";
import {
  closeApplication,
  customAccount,
  management,
  temporaryStore,
  testApplication,
} from "./support/application.ts";
import { idleCore, installFakeCore } from "./support/fake-core.ts";

/**
 * Management of running cores that each list one Claude subscription, unless `signedIn`
 * says otherwise, whose quota reads 75% left.
 */
const subscribed =
  (onQuota = () => {}, signedIn = (_profileId: string) => true): Management =>
  async ({ profileId, path }) => {
    if (path === "/auth-files")
      return {
        files: signedIn(profileId)
          ? [{ name: "claude.json", provider: "claude", auth_index: "1" }]
          : [],
      };
    if (path.startsWith("/usage-queue")) return [];
    if (path !== "/api-call") return {};
    onQuota();
    return {
      status_code: 200,
      body: JSON.stringify({
        five_hour: { utilization: 25, resets_at: "2030-01-01T00:00:00Z" },
      }),
    };
  };
/** Management of a core that never answers. */
const unreachable: Management = () =>
  Promise.reject(new Error("The fixture core does not answer."));
/** An API-key subscription; its quota needs no core. */
const apiKey = (id: string) =>
  customAccount(id, "http://127.0.0.1:9/v1", [
    { id: "fixture-model", protocol: "openai" },
  ]);

test(
  "a core that exits unexpectedly runs again after the next pass, with its quota refreshed",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    let quotaChecks = 0;
    const reports: string[] = [];
    let changed = () => {};
    const cores = new Cores(store, binary, {
      report: (message) => reports.push(message),
      onStatusChange: () => changed(),
      usageTiming: { ...usageTiming, coalesce: 20 },
      management: subscribed(() => quotaChecks++),
    });
    const profile = store.createProfile("Restart fixture");
    try {
      await cores.pool.start(profile.id);
      cores.usage.begin();
      assert.equal(quotaChecks, 0, "a start alone checks no quota");
      const crashed = cores.pool.runtimes.get(profile.id)!;
      const running = new Promise<void>((resolve) => {
        changed = () => {
          const runtime = cores.pool.runtimes.get(profile.id);
          if (runtime !== crashed && runtime?.state === "running") resolve();
        };
      });
      crashed.child.kill("SIGKILL");
      await running;
      // The pass that started it goes on to check its quota.
      await cores.usage.settled();
      assert.equal(crashed.state, "error");
      assert.ok(cores.pool.usable(profile.id));
      assert.equal(quotaChecks, 1);
      assert.equal(store.quota(profile.id, "claude.json")?.status, "available");
      assert.deepEqual(reports, [], "a one-off exit adds no error entry");
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

// Attempt n + 1 comes min(coalesce * 2^(n - 1), restartBackoff) after attempt n ended;
// without the bound the last waits would be 640 and 1280 ms.
const backoff = { ...usageTiming, coalesce: 10, restartBackoff: 80 };
const waits = [10, 20, 40, 80, 80, 80, 80, 80];
function assertBackoff(times: number[]) {
  assert.ok(times.length > waits.length, `${times.length} launches`);
  waits.forEach((wait, i) => {
    const gap = times[i + 1]! - times[i]!;
    assert.ok(
      gap >= wait && gap < wait + 1000,
      `launch ${i + 2} came ${gap} ms after the one before, expected ${wait} ms: ${times}`,
    );
  });
}

test(
  "a core that keeps failing to start is retried with backoff to a bound and reported once",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const log = join(directory, "launches");
    // Records its launch time, then exits.
    const binary = await installFakeCore(
      join(directory, "core"),
      `require("node:fs").appendFileSync(${JSON.stringify(log)}, Date.now() + "\\n");\nprocess.exit(1);\n`,
    );
    const reports: string[] = [];
    let failures = 0;
    let enough!: () => void;
    const failed = new Promise<void>((resolve) => {
      enough = resolve;
    });
    const cores: Cores = new Cores(store, binary, {
      report: (message) => reports.push(message),
      // Each attempt's core exits once, leaving the profile in error.
      onStatusChange: () => {
        if (cores.pool.runtimes.get(profile.id)?.state !== "error") return;
        if (++failures === waits.length + 1) enough();
      },
      usageTiming: backoff,
      management: unreachable,
    });
    // It ran before and has a subscription, but its core no longer starts, as after a failed launch.
    const profile = store.createProfile("Failing fixture");
    store.saveProfile({ ...profile, enabled: true });
    store.saveApiAccount(profile.id, apiKey("failing-key"), "failing-key");
    try {
      cores.usage.begin();
      await failed;
      await cores.shutdown();
      assertBackoff(
        (await readFile(log, "utf8")).trim().split("\n").map(Number),
      );
      assert.equal(reports.length, 1, `one error entry: ${reports}`);
      assert.match(reports[0]!, /^Failing fixture: Proxy exited \(1\)\./);
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "a core that exits right after each start is restarted with the same backoff and reported once",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    const reports: string[] = [];
    // When each core became ready; the next start can only begin after this one returned.
    const ready: number[] = [];
    let enough!: () => void;
    const crashed = new Promise<void>((resolve) => {
      enough = resolve;
    });
    let last: unknown;
    const cores: Cores = new Cores(store, binary, {
      report: (message) => reports.push(message),
      // Every core dies as soon as it is ready, before its start returns.
      onStatusChange: () => {
        const runtime = cores.pool.runtimes.get(profile.id);
        if (runtime?.state !== "running" || runtime === last) return;
        last = runtime;
        ready.push(Date.now());
        runtime.child.kill("SIGKILL");
        if (ready.length === waits.length + 2) enough();
      },
      usageTiming: backoff,
      management: subscribed(),
    });
    const profile = store.createProfile("Crash fixture");
    store.saveApiAccount(profile.id, apiKey("crash-key"), "crash-key");
    try {
      await cores.pool.start(profile.id);
      cores.usage.begin();
      await crashed;
      await cores.shutdown();
      // The first was this test's own start; the scheduler's follow it.
      assertBackoff(ready.slice(1));
      assert.equal(reports.length, 1, `one error entry: ${reports}`);
      assert.match(reports[0]!, /^Crash fixture: Proxy exited \(SIGKILL\)\./);
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "only a profile that ran before and has a subscription is started again",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    let changed = () => {};
    const cores = new Cores(store, binary, {
      report: () => {},
      onStatusChange: () => changed(),
      usageTiming: { ...usageTiming, coalesce: 20 },
      management: subscribed(undefined, (profileId) => profileId !== bare.id),
    });
    const kept = store.createProfile("Kept");
    store.saveApiAccount(kept.id, apiKey("kept-key"), "kept-key");
    // Ran before, but nothing in it needs a core.
    const bare = store.createProfile("Bare");
    // Never started: one with no subscription, as a new profile, and one with a subscription.
    const fresh = store.createProfile("Fresh");
    const unstarted = store.createProfile("Unstarted");
    store.saveApiAccount(unstarted.id, apiKey("unstarted-key"), "key");
    try {
      await cores.pool.start(kept.id);
      await cores.pool.start(bare.id);
      cores.usage.begin();
      const before = cores.pool.runtimes.get(kept.id);
      const running = new Promise<void>((resolve) => {
        changed = () => {
          const runtime = cores.pool.runtimes.get(kept.id);
          if (runtime !== before && runtime?.state === "running") resolve();
        };
      });
      for (const profile of [bare, kept]) {
        const { child } = cores.pool.runtimes.get(profile.id)!;
        const exited = once(child, "exit");
        child.kill("SIGKILL");
        await exited;
      }
      await running;
      await cores.usage.settled();
      assert.equal(cores.pool.runtimes.get(bare.id)?.state, "error");
      assert.equal(cores.pool.runtimes.get(fresh.id), undefined);
      assert.equal(cores.pool.runtimes.get(unstarted.id), undefined);
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "a profile held for a credential swap, and every profile once shutdown began, is left stopped",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    const reports: string[] = [];
    let changed = () => {};
    const cores = new Cores(store, binary, {
      report: (message) => reports.push(message),
      onStatusChange: () => changed(),
      usageTiming: { ...usageTiming, coalesce: 10 },
      management: subscribed(),
    });
    const held = store.createProfile("Held");
    const other = store.createProfile("Other");
    /** Kills the profile's core and waits until a pass has started it again. */
    const crash = async (profileId: string) => {
      const before = cores.pool.runtimes.get(profileId)!;
      const running = new Promise<void>((resolve) => {
        changed = () => {
          const runtime = cores.pool.runtimes.get(profileId);
          if (runtime !== before && runtime?.state === "running") resolve();
        };
      });
      before.child.kill("SIGKILL");
      await running;
      await cores.usage.settled();
    };
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    try {
      await cores.pool.start(held.id);
      await cores.pool.start(other.id);
      cores.usage.begin();
      await cores.pool.whileStopped(held.id, async () => {
        // A pass runs while the held profile is stopped and due a restart.
        await crash(other.id);
        assert.equal(cores.pool.alive(held.id), false, "held stays stopped");
      });
      assert.ok(cores.pool.running(held.id), "the swap itself starts it");
      assert.deepEqual(reports, [], "no start was tried on the held profile");

      const change = cores.credentials.queue(() => gate);
      const shutdown = cores.shutdown();
      const { child } = cores.pool.runtimes.get(other.id)!;
      const exited = once(child, "exit");
      child.kill("SIGKILL");
      await exited;
      // Twenty coalesce periods: any restart would have begun by now.
      await new Promise((resolve) => setTimeout(resolve, 200));
      assert.equal(cores.pool.runtimes.get(other.id)?.child, child);
      release();
      await Promise.all([change, shutdown]);
      assert.equal(cores.pool.alive(held.id), false);
      assert.equal(cores.pool.alive(other.id), false);
    } finally {
      release();
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "shutdown abandons an automatic start still waiting for its core to answer",
  { timeout: 25_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    let asked!: () => void;
    const waiting = new Promise<void>((resolve) => {
      asked = resolve;
    });
    const cores = new Cores(store, binary, {
      report: () => {},
      usageTiming: { ...usageTiming, coalesce: 10 },
      // The core never answers: its start would wait out the 15 s readiness window.
      management: (request) => {
        asked();
        return unreachable(request);
      },
    });
    const profile = store.createProfile("Unready fixture");
    store.saveProfile({ ...profile, enabled: true });
    store.saveApiAccount(profile.id, apiKey("unready-key"), "unready-key");
    try {
      cores.usage.begin();
      await waiting;
      const { child } = cores.pool.runtimes.get(profile.id)!;
      const began = Date.now();
      await cores.shutdown();
      const took = Date.now() - began;
      assert.ok(took < 3000, `shutdown took ${took} ms`);
      assert.ok(child.exitCode !== null || child.signalCode !== null);
      assert.equal(cores.pool.starting.size, 0);
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "Refresh starts a profile the app keeps running, answers a failed start, and stores no failed check",
  { timeout: 20_000 },
  async () => {
    const { directory, remove } = await temporaryStore("nv-restart-");
    const failing = join(directory, "failing");
    const binary = await installFakeCore(
      join(directory, "core"),
      `if (require("node:fs").existsSync(${JSON.stringify(failing)})) process.exit(1);\n${idleCore}`,
    );
    const answering = subscribed();
    const app = await testApplication(join(directory, "data"), {
      binary,
      cores: {
        // A failing core exits before it answers.
        management: (request) =>
          existsSync(failing) ? unreachable(request) : answering(request),
        // The scheduler's own restart would come only after this test.
        usageTiming: { ...usageTiming, coalesce: 60_000 },
      },
    });
    const refresh = (profileId: string) =>
      management(app, `/profiles/${profileId}/refresh`, { method: "POST" });
    const crash = async (profileId: string) => {
      const { child } = app.cores.pool.runtimes.get(profileId)!;
      const exited = once(child, "exit");
      child.kill("SIGKILL");
      await exited;
    };
    try {
      const profile = app.store.createProfile("Refresh fixture");
      await app.cores.pool.start(profile.id);
      await crash(profile.id);
      const response = await refresh(profile.id);
      assert.equal(response.status, 200, await response.clone().text());
      const state: AppState = await response.json();
      const shown = state.profiles.find((p) => p.id === profile.id)!;
      assert.equal(shown.runtime, "running");
      assert.equal(shown.accounts[0]?.quota?.status, "available");

      const checked = app.store.quota(profile.id, "claude.json");
      await crash(profile.id);
      await writeFile(failing, "");
      const failed = await refresh(profile.id);
      assert.equal(failed.status, 503);
      assert.match(await failed.text(), /Proxy exited \(1\)\./);
      assert.deepEqual(app.store.quota(profile.id, "claude.json"), checked);

      // Without a subscription the profile is not started, as before.
      const empty = app.store.createProfile("Empty");
      assert.equal((await refresh(empty.id)).status, 200);
      assert.equal(app.cores.pool.runtimes.get(empty.id), undefined);
    } finally {
      await closeApplication(app);
      await remove();
    }
  },
);
