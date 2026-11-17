import { execFile, spawn, type ChildProcess } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { readFile, readdir, rm } from "node:fs/promises";
import { Socket } from "node:net";
import { join } from "node:path";
import { promisify } from "node:util";
import { setTimeout as delay } from "node:timers/promises";
import { z } from "zod";
import { coreArguments, coreLedgerDirectory } from "./data-directory.ts";

// Cores must not outlive the process that started them. Layer 1: a shell watchdog
// per core blocks reading a pipe only this process can write, so the kernel wakes
// it when this process ends in any way (crash, SIGKILL). Layer 2: a ledger lets the
// next instance on the same data directory reap cores whose owner is provably dead.
// Both identify a process by PID plus start time and full command line.

/** How long a core gets to exit after SIGTERM before SIGKILL, wherever it is stopped. */
export const killGraceMs = 5000;
const run = promisify(execFile);
const psEnvironment = {
  PATH: "/usr/bin:/bin:/usr/sbin:/sbin",
  LC_ALL: "C",
  TZ: "UTC",
};

/**
 * `ps` start time and full command line, or undefined if `ps` reports no such process.
 * Every other failure throws, including `ps` not running at all (ENOENT, also how Bun
 * reports descriptor exhaustion), so nothing unprovable reads as "gone". `ps` is for tests.
 */
export async function processIdentity(
  pid: number,
  ps = "ps",
): Promise<string | undefined> {
  try {
    const { stdout } = await run(
      ps,
      ["-ww", "-o", "lstart=", "-o", "command=", "-p", String(pid)],
      { env: psEnvironment },
    );
    return stdout.trim() || undefined;
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === 1)
      return undefined;
    throw error;
  }
}
/** Identity lookup used by the ledger; injectable so tests can make `ps` fail. */
export type Identify = (pid: number) => Promise<string | undefined>;
/** The command part of an identity; `lstart` is fixed-width in the C locale. */
function identityCommand(identity: string): string {
  return identity.slice(24).trimStart();
}
/** Whether an identity is a core started with a profile config inside `directory`. */
function coreOf(directory: string, identity: string): boolean {
  const command = identityCommand(identity);
  // The arguments core.ts passes, split around the profile ID (no path holds NUL).
  const [prefix = "", suffix = ""] =
    ` ${coreArguments(directory, "\0").join(" ")}`.split("\0");
  const at = command.lastIndexOf(prefix);
  return (
    at > 0 &&
    command.endsWith(suffix) &&
    /^[0-9a-f-]{36}$/.test(
      command.slice(at + prefix.length, command.length - suffix.length),
    )
  );
}

// $1 core PID, $2 its exact command line. Captures the core's identity, then sleeps in
// read until EOF: the write end lives only in the owning process (pipes are close-on-exec).
// The owner SIGKILLs this watchdog as soon as the core exits, so EOF means the owner died.
// Then: SIGTERM, killGraceMs for a graceful exit (checked only during this shutdown), SIGKILL.
const watchdogScript = `trap '' HUP INT
export PATH=${psEnvironment.PATH} LC_ALL=C TZ=UTC
identity() { ps -ww -o lstart= -o command= -p "$1" 2>/dev/null; }
held=$(identity "$1")
command=\${held#????????????????????????}
command=\${command#"\${command%%[! ]*}"}
[ -n "$held" ] && [ "$command" = "$2" ] || exit 0
while read -r _; do :; done
[ "$(identity "$1")" = "$held" ] || exit 0
kill -TERM "$1" 2>/dev/null
i=0
while [ $i -lt ${killGraceMs / 100} ]; do
  sleep 0.1
  [ "$(identity "$1")" = "$held" ] || exit 0
  i=$((i + 1))
done
kill -KILL "$1" 2>/dev/null
exit 0`;

/** Starts the layer-1 watchdog for a freshly spawned core. */
export function watchCore(core: ChildProcess): ChildProcess | undefined {
  if (core.pid === undefined || process.platform === "win32") return undefined;
  const watchdog = spawn(
    "/bin/sh",
    [
      "-c",
      watchdogScript,
      "nonstopvibin-core-watchdog",
      String(core.pid),
      core.spawnargs.join(" "),
    ],
    // Its own session: terminal and process-group signals aimed at the app must not take it first.
    { stdio: ["pipe", "ignore", "ignore"], detached: true },
  );
  watchdog.on("error", () => {}); // Without /bin/sh only layer 2 remains.
  watchdog.stdin?.on("error", () => {});
  // Neither the watchdog nor its pipe may keep this process alive; exit is its cue.
  watchdog.unref();
  if (watchdog.stdin instanceof Socket) watchdog.stdin.unref();
  return watchdog;
}

