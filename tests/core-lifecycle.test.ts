import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { chmod, copyFile, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  CorePool,
  coreTiming,
  httpManagement,
  type ManagementRequest,
} from "../src/server/core.ts";
import { Cores } from "../src/server/cores.ts";
import { usageTiming } from "../src/server/usage.ts";
import { record } from "../src/server/json.ts";
import { coreConfigPath } from "../src/server/data-directory.ts";
import type { Json } from "../src/shared/types.ts";
import { coreBinary, temporaryStore } from "./support/application.ts";
import { idleCore, installFakeCore } from "./support/fake-core.ts";

test(
  "profile cores refresh model catalogs without overriding the pinned core's Claude fingerprint",
  { timeout: 15_000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-model-refresh-");
    const core = new CorePool(store, coreBinary);
    const profile = store.createProfile("Model refresh fixture");
    try {
      await core.start(profile.id);
      assert.ok(
        !core.runtimes
          .get(profile.id)!
          .child.spawnargs.includes("--local-model"),
        "embedded-only mode prevents newly released models reaching profile catalogs",
      );
      const config = record(await core.management(profile.id, "/config"));
      assert.equal(record(config["claude-header-defaults"])["user-agent"], "");
    } finally {
      await core.shutdown();
      await remove();
    }
  },
);

test(
  "a core that never becomes ready and ignores SIGTERM is killed before start fails",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } = await temporaryStore("nv-unready-");
    // Never listens and survives SIGTERM, like a wedged core.
    const binary = join(directory, "wedged-core");
    await writeFile(
      binary,
      `#!${process.execPath}\nprocess.on("SIGTERM", () => {});\nsetInterval(() => {}, 1000);\n`,
    );
    await chmod(binary, 0o700);
    const core = new CorePool(store, binary, {
      timing: { ...coreTiming, ready: 1000, kill: 300 },
    });
    const profile = store.createProfile("Wedged fixture");
    try {
      const started = Date.now();
      await assert.rejects(core.start(profile.id), /did not become ready/);
      assert.ok(Date.now() - started < 5000, "readiness has an overall bound");
      const runtime = core.runtimes.get(profile.id)!;
      assert.equal(runtime.child.signalCode, "SIGKILL");
      assert.equal(runtime.state, "error");
    } finally {
      await core.shutdown();
      await remove();
    }
  },
);

