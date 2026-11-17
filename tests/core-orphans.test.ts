import { test } from "node:test";
import assert from "node:assert/strict";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import { promisify } from "node:util";
import { once } from "node:events";
import {
  chmod,
  mkdir,
  mkdtemp,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { createInterface } from "node:readline";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { CorePool } from "../src/server/core.ts";
import { Cores } from "../src/server/cores.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import {
  processIdentity,
  reapOrphanedCores,
  recordCore,
  type Identify,
} from "../src/server/core-processes.ts";

const run = promisify(execFile);
const binary = resolve(".vendor/core/cli-proxy-api");
const source = (file: string) => JSON.stringify(resolve("src/server", file));

interface Tracked {
  pid: number;
  identity: string;
}
/** Every process a test starts, directly or through an owner, with its identity. */
class Processes {
  readonly tracked: Tracked[] = [];
  readonly children: ChildProcess[] = [];
  async track(pid: number | undefined): Promise<Tracked> {
    assert.ok(pid);
    // While macOS's /bin/sh re-executes the selected shell, ps cannot read its arguments and
    // prints `(sh)`: track the identity only once the full command line is readable.
    let identity = await processIdentity(pid);
    for (let waited = 0; identity && !readable(identity); waited += 10) {
      assert.ok(waited < 2000, `process ${pid} has no readable command line`);
      await delay(10);
      identity = await processIdentity(pid);
    }
    assert.ok(identity, `process ${pid} is running`);
    const entry = { pid, identity };
    this.tracked.push(entry);
    return entry;
  }
  // Signals only processes this test started: tracked ones that still carry the same
  // identity, then any process whose command line names this test's fresh temp root.
  async cleanup(root: string): Promise<void> {
    const kill = (pid: number) => {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        // Already gone.
      }
    };
    for (const child of this.children)
      if (child.exitCode === null && child.signalCode === null)
        child.kill("SIGKILL");
    for (const { pid, identity } of this.tracked)
      if ((await processIdentity(pid)) === identity) kill(pid);
    const { stdout } = await run("ps", ["-ax", "-ww", "-o", "pid=,command="]);
    for (const line of stdout.split("\n")) {
      const [, pid, command] = /^\s*(\d+) (.*)$/.exec(line) ?? [];
      if (pid && command?.includes(root) && Number(pid) !== process.pid)
        kill(Number(pid));
    }
  }
}
/** False for `(comm)` or `[comm]`: ps could not read the process's arguments. */
const readable = (identity: string) =>
  !/^.{24}\s+[([][^ ]*[)\]]$/.test(identity);
async function alive(entry: Tracked): Promise<boolean> {
  return (await processIdentity(entry.pid)) === entry.identity;
}
async function gone(entry: Tracked, within = 5000): Promise<boolean> {
  // Not our child, so its exit cannot be awaited: watch its identity.
  for (const started = Date.now(); Date.now() - started < within;) {
    if (!(await alive(entry))) return true;
    await delay(50);
  }
  return false;
}
/** A separate owning process (an "instance") that starts `count` cores on `data`. */
async function owner(
  root: string,
  data: string,
  count: number,
  processes: Processes,
) {
  const script = join(root, `owner-${processes.children.length}.ts`);
  await writeFile(
    script,
    `import { CorePool } from ${source("core.ts")};
import { Store } from ${source("store.ts")};
import { fileKeyCodec } from ${source("vault.ts")};
const store = new Store(${JSON.stringify(data)}, fileKeyCodec(${JSON.stringify(data)}));
const pool = new CorePool(store, ${JSON.stringify(binary)});
const started = [];
for (let i = 0; i < ${count}; i++) {
  const profile = store.createProfile("Owner " + i);
  await pool.start(profile.id);
  const runtime = pool.runtimes.get(profile.id);
  started.push({ core: runtime.child.pid, watchdog: runtime.watchdog?.pid });
}
console.log(JSON.stringify(started));
setInterval(() => {}, 1 << 30);
`,
  );
  const child = spawn(process.execPath, [script], {
    stdio: ["ignore", "pipe", "inherit"],
  });
  processes.children.push(child);
  const lines = createInterface(child.stdout!);
  // SAFETY: "line" events carry exactly one string; the other branch throws.
  const [line] = (await Promise.race([
    once(lines, "line"),
    once(child, "exit").then(() => assert.fail("the owner exited early")),
  ])) as [string];
  lines.close();
  // SAFETY: the owner script above prints exactly this shape.
  const started = JSON.parse(line) as Array<{
    core: number;
    watchdog: number;
  }>;
  const self = await processes.track(child.pid);
  const cores: Tracked[] = [];
  const watchdogs: Tracked[] = [];
  for (const entry of started) {
    cores.push(await processes.track(entry.core));
    watchdogs.push(await processes.track(entry.watchdog));
  }
  return { child, self, cores, watchdogs };
}
async function fixture() {
  const root = await mkdtemp(join(tmpdir(), "nv-orphans-"));
  const data = join(root, "data");
  await mkdir(data, { mode: 0o700 });
  return { root, data };
}
const ledger = (data: string) => join(data, "core-processes");

