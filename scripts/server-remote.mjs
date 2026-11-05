// Laptop-side conveniences over your own ssh configuration.
//   bun run server:deploy <ssh-destination> [install.sh options]
//   bun run server:open <ssh-destination> [--sign-in] [--no-browser]
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { connect, createServer } from "node:net";
import { resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import pkg from "../package.json" with { type: "json" };

const SIGN_IN_PORTS = [1455, 54545]; // Codex, Claude
const [command, destination, ...rest] = process.argv.slice(2);
let tunnel;
const fail = (message) => {
  tunnel?.kill("SIGTERM");
  console.error(message);
  process.exit(1);
};
process.on("exit", () => tunnel?.kill("SIGTERM"));
process.on("uncaughtException", (error) => fail(error.message));
process.on("unhandledRejection", (error) =>
  fail(error instanceof Error ? error.message : String(error)),
);
if (
  !["deploy", "open"].includes(command) ||
  !destination ||
  destination.startsWith("-")
)
  fail(
    "Usage: bun run server:deploy <ssh-destination> [install.sh options]\n       bun run server:open <ssh-destination> [--sign-in] [--no-browser]",
  );
const quote = (value) => `'${value.replaceAll("'", `'\\''`)}'`;
const ssh = (script) =>
  spawnSync("ssh", [destination, script], {
    encoding: "utf8",
    stdio: ["inherit", "pipe", "inherit"],
  });

if (command === "deploy") {
  const machine = ssh("uname -m");
  if (machine.status !== 0) fail(`Could not run commands on ${destination}.`);
  const arch = {
    x86_64: "x64",
    amd64: "x64",
    aarch64: "arm64",
    arm64: "arm64",
  }[machine.stdout.trim()];
  if (!arch) fail(`Unsupported server architecture: ${machine.stdout.trim()}`);
  const repo = resolve(import.meta.dirname, "..");
  execFileSync("bun", ["run", "dist:server", arch], {
    cwd: repo,
    stdio: "inherit",
  });
  const name = `nonstopvibin-server-${pkg.version}-linux-${arch}`;
  const created = ssh("mktemp -d");
  const temporary = created.stdout.trim();
  if (created.status !== 0 || !/^\/[\w./-]+$/.test(temporary))
    fail("Could not create a temporary directory on the server.");
  let status = 1;
  try {
    execFileSync(
      "scp",
      [
        "-q",
        resolve(repo, "release", `${name}.tar.gz`),
        `${destination}:${temporary}/`,
      ],
      { stdio: "inherit" },
    );
    const install = spawnSync(
      "ssh",
      [
        destination,
        `cd ${quote(temporary)} && tar -xzf ${quote(`${name}.tar.gz`)} && ./${quote(name)}/install.sh ${rest.map(quote).join(" ")}`,
      ],
      { stdio: "inherit" },
    );
    status = install.status ?? 1;
  } finally {
    const cleanup = ssh(`rm -rf ${quote(temporary)}`);
    if (cleanup.status !== 0)
      console.error(
        `Could not remove remote temporary directory ${temporary}.`,
      );
  }
  process.exit(status);
}

const signIn = rest.includes("--sign-in");
const browser = !rest.includes("--no-browser");
const unknown = rest.filter(
  (arg) => !["--sign-in", "--no-browser"].includes(arg),
);
if (unknown.length) fail(`Unknown option: ${unknown.join(" ")}`);
const remote = ssh(
  'if command -v nonstopvibin >/dev/null 2>&1; then exec nonstopvibin url --json; else exec "$HOME/.local/bin/nonstopvibin" url --json; fi',
);
if (remote.status === 127)
  fail(
    `NonstopVibin is not installed on ${destination}. Run bun run server:deploy ${destination}.`,
  );
if (remote.status === 3)
  fail(
    `NonstopVibin is not running on ${destination}. Check systemctl --user status nonstopvibin there.`,
  );
if (remote.status !== 0)
  fail(`Could not read the session from ${destination}.`);
const session = JSON.parse(remote.stdout);
const port = Number(session.port);
if (
  !Number.isInteger(port) ||
  port < 1 ||
  port > 65535 ||
  !String(session.url).startsWith(`http://127.0.0.1:${port}/`)
)
  fail("The server returned an unexpected session.");

// The browser's Host and Origin must match the server's own port, so the
// local end of the tunnel must use the same port number.
const ports = [port, ...(signIn ? SIGN_IN_PORTS : [])];
for (const local of ports) {
  const free = await new Promise((done) => {
    const probe = createServer()
      .once("error", () => done(false))
      .listen(local, "127.0.0.1", () => probe.close(() => done(true)));
  });
  if (!free)
    fail(
      `Local port ${local} is already in use${local === port ? " (the tunnel must use the same port as the server)" : ""}. Stop the program using it${local === port ? `, or reinstall the server with another --port` : ""}.`,
    );
}
tunnel = spawn(
  "ssh",
  [
    "-N",
    "-o",
    "ExitOnForwardFailure=yes",
    ...ports.flatMap((p) => ["-L", `127.0.0.1:${p}:127.0.0.1:${p}`]),
    destination,
  ],
  { stdio: "inherit" },
);
let exited = false;
tunnel.once("error", (error) => fail(`Could not start SSH: ${error.message}`));
tunnel.once("exit", (code) => {
  exited = true;
  if (code) console.error(`The SSH tunnel exited (${code}).`);
  process.exit(code ?? 0);
});
for (const signal of ["SIGINT", "SIGTERM"])
  process.on(signal, () => tunnel.kill("SIGTERM"));
const accepts = () =>
  new Promise((done) => {
    const socket = connect(port, "127.0.0.1")
      .once("connect", () => {
        socket.destroy();
        done(true);
      })
      .once("error", () => {
        socket.destroy();
        done(false);
      });
    socket.setTimeout(1000, () => {
      socket.destroy();
      done(false);
    });
  });
for (let attempt = 0; !(await accepts()); attempt++) {
  if (exited || attempt > 60) fail("The SSH tunnel did not open.");
  await delay(250);
}
console.log(
  `Tunnel open to ${destination} on 127.0.0.1:${ports.join(", ")}. Press Ctrl+C to close it.\n${session.url}`,
);
if (browser) {
  const opener = process.platform === "darwin" ? "open" : "xdg-open";
  spawn(opener, [session.url], { stdio: "ignore", detached: true })
    .once("error", () =>
      console.error(`Could not run ${opener}; open the link above.`),
    )
    .unref();
}