test(
  "a failing usage drain retries once promptly, then backs off to a bound and resets on success",
  { timeout: 10_000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-usage-backoff-");
    // When each queue read began, and what the scheduler decided after each failed one.
    const attempts: number[] = [];
    const decisions: { failures: number; nextAt: number; at: number }[] = [];
    let failing = true;
    let backedOff!: () => void;
    const enough = new Promise<void>((resolve) => {
      backedOff = resolve;
    });
    let succeeded!: () => void;
    const success = new Promise<void>((resolve) => {
      succeeded = resolve;
    });
    const cores: Cores = new Cores(store, coreBinary, {
      // Reported right after the scheduler records a failed drain and its next time.
      report: (message) => {
        if (!message.endsWith("usage queue unavailable")) return;
        const { failures, nextAt } = cores.usage.cadence(profile.id)!;
        decisions.push({ failures, nextAt, at: Date.now() });
        if (decisions.length === 6) backedOff();
      },
      usageTiming: { ...usageTiming, coalesce: 20, usageBackoff: 160 },
      management: async (request) => {
        if (!request.path.startsWith("/usage-queue"))
          return httpManagement(request);
        attempts.push(Date.now());
        if (failing) throw new Error("usage queue unavailable");
        succeeded();
        return [];
      },
    });
    const profile = store.createProfile("Backoff fixture");
    try {
      await cores.pool.start(profile.id);
      cores.usage.begin();
      cores.usage.markActivity(profile.id);
      await enough;
      // Retry n waits min(coalesce * 2^(n-1), usageBackoff); the first retry waits for nothing.
      const waits = [0, 40, 80, 160, 160, 160];
      decisions.forEach(({ failures, nextAt, at }, i) => {
        assert.equal(failures, i + 1);
        if (i === 0) return assert.equal(nextAt, 0, "first retry stays prompt");
        // The decision fell between its read starting and the report.
        assert.ok(
          nextAt - at <= waits[i]! && waits[i]! <= nextAt - attempts[i]!,
          `retry ${i + 1} waits ${waits[i]} ms: ${JSON.stringify(decisions)}`,
        );
        // Timers count from a loop clock that may trail Date.now() by a millisecond.
        if (attempts[i + 1] !== undefined)
          assert.ok(
            attempts[i + 1]! >= nextAt - 1,
            `retry ${i + 2} came before its time: ${attempts}`,
          );
      });
      failing = false;
      cores.usage.markActivity(profile.id);
      await success;
      // Lets the pass that read successfully finish recording it.
      await cores.usage.settled();
      assert.equal(
        cores.usage.cadence(profile.id)?.failures,
        0,
        "a success resets the backoff",
      );
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "idle profiles stop draining usage after one trailing drain unless a request is in flight",
  { timeout: 10_000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-idle-drain-");
    let drains = 0;
    let drained = () => {};
    let inFlight = 0;
    const cores = new Cores(store, coreBinary, {
      report: () => {},
      // Activity keeps the queue draining for 100 ms instead of 10 s.
      usageTiming: { ...usageTiming, coalesce: 20, activity: 100 },
      inFlight: () => inFlight,
      management: async (request) => {
        const result = await httpManagement(request);
        if (request.path.startsWith("/usage-queue")) {
          drains++;
          drained();
        }
        return result;
      },
    });
    const profile = store.createProfile("Idle fixture");
    // Resolves once the scheduler has acted on a drain that began after the window closed.
    const trailingDrain = (cadence: { activeUntil: number }) =>
      new Promise<void>((resolve) => {
        const opened = Date.now();
        drained = () => {
          if (
            opened >= cadence.activeUntil ||
            Date.now() >= cadence.activeUntil
          )
            setImmediate(resolve);
        };
      });
    try {
      await cores.pool.start(profile.id);
      const cadence = cores.usage.cadence(profile.id)!;
      cores.usage.begin();
      cores.usage.markActivity(profile.id);
      await trailingDrain(cadence);
      assert.equal(cadence.nextAt, Infinity, "idle after the trailing drain");
      const settled = drains;
      await new Promise((resolve) => setTimeout(resolve, 300));
      assert.equal(drains, settled, "no usage wakeups while idle");

      inFlight = 1;
      cores.usage.markActivity(profile.id);
      await trailingDrain(cadence);
      assert.ok(
        Number.isFinite(cadence.nextAt) && cadence.nextAt > Date.now(),
        "an open stream or WebSocket keeps its periodic drain",
      );
      inFlight = 0;

      const beforeStop = drains;
      await cores.pool.stop(profile.id);
      assert.equal(drains, beforeStop + 1, "stop still drains the queue");
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "shutdown completes after the core could not spawn",
  { timeout: 10000 },
  async () => {
    const { directory, store, remove } = await temporaryStore(
      "nv-core-spawn-failure-",
    );
    const binary = join(directory, "non-executable-core");
    await copyFile(coreBinary, binary);
    await chmod(binary, 0o600);
    const core = new CorePool(store, binary);
    const profile = store.createProfile("Spawn failure");
    try {
      await assert.rejects(core.start(profile.id), /Could not launch proxy/);
      assert.equal(core.runtimes.get(profile.id)?.child.pid, undefined);
      await core.shutdown();
      assert.equal(core.starting.size, 0);
    } finally {
      await core.shutdown();
      await remove();
    }
  },
);

async function runningPool(prefix: string, count: number) {
  const { store, remove } = await temporaryStore(prefix);
  const drained = new Set<string>();
  const cores = new Cores(store, coreBinary, {
    report: () => {},
    management: (request) => {
      if (request.path.startsWith("/usage-queue"))
        drained.add(request.profileId);
      return httpManagement(request);
    },
  });
  const profiles = [];
  for (let i = 0; i < count; i++) {
    const profile = store.createProfile(`Shutdown ${i}`);
    await cores.pool.start(profile.id);
    profiles.push(profile);
  }
  const close = async () => {
    await cores.shutdown();
    await remove();
  };
  return { cores, profiles, drained, close };
}
const exited = (child: { exitCode: number | null; signalCode: unknown }) =>
  child.exitCode !== null || child.signalCode !== null;

test(
  "shutdown stops slow-exiting profiles together, in about the time of the slowest",
  { timeout: 20_000 },
  async () => {
    const pool = await runningPool("nv-shutdown-together-", 3);
    try {
      const exitDelay = 1000;
      const signalled: number[] = [];
      const exits: number[] = [];
      for (const profile of pool.profiles) {
        const child = pool.cores.pool.runtimes.get(profile.id)!.child;
        const kill = child.kill.bind(child);
        // As on a slow runner: each core exits a second after it is told to.
        child.kill = (signal) => {
          signalled.push(Date.now());
          setTimeout(() => kill(signal), exitDelay);
          return true;
        };
        child.once("exit", () => exits.push(Date.now()));
      }
      const started = Date.now();
      await pool.cores.shutdown();
      const elapsed = Date.now() - started;
      assert.ok(
        Math.max(...signalled) < Math.min(...exits),
        "every core is told to exit before the first one has",
      );
      assert.ok(
        elapsed < 2 * exitDelay,
        `shutdown took ${elapsed} ms; one at a time takes over ${3 * exitDelay} ms`,
      );
      assert.equal(pool.drained.size, 3, "every profile's usage was drained");
      for (const profile of pool.profiles) {
        const runtime = pool.cores.pool.runtimes.get(profile.id)!;
        assert.equal(runtime.state, "stopped");
        assert.ok(exited(runtime.child));
      }
    } finally {
      await pool.close();
    }
  },
);

test(
  "a failing stop does not keep shutdown from draining and stopping the other profiles",
  { timeout: 20_000 },
  async () => {
    // The failing profiles come first, where one-at-a-time shutdown gave up on the rest.
    for (const failing of [[0], [0, 1]]) {
      const pool = await runningPool("nv-shutdown-failure-", 3);
      const broken = failing.map(
        (i) => pool.cores.pool.runtimes.get(pool.profiles[i]!.id)!.child,
      );
      try {
        broken.forEach((child, i) => {
          child.kill = () => {
            throw new Error(`synthetic stop failure ${i}`);
          };
        });
        await assert.rejects(pool.cores.shutdown(), (error: Error) => {
          const errors =
            error instanceof AggregateError ? error.errors : [error];
          assert.equal(error instanceof AggregateError, failing.length > 1);
          assert.deepEqual(
            errors.map((e: Error) => e.message),
            failing.map((i) => `synthetic stop failure ${i}`),
          );
          for (const e of errors)
            assert.match(error.message, RegExp(e.message));
          return true;
        });
        assert.equal(pool.drained.size, 3, "every profile's usage was drained");
        for (const profile of pool.profiles.slice(failing.length)) {
          const runtime = pool.cores.pool.runtimes.get(profile.id)!;
          assert.equal(runtime.state, "stopped");
          assert.ok(exited(runtime.child));
        }
      } finally {
        // Restore the real kill so the second shutdown in close() ends these cores.
        for (const child of broken) Reflect.deleteProperty(child, "kill");
        await pool.close();
      }
      for (const child of broken) assert.ok(exited(child));
    }
  },
);

test(
  "queued starts reject shutdown after a pending accounting drain",
  { timeout: 10000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-stop-race-");
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let entered!: () => void;
    const draining = new Promise<void>((resolve) => {
      entered = resolve;
    });
    let gated = false;
    const cores = new Cores(store, coreBinary, {
      report: () => {},
      management: async (request) => {
        if (!gated || !request.path.startsWith("/usage-queue"))
          return httpManagement(request);
        entered();
        await gate;
        return [];
      },
    });
    const core = cores.pool;
    const profile = store.createProfile("Shutdown fixture");
    try {
      await Promise.all([core.start(profile.id), core.stop(profile.id)]);
      assert.equal(
        core.runtimes.get(profile.id)?.state,
        "stopped",
        "an immediate stop must observe the new startup",
      );
      await core.start(profile.id);
      gated = true;
      const stopping = core.stop(profile.id);
      await draining;
      const starting = assert.rejects(core.start(profile.id), /shutting down/);
      const shutdown = cores.shutdown();
      release();
      await Promise.all([stopping, starting, shutdown]);
      assert.equal(core.starting.size, 0);
      const runtime = core.runtimes.get(profile.id)!;
      assert.equal(runtime.state, "stopped");
      assert.ok(
        runtime.child.exitCode !== null || runtime.child.signalCode !== null,
      );
      await assert.rejects(core.start(profile.id), /shutting down/);
    } finally {
      release();
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "OAuth cancellation closes callbacks queued behind cancellation",
  { timeout: 10000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-oauth-race-");
    // Once set, this profile's management calls reach the fixture instead of its core.
    let fixturePort: number | undefined;
    const cores = new Cores(store, coreBinary, {
      report: () => {},
      management: (request) =>
        httpManagement(
          fixturePort === undefined
            ? request
            : { ...request, port: fixturePort },
        ),
    });
    const { pool: core, signIn } = cores;
    const profile = store.createProfile("OAuth fixture");
    // Port 0 lets the callback server pick a free port itself; reserving one first races.
    const callbackPort = () => {
      const address = signIn.callbackServer?.address();
      assert.ok(address instanceof Object);
      return address.port;
    };
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let entered!: () => void;
    const cancelling = new Promise<void>((resolve) => {
      entered = resolve;
    });
    const fixture = http.createServer(async (req, res) => {
      res.setHeader("Content-Type", "application/json");
      if (req.method === "DELETE") {
        entered();
        await gate;
      }
      res.end(
        JSON.stringify(
          req.url?.startsWith("/v0/management/codex-auth-url")
            ? {
                url: `https://example.invalid/auth?redirect_uri=${encodeURIComponent("http://127.0.0.1:0/callback")}`,
                state: "synthetic-state",
              }
            : {},
        ),
      );
    });
    await new Promise<void>((resolve) =>
      fixture.listen(0, "127.0.0.1", resolve),
    );
    let request: http.ClientRequest | undefined;
    try {
      await core.start(profile.id);
      const managementAddress = fixture.address();
      assert.ok(managementAddress instanceof Object);
      fixturePort = managementAddress.port;
      await signIn.begin(profile.id, "codex");
      const port = callbackPort();
      const cancel = signIn.cancel(profile.id, "synthetic-state");
      await cancelling;
      // Its own handler runs first and queues the callback behind the cancellation.
      const callbackReceived = new Promise<void>((resolve) =>
        signIn.callbackServer?.once("request", () => resolve()),
      );
      request = http.get(
        `http://127.0.0.1:${port}/callback?state=synthetic-state`,
        (res) => res.resume(),
      );
      request.on("error", () => {}); // Cancellation deliberately closes the socket.
      await callbackReceived;
      release();
      await cancel;
      const next = await signIn.begin(profile.id, "codex");
      const successful = await fetch(
        `http://127.0.0.1:${callbackPort()}/callback?state=${next.state}`,
      );
      assert.equal(successful.status, 200);
      assert.match(await successful.text(), /Sign-in received/);
      await signIn.cancel(profile.id, next.state);
    } finally {
      release();
      request?.destroy();
      await cores.shutdown();
      fixture.close();
      fixture.closeAllConnections();
      await remove();
    }
  },
);

test(
  "shutdown lets a sign-in action finish before it waits for the credential changes the action queues",
  { timeout: 15_000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-shutdown-sign-in-");
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let entered!: () => void;
    const signingIn = new Promise<void>((resolve) => {
      entered = resolve;
    });
    let cleared = false;
    let change: Promise<boolean> | undefined;
    const cores: Cores = new Cores(store, coreBinary, {
      report: () => {},
      management: async (request) => {
        if (request.path.startsWith("/oauth-session")) cleared = true;
        if (request.path !== "/codex-auth-url") return httpManagement(request);
        entered();
        await gate;
        // The action queues a credential change as it ends, after shutdown began.
        change = cores.credentials.queue(async () => {
          await new Promise((resolve) => setImmediate(resolve));
          return cleared;
        });
        // No redirect, so no callback server: only the action's ordering matters here.
        return { url: "https://example.invalid/auth", state: "fixture" };
      },
    });
    const profile = store.createProfile("Sign-in order fixture");
    try {
      await cores.pool.start(profile.id);
      const action = cores.signIn.begin(profile.id, "codex");
      await signingIn;
      const shutdown = cores.shutdown();
      release();
      await action;
      assert.equal(
        await change,
        false,
        "the change ran before shutdown dropped the sign-in",
      );
      await shutdown;
      assert.equal(cores.pool.alive(profile.id), false);
    } finally {
      release();
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "shutdown lets the scheduler's running drain pass finish before it stops the cores",
  { timeout: 15_000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-shutdown-pass-");
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let entered!: () => void;
    const draining = new Promise<void>((resolve) => {
      entered = resolve;
    });
    let gated = false;
    const cores = new Cores(store, coreBinary, {
      report: () => {},
      usageTiming: { ...usageTiming, coalesce: 20 },
      management: async (request) => {
        if (!gated || !request.path.startsWith("/usage-queue"))
          return httpManagement(request);
        gated = false;
        entered();
        await gate;
        return [];
      },
    });
    const profile = store.createProfile("Drain pass fixture");
    try {
      await cores.pool.start(profile.id);
      gated = true;
      cores.usage.begin();
      cores.usage.markActivity(profile.id);
      await draining;
      const shutdown = cores.shutdown();
      // Every step before the stops settles within this turn when nothing holds it.
      await new Promise((resolve) => setImmediate(resolve));
      assert.equal(
        cores.pool.runtimes.get(profile.id)?.stopping,
        false,
        "no stop began during the scheduler's pass",
      );
      release();
      await shutdown;
      assert.equal(cores.pool.alive(profile.id), false);
    } finally {
      release();
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "shutdown lets a queued credential change finish before it stops the core",
  { timeout: 15_000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-shutdown-order-");
    const cores = new Cores(store, coreBinary, {
      report: () => {},
    });
    const profile = store.createProfile("Order fixture");
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let entered!: () => void;
    const changing = new Promise<void>((resolve) => {
      entered = resolve;
    });
    try {
      await cores.pool.start(profile.id);
      const change = cores.credentials.queue(async () => {
        entered();
        await gate;
        return cores.pool.runtimes.get(profile.id)?.stopping;
      });
      await changing;
      const shutdown = cores.shutdown();
      await assert.rejects(cores.pool.start(profile.id), /shutting down/);
      // Every step before the stops settles within this turn when nothing holds it.
      await new Promise((resolve) => setImmediate(resolve));
      release();
      assert.equal(await change, false, "no stop began during the change");
      await shutdown;
      assert.equal(cores.pool.alive(profile.id), false);
    } finally {
      release();
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "a core held stopped for a credential swap cannot start until the swap ends, then restarts",
  { timeout: 15_000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-swap-order-");
    const pool = new CorePool(store, coreBinary);
    const profile = store.createProfile("Swap fixture");
    try {
      await pool.start(profile.id);
      const before = pool.runtimes.get(profile.id)!.child;
      await pool.whileStopped(profile.id, async () => {
        assert.equal(pool.alive(profile.id), false, "stopped before the swap");
        await assert.rejects(pool.start(profile.id), /reconnecting/);
      });
      assert.ok(pool.running(profile.id), "running again after the swap");
      assert.notEqual(pool.runtimes.get(profile.id)!.child, before);
    } finally {
      await pool.shutdown();
      await remove();
    }
  },
);

test(
  "an account sync that outlives its core does not overwrite the restarted core's accounts",
  { timeout: 15_000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-stale-sync-");
    let hold: Promise<void> | undefined;
    let held!: () => void;
    const holding = new Promise<void>((resolve) => {
      held = resolve;
    });
    let release!: () => void;
    const cores = new Cores(store, coreBinary, {
      report: () => {},
      // The held listing is the old core's: one subscription the new core no longer has.
      management: async (request) => {
        if (request.path !== "/auth-files" || !hold)
          return httpManagement(request);
        const gate = hold;
        hold = undefined;
        held();
        await gate;
        return { files: [{ name: "old.json", provider: "codex" }] };
      },
    });
    const profile = store.createProfile("Stale sync fixture");
    try {
      await cores.pool.start(profile.id);
      hold = new Promise<void>((resolve) => {
        release = resolve;
      });
      const stale = cores.accounts.sync(profile.id);
      await holding;
      await cores.pool.stop(profile.id);
      await cores.pool.start(profile.id);
      release();
      await stale;
      assert.deepEqual(cores.accounts.list(profile.id), []);
      assert.deepEqual(store.accountMetadata(profile.id), []);
    } finally {
      release?.();
      await cores.shutdown();
      await remove();
    }
  },
);

test(
  "readiness retries of one core keep its usage cadence",
  { timeout: 15_000 },
  async () => {
    const { store, remove } = await temporaryStore("nv-ready-retry-");
    let failed = false;
    const cores: Cores = new Cores(store, coreBinary, {
      report: () => {},
      // The first account sync fails after activity was marked, so readiness retries.
      management: async (request) => {
        if (request.path !== "/auth-files" || failed)
          return httpManagement(request);
        failed = true;
        cores.usage.markActivity(profile.id);
        throw new Error("synthetic account list failure");
      },
    });
    const profile = store.createProfile("Retry fixture");
    try {
      await cores.pool.start(profile.id);
      assert.ok(failed, "readiness retried");
      assert.ok((cores.usage.cadence(profile.id)?.activeUntil ?? 0) > 0);
    } finally {
      await cores.shutdown();
      await remove();
    }
  },
);

/** A pool whose profile's first readiness answer waits for `release`; `holding` resolves when it waits. */
async function heldReadiness(prefix: string) {
  const { store, remove } = await temporaryStore(prefix);
  let held!: () => void;
  const holding = new Promise<void>((resolve) => {
    held = resolve;
  });
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let waiting = true;
  // Sees each management request first; an answer it returns replaces the core's.
  let intercept:
    | ((request: ManagementRequest) => Promise<Json> | undefined)
    | undefined;
  const pool = new CorePool(store, coreBinary, {
    management: async (request) => {
      const answer = intercept?.(request);
      if (answer) return answer;
      if (waiting && request.path === "/config") {
        waiting = false;
        held();
        await gate;
      }
      return httpManagement(request);
    },
  });
  const profile = store.createProfile("Starting settings fixture");
  const strategy = async () =>
    record(record(await pool.management(profile.id, "/config")).routing)
      .strategy;
  const close = async () => {
    release();
    await pool.shutdown();
    await remove();
  };
  return {
    store,
    pool,
    profile,
    holding,
    release,
    intercept: (next: typeof intercept) => {
      intercept = next;
    },
    strategy,
    close,
  };
}

test(
  "a settings change saved while the core starts is applied once the core is ready",
  { timeout: 15_000 },
  async () => {
    const fixture = await heldReadiness("nv-start-settings-");
    const { store, pool, profile } = fixture;
    try {
      const start = pool.start(profile.id);
      await fixture.holding;
      assert.equal(pool.runtimes.get(profile.id)?.state, "starting");
      // As PATCH /profiles/:profileId does: save, then configure.
      store.saveProfile({
        ...store.profile(profile.id),
        strategy: "round-robin",
      });
      const configure = pool.configure(profile.id);
      fixture.release();
      await Promise.all([start, configure]);
      assert.equal(await fixture.strategy(), "round-robin");
    } finally {
      await fixture.close();
    }
  },
);

test(
  "a settings change that waited for a start leaves alone a core that a swap or shutdown then stops",
  { timeout: 15_000 },
  async () => {
    for (const interruption of ["swap", "shutdown"] as const) {
      const fixture = await heldReadiness(`nv-start-settings-${interruption}-`);
      const { store, pool, profile } = fixture;
      try {
        const start = pool.start(profile.id);
        await fixture.holding;
        const first = pool.runtimes.get(profile.id);
        // Once the first core has answered, a request to it can only be the change's confirmation.
        let confirmations = 0;
        fixture.intercept((request) => {
          if (
            pool.runtimes.get(request.profileId) === first &&
            first?.state === "running"
          )
            confirmations++;
          return undefined;
        });
        store.saveProfile({
          ...store.profile(profile.id),
          strategy: "round-robin",
        });
        const configure = pool.configure(profile.id);
        const stopping =
          interruption === "swap"
            ? pool.whileStopped(profile.id, async () => {})
            : pool.shutdown();
        fixture.release();
        await Promise.all([start, configure, stopping]);
        assert.equal(
          confirmations,
          0,
          `${interruption}: no request to the stopped core`,
        );
        assert.equal(first?.state, "stopped");
        if (interruption === "swap")
          assert.equal(
            await fixture.strategy(),
            "round-robin",
            "the core started after the swap has the change",
          );
        else assert.equal(pool.alive(profile.id), false);
      } finally {
        await fixture.close();
      }
    }
  },
);

test(
  "a settings change that waited for a start leaves alone a core that a plain stop then stops",
  { timeout: 15_000 },
  async () => {
    for (const order of ["change first", "stop first"] as const) {
      const fixture = await heldReadiness("nv-start-settings-stop-");
      const { store, pool, profile } = fixture;
      try {
        const start = pool.start(profile.id);
        await fixture.holding;
        const first = pool.runtimes.get(profile.id);
        let afterStop = 0;
        fixture.intercept((request) => {
          if (pool.runtimes.get(request.profileId) === first && first?.stopping)
            afterStop++;
          return undefined;
        });
        store.saveProfile({
          ...store.profile(profile.id),
          strategy: "round-robin",
        });
        // As sign-in cleanup does for a core it cannot reach: a stop that is neither a swap nor shutdown.
        const change = () => pool.configure(profile.id);
        const stop = () => pool.stop(profile.id);
        const pending =
          order === "change first" ? [change(), stop()] : [stop(), change()];
        fixture.release();
        await Promise.all([start, ...pending]);
        assert.equal(afterStop, 0, `${order}: no request to the stopping core`);
        assert.equal(first?.state, "stopped");
        await pool.start(profile.id);
        assert.equal(
          await fixture.strategy(),
          "round-robin",
          `${order}: the next start runs with the saved setting`,
        );
      } finally {
        await fixture.close();
      }
    }
  },
);

test(
  "a confirmation outstanding while a swap replaces the core leaves the successor's config alone",
  { timeout: 15_000 },
  async () => {
    const fixture = await heldReadiness("nv-replaced-confirmation-");
    const { store, pool, profile } = fixture;
    try {
      fixture.release();
      await pool.start(profile.id);
      const first = pool.runtimes.get(profile.id);
      let polls = 0;
      let held!: () => void;
      const holding = new Promise<void>((resolve) => {
        held = resolve;
      });
      let answer!: () => void;
      const gate = new Promise<void>((resolve) => {
        answer = resolve;
      });
      // The first core's confirmations report the old settings; the tenth waits for `answer`.
      fixture.intercept((request) => {
        if (
          request.path !== "/config" ||
          pool.runtimes.get(request.profileId) !== first
        )
          return undefined;
        if (++polls < 10) return Promise.resolve({});
        held();
        return gate.then(() => ({}));
      });
      store.saveProfile({
        ...store.profile(profile.id),
        strategy: "round-robin",
      });
      const configure = pool.configure(profile.id);
      await holding;
      await pool.whileStopped(profile.id, async () => {});
      const successor = pool.runtimes.get(profile.id);
      assert.ok(successor && successor !== first);
      assert.notEqual(successor.port, first?.port);
      answer();
      await configure;
      assert.match(
        await readFile(coreConfigPath(store.directory, profile.id), "utf8"),
        new RegExp(`^port: ${successor.port}$`, "m"),
        "the successor's config keeps its own port",
      );
      assert.equal(await fixture.strategy(), "round-robin");
    } finally {
      await fixture.close();
    }
  },
);

test(
  "a core that misses the first write to its config applies the settings once configure writes it again",
  { timeout: 15_000 },
  async () => {
    const { directory, store, remove } =
      await temporaryStore("nv-missed-write-");
    // Reads its config on each request and ignores the first change to it, as a core whose
    // file watcher is not ready yet; it answers /config with the routing it has loaded.
    const binary = await installFakeCore(
      join(directory, "core"),
      `${idleCore}const fs = require("node:fs");
const config = process.argv[process.argv.indexOf("--config") + 1];
let loaded = fs.readFileSync(config, "utf8");
let seen = fs.statSync(config).mtimeMs;
let changes = 0;
require("node:http").createServer((_req, res) => {
  const mtime = fs.statSync(config).mtimeMs;
  if (mtime !== seen) {
    seen = mtime;
    if (++changes > 1) loaded = fs.readFileSync(config, "utf8");
  }
  res.setHeader("Content-Type", "application/json");
  res.end(JSON.stringify({ routing: {
    strategy: /^  strategy: (\\S+)$/m.exec(loaded)[1],
    "session-affinity": /^  session-affinity: true$/m.test(loaded),
    "session-affinity-ttl": "1h",
  } }));
}).listen(Number(/^port: (\\d+)$/m.exec(loaded)[1]), "127.0.0.1");
`,
    );
    const pool = new CorePool(store, binary);
    const profile = store.createProfile("Missed write fixture");
    try {
      await pool.start(profile.id);
      store.saveProfile({
        ...store.profile(profile.id),
        strategy: "round-robin",
      });
      await pool.configure(profile.id);
      const routing = record(
        record(await pool.management(profile.id, "/config")).routing,
      );
      assert.equal(routing.strategy, "round-robin");
    } finally {
      await pool.shutdown();
      await remove();
    }
  },
);
