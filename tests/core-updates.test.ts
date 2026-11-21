// node:test hooks ignore their timeout option under Bun (5 s default); these build an archive.
import { afterAll, beforeAll } from "bun:test";
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { execFileSync } from "node:child_process";
import { createReadStream } from "node:fs";
import {
  copyFile,
  mkdir,
  mkdtemp,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { SettingsPage } from "../src/client/Settings.tsx";
import type { AppState, CoreState, Json } from "../src/shared/types.ts";
import {
  bundledPin,
  coreArchive,
  coreLayout,
  coreTarget,
  coreVersion,
  fileSha256,
  type CorePin,
} from "../src/server/core-binary.ts";
import { selectInstalledCore } from "../src/server/core-updates.ts";
import { coresDirectory } from "../src/server/data-directory.ts";
import type { Application } from "../src/server/server.ts";
import {
  closeApplication,
  closeServer,
  coreBinary,
  customAccount,
  listen,
  management,
  testApplication,
} from "./support/application.ts";

// Above any real core, so it stays newer than the bundled one.
const update = "99.0.0";
const archiveName = coreArchive(update, process.platform, process.arch);
let work: string;
let archive: string;
let pin: CorePin;
/** What the release server answers: the feed, and the archive's bytes unless replaced. */
let feed: Json;
let served: Buffer | undefined;
let releases: http.Server;
let origin: string;
let upstream: http.Server;
let upstreamBase: string;

beforeAll(async () => {
  work = await mkdtemp(join(tmpdir(), "nv-core-updates-"));
  // The verified core, released again under a version above the bundled one.
  const stage = join(work, "stage");
  await mkdir(stage);
  await copyFile(coreBinary, join(stage, coreLayout.binary));
  archive = join(work, archiveName);
  execFileSync("tar", ["-czf", archive, "-C", stage, coreLayout.binary]);
  const target = coreTarget(process.platform, process.arch);
  assert.ok(target, "the verified core's target has a pin");
  pin = {
    version: update,
    checksums: { ...bundledPin.checksums, [target]: await fileSha256(archive) },
    binaries: {
      ...bundledPin.binaries,
      [target]: await fileSha256(coreBinary),
    },
  };
  releases = http.createServer((req, res) => {
    if (req.url === "/feed.json") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify(feed));
    } else if (req.url === `/releases/v${update}/${archiveName}`) {
      res.writeHead(200, { "Content-Type": "application/gzip" });
      if (served) res.end(served);
      else createReadStream(archive).pipe(res);
    } else {
      res.writeHead(404);
      res.end();
    }
  });
  origin = `http://127.0.0.1:${await listen(releases)}`;
  // A synthetic provider: each completion names the key it was sent with.
  upstream = http.createServer(async (req, res) => {
    for await (const _ of req);
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(
      JSON.stringify({
        id: "fixture",
        object: "chat.completion",
        created: 1,
        model: "fixture-model",
        choices: [
          {
            index: 0,
            message: {
              role: "assistant",
              content: `hello from ${req.headers.authorization}`,
            },
            finish_reason: "stop",
          },
        ],
      }),
    );
  });
  upstreamBase = `http://127.0.0.1:${await listen(upstream)}/v1`;
}, 30_000);

afterAll(async () => {
  await closeServer(releases);
  await closeServer(upstream);
  await rm(work, { recursive: true, force: true });
}, 30_000);

/** An installed build: it reads the test feed and downloads from the test server. */
function installedApp(
  directory: string,
  binary = coreBinary,
): Promise<Application> {
  return testApplication(directory, {
    binary,
    coreFeed: `${origin}/feed.json`,
    cores: { archives: `${origin}/releases` },
  });
}
async function call(
  app: Application,
  path: string,
  method: string,
  body?: Json,
): Promise<{ status: number; state: AppState; message?: string }> {
  const response = await management(app, path, { method, body });
  const answer = await response.json();
  return response.ok
    ? { status: response.status, state: answer }
    : {
        status: response.status,
        state: app.state(),
        message: answer.error.message,
      };
}
async function completes(app: Application, profileId: string): Promise<void> {
  const response = await fetch(`${app.endpoint(profileId)}/chat/completions`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${app.store.clientKey(profileId)}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      model: "fixture-model",
      messages: [{ role: "user", content: "hello" }],
    }),
    signal: AbortSignal.timeout(20_000),
  });
  const text = await response.text();
  assert.equal(response.status, 200, text);
  assert.match(text, /hello from Bearer update-key/);
}
const spawnedFrom = (app: Application, profileId: string) =>
  app.cores.pool.runtimes.get(profileId)?.child.spawnfile;

