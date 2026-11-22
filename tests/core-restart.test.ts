// The scheduler starts again a profile the app keeps running (it has a subscription) whose
// core is not running; Refresh starts such a profile before asking quota.
import { test } from "node:test";
import assert from "node:assert/strict";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { coreTiming, type Management } from "../src/server/core.ts";
import { Cores } from "../src/server/cores.ts";
import { AppError } from "../src/server/errors.ts";
import { providers } from "../src/shared/providers.ts";
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
/** An OpenCode Go subscription: its quota is asked from the provider with the key. */
const goAccount = () => ({
  id: "go-key",
  name: "OpenCode Go",
  provider: "opencode-go" as const,
  baseUrl: providers["opencode-go"].preset.baseUrl,
  prefix: "",
  models: [],
  disabled: false,
});
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

// Attempt n + 1 comes min(coalesce * 2^(n - 1), restartBackoff) after attempt n ended, and
// no sooner: a loaded machine may start one late, so only that is checked here. That the
// wait is capped is checked on the policy itself (restart-policy.test.ts).
// The limit stays above the nine starts these tests watch.
const backoff = {
  ...usageTiming,
  coalesce: 10,
  restartBackoff: 80,
  restartLimit: 20,
};
const waits = [10, 20, 40, 80, 80, 80, 80, 80];
function assertBackoff(times: number[]) {
  assert.ok(times.length > waits.length, `${times.length} launches`);
  waits.forEach((wait, i) => {
    const gap = times[i + 1]! - times[i]!;
    assert.ok(
      gap >= wait,
      `launch ${i + 2} came ${gap} ms after the one before, expected at least ${wait} ms: ${times}`,
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
    // A first subscription whose core never started: the profile never ran.
    const profile = store.createProfile("Failing fixture");
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
  "only a profile with a subscription is started again, whether or not it ran before",
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
    // Never started: one with no subscription, as a new profile, and one whose first start failed.
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
          if (
            runtime !== before &&
            runtime?.state === "running" &&
            cores.pool.running(unstarted.id)
          )
            resolve();
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

test(
  "Refresh still checks API-key quota when the profile's core cannot start or is held",
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
        management: (request) =>
          existsSync(failing) ? unreachable(request) : answering(request),
        usageTiming: { ...usageTiming, coalesce: 60_000 },
      },
    });
    // OpenCode Go quota is asked from the provider with the key; no core is involved.
    const usage = `${providers["opencode-go"].preset.baseUrl}/usage`;
    let goChecks = 0;
    const originalFetch = globalThis.fetch;
    globalThis.fetch = async (input, init) => {
      if (String(input) !== usage) return originalFetch(input, init);
      goChecks++;
      return new Response(
        JSON.stringify({ usage: { rolling: { usagePercent: 40 } } }),
      );
    };
    const profile = app.store.createProfile("Go fixture");
    app.store.saveApiAccount(profile.id, goAccount(), "synthetic-go-key");
    const refresh = () =>
      management(app, `/profiles/${profile.id}/refresh`, { method: "POST" });
    try {
      await app.cores.pool.start(profile.id);
      await app.cores.pool.whileStopped(profile.id, async () => {
        const held = await refresh();
        assert.equal(held.status, 409);
        assert.match(await held.text(), /reconnecting a subscription/);
        assert.equal(goChecks, 1, "checked while the core is held");
      });
      const { child } = app.cores.pool.runtimes.get(profile.id)!;
      const exited = once(child, "exit");
      child.kill("SIGKILL");
      await exited;
      await writeFile(failing, "");
      const failed = await refresh();
      assert.equal(failed.status, 503);
      assert.match(await failed.text(), /Proxy exited \(1\)\./);
      assert.equal(goChecks, 2, "checked although the core cannot start");
      assert.equal(app.store.quota(profile.id, "go-key")?.status, "available");
      assert.deepEqual(app.errors, []);
    } finally {
      globalThis.fetch = originalFetch;
      await closeApplication(app);
      await remove();
    }
  },
);