const entrySchema = z
  .object({
    pid: z.number().int().positive(),
    identity: z.string().min(25),
    owner: z.object({
      pid: z.number().int().positive(),
      identity: z.string().min(25),
    }),
  })
  .strict();
type Entry = z.infer<typeof entrySchema>;

// Per lookup, so an injected one never shares this process's cached identity.
const ownerIdentities = new WeakMap<Identify, Promise<string | undefined>>();

/** Records a running core so a later instance can reap it if this process dies. Returns the entry path. */
export async function recordCore(
  directory: string,
  core: ChildProcess,
  identify: Identify = processIdentity,
): Promise<string | undefined> {
  const pid = core.pid;
  if (pid === undefined) return undefined;
  let ownerIdentity = ownerIdentities.get(identify);
  if (!ownerIdentity) {
    ownerIdentity = identify(process.pid);
    ownerIdentities.set(identify, ownerIdentity);
    // Only a determined identity is cached; a failed or empty lookup is retried by the next record.
    const forget = () => ownerIdentities.delete(identify);
    ownerIdentity.then((owner) => owner || forget(), forget);
  }
  const [identity, owner] = await Promise.all([identify(pid), ownerIdentity]);
  // Exit can win the race; an entry for an exited core would only be dropped later.
  // A read before the core's exec settles (spawn can return first, e.g. Bun under
  // qemu-user) shows another command line that could never be reaped: record nothing.
  if (
    !identity ||
    !owner ||
    !coreOf(directory, identity) ||
    core.exitCode !== null ||
    core.signalCode !== null
  )
    return undefined;
  const ledger = coreLedgerDirectory(directory);
  mkdirSync(ledger, { recursive: true, mode: 0o700 });
  const path = join(ledger, `${pid}.json`);
  const entry: Entry = {
    pid,
    identity,
    owner: { pid: process.pid, identity: owner },
  };
  // Synchronous after the last await, so the exit handler cannot interleave.
  writeFileSync(path, JSON.stringify(entry), { mode: 0o600 });
  return path;
}
export function forgetCore(path: string | undefined): void {
  if (path) rmSync(path, { force: true });
}

/**
 * Stops cores recorded under this data directory whose owner has exited. Live
 * owners, other directories and reused PIDs are never signalled; stale entries go.
 */
export async function reapOrphanedCores(
  directory: string,
  identify: Identify = processIdentity,
): Promise<void> {
  const ledger = coreLedgerDirectory(directory);
  let names: string[];
  try {
    names = await readdir(ledger);
  } catch {
    return;
  }
  for (const name of names) {
    const path = join(ledger, name);
    let entry: Entry;
    try {
      entry = entrySchema.parse(JSON.parse(await readFile(path, "utf8")));
    } catch {
      await rm(path, { force: true });
      continue;
    }
    try {
      // Its owner still runs and manages it.
      if ((await identify(entry.owner.pid)) === entry.owner.identity) continue;
      // Only cores started with a profile config inside this data directory.
      const ours =
        name === `${entry.pid}.json` && coreOf(directory, entry.identity);
      if (ours && (await identify(entry.pid)) === entry.identity)
        await stopOrphan(entry, identify);
      await rm(path, { force: true });
    } catch {
      // Unprovable this time (e.g. ps could not run): keep the entry for the next start.
    }
  }
}
function signal(pid: number, name: NodeJS.Signals): void {
  try {
    process.kill(pid, name);
  } catch {
    // It exited between the identity check and the signal.
  }
}
async function stopOrphan(entry: Entry, identify: Identify): Promise<void> {
  signal(entry.pid, "SIGTERM");
  // Not our child, so exit cannot be awaited; this bounded wait runs only while reaping.
  for (let waited = 0; waited < killGraceMs; waited += 100) {
    await delay(100);
    if ((await identify(entry.pid)) !== entry.identity) return;
  }
  if ((await identify(entry.pid)) === entry.identity)
    signal(entry.pid, "SIGKILL");
}