test(
  "an installed build updates its core, keeps it across launches, and goes back to the bundled one",
  { timeout: 120_000 },
  async () => {
    feed = pin;
    served = undefined;
    const directory = await mkdtemp(join(tmpdir(), "nv-core-update-app-"));
    const installed = join(
      coresDirectory(directory),
      update,
      coreLayout.binary,
    );
    let app = await installedApp(directory);
    try {
      const profile = app.store.createProfile("Update fixture");
      app.store.saveApiAccount(
        profile.id,
        customAccount("update-key", upstreamBase, [
          { id: "fixture-model", protocol: "openai" },
        ]),
        "update-key",
      );
      await app.cores.pool.start(profile.id);

      const checked = await call(app, "/core/check", "POST");
      assert.equal(checked.status, 200, checked.message);
      assert.equal(checked.state.core.available, update);
      assert.equal(checked.state.coreVersion, coreVersion);

      const updated = await call(app, "/core/install", "POST", {
        version: update,
      });
      assert.equal(updated.status, 200, updated.message);
      assert.equal(updated.state.coreVersion, update);
      assert.equal(updated.state.core.bundled, coreVersion);
      assert.equal(updated.state.core.available, undefined);
      assert.equal(updated.state.core.phase, undefined);
      assert.equal(
        updated.state.profiles.find((p) => p.id === profile.id)?.runtime,
        "running",
      );
      assert.equal(spawnedFrom(app, profile.id), installed);
      assert.deepEqual(await readdir(coresDirectory(directory)), [update]);
      await completes(app, profile.id);

      await closeApplication(app);
      app = await installedApp(directory);
      assert.equal(app.state().coreVersion, update);
      assert.equal(spawnedFrom(app, profile.id), installed);

      const reverted = await call(app, "/core/install", "DELETE");
      assert.equal(reverted.status, 200, reverted.message);
      assert.equal(reverted.state.coreVersion, coreVersion);
      assert.deepEqual(await readdir(coresDirectory(directory)), []);
      assert.equal(spawnedFrom(app, profile.id), coreBinary);
      await completes(app, profile.id);
    } finally {
      await closeApplication(app);
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "a core that is not the reviewed pin, too new for this app, or not newer is refused",
  { timeout: 60_000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-core-refusal-app-"));
    const app = await installedApp(directory);
    try {
      feed = pin;
      served = Buffer.from("not the reviewed archive");
      const tampered = await call(app, "/core/install", "POST", {
        version: update,
      });
      assert.equal(tampered.status, 502);
      assert.equal(
        tampered.message,
        "The downloaded core does not match the reviewed pin. Nothing was changed.",
      );
      assert.equal(tampered.state.coreVersion, coreVersion);
      assert.equal(tampered.state.core.phase, undefined);
      assert.deepEqual(await readdir(coresDirectory(directory)), []);

      served = undefined;
      for (const refused of [
        { ...pin, minApp: "999.0.0" },
        { ...pin, version: coreVersion },
      ]) {
        feed = refused;
        const checked = await call(app, "/core/check", "POST");
        assert.equal(checked.status, 200, checked.message);
        assert.equal(checked.state.core.available, undefined);
        const install = await call(app, "/core/install", "POST", {
          version: refused.version,
        });
        assert.equal(install.status, 409);
        assert.equal(install.state.coreVersion, coreVersion);
      }
      assert.deepEqual(await readdir(coresDirectory(directory)), []);
    } finally {
      await closeApplication(app);
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "going back to a bundled core that fails its integrity check keeps the update",
  { timeout: 60_000 },
  async () => {
    feed = pin;
    served = undefined;
    const directory = await mkdtemp(join(tmpdir(), "nv-core-bundled-app-"));
    // A bundled core of this app's own, so it can be damaged after launch.
    const bundled = join(directory, "bundled");
    await mkdir(bundled);
    for (const file of [coreLayout.binary, coreLayout.manifest])
      await copyFile(join(coreBinary, "..", file), join(bundled, file));
    const app = await installedApp(
      join(directory, "data"),
      join(bundled, coreLayout.binary),
    );
    try {
      const updated = await call(app, "/core/install", "POST", {
        version: update,
      });
      assert.equal(updated.status, 200, updated.message);
      await writeFile(join(bundled, coreLayout.binary), "damaged");

      const reverted = await call(app, "/core/install", "DELETE");
      assert.equal(reverted.status, 503);
      assert.equal(
        reverted.message,
        "The proxy core failed its integrity check. Reinstall NonstopVibin.",
      );
      assert.equal(reverted.state.coreVersion, update);
      assert.deepEqual(await readdir(coresDirectory(join(directory, "data"))), [
        update,
      ]);
    } finally {
      await closeApplication(app);
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test("startup selects the newest intact core above the bundled one and deletes the rest", async () => {
  const directory = await mkdtemp(join(tmpdir(), "nv-core-select-"));
  try {
    // "core" hashes to this.
    const hash =
      "0d45f5fd462b8c70bffb10021ac1bcff3f58f29b1faf7568595095427d42812c";
    const core = async (
      name: string,
      manifest: Record<string, string>,
      body = "core",
    ) => {
      await mkdir(join(directory, name));
      await writeFile(join(directory, name, "cli-proxy-api"), body);
      await writeFile(
        join(directory, name, "manifest.json"),
        JSON.stringify(manifest),
      );
    };
    const intact = (version: string) => ({
      version,
      platform: "linux",
      arch: "x64",
      binarySha256: hash,
    });
    await core("9.0.0", { ...intact("9.0.0"), platform: "darwin" });
    await core("8.6.0", { ...intact("8.6.0"), arch: "arm64" });
    await core("8.5.0", intact("8.5.0"), "tampered");
    await core("8.4.0", intact("8.3.0"));
    await core("8.2.0", intact("8.2.0"));
    await core("8.1.0", intact("8.1.0"));
    await core("8.0.67", intact("8.0.67"));
    await core("8.0.1", intact("8.0.1"));
    await mkdir(join(directory, ".staging-x1"));
    await writeFile(join(directory, "8.3.0"), "not a directory");

    assert.deepEqual(
      await selectInstalledCore(directory, "8.0.67", "linux", "x64"),
      { version: "8.2.0", binary: join(directory, "8.2.0", "cli-proxy-api") },
    );
    assert.deepEqual(await readdir(directory), ["8.2.0"]);
    assert.equal(
      await selectInstalledCore(
        join(directory, "none"),
        "8.0.67",
        "linux",
        "x64",
      ),
      undefined,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("Settings offers a reviewed core, then the bundled one, and shows progress", () => {
  const render = (coreVersion: string, core: CoreState) =>
    renderToStaticMarkup(
      createElement(SettingsPage, {
        state: {
          profiles: [],
          coreVersion,
          core,
          gateway: "",
          usageRetentionDays: 0,
          version: "0.2.0",
          errors: [],
        },
        refresh: async () => {},
        onError: assert.fail,
      }),
    );
  const bundled = { bundled: "8.0.67", updates: true };
  const development = render("8.0.67", { ...bundled, updates: false });
  assert.match(development, /Installed builds update it here/);
  assert.doesNotMatch(development, /<button[^>]*>Update/);

  const offered = render("8.0.67", { ...bundled, available: "8.0.99" });
  assert.match(offered, /Version 8.0.99 is available/);
  assert.match(offered, />Update to 8.0.99<\/button>/);
  assert.doesNotMatch(offered, /Use bundled/);

  const installed = render("8.0.99", bundled);
  assert.match(installed, /Updated from the bundled 8.0.67/);
  assert.match(installed, />Use bundled 8.0.67<\/button>/);
  assert.match(installed, /<code>8.0.99<\/code>/);

  const busy = render("8.0.67", {
    ...bundled,
    available: "8.0.99",
    phase: "downloading",
  });
  assert.match(
    busy,
    /<button class="button" disabled="">.*Downloading…<\/button>/,
  );
  assert.doesNotMatch(busy, /Update to/);
});
