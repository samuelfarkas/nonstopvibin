import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { CorePool } from "../src/server/core.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";

test("profile cores allow upstream model catalog refreshes", async () => {
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
  } finally {
    await core.shutdown();
    store.close();
    await rm(directory, { recursive: true, force: true });
  }
});

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
    const listener = http.createServer();
    await new Promise<void>((resolve) =>
      listener.listen(0, "127.0.0.1", resolve),
    );
    const address = listener.address();
    assert.ok(address instanceof Object);
    await new Promise<void>((resolve) => listener.close(() => resolve()));
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
                url: `https://example.invalid/auth?redirect_uri=${encodeURIComponent(`http://127.0.0.1:${address.port}/callback`)}`,
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
        `http://127.0.0.1:${address.port}/callback?state=synthetic-state`,
        (res) => res.resume(),
      );
      request.on("error", () => {}); // Cancellation deliberately closes the socket.
      await callbackReceived;
      release();
      await cancel;
      const next = await core.beginOAuth(profile.id, "codex");
      const successful = await fetch(
        `http://127.0.0.1:${address.port}/callback?state=${next.state}`,
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
