import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { chmod, lstat, mkdtemp, rm, symlink } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { readSession, startServer, stopServer } from "../src/server/cli.ts";

const binary = resolve(".vendor/core/cli-proxy-api");
const clientDirectory = resolve("dist/client");
const cli = resolve("src/server/cli.ts");
async function launch(directory: string) {
  const listener = createServer();
  await new Promise<void>((done) => listener.listen(0, "127.0.0.1", done));
  const address = listener.address();
  assert.ok(address instanceof Object);
  const port = address.port;
  assert.notEqual(port, 4318);
  await new Promise<void>((done) => listener.close(() => done()));
  const child = spawn(
    process.execPath,
    [cli, "serve", "--port", String(port)],
    {
      env: { ...process.env, NONSTOPVIBIN_DATA_DIR: directory },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let output = "";
  child.stdout.setEncoding("utf8").on("data", (text: string) => {
    output += text;
  });
  child.stderr.setEncoding("utf8").on("data", (text: string) => {
    output += text;
  });
  const exited = new Promise<number | null>((done, reject) => {
    child.once("error", reject);
    child.once("exit", (code) => done(code));
  });
  return { child, port, exited, output: () => output };
}
type RunningCli = Awaited<ReturnType<typeof launch>>;
async function waitFor(check: () => boolean | Promise<boolean>) {
  const deadline = Date.now() + 15000;
  while (!(await check())) {
    assert.ok(Date.now() < deadline, "lifecycle condition timed out");
    await delay(10);
  }
}
function corePids(directory: string): number[] {
  return execFileSync("ps", ["-axo", "pid=,command="], { encoding: "utf8" })
    .split("\n")
    .filter(
      (row) =>
        row.includes(join(directory, "profiles")) &&
        row.includes("cli-proxy-api"),
    )
    .map((row) => Number(row.trim().split(/\s+/, 1)[0]));
}
function signal(pid: number, name: NodeJS.Signals) {
  try {
    process.kill(pid, name);
  } catch (error) {
    if (!(error instanceof Error && "code" in error && error.code === "ESRCH"))
      throw error;
  }
}
async function cleanup(directory: string, processes: RunningCli[]) {
  // Cleanup is after assertions, so it cannot turn orphaning into a test pass.
  for (const pid of corePids(directory)) signal(pid, "SIGCONT");
  for (const proc of processes) {
    if (proc.child.exitCode === null && proc.child.signalCode === null)
      proc.child.kill("SIGTERM");
    const force = setTimeout(() => proc.child.kill("SIGKILL"), 10000);
    try {
      await proc.exited;
    } finally {
      clearTimeout(force);
    }
  }
  for (const pid of corePids(directory)) signal(pid, "SIGKILL");
  await rm(directory, { recursive: true, force: true });
}

test(
  "different-port CLI starts cannot share a directory during restore or shutdown",
  { timeout: 30000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-cli-owner-"));
    const store = new Store(directory, fileKeyCodec(directory));
    for (let i = 0; i < 3; i++) {
      const profile = store.createProfile(`Lifecycle ${i}`, "forest");
      store.saveProfile({ ...profile, enabled: true });
    }
    store.close();
    const processes: RunningCli[] = [];
    try {
      const owner = await launch(directory);
      processes.push(owner);
      await waitFor(() => corePids(directory).length > 0);
      // Suspend the real pinned core to hold initialization open deterministically.
      for (const pid of corePids(directory)) signal(pid, "SIGSTOP");
      assert.equal(await readSession(directory), undefined);
      const contender = await launch(directory);
      processes.push(contender);
      assert.notEqual(contender.port, owner.port);
      await waitFor(
        async () =>
          contender.child.exitCode !== null ||
          (await readSession(directory)) !== undefined,
      );
      assert.equal(contender.child.exitCode, 1, contender.output());
      assert.equal(await contender.exited, 1);
      assert.match(contender.output(), /already starting, running or stopping/);
      assert.equal(await readSession(directory), undefined);
      owner.child.kill("SIGTERM");
      owner.child.kill("SIGINT");
      const duringStop = await launch(directory);
      processes.push(duringStop);
      await waitFor(() => duringStop.child.exitCode !== null);
      assert.equal(await duringStop.exited, 1, duringStop.output());
      assert.match(
        duringStop.output(),
        /already starting, running or stopping/,
      );
      assert.equal(await owner.exited, 0, owner.output());
      assert.deepEqual(
        corePids(directory),
        [],
        "no core may survive startup SIGTERM",
      );
      assert.equal(await readSession(directory), undefined);
      // A new owner can now restore all profiles, proving release happened only
      // after cleanup, and not merely because discovery had not been published.
      const successor = await launch(directory);
      processes.push(successor);
      await waitFor(
        async () => (await readSession(directory))?.pid === successor.child.pid,
      );
      successor.child.kill("SIGTERM");
      assert.equal(await successor.exited, 0, successor.output());
      assert.deepEqual(corePids(directory), []);
    } finally {
      await cleanup(directory, processes);
    }
  },
);

test(
  "a crashed CLI releases the persistent reservation without replacing its inode",
  { timeout: 15000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-cli-crash-"));
    const processes: RunningCli[] = [];
    try {
      const owner = await launch(directory);
      processes.push(owner);
      await waitFor(
        async () => (await readSession(directory))?.pid === owner.child.pid,
      );
      const reservation = join(directory, "server.lock.sqlite");
      const before = await lstat(reservation);
      assert.equal(before.mode & 0o777, 0o600);
      owner.child.kill("SIGKILL");
      await owner.exited;
      const successor = await launch(directory);
      processes.push(successor);
      await waitFor(
        async () => (await readSession(directory))?.pid === successor.child.pid,
      );
      assert.equal((await lstat(reservation)).ino, before.ino);
      successor.child.kill("SIGTERM");
      assert.equal(await successor.exited, 0, successor.output());
      assert.equal(await readSession(directory), undefined);
    } finally {
      await cleanup(directory, processes);
    }
  },
);

test(
  "failed startup and simultaneous normal closes release ownership securely",
  { timeout: 20000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-cli-release-"));
    const options = {
      directory,
      binary,
      clientDirectory,
      port: 0,
      allowHosts: [],
    };
    try {
      await assert.rejects(
        startServer({ ...options, binary: join(directory, "missing") }),
        /proxy core is missing/,
      );
      await chmod(directory, 0o755);
      const app = await startServer(options);
      assert.equal((await lstat(directory)).mode & 0o777, 0o700);
      const failedDirectory = join(directory, "bind-failure");
      await assert.rejects(
        startServer({ ...options, directory: failedDirectory, port: app.port }),
        /already in use/,
      );
      const recovered = await startServer({
        ...options,
        directory: failedDirectory,
      });
      await stopServer(recovered, failedDirectory);
      const profile = app.store.createProfile("Concurrent close", "forest");
      await app.core.start(profile.id);
      const pid = app.core.runtimes.get(profile.id)?.child.pid;
      assert.ok(pid);
      signal(pid, "SIGSTOP");
      const closing = Promise.all([
        stopServer(app, directory),
        stopServer(app, directory),
      ]);
      let contender: RunningCli | undefined;
      try {
        const peer = await launch(directory);
        contender = peer;
        await waitFor(
          async () =>
            peer.child.exitCode !== null ||
            (await readSession(directory))?.pid === peer.child.pid,
        );
        assert.equal(contender.child.exitCode, 1, contender.output());
        assert.match(
          contender.output(),
          /already starting, running or stopping/,
        );
      } finally {
        signal(pid, "SIGCONT");
        await closing;
        if (contender) {
          contender.child.kill("SIGTERM");
          await contender.exited;
        }
      }
      assert.deepEqual(corePids(directory), []);
      const replacement = await startServer(options);
      await stopServer(replacement, directory);
      const reservation = join(directory, "server.lock.sqlite");
      await chmod(reservation, 0o644);
      await assert.rejects(startServer(options), /owner-only ordinary file/);
      assert.equal(
        (await lstat(reservation)).mode & 0o777,
        0o644,
        "do not silently change permissions",
      );
      await rm(reservation);
      const target = join(directory, "unexpected");
      await symlink(target, reservation);
      await assert.rejects(startServer(options), /without links/);
      assert.equal((await lstat(reservation)).isSymbolicLink(), true);
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  },
);