test(
  "cores exit on their own when the process that started them is SIGKILLed",
  { timeout: 20_000 },
  async () => {
    const { root, data } = await fixture();
    const processes = new Processes();
    try {
      const instance = await owner(root, data, 2, processes);
      assert.equal((await readdir(ledger(data))).length, 2);
      instance.child.kill("SIGKILL");
      await once(instance.child, "exit");
      for (const core of instance.cores)
        assert.ok(
          await gone(core),
          "an orphaned core is terminated by its watchdog",
        );
      for (const watchdog of instance.watchdogs)
        assert.ok(await gone(watchdog), "the watchdog exits after its core");
    } finally {
      await processes.cleanup(root);
      await rm(root, { recursive: true, force: true });
    }
  },
);

test(
  "a new instance reaps cores of a dead instance and refuses entries whose PID is now another process",
  { timeout: 30_000 },
  async () => {
    const { root, data } = await fixture();
    const processes = new Processes();
    let store: Store | undefined;
    let cores: Cores | undefined;
    try {
      const instance = await owner(root, data, 1, processes);
      // Defeat layer 1 so the core really outlives its owner.
      process.kill(instance.watchdogs[0]!.pid, "SIGKILL");
      assert.ok(await gone(instance.watchdogs[0]!));
      instance.child.kill("SIGKILL");
      await once(instance.child, "exit");
      const orphan = instance.cores[0]!;
      await delay(300);
      assert.ok(await alive(orphan), "without its watchdog the core survives");

      // Entries from the dead owner whose PIDs now belong to unrelated processes.
      const bystanders: Tracked[] = [];
      for (let i = 0; i < 2; i++) {
        const bystander = spawn("/bin/sleep", ["60"], { stdio: "ignore" });
        processes.children.push(bystander);
        bystanders.push(await processes.track(bystander.pid));
      }
      const profileConfig = join(
        data,
        "profiles",
        "00000000-0000-4000-8000-000000000000",
        "config.yaml",
      );
      const decoys = [
        // A core of this directory once had this PID; another process has it now.
        `Thu Jan  1 00:00:00 1970     ${binary} --config ${profileConfig}`,
        // Exactly this process's identity, but it is not a core of this directory.
        bystanders[1]!.identity,
      ];
      for (const [i, identity] of decoys.entries())
        await writeFile(
          join(ledger(data), `${bystanders[i]!.pid}.json`),
          JSON.stringify({
            pid: bystanders[i]!.pid,
            identity,
            owner: instance.self,
          }),
          { mode: 0o600 },
        );
      store = new Store(data, fileKeyCodec(data));
      cores = new Cores(store, binary, { report: () => {} });
      await cores.restore();
      assert.ok(
        !(await alive(orphan)),
        "the orphan is stopped before restore returns",
      );
      const restarted = [...cores.pool.runtimes.values()];
      assert.equal(restarted.length, 1, "the enabled profile starts again");
      assert.notEqual(restarted[0]!.child.pid, orphan.pid);
      assert.deepEqual(
        await readdir(ledger(data)),
        [`${restarted[0]!.child.pid}.json`],
        "only the new core is recorded",
      );
      for (const bystander of bystanders)
        assert.ok(await alive(bystander), "a reused PID is never signalled");
    } finally {
      await cores?.shutdown();
      store?.close();
      await processes.cleanup(root);
      await rm(root, { recursive: true, force: true });
    }
  },
);

