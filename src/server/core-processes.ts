import { execFile, spawn, type ChildProcess } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { readFile, readdir, rm } from "node:fs/promises";
import { Socket } from "node:net";
import { join } from "node:path";
import { promisify } from "node:util";
import { setTimeout as delay } from "node:timers/promises";
import { z } from "zod";

// Cores must not outlive the process that started them. Layer 1: a shell watchdog
// per core blocks reading a pipe only this process can write, so the kernel wakes
// it when this process ends in any way (crash, SIGKILL). Layer 2: a ledger lets the
// next instance on the same data directory reap cores whose owner is provably dead.
// Both identify a process by PID plus start time and full command line.

const run = promisify(execFile);
const psEnvironment = {
  PATH: "/usr/bin:/bin:/usr/sbin:/sbin",
  LC_ALL: "C",
  TZ: "UTC",
};

/**
 * `ps` start time and full command line, or undefined if no such process (or no
 * `ps`, which makes nothing provable). Other failures throw rather than read as "gone".
 */
export async function processIdentity(
  pid: number,
): Promise<string | undefined> {
  try {
    const { stdout } = await run(
      "ps",
      ["-ww", "-o", "lstart=", "-o", "command=", "-p", String(pid)],
      { env: psEnvironment },
    );
    return stdout.trim() || undefined;
  } catch (error) {
    const code = error instanceof Error && "code" in error ? error.code : "";
    if (code === 1 || code === "ENOENT") return undefined;
    throw error;
  }
}
/** The command part of an identity; `lstart` is fixed-width in the C locale. */
function identityCommand(identity: string): string {
  return identity.slice(24).trimStart();
}

// $1 core PID, $2 its exact command line. Captures the core's identity, then sleeps in
// read until EOF: the write end lives only in the owning process (pipes are close-on-exec).
// The owner SIGKILLs this watchdog as soon as the core exits, so EOF means the owner died.
// Then: SIGTERM, up to 5 s for a graceful exit (checked only during this shutdown), SIGKILL.
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
while [ $i -lt 50 ]; do
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

let ownerIdentity: Promise<string | undefined> | undefined;
function ledgerDirectory(directory: string): string {
  return join(directory, "core-processes");
}

/** Records a running core so a later instance can reap it if this process dies. Returns the entry path. */
export async function recordCore(
  directory: string,
  core: ChildProcess,
): Promise<string | undefined> {
  const pid = core.pid;
  if (pid === undefined) return undefined;
  ownerIdentity ??= processIdentity(process.pid);
  const [identity, owner] = await Promise.all([
    processIdentity(pid),
    ownerIdentity,
  ]);
  // Exit can win the race; an entry for an exited core would only be dropped later.
  if (!identity || !owner || core.exitCode !== null || core.signalCode !== null)
    return undefined;
  const ledger = ledgerDirectory(directory);
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
export async function reapOrphanedCores(directory: string): Promise<void> {
  const ledger = ledgerDirectory(directory);
  let names: string[];
  try {
    names = await readdir(ledger);
  } catch {
    return;
  }
  // Only cores started with a profile config inside this data directory.
  const prefix = ` --config ${join(directory, "profiles")}/`;
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
      if ((await processIdentity(entry.owner.pid)) === entry.owner.identity)
        continue; // Its owner still runs and manages it.
      const command = identityCommand(entry.identity);
      const at = command.lastIndexOf(prefix);
      const ours =
        name === `${entry.pid}.json` &&
        at > 0 &&
        /^[0-9a-f-]{36}\/config\.yaml$/.test(command.slice(at + prefix.length));
      if (ours && (await processIdentity(entry.pid)) === entry.identity)
        await stopOrphan(entry);
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
async function stopOrphan(entry: Entry): Promise<void> {
  signal(entry.pid, "SIGTERM");
  // Not our child, so exit cannot be awaited; this bounded wait runs only while reaping.
  for (let waited = 0; waited < 5000; waited += 100) {
    await delay(100);
    if ((await processIdentity(entry.pid)) !== entry.identity) return;
  }
  if ((await processIdentity(entry.pid)) === entry.identity)
    signal(entry.pid, "SIGKILL");
}
