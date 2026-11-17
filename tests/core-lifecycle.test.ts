import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { chmod, copyFile, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { CorePool, coreTiming, httpManagement } from "../src/server/core.ts";
import { Cores } from "../src/server/cores.ts";
import { usageTiming } from "../src/server/usage.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { record } from "../src/server/json.ts";

test(
  "profile cores refresh model catalogs without overriding the pinned core's Claude fingerprint",
  { timeout: 15_000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-model-refresh-"));
    const store = new Store(directory, fileKeyCodec(directory));
    const core = new CorePool(store, resolve(".vendor/core/cli-proxy-api"));
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
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "a core that never becomes ready and ignores SIGTERM is killed before start fails",
  { timeout: 15_000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-unready-"));
    const store = new Store(directory, fileKeyCodec(directory));
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
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "a failing usage drain retries once promptly, then backs off to a bound and resets on success",
  { timeout: 10_000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-usage-backoff-"));
    const store = new Store(directory, fileKeyCodec(directory));
    const attempts: number[] = [];
    let failing = true;
    const cores = new Cores(store, resolve(".vendor/core/cli-proxy-api"), {
      report: () => {},
      usageTiming: { ...usageTiming, coalesce: 20, usageBackoff: 160 },
      management: async (request) => {
        if (!request.path.startsWith("/usage-queue"))
          return httpManagement(request);
        attempts.push(Date.now());
        if (failing) throw new Error("usage queue unavailable");
        return [];
      },
    });
    const profile = store.createProfile("Backoff fixture");
    try {
      await cores.pool.start(profile.id);
      cores.usage.begin();
      cores.usage.markActivity(profile.id);
      await new Promise((resolve) => setTimeout(resolve, 900));
      const gaps = attempts.slice(1).map((at, i) => at - attempts[i]!);
      // Unbounded 20 ms retries would make about 45 attempts here.
      assert.ok(attempts.length >= 5 && attempts.length <= 10, `${gaps}`);
      assert.ok(gaps[0]! < 80, `first retry stays prompt: ${gaps}`);
      assert.ok(gaps[2]! >= 70, `retries back off: ${gaps}`);
      assert.ok(
        gaps.every((gap) => gap < 160 + 80),
        `backoff is bounded: ${gaps}`,
      );
      failing = false;
      cores.usage.markActivity(profile.id);
      await new Promise((resolve) => setTimeout(resolve, 100));
      assert.equal(
        cores.usage.cadence(profile.id)?.failures,
        0,
        "a success resets the backoff",
      );
    } finally {
      await cores.shutdown();
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "idle profiles stop draining usage after one trailing drain unless a request is in flight",
  { timeout: 10_000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-idle-drain-"));
    const store = new Store(directory, fileKeyCodec(directory));
    let drains = 0;
    let drained = () => {};
    let inFlight = 0;
    const cores = new Cores(store, resolve(".vendor/core/cli-proxy-api"), {
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
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "shutdown completes after the core could not spawn",
  { timeout: 10000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-core-spawn-failure-"));
    const store = new Store(directory, fileKeyCodec(directory));
    const binary = join(directory, "non-executable-core");
    await copyFile(resolve(".vendor/core/cli-proxy-api"), binary);
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
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);

async function runningPool(prefix: string, count: number) {
  const directory = await mkdtemp(join(tmpdir(), prefix));
  const store = new Store(directory, fileKeyCodec(directory));
  const drained = new Set<string>();
  const cores = new Cores(store, resolve(".vendor/core/cli-proxy-api"), {
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
    store.close();
    await rm(directory, { recursive: true, force: true });
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
    const directory = await mkdtemp(join(tmpdir(), "nv-stop-race-"));
    const store = new Store(directory, fileKeyCodec(directory));
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let entered!: () => void;
    const draining = new Promise<void>((resolve) => {
      entered = resolve;
    });
    let gated = false;
    const cores = new Cores(store, resolve(".vendor/core/cli-proxy-api"), {
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
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "OAuth cancellation closes callbacks queued behind cancellation",
  { timeout: 10000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-oauth-race-"));
    const store = new Store(directory, fileKeyCodec(directory));
    // Once set, this profile's management calls reach the fixture instead of its core.
    let fixturePort: number | undefined;
    const cores = new Cores(store, resolve(".vendor/core/cli-proxy-api"), {
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
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "shutdown lets a queued credential change finish before it stops the core",
  { timeout: 15_000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-shutdown-order-"));
    const store = new Store(directory, fileKeyCodec(directory));
    const cores = new Cores(store, resolve(".vendor/core/cli-proxy-api"), {
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
      // Time enough for a shutdown that did not wait to begin stopping the core.
      await new Promise((resolve) => setTimeout(resolve, 100));
      release();
      assert.equal(await change, false, "no stop began during the change");
      await shutdown;
      assert.equal(cores.pool.alive(profile.id), false);
    } finally {
      release();
      await cores.shutdown();
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "a core held stopped for a credential swap cannot start until the swap ends, then restarts",
  { timeout: 15_000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-swap-order-"));
    const store = new Store(directory, fileKeyCodec(directory));
    const pool = new CorePool(store, resolve(".vendor/core/cli-proxy-api"));
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
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "an account sync that outlives its core does not overwrite the restarted core's accounts",
  { timeout: 15_000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-stale-sync-"));
    const store = new Store(directory, fileKeyCodec(directory));
    let hold: Promise<void> | undefined;
    let held!: () => void;
    const holding = new Promise<void>((resolve) => {
      held = resolve;
    });
    let release!: () => void;
    const cores = new Cores(store, resolve(".vendor/core/cli-proxy-api"), {
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
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "readiness retries of one core keep its usage cadence",
  { timeout: 15_000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-ready-retry-"));
    const store = new Store(directory, fileKeyCodec(directory));
    let failed = false;
    const cores: Cores = new Cores(
      store,
      resolve(".vendor/core/cli-proxy-api"),
      {
        report: () => {},
        // The first account sync fails after activity was marked, so readiness retries.
        management: async (request) => {
          if (request.path !== "/auth-files" || failed)
            return httpManagement(request);
          failed = true;
          cores.usage.markActivity(profile.id);
          throw new Error("synthetic account list failure");
        },
      },
    );
    const profile = store.createProfile("Retry fixture");
    try {
      await cores.pool.start(profile.id);
      assert.ok(failed, "readiness retried");
      assert.ok((cores.usage.cadence(profile.id)?.activeUntil ?? 0) > 0);
    } finally {
      await cores.shutdown();
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);