test(
  "reaping never touches cores of a live instance or of another data directory",
  { timeout: 20_000 },
  async () => {
    const { root, data } = await fixture();
    const other = join(root, "other");
    await mkdir(join(other, "core-processes"), {
      recursive: true,
      mode: 0o700,
    });
    const processes = new Processes();
    try {
      const instance = await owner(root, data, 1, processes);
      const core = instance.cores[0]!;
      const entries = await readdir(ledger(data));
      await reapOrphanedCores(data);
      assert.ok(await alive(core), "a live owner keeps its core");
      assert.deepEqual(await readdir(ledger(data)), entries);
      // Another directory's ledger naming this exact core, claimed by a dead owner.
      await writeFile(
        join(other, "core-processes", `${core.pid}.json`),
        JSON.stringify({
          pid: core.pid,
          identity: core.identity,
          owner: { pid: 1, identity: "Thu Jan  1 00:00:00 1970     gone" },
        }),
        { mode: 0o600 },
      );
      await reapOrphanedCores(other);
      assert.ok(
        await alive(core),
        "a core of another data directory is left alone",
      );
      assert.deepEqual(await readdir(join(other, "core-processes")), []);
    } finally {
      await processes.cleanup(root);
      await rm(root, { recursive: true, force: true });
    }
  },
);

/** A synthetic identity: `lstart` width, then a command. */
const fakeIdentity = (pid: number) =>
  `Thu Jan  1 00:00:00 1970     process ${pid}`;
const coreConfig = (data: string) =>
  join(data, "profiles", "00000000-0000-4000-8000-000000000000", "config.yaml");
const coreIdentity = (data: string) =>
  `Thu Jan  1 00:00:00 1970     core --config ${coreConfig(data)}`;
/** The real lookup, but `ps` cannot be executed (as under descriptor exhaustion in Bun). */
const psUnavailable = (root: string) => (pid: number) =>
  processIdentity(pid, join(root, "missing-ps"));

test("a failed owner identity lookup is retried by the next record", async () => {
  const { root, data } = await fixture();
  const processes = new Processes();
  try {
    const core = spawn("/bin/sleep", ["60"], { stdio: "ignore" });
    processes.children.push(core);
    let failures = 1;
    const identify: Identify = async (pid) => {
      if (pid !== process.pid) return coreIdentity(data);
      if (failures-- > 0)
        throw Object.assign(new Error("spawn ps EMFILE"), { code: "EMFILE" });
      return fakeIdentity(pid);
    };
    await assert.rejects(recordCore(data, core, identify), { code: "EMFILE" });
    assert.equal(
      await recordCore(data, core, identify),
      join(ledger(data), `${core.pid}.json`),
    );
    assert.deepEqual(await readdir(ledger(data)), [`${core.pid}.json`]);
  } finally {
    await processes.cleanup(root);
    await rm(root, { recursive: true, force: true });
  }
});

test("an owner identity lookup without ps writes nothing and is retried", async () => {
  const { root, data } = await fixture();
  const processes = new Processes();
  try {
    const core = spawn("/bin/sleep", ["60"], { stdio: "ignore" });
    processes.children.push(core);
    const unavailable = psUnavailable(root);
    let calls = 0;
    // Only the first owner lookup lacks ps; later owner lookups are real.
    const identify: Identify = async (pid) =>
      pid !== process.pid
        ? coreIdentity(data)
        : calls++ === 0
          ? unavailable(pid)
          : processIdentity(pid);
    await assert.rejects(recordCore(data, core, identify), { code: "ENOENT" });
    assert.deepEqual(await readdir(data), [], "no entry for that core");
    assert.equal(
      await recordCore(data, core, identify),
      join(ledger(data), `${core.pid}.json`),
    );
    assert.equal(calls, 2, "the owner was looked up again");
  } finally {
    await processes.cleanup(root);
    await rm(root, { recursive: true, force: true });
  }
});

test("a core identity read before its exec settles is not recorded", async () => {
  const { root, data } = await fixture();
  const processes = new Processes();
  try {
    const core = spawn("/bin/sleep", ["60"], { stdio: "ignore" });
    processes.children.push(core);
    // Until exec settles, ps can show the parent's command line (Bun under qemu-user).
    let settled = false;
    const identify: Identify = async (pid) =>
      pid === process.pid
        ? fakeIdentity(pid)
        : settled
          ? coreIdentity(data)
          : "Thu Jan  1 00:00:00 1970     bun server.ts";
    assert.equal(await recordCore(data, core, identify), undefined);
    assert.deepEqual(
      await readdir(data),
      [],
      "no entry that could never be reaped",
    );
    settled = true;
    assert.equal(
      await recordCore(data, core, identify),
      join(ledger(data), `${core.pid}.json`),
    );
  } finally {
    await processes.cleanup(root);
    await rm(root, { recursive: true, force: true });
  }
});