test(
  "a restart blocked by a credential swap arms no wakeup of its own and begins when the swap ends",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    let changed = () => {};
    const cores = new Cores(store, binary, {
      report: () => {},
      onStatusChange: () => changed(),
      usageTiming: { ...usageTiming, coalesce: 10 },
      management: subscribed(),
    });
    const profile = store.createProfile("Swap fixture");
    store.saveProfile({ ...profile, enabled: true });
    store.saveApiAccount(profile.id, apiKey("swap-key"), "swap-key");
    const running = new Promise<void>((resolve) => {
      changed = () => {
        if (cores.pool.running(profile.id)) resolve();
      };
    });
    try {
      await cores.pool.whileStopped(profile.id, async () => {
        // Its start is due, but nothing may start it until the swap ends.
        cores.usage.begin();
        const wait = cores.usage.wakesAt - Date.now();
        // With no core running, only the daily history cleanup is due.
        assert.ok(wait > 3_600_000, `the scheduler wakes in ${wait} ms`);
      });
      await running;
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "an automatic start waiting for its core does not hold up another profile's maintenance",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    const healthy = store.createProfile("Healthy");
    const unready = store.createProfile("Unready");
    store.saveProfile({ ...unready, enabled: true });
    store.saveApiAccount(unready.id, apiKey("unready-key"), "unready-key");
    let waiting = false;
    let asked!: () => void;
    const starting = new Promise<void>((resolve) => {
      asked = resolve;
    });
    let synced!: () => void;
    const maintained = new Promise<void>((resolve) => {
      synced = resolve;
    });
    const answering = subscribed();
    const cores = new Cores(store, binary, {
      report: () => {},
      usageTiming: { ...usageTiming, coalesce: 10, accounts: 50 },
      management: (request) => {
        // The unready core answers nothing until its start is abandoned.
        if (request.profileId === unready.id) {
          waiting = true;
          asked();
          return new Promise((_, reject) =>
            request.signal?.addEventListener("abort", () =>
              reject(request.signal?.reason),
            ),
          );
        }
        if (waiting && request.path === "/auth-files") synced();
        return answering(request);
      },
    });
    try {
      await cores.pool.start(healthy.id);
      // A UI is open, so account maintenance runs.
      await cores.usage.watch();
      cores.usage.begin();
      await starting;
      // The healthy profile's account sync falls due 50 ms after `begin`.
      await maintained;
      assert.ok(cores.pool.starting.has(unready.id), "still waiting");
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "a start joined to an automatic one ends with shutdown's 503 when shutdown lands in its retry wait",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    const reports: string[] = [];
    let asked!: () => void;
    const waiting = new Promise<void>((resolve) => {
      asked = resolve;
    });
    const cores = new Cores(store, binary, {
      report: (message) => reports.push(message),
      usageTiming: { ...usageTiming, coalesce: 10 },
      // Each readiness check fails at once, so the start waits 100 ms before the next.
      management: (request) => {
        asked();
        return unreachable(request);
      },
    });
    const profile = store.createProfile("Joined fixture");
    store.saveProfile({ ...profile, enabled: true });
    store.saveApiAccount(profile.id, apiKey("joined-key"), "joined-key");
    try {
      cores.usage.begin();
      await waiting;
      // A user's Start, Refresh or credential change joins the automatic start.
      const joined = cores.pool.start(profile.id).then(
        () => assert.fail("the start cannot succeed"),
        (error: Error) => error,
      );
      // Lets the failed check settle: the start now sits in its retry wait.
      await new Promise((resolve) => setImmediate(resolve));
      await cores.shutdown();
      const error = await joined;
      assert.ok(error instanceof AppError, `${error.name}: ${error.message}`);
      assert.equal(error.status, 503);
      assert.deepEqual(reports, []);
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "a core that answers but never finishes its readiness keeps its backoff and is reported once",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    // When each core was first asked whether it runs, and when each one that failed exited.
    const asked: number[] = [];
    const ended: number[] = [];
    const answered = new Set<unknown>();
    const exits = new Set<unknown>();
    const reports: string[] = [];
    let enough!: () => void;
    const failed = new Promise<void>((resolve) => {
      enough = resolve;
    });
    const cores: Cores = new Cores(store, binary, {
      report: (message) => reports.push(message),
      timing: { ...coreTiming, ready: 300 },
      usageTiming: backoff,
      onStatusChange: () => {
        const runtime = cores.pool.runtimes.get(profile.id);
        const { child } = runtime ?? {};
        if (!runtime || !child || exits.has(runtime)) return;
        if (child.exitCode === null && child.signalCode === null) return;
        exits.add(runtime);
        ended.push(Date.now());
        if (ended.length === waits.length + 1) enough();
      },
      // The core answers, so it counts as running, but the readiness account sync always fails.
      management: async ({ path }) => {
        if (path === "/auth-files")
          throw new Error("The fixture's account list fails.");
        // By its process record: a later core may be given an earlier one's port.
        const runtime = cores.pool.runtimes.get(profile.id);
        if (path === "/config" && !answered.has(runtime)) {
          answered.add(runtime);
          asked.push(Date.now());
        }
        return path.startsWith("/usage-queue") ? [] : {};
      },
    });
    const profile = store.createProfile("Unready fixture");
    store.saveApiAccount(profile.id, apiKey("unready-key"), "unready-key");
    try {
      cores.usage.begin();
      await failed;
      await cores.shutdown();
      // Start n + 1 waits for its backoff after start n failed.
      waits.forEach((wait, i) => {
        const gap = asked[i + 1]! - ended[i]!;
        // No upper bound: a loaded machine may start it late, and the bound has its own test.
        assert.ok(
          gap >= wait,
          `start ${i + 2} came ${gap} ms after start ${i + 1} failed, expected at least ${wait} ms`,
        );
      });
      assert.equal(reports.length, 1, `one error entry: ${reports}`);
      assert.match(
        reports[0]!,
        /^Unready fixture: Proxy did not become ready\. The fixture's account list fails\./,
      );
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "a Refresh that shutdown interrupts asks no provider",
  { timeout: 20_000 },
  async () => {
    const { directory, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    let askedReady!: () => void;
    const starting = new Promise<void>((resolve) => {
      askedReady = resolve;
    });
    const answering = subscribed();
    const app = await testApplication(join(directory, "data"), {
      binary,
      cores: {
        usageTiming: { ...usageTiming, coalesce: 10 },
        // The core never answers until its automatic start is abandoned.
        management: (request) => {
          if (request.path !== "/config") return answering(request);
          askedReady();
          return new Promise((_, reject) =>
            request.signal?.addEventListener("abort", () =>
              reject(request.signal?.reason),
            ),
          );
        },
      },
    });
    const usage = `${providers["opencode-go"].preset.baseUrl}/usage`;
    let goChecks = 0;
    const originalFetch = globalThis.fetch;
    globalThis.fetch = async (input, init) => {
      if (String(input) !== usage) return originalFetch(input, init);
      goChecks++;
      return new Response(JSON.stringify({ usage: {} }));
    };
    // Tells when Refresh has joined the automatic start: joining happens within the call.
    const pool = app.cores.pool;
    const start = pool.start.bind(pool);
    let joined!: () => void;
    const joining = new Promise<void>((resolve) => {
      joined = resolve;
    });
    pool.start = (profileId, signal, automatic) => {
      const started = start(profileId, signal, automatic);
      if (!signal) joined();
      return started;
    };
    try {
      const profile = app.store.createProfile("Interrupted fixture");
      app.store.saveApiAccount(profile.id, goAccount(), "synthetic-go-key");
      app.cores.usage.changed();
      await starting;
      const refreshed = management(app, `/profiles/${profile.id}/refresh`, {
        method: "POST",
      });
      await joining;
      await app.cores.shutdown();
      const response = await refreshed;
      assert.equal(response.status, 503);
      assert.match(await response.text(), /shutting down/);
      assert.equal(goChecks, 0, "no provider request after shutdown began");
      assert.deepEqual(app.errors, []);
    } finally {
      globalThis.fetch = originalFetch;
      await closeApplication(app);
      await remove();
    }
  },
);

test(
  "a profile the launch started that exited before the scheduler began is reported on its first failed restart",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    const early = store.createProfile("Early");
    const late = store.createProfile("Late");
    for (const profile of [early, late]) {
      store.saveProfile({ ...profile, enabled: true });
      store.saveApiAccount(profile.id, apiKey(`${profile.id}-key`), "key");
    }
    let releaseLate!: () => void;
    const lateGate = new Promise<void>((resolve) => {
      releaseLate = resolve;
    });
    let lateAsked!: () => void;
    const lateStarting = new Promise<void>((resolve) => {
      lateAsked = resolve;
    });
    let earlyDown = false;
    const reports: string[] = [];
    let reported!: () => void;
    const firstReport = new Promise<void>((resolve) => {
      reported = resolve;
    });
    const answering = subscribed();
    const cores = new Cores(store, binary, {
      report: (message) => {
        reports.push(message);
        reported();
      },
      timing: { ...coreTiming, ready: 300 },
      usageTiming: { ...usageTiming, coalesce: 10 },
      management: async (request) => {
        if (request.profileId === late.id && request.path === "/config") {
          lateAsked();
          await lateGate;
        }
        if (request.profileId === early.id && earlyDown)
          throw new Error("The early core no longer answers.");
        return answering(request);
      },
    });
    try {
      const restoring = cores.restore();
      // Early is running; the restore waits on Late's start.
      await lateStarting;
      const { child } = cores.pool.runtimes.get(early.id)!;
      const exited = once(child, "exit");
      child.kill("SIGKILL");
      await exited;
      earlyDown = true;
      releaseLate();
      await restoring;
      assert.deepEqual(reports, [], "the restore reported no failure");
      await firstReport;
      assert.equal(reports.length, 1);
      assert.match(reports[0]!, /^Early: Proxy did not become ready\./);
    } finally {
      releaseLate();
      await cores.shutdown();
      await remove();
    }
  },
);

/**
 * An application whose one profile, with a subscription and never started, is started by the
 * scheduler with a limit of four. While `crashing`, each core is killed as soon as it runs; with
 * `failing`, each core exits at once; `quota` runs before each quota answer. Every core the
 * profile had is kept, by record.
 */
async function givingUp(
  name: string,
  mode: { failing?: boolean; quota?: () => Promise<void> | undefined },
) {
  const { directory, remove } = await temporaryStore("nv-restart-");
  const failing = join(directory, "failing");
  if (mode.failing) await writeFile(failing, "");
  const binary = await installFakeCore(
    join(directory, "core"),
    `if (require("node:fs").existsSync(${JSON.stringify(failing)})) process.exit(1);\n${idleCore}`,
  );
  const answering = subscribed();
  const app = await testApplication(join(directory, "data"), {
    binary,
    cores: {
      usageTiming: { ...backoff, restartLimit: 4 },
      management: async (request) => {
        if (existsSync(failing)) return unreachable(request);
        if (request.path === "/api-call") await mode.quota?.();
        return answering(request);
      },
    },
  });
  const profile = app.store.createProfile(name);
  app.store.saveApiAccount(profile.id, apiKey(`${name}-key`), "key");
  const fixture = {
    app,
    profile,
    crashing: !mode.failing,
    cores: new Set<unknown>(),
    /** Resolves once the scheduler reports that it gave up. */
    gaveUp: Promise.resolve(),
    close: async () => {
      await closeApplication(app);
      await remove();
    },
  };
  app.on("status", () => {
    const runtime = app.cores.pool.runtimes.get(profile.id);
    if (!runtime) return;
    fixture.cores.add(runtime);
    if (fixture.crashing && runtime.state === "running")
      runtime.child.kill("SIGKILL");
  });
  const report = app.report.bind(app);
  let reported = () => {};
  const watch = () => {
    fixture.gaveUp = new Promise<void>((resolve) => {
      reported = resolve;
    });
  };
  watch();
  app.report = (message) => {
    report(message);
    if (message.includes("Stopped restarting")) {
      reported();
      watch();
    }
  };
  return fixture;
}
/** Settles the scheduler's turn, then checks it gave up: no restart due and the profile says so. */
async function assertGaveUp(fixture: Awaited<ReturnType<typeof givingUp>>) {
  await new Promise((resolve) => setImmediate(resolve));
  const { app, profile } = fixture;
  const wait = app.cores.usage.wakesAt - Date.now();
  // With no core running, only the daily history cleanup is due.
  assert.ok(wait > 3_600_000, `the scheduler wakes in ${wait} ms`);
  const shown = app.state().profiles.find((p) => p.id === profile.id)!;
  assert.equal(shown.runtime, "error");
  assert.equal(
    shown.error,
    "Stopped restarting after 4 failed starts in a row. Use Restart to try again.",
  );
}

test(
  "a core that cannot start is started the limit's number of times, then no more",
  { timeout: 15_000 },
  async () => {
    const fixture = await givingUp("Failing", { failing: true });
    const { app } = fixture;
    try {
      app.cores.usage.changed();
      await fixture.gaveUp;
      await assertGaveUp(fixture);
      assert.equal(fixture.cores.size, 4);
      assert.equal(app.errors.length, 2, `${app.errors}`);
      assert.match(app.errors[0]!, /^Failing: Proxy exited \(1\)\./);
      assert.equal(
        app.errors[1],
        "Failing: Stopped restarting after 4 failed starts in a row. Use Restart to try again.",
      );
    } finally {
      await fixture.close();
    }
  },
);

test(
  "a core that exits as soon as it runs gives up the same way",
  { timeout: 15_000 },
  async () => {
    const fixture = await givingUp("Crashing", {});
    const { app } = fixture;
    try {
      app.cores.usage.changed();
      await fixture.gaveUp;
      await assertGaveUp(fixture);
      assert.equal(fixture.cores.size, 4);
      assert.equal(app.errors.length, 2, `${app.errors}`);
      assert.match(app.errors[0]!, /^Crashing: Proxy exited \(SIGKILL\)\./);
    } finally {
      await fixture.close();
    }
  },
);

test(
  "a Start that succeeds after the scheduler gave up gives the profile its full attempts again",
  { timeout: 20_000 },
  async () => {
    const fixture = await givingUp("Revived", {});
    const { app, profile } = fixture;
    try {
      app.cores.usage.changed();
      await fixture.gaveUp;
      fixture.crashing = false;
      const started = await management(app, `/profiles/${profile.id}/start`, {
        method: "POST",
      });
      assert.equal(started.status, 200, await started.text());
      const shown = app.state().profiles.find((p) => p.id === profile.id)!;
      assert.equal(shown.runtime, "running");
      assert.equal(shown.error, undefined);
      // It crashes again: the scheduler starts it the full four times before giving up.
      fixture.crashing = true;
      const before = fixture.cores.size;
      app.cores.pool.runtimes.get(profile.id)!.child.kill("SIGKILL");
      await fixture.gaveUp;
      await assertGaveUp(fixture);
      assert.equal(fixture.cores.size - before, 4);
    } finally {
      await fixture.close();
    }
  },
);

test(
  "a Start that fails after the scheduler gave up leaves it given up",
  { timeout: 15_000 },
  async () => {
    const fixture = await givingUp("Still failing", { failing: true });
    const { app, profile } = fixture;
    try {
      app.cores.usage.changed();
      await fixture.gaveUp;
      const errors = [...app.errors];
      const started = await management(app, `/profiles/${profile.id}/start`, {
        method: "POST",
      });
      assert.equal(started.status, 503);
      await assertGaveUp(fixture);
      assert.equal(fixture.cores.size, 5, "only the user's start ran");
      assert.deepEqual(app.errors, errors);
    } finally {
      await fixture.close();
    }
  },
);

/**
 * Lets the scheduler's first three cores crash and keeps the fourth, its last allowed start,
 * running; answers that core once its start has ended, and when.
 */
async function lastAllowedCore(fixture: Awaited<ReturnType<typeof givingUp>>) {
  const { app, profile } = fixture;
  const ready = new Promise<Promise<void> | undefined>((resolve) => {
    app.on("status", () => {
      // The fixture's own listener has just killed the third.
      if (fixture.cores.size === 3) fixture.crashing = false;
      const runtime = app.cores.pool.runtimes.get(profile.id);
      // Running counts before the start ends; that start is still in flight.
      if (fixture.cores.size === 4 && runtime?.state === "running")
        resolve(app.cores.pool.starting.get(profile.id));
    });
  });
  app.cores.usage.changed();
  await await ready;
  // The pool hears its start end before this continuation runs, so this is no earlier.
  const ended = Date.now();
  const core = app.cores.pool.runtimes.get(profile.id)!;
  assert.equal(core.state, "running");
  return { core, ended };
}
/** Resolves with whether the scheduler started a core numbered `count` or gave up first. */
function restartedOrGaveUp(
  fixture: Awaited<ReturnType<typeof givingUp>>,
  count: number,
) {
  const { app, profile } = fixture;
  return Promise.race([
    fixture.gaveUp.then(() => "gave up"),
    new Promise<string>((resolve) =>
      app.on("status", () => {
        const runtime = app.cores.pool.runtimes.get(profile.id);
        if (fixture.cores.size === count && runtime?.state === "running")
          resolve("restarted");
      }),
    ),
  ]);
}

test(
  "the limit's last core, up past the stability bound with no tick in between, is restarted when it exits",
  { timeout: 15_000 },
  async () => {
    const fixture = await givingUp("Steady", {});
    const { app } = fixture;
    try {
      const { core, ended } = await lastAllowedCore(fixture);
      // Time itself is what this waits for: the core must outlast the bound.
      await new Promise((resolve) =>
        setTimeout(resolve, 2 * backoff.restartBackoff),
      );
      const up = Date.now() - ended;
      assert.ok(up >= backoff.restartBackoff, `up ${up} ms`);
      // The scheduler's next wakeup is the account refresh: no tick looked at this core after its bound.
      const wait = app.cores.usage.wakesAt - Date.now();
      assert.ok(wait > 10_000, `the scheduler wakes in ${wait} ms`);
      const outcome = restartedOrGaveUp(fixture, 5);
      core.child.kill("SIGKILL");
      assert.equal(await outcome, "restarted");
      assert.ok(
        !app.errors.some((e) => e.includes("Stopped restarting")),
        `${app.errors}`,
      );
    } finally {
      await fixture.close();
    }
  },
);

test(
  "a Start that succeeds while the last automatic start's quota check is pending gives a fresh run",
  { timeout: 15_000 },
  async () => {
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let asked!: () => void;
    const checking = new Promise<void>((resolve) => {
      asked = resolve;
    });
    // The fourth core's quota check waits until this test lets it go.
    const fixture: Awaited<ReturnType<typeof givingUp>> = await givingUp(
      "Hidden",
      {
        quota: () => {
          if (fixture.cores.size < 4) return undefined;
          asked();
          return gate;
        },
      },
    );
    const { app, profile } = fixture;
    try {
      const { core } = await lastAllowedCore(fixture);
      await checking;
      const exited = once(core.child, "exit");
      core.child.kill("SIGKILL");
      await exited;
      // The test's Start begins in the same turn as the exit, before any tick.
      await app.cores.pool.start(profile.id);
      const started = app.cores.pool.runtimes.get(profile.id)!;
      assert.notEqual(started, core);
      assert.equal(started.state, "running");
      const outcome = restartedOrGaveUp(fixture, 6);
      const gone = once(started.child, "exit");
      started.child.kill("SIGKILL");
      await gone;
      release();
      assert.equal(await outcome, "restarted");
      assert.ok(
        !app.errors.some((e) => e.includes("Stopped restarting")),
        `${app.errors}`,
      );
    } finally {
      release();
      await fixture.close();
    }
  },
);

test(
  "a quota answer that arrives just as shutdown abandons the check is not stored or reported",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    const reports: string[] = [];
    let restarted = false;
    let begin!: (shutdown: Promise<void>) => void;
    const shutdown = new Promise<void>((resolve) => {
      begin = resolve;
    });
    const answering = subscribed();
    const cores: Cores = new Cores(store, binary, {
      report: (message) => reports.push(message),
      usageTiming: { ...usageTiming, coalesce: 10 },
      // The restarted core's quota answer is delivered, and shutdown begins before it is stored.
      management: (request) => {
        if (restarted && request.path === "/api-call") begin(cores.shutdown());
        return answering(request);
      },
    });
    const profile = store.createProfile("Late answer fixture");
    try {
      await cores.pool.start(profile.id);
      cores.usage.begin();
      restarted = true;
      const { child } = cores.pool.runtimes.get(profile.id)!;
      const exited = once(child, "exit");
      child.kill("SIGKILL");
      await exited;
      await shutdown;
      assert.equal(store.quota(profile.id, "claude.json"), undefined);
      assert.deepEqual(reports, []);
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "shutdown abandons a restarted profile's quota check and stores nothing for it",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-restart-");
    const binary = await installFakeCore(join(directory, "core"), idleCore);
    const reports: string[] = [];
    let restarted = false;
    let asked!: () => void;
    const checking = new Promise<void>((resolve) => {
      asked = resolve;
    });
    const answering = subscribed();
    const cores = new Cores(store, binary, {
      report: (message) => reports.push(message),
      usageTiming: { ...usageTiming, coalesce: 10 },
      // Once the scheduler restarted the core, the provider never answers its quota check.
      management: (request) => {
        if (!restarted || request.path !== "/api-call")
          return answering(request);
        asked();
        return new Promise((_, reject) =>
          request.signal?.addEventListener("abort", () =>
            reject(request.signal?.reason),
          ),
        );
      },
    });
    const profile = store.createProfile("Quota fixture");
    try {
      await cores.pool.start(profile.id);
      cores.usage.begin();
      restarted = true;
      cores.pool.runtimes.get(profile.id)!.child.kill("SIGKILL");
      await checking;
      const began = Date.now();
      await cores.shutdown();
      const took = Date.now() - began;
      assert.ok(took < 3000, `shutdown took ${took} ms`);
      assert.equal(store.quota(profile.id, "claude.json"), undefined);
      assert.deepEqual(reports, []);
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);
