import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import {
  chmod,
  lstat,
  mkdtemp,
  readdir,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { DatabaseSync } from "node:sqlite";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import {
  coreLedgerDirectory,
  profileDirectory,
  serverLockPath,
} from "../src/server/data-directory.ts";
import { probe, readSession, startServer } from "../src/server/cli.ts";
import { clientDirectory, coreBinary } from "./support/application.ts";
import { unusedPort } from "./support/ports.ts";

const binary = coreBinary;
const cli = resolve("src/server/cli.ts");
async function launch(directory: string) {
  const port = await unusedPort();
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
const ended = (cli: RunningCli) =>
  cli.child.exitCode !== null || cli.child.signalCode !== null;
const report = (clis: RunningCli[]) =>
  clis
    .map(
      (cli) =>
        `CLI on port ${cli.port} (pid ${cli.child.pid}): ${ended(cli) ? `exited with ${cli.child.exitCode ?? cli.child.signalCode}` : "running"}\n${cli.output() || "(no output)"}`,
    )
    .join("\n");
/**
 * Waits until `check` holds. A timeout reports the output and exit status of `clis`, the CLIs
 * the wait concerns; unless the check is itself waiting for exits, one that exits fails it at once.
 */
async function waitFor(
  what: string,
  check: () => boolean | Promise<boolean>,
  clis: RunningCli[],
  { exits = false } = {},
) {
  const deadline = Date.now() + 15000;
  const fail = (message: string) => {
    // Printed now: a cleanup that outlasts the test's limit would replace the failure.
    console.error(message);
    assert.fail(message);
  };
  while (!(await check())) {
    if (!exits && clis.some(ended))
      fail(`A CLI exited while waiting for ${what}.\n${report(clis)}`);
    if (Date.now() > deadline)
      fail(`Timed out waiting for ${what}.\n${report(clis)}`);
    await delay(10);
  }
}
function corePids(directory: string): number[] {
  return execFileSync("ps", ["-axo", "pid=,command="], { encoding: "utf8" })
    .split("\n")
    .filter(
      (row) =>
        row.includes(profileDirectory(directory, "")) &&
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
  "simultaneous initial CLI starts elect one owner despite a shared lock reader",
  { timeout: 20000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-cli-initial-race-"));
    const path = serverLockPath(directory);
    await writeFile(path, "", { mode: 0o600 });
    const inode = (await lstat(path)).ino;
    const reader = new DatabaseSync(path);
    // Hold the SHARED phase of a competing connection open. EXCLUSIVE upgrades
    // reject both starters; a unique RESERVED writer must coexist with readers.
    reader.exec("BEGIN; SELECT name FROM sqlite_master");
    const processes: RunningCli[] = [];
    try {
      processes.push(
        ...(await Promise.all([launch(directory), launch(directory)])),
      );
      assert.notEqual(processes[0].port, processes[1].port);
      await waitFor(
        "one owner and one exited contender",
        async () =>
          processes.every((p) => p.child.exitCode !== null) ||
          ((await readSession(directory)) !== undefined &&
            processes.some((p) => p.child.exitCode !== null)),
        processes,
        { exits: true },
      );
      const owners = processes.filter((p) => p.child.exitCode === null);
      assert.equal(
        owners.length,
        1,
        processes.map((p) => p.output()).join("\n"),
      );
      const loser = processes.find((p) => p.child.exitCode !== null);
      assert.ok(loser);
      assert.equal(await loser.exited, 1, loser.output());
      assert.match(
        loser.output(),
        /already (?:running|starting, running or stopping)/,
      );
      const session = await readSession(directory);
      assert.ok(session);
      assert.equal(session.pid, owners[0].child.pid);
      assert.ok(
        await probe(session),
        "elected owner is authenticated and live",
      );
      assert.equal((await lstat(path)).ino, inode);
      owners[0].child.kill("SIGTERM");
      assert.equal(await owners[0].exited, 0);
      assert.equal(await readSession(directory), undefined);
    } finally {
      reader.close();
      await cleanup(directory, processes);
    }
  },
);

test(
  "different-port CLI starts cannot share a directory during restore or shutdown",
  { timeout: 30000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-cli-owner-"));
    const store = new Store(directory, fileKeyCodec(directory));
    for (let i = 0; i < 3; i++) {
      const profile = store.createProfile(`Lifecycle ${i}`);
      store.saveProfile({ ...profile, enabled: true });
    }
    store.close();
    const processes: RunningCli[] = [];
    try {
      const owner = await launch(directory);
      processes.push(owner);
      await waitFor("a core", () => corePids(directory).length > 0, [owner]);
      // Suspend the real pinned core to hold initialization open deterministically.
      for (const pid of corePids(directory)) signal(pid, "SIGSTOP");
      assert.equal(await readSession(directory), undefined);
      const contender = await launch(directory);
      processes.push(contender);
      assert.notEqual(contender.port, owner.port);
      await waitFor(
        "the contender to exit",
        async () =>
          contender.child.exitCode !== null ||
          (await readSession(directory)) !== undefined,
        [contender, owner],
        { exits: true },
      );
      assert.equal(contender.child.exitCode, 1, contender.output());
      assert.equal(await contender.exited, 1);
      assert.match(contender.output(), /already starting, running or stopping/);
      assert.equal(await readSession(directory), undefined);
      owner.child.kill("SIGTERM");
      owner.child.kill("SIGINT");
      const duringStop = await launch(directory);
      processes.push(duringStop);
      await waitFor(
        "the contender during stop to exit",
        () => duringStop.child.exitCode !== null,
        [duringStop, owner],
        { exits: true },
      );
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
        "the successor's session",
        async () => (await readSession(directory))?.pid === successor.child.pid,
        [successor],
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
        "the owner's session",
        async () => (await readSession(directory))?.pid === owner.child.pid,
        [owner],
      );
      const reservation = serverLockPath(directory);
      const before = await lstat(reservation);
      assert.equal(before.mode & 0o777, 0o600);
      owner.child.kill("SIGKILL");
      await owner.exited;
      const successor = await launch(directory);
      processes.push(successor);
      await waitFor(
        "the successor's session",
        async () => (await readSession(directory))?.pid === successor.child.pid,
        [successor],
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
  "a hangup stops a CLI with a running profile through its orderly close",
  { timeout: 20000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-cli-hangup-"));
    const store = new Store(directory, fileKeyCodec(directory));
    const profile = store.createProfile("Hangup");
    store.saveProfile({ ...profile, enabled: true });
    store.close();
    const processes: RunningCli[] = [];
    try {
      const owner = await launch(directory);
      processes.push(owner);
      await waitFor(
        "the owner's session and its core",
        async () =>
          (await readSession(directory))?.pid === owner.child.pid &&
          corePids(directory).length > 0,
        [owner],
      );
      // A closed SSH terminal sends SIGHUP; it must not skip the accounting drain.
      owner.child.kill("SIGHUP");
      assert.equal(await owner.exited, 0, owner.output());
      assert.deepEqual(corePids(directory), [], "no core or watchdog remains");
      assert.deepEqual(await readdir(coreLedgerDirectory(directory)), []);
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
      await recovered.close();
      const profile = app.store.createProfile("Concurrent close");
      await app.cores.pool.start(profile.id);
      const pid = app.cores.pool.runtimes.get(profile.id)?.child.pid;
      assert.ok(pid);
      signal(pid, "SIGSTOP");
      const closing = Promise.all([app.close(), app.close()]);
      let contender: RunningCli | undefined;
      try {
        const peer = await launch(directory);
        contender = peer;
        await waitFor(
          "the contender to exit",
          async () =>
            peer.child.exitCode !== null ||
            (await readSession(directory))?.pid === peer.child.pid,
          [peer],
          { exits: true },
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
      await replacement.close();
      const reservation = serverLockPath(directory);
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