test(
  "reaping without ps keeps the entry and signals nothing",
  { timeout: 20_000 },
  async () => {
    const { root, data } = await fixture();
    const processes = new Processes();
    try {
      // A shell blocked in its builtin read, with the command line of a core of this directory.
      const child = spawn(
        "/bin/sh",
        ["-c", "read _", "core", "--config", coreConfig(data)],
        { stdio: ["pipe", "ignore", "ignore"] },
      );
      processes.children.push(child);
      const core = await processes.track(child.pid);
      await mkdir(ledger(data), { mode: 0o700 });
      const name = `${core.pid}.json`;
      await writeFile(
        join(ledger(data), name),
        JSON.stringify({
          ...core,
          owner: { pid: process.pid, identity: fakeIdentity(process.pid) },
        }),
        { mode: 0o600 },
      );
      await reapOrphanedCores(data, psUnavailable(root));
      assert.deepEqual(await readdir(ledger(data)), [name], "the entry stays");
      assert.ok(await alive(core), "nothing is signalled");
      // Control: with ps, the same entry is an orphan of a dead owner and is reaped.
      await reapOrphanedCores(data);
      assert.ok(!(await alive(core)));
      assert.deepEqual(await readdir(ledger(data)), []);
    } finally {
      await processes.cleanup(root);
      await rm(root, { recursive: true, force: true });
    }
  },
);

test(
  "normal start, stop and restart leave no watchdog, core or ledger entry",
  { timeout: 20_000 },
  async () => {
    const { root, data } = await fixture();
    const processes = new Processes();
    const store = new Store(data, fileKeyCodec(data));
    const pool = new CorePool(store, binary);
    try {
      const profile = store.createProfile("Lifecycle");
      for (const finish of [
        () => pool.stop(profile.id),
        () => pool.shutdown(),
      ]) {
        await pool.start(profile.id);
        const runtime = pool.runtimes.get(profile.id)!;
        const core = await processes.track(runtime.child.pid);
        const watchdog = await processes.track(runtime.watchdog?.pid);
        assert.deepEqual(await readdir(ledger(data)), [`${core.pid}.json`]);
        await finish();
        assert.equal(runtime.state, "stopped");
        assert.ok(!(await alive(core)));
        assert.ok(
          await gone(watchdog, 2000),
          "the watchdog leaves with its core",
        );
        assert.deepEqual(await readdir(ledger(data)), []);
      }
    } finally {
      await pool.shutdown();
      store.close();
      await processes.cleanup(root);
      await rm(root, { recursive: true, force: true });
    }
  },
);

test(
  "an undeletable pending sign-in does not keep restore from reaping the data directory's orphans",
  // Root may delete anything, so the cleanup would not fail.
  { timeout: 20_000, skip: process.getuid?.() === 0 },
  async () => {
    const { root, data } = await fixture();
    const processes = new Processes();
    const stuck = join(data, "oauth-pending", "claude-stuck");
    const store = new Store(data, fileKeyCodec(data));
    try {
      // A shell blocked in its builtin read, with the command line of a core of this directory.
      const child = spawn(
        "/bin/sh",
        ["-c", "read _", "core", "--config", coreConfig(data)],
        { stdio: ["pipe", "ignore", "ignore"] },
      );
      processes.children.push(child);
      const core = await processes.track(child.pid);
      await mkdir(ledger(data), { mode: 0o700 });
      await writeFile(
        join(ledger(data), `${core.pid}.json`),
        JSON.stringify({
          ...core,
          owner: { pid: process.pid, identity: fakeIdentity(process.pid) },
        }),
        { mode: 0o600 },
      );
      await mkdir(join(stuck, "entry"), { recursive: true });
      await chmod(stuck, 0o500);
      const cores = new Cores(store, binary, { report: () => {} });
      await assert.rejects(cores.restore());
      assert.ok(!(await alive(core)), "the orphan was reaped first");
    } finally {
      await chmod(stuck, 0o700).catch(() => undefined);
      store.close();
      await processes.cleanup(root);
      await rm(root, { recursive: true, force: true });
    }
  },
);
