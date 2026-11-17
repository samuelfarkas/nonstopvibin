import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { chmod, copyFile, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { CorePool } from "../src/server/core.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { record } from "../src/server/json.ts";

test("profile cores refresh model catalogs without overriding the pinned core's Claude fingerprint", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-model-refresh-"));
  const store = new Store(directory, fileKeyCodec(directory));
  const core = new CorePool(store, resolve(".vendor/core/cli-proxy-api"));
  const profile = store.createProfile("Model refresh fixture", "forest");
  try {
    await core.start(profile.id);
    assert.ok(
      !core.runtimes.get(profile.id)!.child.spawnargs.includes("--local-model"),
      "embedded-only mode prevents newly released models reaching profile catalogs",
    );
    const config = record(await core.management(profile.id, "/config"));
    assert.equal(record(config["claude-header-defaults"])["user-agent"], "");
  } finally {
    await core.shutdown();
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

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
    const core = new CorePool(store, binary);
    core.timing.ready = 1000;
    core.timing.kill = 300;
    const profile = store.createProfile("Wedged fixture", "forest");
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
    const core = new CorePool(store, resolve(".vendor/core/cli-proxy-api"));
    core.timing.coalesce = 20;
    core.timing.usageBackoff = 160;
    const profile = store.createProfile("Backoff fixture", "forest");
    const attempts: number[] = [];
    let failing = true;
    core.collectUsage = async () => {
      attempts.push(Date.now());
      if (failing) throw new Error("usage queue unavailable");
      return 0;
    };
    try {
      await core.start(profile.id);
      const runtime = core.runtimes.get(profile.id)!;
      core.beginPolling();
      core.markUsageActivity(profile.id);
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
      core.markUsageActivity(profile.id);
      await new Promise((resolve) => setTimeout(resolve, 100));
      assert.equal(runtime.usageFailures, 0, "a success resets the backoff");
    } finally {
      await core.shutdown();
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
    const core = new CorePool(store, resolve(".vendor/core/cli-proxy-api"));
    core.timing.coalesce = 20;
    const profile = store.createProfile("Idle fixture", "forest");
    const collectUsage = core.collectUsage;
    let drains = 0;
    let drained = () => {};
    core.collectUsage = async (id) => {
      const count = await collectUsage.call(core, id);
      drains++;
      drained();
      return count;
    };
    // Resolves once the scheduler has acted on a drain that began after the window closed.
    const trailingDrain = (runtime: { usageActiveUntil: number }) =>
      new Promise<void>((resolve) => {
        const opened = Date.now();
        drained = () => {
          if (
            opened >= runtime.usageActiveUntil ||
            Date.now() >= runtime.usageActiveUntil
          )
            setImmediate(resolve);
        };
      });
    try {
      await core.start(profile.id);
      const runtime = core.runtimes.get(profile.id)!;
      core.beginPolling();
      core.markUsageActivity(profile.id);
      runtime.usageActiveUntil = Date.now() + 100;
      await trailingDrain(runtime);
      assert.equal(
        runtime.nextUsageAt,
        Infinity,
        "idle after the trailing drain",
      );
      const settled = drains;
      await new Promise((resolve) => setTimeout(resolve, 300));
      assert.equal(drains, settled, "no usage wakeups while idle");

      core.usageInFlight = () => 1;
      core.markUsageActivity(profile.id);
      runtime.usageActiveUntil = Date.now() + 100;
      await trailingDrain(runtime);
      assert.ok(
        Number.isFinite(runtime.nextUsageAt) &&
          runtime.nextUsageAt > Date.now(),
        "an open stream or WebSocket keeps its periodic drain",
      );
      core.usageInFlight = () => 0;

      const beforeStop = drains;
      await core.stop(profile.id);
      assert.equal(drains, beforeStop + 1, "stop still drains the queue");
    } finally {
      await core.shutdown();
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
    const profile = store.createProfile("Spawn failure", "forest");
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

test(
  "queued starts reject shutdown after a pending accounting drain",
  { timeout: 10000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-stop-race-"));
    const store = new Store(directory, fileKeyCodec(directory));
    const core = new CorePool(store, resolve(".vendor/core/cli-proxy-api"));
    const profile = store.createProfile("Shutdown fixture", "forest");
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let entered!: () => void;
    const draining = new Promise<void>((resolve) => {
      entered = resolve;
    });
    try {
      await Promise.all([core.start(profile.id), core.stop(profile.id)]);
      assert.equal(
        core.runtimes.get(profile.id)?.state,
        "stopped",
        "an immediate stop must observe the new startup",
      );
      await core.start(profile.id);
      core.collectUsage = async () => {
        entered();
        await gate;
        return 0;
      };
      const stopping = core.stop(profile.id);
      await draining;
      const starting = assert.rejects(core.start(profile.id), /shutting down/);
      const shutdown = core.shutdown();
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
      await core.shutdown();
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
    const core = new CorePool(store, resolve(".vendor/core/cli-proxy-api"));
    const profile = store.createProfile("OAuth fixture", "forest");
    // Port 0 lets the callback server pick a free port itself; reserving one first races.
    const callbackPort = () => {
      const address = core["callbackServer"]?.address();
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
      core.runtimes.get(profile.id)!.port = managementAddress.port;
      await core.beginOAuth(profile.id, "codex");
      const port = callbackPort();
      const cancel = core.cancelOAuth(profile.id, "synthetic-state");
      await cancelling;
      let received!: () => void;
      const callbackReceived = new Promise<void>((resolve) => {
        received = resolve;
      });
      const callback = core.oauthCallback.bind(core);
      core.oauthCallback = (...args) => {
        received();
        return callback(...args);
      };
      request = http.get(
        `http://127.0.0.1:${port}/callback?state=synthetic-state`,
        (res) => res.resume(),
      );
      request.on("error", () => {}); // Cancellation deliberately closes the socket.
      await callbackReceived;
      release();
      await cancel;
      const next = await core.beginOAuth(profile.id, "codex");
      const successful = await fetch(
        `http://127.0.0.1:${callbackPort()}/callback?state=${next.state}`,
      );
      assert.equal(successful.status, 200);
      assert.match(await successful.text(), /Sign-in received/);
      await core.cancelOAuth(profile.id, next.state);
    } finally {
      release();
      request?.destroy();
      await core.shutdown();
      fixture.close();
      fixture.closeAllConnections();
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);
