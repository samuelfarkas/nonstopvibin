// `nonstopvibin`: the headless Linux server entry point.
import { parseArgs } from "node:util";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  closeSync,
  constants,
  existsSync,
  lstatSync,
  mkdirSync,
  openSync,
  realpathSync,
} from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { mkdir, open, readFile, rm } from "node:fs/promises";
import { homedir, userInfo } from "node:os";
import { dirname, join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { z } from "zod";
import { version } from "../../package.json";
import coreRelease from "../../scripts/core-release.json";
import { Application, validAllowHost } from "./server.ts";
import { dataDirectory } from "./data-directory.ts";
import { isMissing, writeAtomic } from "./native-config-files.ts";
import { errorMessage } from "./errors.ts";

// Replaced with `true` by scripts/build-server.mjs.
declare const NONSTOPVIBIN_PACKAGED: boolean;
const packaged = typeof NONSTOPVIBIN_PACKAGED !== "undefined";
// The packaged binary is reached through a ~/.local/bin symlink; its siblings
// live beside the real file.
const root = packaged
  ? dirname(realpathSync(process.execPath))
  : resolve(import.meta.dirname, "../..");
const coreBinary = packaged
  ? join(root, "core/cli-proxy-api")
  : join(root, ".vendor/core/cli-proxy-api");
const clientDirectory = packaged
  ? join(root, "client")
  : join(root, "dist/client");

export const DEFAULT_PORT = 4320;
const NOT_RUNNING = 3;
const UNIT = "nonstopvibin.service";
const SIGN_IN_PORTS = [1455, 54545]; // Codex, Claude

class CliError extends Error {
  readonly code: number;
  constructor(message: string, code = 1) {
    super(message);
    this.code = code;
  }
}

const sessionSchema = z.object({
  pid: z.number().int().positive(),
  port: z.number().int().min(1).max(65535),
  token: z.string().min(1),
  allowHosts: z.array(z.string().refine(validAllowHost)),
  version: z.string(),
  startedAt: z.string(),
});
export type Session = z.infer<typeof sessionSchema>;

export function sessionPath(directory: string): string {
  return join(directory, "server.json");
}
export async function readSession(
  directory: string,
): Promise<Session | undefined> {
  try {
    const file = await open(
      sessionPath(directory),
      constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK,
    );
    try {
      const status = await file.stat();
      if (
        !status.isFile() ||
        status.uid !== process.getuid?.() ||
        (status.mode & 0o077) !== 0 ||
        status.size > 65536
      )
        throw new CliError(
          "server.json must be an owner-only ordinary file you own (chmod 600).",
        );
      return sessionSchema.parse(JSON.parse(await file.readFile("utf8")));
    } finally {
      await file.close();
    }
  } catch (error) {
    if (isMissing(error) || error instanceof SyntaxError) return undefined;
    if (error instanceof z.ZodError) return undefined;
    throw error;
  }
}
// Removes the discovery file only while it still describes this process; a
// newer instance may already have replaced it.
export async function removeSession(
  directory: string,
  pid = process.pid,
): Promise<void> {
  if ((await readSession(directory))?.pid === pid)
    await rm(sessionPath(directory), { force: true });
}

export interface ServerState {
  version: string;
  coreVersion: string;
}
const stateSchema = z.object({ version: z.string(), coreVersion: z.string() });
// Authenticated liveness: a stale file or a recycled PID cannot pass.
export async function probe(session: Session): Promise<ServerState | null> {
  try {
    const response = await fetch(`http://127.0.0.1:${session.port}/api/state`, {
      headers: { Authorization: `Bearer ${session.token}` },
      signal: AbortSignal.timeout(2000),
    });
    if (!response.ok) return null;
    return stateSchema.parse(await response.json());
  } catch {
    return null;
  }
}

// Never unlink this inode: SQLite's OS-backed writer lock is released on
// process exit/crash, without a stale-owner check/delete race. The separate
// database leaves the application's WAL available to normal store operations.
const ownedDirectories = new Set<string>();
const reservations = new WeakMap<Application, () => void>();
function reserveDirectory(directory: string): () => void {
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const folder = lstatSync(directory);
  if (
    !folder.isDirectory() ||
    folder.isSymbolicLink() ||
    folder.uid !== process.getuid?.()
  )
    throw new CliError(
      "The server data directory must be owner-only, owned by you, and not a symbolic link.",
    );
  // Preserve the store's existing permission tightening, before creating the lock.
  if ((folder.mode & 0o777) !== 0o700) chmodSync(directory, 0o700);
  const canonical = realpathSync(directory);
  const occupied = () =>
    new CliError(
      `NonstopVibin is already starting, running or stopping for ${canonical}. Run \`nonstopvibin url\` for its link.`,
    );
  // Do not open/close another descriptor for the locked inode in this process:
  // POSIX advisory locks are process-scoped, and close would release them.
  if (ownedDirectories.has(canonical)) throw occupied();
  const path = join(canonical, "server.lock.sqlite");
  try {
    closeSync(
      openSync(
        path,
        constants.O_CREAT |
          constants.O_EXCL |
          constants.O_WRONLY |
          constants.O_NOFOLLOW,
        0o600,
      ),
    );
  } catch (error) {
    if (!(error instanceof Error && "code" in error && error.code === "EEXIST"))
      throw error;
  }
  const file = lstatSync(path);
  if (
    !file.isFile() ||
    file.isSymbolicLink() ||
    file.nlink !== 1 ||
    file.uid !== process.getuid?.() ||
    (file.mode & 0o077) !== 0
  )
    throw new CliError(
      "server.lock.sqlite must be an owner-only ordinary file you own, without links.",
    );
  const lock = new DatabaseSync(path);
  try {
    // Reserve the single writer without upgrading past concurrent SHARED readers:
    // EXCLUSIVE upgrades can make both initial contenders fail with SQLITE_BUSY.
    lock.exec("PRAGMA busy_timeout=0; BEGIN IMMEDIATE");
  } catch (error) {
    lock.close();
    if (error instanceof Error && "errcode" in error && error.errcode === 5)
      throw occupied();
    throw error;
  }
  ownedDirectories.add(canonical);
  return () => {
    lock.close();
    ownedDirectories.delete(canonical);
  };
}

export interface ServeOptions {
  directory: string;
  binary: string;
  clientDirectory: string;
  port: number;
  allowHosts: string[];
}
export async function startServer(
  options: ServeOptions,
  signal?: AbortSignal,
): Promise<Application> {
  signal?.throwIfAborted();
  const existing = await readSession(options.directory);
  if (existing && (await probe(existing)))
    throw new CliError(
      `NonstopVibin is already running on 127.0.0.1:${existing.port} for ${options.directory}. Run \`nonstopvibin url\` for its link.`,
    );
  const release = reserveDirectory(options.directory);
  let application: Application | undefined;
  try {
    signal?.throwIfAborted();
    if (!existsSync(options.binary))
      throw new CliError(
        `The proxy core is missing at ${options.binary}. Reinstall NonstopVibin from its release archive.`,
      );
    application = await Application.create(
      {
        directory: options.directory,
        binary: options.binary,
        clientDirectory: options.clientDirectory,
        port: options.port,
        allowHosts: options.allowHosts,
        desktop: false,
        development: false,
        requirePinnedCore: true,
      },
      signal,
    );
    signal?.throwIfAborted();
    await writeAtomic(
      sessionPath(options.directory),
      JSON.stringify({
        pid: process.pid,
        port: application.port,
        token: application.token,
        allowHosts: options.allowHosts,
        version,
        startedAt: new Date().toISOString(),
      } satisfies Session),
    );
    signal?.throwIfAborted();
    reservations.set(application, release);
    return application;
  } catch (error) {
    try {
      if (application) await application.close();
      await removeSession(options.directory);
    } catch (cleanupError) {
      throw new AggregateError(
        [error, cleanupError],
        `${errorMessage(error)} Cleanup failed: ${errorMessage(cleanupError)}`,
        { cause: cleanupError },
      );
    } finally {
      release();
    }
    if (
      error instanceof Error &&
      "code" in error &&
      error.code === "EADDRINUSE"
    )
      throw new CliError(
        `Port ${options.port} on 127.0.0.1 is already in use. Stop the other program, or choose another port with --port.`,
      );
    throw error;
  }
}
export async function stopServer(
  application: Application,
  directory: string,
): Promise<void> {
  try {
    await application.close();
  } finally {
    await removeSession(directory);
    const release = reservations.get(application);
    if (release) {
      reservations.delete(application);
      release();
    }
  }
}

export function parseHost(value: string): string {
  const host = value.trim().toLowerCase();
  if (!validAllowHost(host))
    throw new CliError(
      `--allow-host needs an exact DNS name such as myserver.tailnet.ts.net (no scheme, port, wildcard or IP address); got ${JSON.stringify(value)}.`,
      2,
    );
  return host;
}
function parsePort(value: string | undefined): number {
  if (value === undefined) return DEFAULT_PORT;
  const port = Number(value);
  if (!/^\d+$/.test(value) || port < 1 || port > 65535)
    throw new CliError(`--port must be between 1 and 65535.`, 2);
  return port;
}

// OpenSSH sets SSH_CONNECTION to "client-ip client-port server-ip server-port".
export function tunnelCommand(
  sshConnection: string,
  user: string,
  port: number,
): string | undefined {
  const [, , rawAddress, sshPort] = sshConnection.trim().split(/\s+/);
  if (!rawAddress || !sshPort) return undefined;
  const address = rawAddress.replace(/^::ffff:(?=\d+\.)/, "");
  return [
    "ssh -N -o ExitOnForwardFailure=yes",
    `-L 127.0.0.1:${port}:127.0.0.1:${port}`,
    ...(sshPort === "22" ? [] : [`-p ${sshPort}`]),
    `${user}@${address}`,
  ].join(" ");
}

export function sessionLinks(session: Session) {
  return {
    url: `http://127.0.0.1:${session.port}/#session=${session.token}`,
    httpsUrls: session.allowHosts.map(
      (host) => `https://${host}/#session=${session.token}`,
    ),
  };
}

export function urlText(session: Session, sshConnection?: string): string {
  const { url, httpsUrls } = sessionLinks(session);
  const tunnel = sshConnection
    ? tunnelCommand(sshConnection, userInfo().username, session.port)
    : undefined;
  const lines = [
    `NonstopVibin is running on 127.0.0.1:${session.port} (pid ${session.pid}).`,
    "",
  ];
  if (tunnel)
    lines.push(
      "1. On your laptop, start a tunnel (use your usual SSH host name if you have one):",
      `   ${tunnel}`,
      "",
      "2. Then open:",
      `   ${url}`,
      "",
      "3. Optional, for automatic Codex and Claude sign-in, add to the tunnel:",
      `   ${SIGN_IN_PORTS.map((p) => `-L 127.0.0.1:${p}:127.0.0.1:${p}`).join(" ")}`,
      "   Otherwise, paste the callback URL into the app when asked.",
    );
  else lines.push("Open this link on this machine:", `   ${url}`);
  if (httpsUrls.length)
    lines.push(
      "",
      `Tailscale (HTTPS only), after \`tailscale serve --bg ${session.port}\`:`,
      ...httpsUrls.map((link) => `   ${link}`),
    );
  lines.push(
    "",
    "Keep this link private: it grants full control of NonstopVibin until the server restarts.",
  );
  return lines.join("\n");
}

async function running(directory: string) {
  const session = await readSession(directory);
  const state = session && (await probe(session));
  if (!session || !state)
    throw new CliError(
      `NonstopVibin is not running for ${directory}.\nCheck \`systemctl --user status nonstopvibin\`, or start it with \`nonstopvibin serve\`.`,
      NOT_RUNNING,
    );
  return { session, state };
}

// systemd unit quoting: C-style escapes inside double quotes; % starts a
// specifier and $ a variable in ExecStart arguments (not the executable).
function unitQuote(value: string, dollar: boolean): string {
  if ([...value].some((c) => c < " " || c === "\x7f"))
    throw new CliError(
      `Cannot write a control character to the service unit: ${JSON.stringify(value)}`,
    );
  let quoted = value
    .replaceAll("\\", "\\\\")
    .replaceAll('"', '\\"')
    .replaceAll("%", "%%");
  if (dollar) quoted = quoted.replaceAll("$", "$$$$");
  return `"${quoted}"`;
}
// Agent configuration lands where the user's agents read it.
export const SERVICE_ENVIRONMENT = [
  "NONSTOPVIBIN_DATA_DIR",
  "CODEX_HOME",
  "OPENCODE_CONFIG_DIR",
  "XDG_CONFIG_HOME",
  "PI_CODING_AGENT_DIR",
];
export function renderUnit(
  executable: string,
  args: string[],
  environment: Array<[string, string]>,
): string {
  return `# Generated by \`nonstopvibin service install\`. Re-run it to change options.
[Unit]
Description=NonstopVibin AI subscription gateway
StartLimitIntervalSec=120
StartLimitBurst=5

[Service]
Type=simple
ExecStart=${[unitQuote(executable, false), ...args.map((arg) => unitQuote(arg, true))].join(" ")}
${environment.map(([key, value]) => `Environment=${unitQuote(`${key}=${value}`, false)}\n`).join("")}Restart=on-failure
RestartSec=2
KillMode=mixed
TimeoutStopSec=30
UMask=0077
NoNewPrivileges=true
# Deliberately not set: PrivateTmp (the agent credential socket lives in
# /tmp/nonstopvibin-UID-*), ProtectHome/ProtectSystem (agent settings are
# written to ~/.claude, ~/.codex and similar), MemoryDenyWriteExecute (the
# JavaScript JIT needs it).

[Install]
WantedBy=default.target
`;
}
function unitPath(): string {
  return join(
    process.env.XDG_CONFIG_HOME || join(homedir(), ".config"),
    "systemd/user",
    UNIT,
  );
}
function systemctl(...args: string[]) {
  return spawnSync("systemctl", ["--user", ...args], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
}
function requireUserManager(): void {
  if (process.platform !== "linux")
    throw new CliError("The service commands need Linux with systemd.");
  const check = process.env.XDG_RUNTIME_DIR
    ? systemctl("show-environment")
    : undefined;
  if (check?.status !== 0)
    throw new CliError(
      [
        "The systemd user manager is not reachable" +
          (process.env.XDG_RUNTIME_DIR
            ? "."
            : " (XDG_RUNTIME_DIR is not set)."),
        "Log in over SSH directly as this user (not through su or sudo).",
        "If it still fails, run `sudo apt install dbus-user-session`, then reconnect.",
        "Without a service, run `nonstopvibin serve` in a terminal instead.",
      ].join("\n"),
    );
}
async function waitUntilRunning(
  directory: string,
  since: number,
): Promise<boolean> {
  for (let attempt = 0; attempt < 60; attempt++) {
    const session = await readSession(directory);
    if (
      session &&
      Date.parse(session.startedAt) >= since &&
      (await probe(session))
    )
      return true;
    await delay(500);
  }
  return false;
}
function ensureLinger(): void {
  const user = userInfo().username;
  const linger = spawnSync(
    "loginctl",
    ["show-user", user, "--property=Linger", "--value"],
    { encoding: "utf8" },
  );
  if (linger.stdout?.trim() === "yes") return;
  const enable = spawnSync("loginctl", ["enable-linger", "--no-ask-password"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  if (enable.status === 0) {
    console.log("Enabled lingering so the service keeps running after logout.");
    return;
  }
  console.warn(
    [
      "",
      "WARNING: could not enable lingering for this user. Without it, the service",
      "stops when you log out and does not start at boot. Ask an administrator to run:",
      `  sudo loginctl enable-linger ${user}`,
    ].join("\n"),
  );
}
// Preserve the rest of an existing unit, including custom executable paths,
// arguments and agent directories. Only explicitly supplied options change.
export function updateUnit(
  previous: string,
  port: number | undefined,
  allowHosts: string[] | undefined,
  environment: Array<[string, string]>,
): string {
  let unit = previous;
  if (port !== undefined || allowHosts !== undefined) {
    const match = unit.match(/^ExecStart=(.+)$/m);
    if (!match || !match[1]?.includes('"serve"'))
      throw new CliError(
        "Cannot update this custom ExecStart. Edit the service unit directly instead.",
      );
    let command = match[1];
    if (port !== undefined) {
      command = command.replace(/ "--port" "\d+"/g, "");
      command += ` "--port" "${port}"`;
    }
    if (allowHosts !== undefined) {
      command = command.replace(/ "--allow-host" "[a-z0-9.-]+"/g, "");
      command += allowHosts
        .map((host) => ` "--allow-host" ${unitQuote(host, true)}`)
        .join("");
    }
    unit = unit.replace(match[0], () => `ExecStart=${command}`);
  }
  for (const [key, value] of environment) {
    const line = `Environment=${unitQuote(`${key}=${value}`, false)}`;
    const pattern = new RegExp(`^Environment="${key}=.*"$`, "m");
    unit = pattern.test(unit)
      ? unit.replace(pattern, () => line)
      : unit.replace("[Service]\n", `[Service]\n${line}\n`);
  }
  return unit;
}

// Upgrades need to find the service's data directory even when it was installed
// from a shell with a custom NONSTOPVIBIN_DATA_DIR. Read the live process's
// environment, not shell-eval of systemd's quoted Environment output.
async function discoveryDirectory(): Promise<string> {
  if (process.env.NONSTOPVIBIN_DATA_DIR || process.platform !== "linux")
    return dataDirectory();
  const pid = systemctl(
    "show",
    UNIT,
    "--property=MainPID",
    "--value",
  ).stdout?.trim();
  if (pid && /^\d+$/.test(pid) && pid !== "0") {
    try {
      const environment = await readFile(`/proc/${pid}/environ`, "utf8");
      const value = environment
        .split("\0")
        .find((entry) => entry.startsWith("NONSTOPVIBIN_DATA_DIR="))
        ?.slice("NONSTOPVIBIN_DATA_DIR=".length);
      if (value) return dataDirectory(value);
    } catch (error) {
      if (
        !isMissing(error) &&
        !(error instanceof Error && "code" in error && error.code === "EACCES")
      )
        throw error;
    }
  }
  return dataDirectory();
}

async function serviceInstall(
  port: number | undefined,
  allowHosts: string[] | undefined,
) {
  requireUserManager();
  if (!packaged)
    throw new CliError(
      "Install the service from the packaged release, not from source.",
    );
  const directory = dataDirectory();
  const path = unitPath();
  const previous = await readFile(path, "utf8").catch((error) => {
    if (isMissing(error)) return undefined;
    throw error;
  });
  const args = [
    "serve",
    "--port",
    String(port ?? DEFAULT_PORT),
    ...(allowHosts ?? []).flatMap((host) => ["--allow-host", host]),
  ];
  const environment = SERVICE_ENVIRONMENT.flatMap(
    (key): Array<[string, string]> => {
      const value = process.env[key];
      return value
        ? [[key, key === "NONSTOPVIBIN_DATA_DIR" ? directory : value]]
        : [];
    },
  );
  await mkdir(dirname(path), { recursive: true });
  await writeAtomic(
    path,
    previous
      ? updateUnit(previous, port, allowHosts, environment)
      : renderUnit(realpathSync(process.execPath), args, environment),
  );
  const previousPort = previous?.match(/"--port" "(\d+)"/)?.[1];
  if (port !== undefined && previousPort && previousPort !== String(port))
    console.warn(
      `The port changed from ${previousPort} to ${port}. Reconnect your agents in Connect agents; their settings still use the old port.`,
    );
  const since = Date.now() - 1000;
  for (const args of [["daemon-reload"], ["enable", UNIT], ["restart", UNIT]]) {
    const result = systemctl(...args);
    if (result.status !== 0)
      throw new CliError(
        `systemctl --user ${args.join(" ")} failed:\n${result.stderr.trim()}`,
      );
  }
  console.log(`Installed ${path}`);
  if (!(await waitUntilRunning(await discoveryDirectory(), since)))
    throw new CliError(
      "The service did not become ready. Check `journalctl --user -u nonstopvibin -e`.",
    );
  console.log(
    "NonstopVibin is running as a user service. Run `nonstopvibin status` for its port.",
  );
  ensureLinger();
  console.log("Run `nonstopvibin url` for the session link.");
}
async function serviceUninstall() {
  requireUserManager();
  const directory = await discoveryDirectory();
  const path = unitPath();
  if (existsSync(path)) {
    const disable = systemctl("disable", "--now", UNIT);
    if (disable.status !== 0)
      throw new CliError(
        `Could not stop and disable the service:\n${disable.stderr.trim()}`,
      );
  }
  await rm(path, { force: true });
  const reload = systemctl("daemon-reload");
  if (reload.status !== 0)
    throw new CliError(
      `Could not reload the user manager:\n${reload.stderr.trim()}`,
    );
  console.log(
    [
      `Removed ${path}.`,
      `Your data remains in ${directory}. Lingering is unchanged; run \`loginctl disable-linger\` if you no longer need it.`,
    ].join("\n"),
  );
}

const HELP = `NonstopVibin ${version} — headless server

Usage:
  nonstopvibin serve [--port ${DEFAULT_PORT}] [--allow-host FQDN]
      Run in the foreground (Ctrl+C stops it).
  nonstopvibin url [--json]
      Print the session link and the SSH tunnel command.
  nonstopvibin status
      Show whether the server is running (exit 0) or not (exit 3).
  nonstopvibin service install [--port ${DEFAULT_PORT}] [--allow-host FQDN]
      Install and start a systemd user service.
  nonstopvibin service uninstall
      Stop and remove the user service. Your data is kept.
  nonstopvibin --version

Options:
  --port N           Loopback port (default ${DEFAULT_PORT}). Changing it later
                     requires reconnecting agents.
  --allow-host FQDN  Also accept this exact HTTPS host name for the web UI,
                     e.g. from \`tailscale serve --bg ${DEFAULT_PORT}\`.

Manage the service with systemd:
  systemctl --user status nonstopvibin
  systemctl --user restart nonstopvibin    (issues a new session link)
  journalctl --user -u nonstopvibin -f

Data: ${dataDirectory()} (override with NONSTOPVIBIN_DATA_DIR).
Profiles, subscriptions and agent connections are managed in the web UI.`;

export async function main(argv: string[]): Promise<number> {
  const { values, positionals } = parseArgs({
    args: argv,
    allowPositionals: true,
    options: {
      port: { type: "string" },
      "allow-host": { type: "string", multiple: true },
      json: { type: "boolean" },
      help: { type: "boolean", short: "h" },
      version: { type: "boolean", short: "v" },
    },
  });
  const [command, subcommand, ...rest] = positionals;
  if (values.version) {
    console.log(`nonstopvibin ${version} (CLIProxyAPI ${coreRelease.version})`);
    return 0;
  }
  if (values.help || !command) {
    console.log(HELP);
    return 0;
  }
  const allowHosts = (values["allow-host"] ?? []).map(parseHost);
  const port = parsePort(values.port);
  const directory =
    command === "url" || command === "status"
      ? await discoveryDirectory()
      : dataDirectory();
  if (command === "serve" && !subcommand) {
    const controller = new AbortController();
    let application: Application | undefined;
    let stopping = false;
    const stop = () => {
      controller.abort();
      if (!application || stopping) return;
      stopping = true;
      stopServer(application, directory).then(
        () => process.exit(0),
        (error) => {
          console.error(error);
          process.exit(1);
        },
      );
    };
    // Receive termination before directory reservation, store creation or any
    // core spawn. Startup cancellation itself awaits partial-app cleanup.
    process.on("SIGINT", stop);
    process.on("SIGTERM", stop);
    try {
      application = await startServer(
        {
          directory,
          binary: coreBinary,
          clientDirectory,
          port,
          allowHosts,
        },
        controller.signal,
      );
    } catch (error) {
      process.removeListener("SIGINT", stop);
      process.removeListener("SIGTERM", stop);
      if (error === controller.signal.reason) return 0;
      throw error;
    }
    // journald must never receive the session token.
    const session = await readSession(directory);
    if (process.stdout.isTTY && session)
      console.log(urlText(session, process.env.SSH_CONNECTION));
    else
      console.log(
        `NonstopVibin ${version} is listening on 127.0.0.1:${application.port}. Run \`nonstopvibin url\` for the session link.`,
      );
    return -1;
  }
  if (command === "url" && !subcommand) {
    const { session, state } = await running(directory);
    if (values.json) {
      console.log(
        JSON.stringify({
          running: true,
          pid: session.pid,
          port: session.port,
          version: state.version,
          coreVersion: state.coreVersion,
          ...sessionLinks(session),
        }),
      );
    } else console.log(urlText(session, process.env.SSH_CONNECTION));
    return 0;
  }
  if (command === "status" && !subcommand) {
    const unit =
      process.platform === "linux"
        ? systemctl("is-active", UNIT).stdout?.trim()
        : undefined;
    const session = await readSession(directory);
    const state = session && (await probe(session));
    if (!session || !state) {
      console.log(
        `NonstopVibin is not running for ${directory}.${unit ? ` Unit nonstopvibin.service: ${unit}.` : ""}`,
      );
      return NOT_RUNNING;
    }
    console.log(
      [
        `NonstopVibin ${state.version} is running on 127.0.0.1:${session.port} (pid ${session.pid}, since ${session.startedAt}).`,
        `Core: CLIProxyAPI ${state.coreVersion}.${unit ? ` Unit nonstopvibin.service: ${unit}.` : ""}`,
      ].join("\n"),
    );
    return 0;
  }
  if (command === "service" && !rest.length) {
    if (subcommand === "install") {
      await serviceInstall(
        values.port === undefined ? undefined : port,
        values["allow-host"] === undefined ? undefined : allowHosts,
      );
      return 0;
    }
    if (subcommand === "uninstall") {
      await serviceUninstall();
      return 0;
    }
  }
  throw new CliError(
    `Unknown command: ${positionals.join(" ")}. Run \`nonstopvibin --help\` for usage.`,
    2,
  );
}

if (import.meta.main)
  main(process.argv.slice(2)).then(
    (code) => {
      if (code >= 0) process.exit(code);
    },
    (error) => {
      if (error instanceof CliError) {
        console.error(error.message);
        process.exit(error.code);
      }
      if (
        error instanceof Error &&
        "code" in error &&
        String(error.code).startsWith("ERR_PARSE_ARGS")
      ) {
        console.error(
          `${error.message}\nRun \`nonstopvibin --help\` for usage.`,
        );
        process.exit(2);
      }
      console.error(error instanceof Error ? error.message : error);
      process.exit(1);
    },
  );
