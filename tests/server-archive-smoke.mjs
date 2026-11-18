// Run with `bun run test:server` on Linux x64 after `bun run dist:server`: installs the
// linux-x64 archive from release/ into temporary directories and runs the installed CLI.
import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import pkg from "../package.json" with { type: "json" };
import { serverArchiveName } from "../scripts/distribution.ts";
import { notRunningExit, serverArchive } from "../src/server/host-defaults.ts";
import { unusedPort } from "./support/ports.ts";

const name = serverArchiveName(pkg.version, "x64");
const archive = resolve(import.meta.dirname, "../release", `${name}.tar.gz`);
const work = await mkdtemp(join(tmpdir(), "nv-server-smoke-"));
const env = {
  ...process.env,
  HOME: join(work, "home"),
  NONSTOPVIBIN_DATA_DIR: join(work, "data"),
};
const exited = (child) =>
  new Promise((done) =>
    child.once("exit", (code, signal) => done({ code, signal })),
  );
let server;
let exit;
try {
  await mkdir(env.HOME);
  execFileSync("tar", ["-xzf", archive, "-C", work]);
  execFileSync(
    join(work, name, serverArchive.installer),
    ["--no-service", "--prefix", join(work, "prefix")],
    { env, stdio: "inherit" },
  );
  const nv = join(work, "prefix/bin", serverArchive.executable);
  const status = () =>
    spawnSync(nv, ["status"], { env, stdio: "ignore" }).status;
  const port = await unusedPort();
  // A pipe is not a TTY, like journald.
  const log = [];
  server = spawn(nv, ["serve", "--port", String(port)], {
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  exit = exited(server);
  server.stdout.on("data", (chunk) => log.push(chunk));
  server.stderr.on("data", (chunk) => log.push(chunk));
  for (let attempt = 0; attempt < 30 && status() !== 0; attempt++) {
    // A server that exited will not become ready.
    if (server.exitCode !== null || server.signalCode !== null) break;
    await delay(1000);
  }
  if (status() !== 0)
    throw new Error(`serve is not running. Its output:\n${Buffer.concat(log)}`);
  execFileSync(nv, ["status"], { env, stdio: "inherit" });
  const { url } = JSON.parse(
    execFileSync(nv, ["url", "--json"], { env, encoding: "utf8" }),
  );
  const token = url.split("#session=")[1];
  assert.ok(token, "url --json has no session token");
  const page = await fetch(`http://127.0.0.1:${port}/`);
  assert.ok(page.ok, `GET / answered ${page.status}`);
  assert.match(await page.text(), /<div id="root">/);
  // serve refuses to start unless the pinned core exists and verifies.
  const state = await fetch(`http://127.0.0.1:${port}/api/state`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  assert.ok(state.ok, `GET /api/state answered ${state.status}`);
  assert.match(await state.text(), /"coreVersion":/);
  server.kill("SIGTERM");
  assert.deepEqual(await exit, { code: 0, signal: null }, "serve stopped");
  server = undefined;
  assert.equal(status(), notRunningExit, "status of a stopped server");
  // A non-TTY log (journald) must never contain the session token.
  const output = Buffer.concat(log).toString();
  assert.doesNotMatch(output, /session=/);
  assert.ok(!output.includes(token), "the log contains the session token");
  console.log(`Server archive ${name} installed, served and stopped.`);
} finally {
  if (server) {
    server.kill("SIGTERM");
    await exit;
  }
  await rm(work, { recursive: true, force: true });
}
