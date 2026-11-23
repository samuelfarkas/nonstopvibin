// The headless CLI as a user runs it (`bun src/server/cli.ts ...` in a child process with
// a temporary HOME, data directory and a stub systemctl), against no, stale, malformed
// and running server.json files; plus the exported pure helpers: unit rendering and
// updates, tunnel commands, URL text, allow-host validation, data directory resolution,
// release channels and the desktop's external host allowlist.
import { execFile } from "node:child_process";
import { chmodSync, mkdirSync, writeFileSync } from "node:fs";
import http from "node:http";
import { userInfo } from "node:os";
import { join } from "node:path";
import {
  renderUnit,
  tunnelCommand,
  updateUnit,
  urlText,
  type Session,
} from "../../../src/server/cli.ts";
import { dataDirectory } from "../../../src/server/data-directory.ts";
import { validAllowHost } from "../../../src/server/server.ts";
import {
  providers,
  type ProviderEntry,
} from "../../../src/shared/providers.ts";
import { releaseChannel } from "../../../scripts/release-channel.ts";
import {
  type Case,
  outcome,
  Placeholders,
  repository,
  scratch,
  type Recorded,
} from "../lib.ts";

/** A stub server answering GET /api/state for one token, as a running server does. */
const statePort = 24319;
/** Nothing listens here: a stale server.json. */
const stalePort = 24399;
const token = "synthetic-session-token";

const session = (port: number, extra: Partial<Session> = {}): Session => ({
  pid: 4242,
  port,
  token,
  allowHosts: [],
  version: "0.2.0",
  startedAt: "2026-11-22T11:00:00.000Z",
  ...extra,
});

export async function recordCli(): Promise<Case[]> {
  const root = scratch("cli");
  const home = join(root, "home");
  const data = join(root, "data");
  const bin = join(root, "bin");
  const config = join(home, ".config");
  mkdirSync(home, { recursive: true });
  mkdirSync(bin);
  // systemctl answers as a reachable user manager with the unit inactive; every call is logged.
  writeFileSync(
    join(bin, "systemctl"),
    `#!/bin/sh\necho "$@" >> "${root}/systemctl.log"\ncase "$*" in *is-active*) echo inactive; exit 3;; *show-environment*) exit 0;; *) exit 0;; esac\n`,
    { mode: 0o755 },
  );
  const placeholders = new Placeholders()
    .add(root, "<root>")
    .add(userInfo().username, "<user>");
  const state = http.createServer((req, res) => {
    if (
      req.url === "/api/state" &&
      req.headers.authorization === `Bearer ${token}`
    ) {
      res.setHeader("Content-Type", "application/json");
      res.end(
        JSON.stringify({
          version: "0.2.0",
          coreVersion: "8.1.0",
          profiles: [],
        }),
      );
    } else res.writeHead(401).end("{}");
  });
  await new Promise<void>((done) => state.listen(statePort, "127.0.0.1", done));
  const run = (args: string[], environment: Record<string, string> = {}) =>
    new Promise<{ code: number; stdout: string; stderr: string }>((resolve) =>
      execFile(
        process.execPath,
        [join(repository, "src/server/cli.ts"), ...args],
        {
          cwd: root,
          encoding: "utf8",
          timeout: 20_000,
          env: {
            PATH: `${bin}:/usr/bin:/bin`,
            HOME: home,
            NONSTOPVIBIN_DATA_DIR: data,
            XDG_CONFIG_HOME: config,
            XDG_RUNTIME_DIR: join(root, "runtime"),
            ...environment,
          },
        },
        (error, stdout, stderr) =>
          resolve({
            code: error ? Number(error.code ?? 1) : 0,
            stdout,
            stderr,
          }),
      ),
    );
  const writeSession = (value: Recorded, mode = 0o600) =>
    writeSessionText(JSON.stringify(value), mode);
  const writeSessionText = (content: string, mode = 0o600) => {
    mkdirSync(data, { recursive: true, mode: 0o700 });
    writeFileSync(join(data, "server.json"), content, { mode });
    chmodSync(join(data, "server.json"), mode);
  };
  const cases: Case[] = [];
  const cli = async (
    name: string,
    args: string[],
    environment: Record<string, string> = {},
  ) =>
    cases.push({
      name: `cli ${name}`,
      input: { args, environment },
      output: await run(args, environment),
    });
  try {
    for (const args of [
      ["--help"],
      ["-h"],
      [],
      ["--version"],
      ["-v"],
      ["foo"],
      ["serve", "extra"],
      ["status", "extra"],
      ["--bogus"],
      ["url", "--port", "abc"],
      ["url", "--port", "0"],
      ["url", "--port", "65536"],
      ["url", "--allow-host", "https://x.ts.net"],
      ["url", "--allow-host", "*.ts.net"],
      ["service"],
      ["service", "restart"],
      ["service", "unit"],
      ["service", "unit", "a", "b"],
    ])
      await cli(args.join(" ") || "no arguments", args);
    await cli("status without server.json", ["status"]);
    await cli("url without server.json", ["url"]);
    writeSession(session(stalePort));
    await cli("status stale", ["status"]);
    await cli("url stale", ["url"]);
    writeSession(session(statePort, { token: "other" }));
    await cli("status wrong token", ["status"]);
    writeSessionText("{not json");
    await cli("status malformed server.json", ["status"]);
    writeSession({ ...session(statePort), port: "x" });
    await cli("status invalid server.json", ["status"]);
    writeSession({ ...session(statePort), allowHosts: ["Bad Host"] });
    await cli("status invalid allow host in server.json", ["status"]);
    writeSession(session(statePort), 0o644);
    await cli("status group-readable server.json", ["status"]);
    writeSession(
      session(statePort, {
        allowHosts: ["myserver.tailnet.ts.net", "other.example.com"],
      }),
    );
    await cli("status running", ["status"]);
    await cli("url running", ["url"]);
    await cli("url running json", ["url", "--json"]);
    await cli("url running over ssh", ["url"], {
      SSH_CONNECTION: "203.0.113.5 50000 198.51.100.7 22",
    });
    await cli("url running over ssh other port", ["url"], {
      SSH_CONNECTION: "203.0.113.5 50000 ::ffff:198.51.100.7 2222",
    });
    await cli("url running over ssh ipv6", ["url"], {
      SSH_CONNECTION: "2001:db8::1 50000 2001:db8::2 22",
    });
    await cli("url running over malformed ssh", ["url"], {
      SSH_CONNECTION: "garbage",
    });
    await cli("service install from source", ["service", "install"]);
    await cli("service install without user manager", ["service", "install"], {
      XDG_RUNTIME_DIR: "",
    });
    await cli("service unit without a unit", [
      "service",
      "unit",
      "/opt/nonstopvibin/nonstopvibin",
    ]);
    const unitDirectory = join(config, "systemd/user");
    mkdirSync(unitDirectory, { recursive: true });
    writeFileSync(
      join(unitDirectory, "nonstopvibin.service"),
      renderUnit(
        "/opt/nonstopvibin/nonstopvibin",
        ["serve", "--port", "4320"],
        [],
      ),
    );
    await cli("service unit same executable", [
      "service",
      "unit",
      "/opt/nonstopvibin/nonstopvibin",
    ]);
    await cli("service unit other executable", [
      "service",
      "unit",
      "/usr/local/bin/nonstopvibin",
    ]);
    await cli("service uninstall", ["service", "uninstall"]);
  } finally {
    await new Promise<void>((done) => state.close(() => done()));
  }

  // Unit text.
  const units: Array<[string, string, string[], Array<[string, string]>]> = [
    [
      "plain",
      "/home/u/.local/bin/nonstopvibin",
      ["serve", "--port", "4320"],
      [],
    ],
    [
      "hosts and environment",
      "/opt/nv/nonstopvibin",
      [
        "serve",
        "--port",
        "4400",
        "--allow-host",
        "a.ts.net",
        "--allow-host",
        "b.example.com",
      ],
      [
        ["NONSTOPVIBIN_DATA_DIR", "/data/nv"],
        ["CODEX_HOME", "/home/u/.codex-alt"],
        ["XDG_CONFIG_HOME", "/home/u/.cfg"],
      ],
    ],
    [
      "quoting",
      '/opt/with space/"quoted"\\nv%',
      ["serve", "$HOME", "100%", 'a"b'],
      [["CLAUDE_CONFIG_DIR", "/x/$y/%z"]],
    ],
    ["control character", "/opt/nv\nx", ["serve"], []],
  ];
  for (const [name, executable, args, environment] of units)
    cases.push(
      await outcome(
        `renderUnit ${name}`,
        { executable, args, environment },
        () => renderUnit(executable, args, environment),
      ),
    );
  const base = renderUnit(
    "/opt/nv/nonstopvibin",
    ["serve", "--port", "4320", "--allow-host", "a.ts.net"],
    [["NONSTOPVIBIN_DATA_DIR", "/data/nv"]],
  );
  const updates: Array<
    [
      string,
      string,
      number | undefined,
      string[] | undefined,
      Array<[string, string]>,
    ]
  > = [
    ["nothing", base, undefined, undefined, []],
    ["port", base, 4400, undefined, []],
    ["hosts replaced", base, undefined, ["b.ts.net", "c.ts.net"], []],
    ["hosts cleared", base, undefined, [], []],
    [
      "environment replaced and added",
      base,
      undefined,
      undefined,
      [
        ["NONSTOPVIBIN_DATA_DIR", "/new"],
        ["CODEX_HOME", "/c"],
      ],
    ],
    [
      "custom exec start",
      base.replace(/^ExecStart=.*$/m, "ExecStart=/usr/bin/env custom"),
      4400,
      undefined,
      [],
    ],
    [
      "custom exec start environment only",
      base.replace(/^ExecStart=.*$/m, "ExecStart=/usr/bin/env custom"),
      undefined,
      undefined,
      [["CODEX_HOME", "/c"]],
    ],
    [
      "repeated port flags",
      base.replace('"--port" "4320"', '"--port" "4320" "--port" "1"'),
      4401,
      undefined,
      [],
    ],
  ];
  for (const [name, previous, port, hosts, environment] of updates)
    cases.push(
      await outcome(
        `updateUnit ${name}`,
        {
          previous,
          port: port ?? null,
          allowHosts: hosts ?? null,
          environment,
        },
        () => updateUnit(previous, port, hosts, environment),
      ),
    );

  // Tunnel commands and URL text.
  for (const connection of [
    "203.0.113.5 50000 198.51.100.7 22",
    "203.0.113.5 50000 198.51.100.7 2222",
    "1.2.3.4 5 ::ffff:10.0.0.1 22",
    "1.2.3.4 5 ::ffff:fe80::1 22",
    "2001:db8::1 50000 2001:db8::2 22",
    "  1 2 3 4  ",
    "1 2 3",
    "",
    "a b c d e",
  ])
    cases.push({
      name: `tunnelCommand ${JSON.stringify(connection)}`,
      input: { sshConnection: connection, user: "alice", port: 4320 },
      output: tunnelCommand(connection, "alice", 4320) ?? null,
    });
  for (const [name, value, ssh] of [
    ["local", session(4320), undefined],
    ["hosts", session(4320, { allowHosts: ["a.ts.net"] }), undefined],
    [
      "ssh and hosts",
      session(4400, { allowHosts: ["a.ts.net", "b.example.com"] }),
      "1.2.3.4 5 6.7.8.9 22",
    ],
    ["malformed ssh", session(4320), "x"],
  ] as const)
    cases.push({
      name: `urlText ${name}`,
      input: { session: value, sshConnection: ssh ?? null, user: "<user>" },
      output: urlText(value, ssh),
    });

  // Allow-host names.
  for (const host of [
    "myserver.tailnet.ts.net",
    "a.b",
    "localhost",
    "Example.com",
    "example.com.",
    "-a.com",
    "a-.com",
    "a--b.com",
    "1.2.3.4",
    "1.2.3.com",
    "a.123",
    "a.b1",
    "a.1b",
    "*.ts.net",
    "a_b.com",
    "xn--bcher-kva.example",
    "a.com:443",
    "https://a.com",
    "",
    "a".repeat(63) + ".com",
    "a".repeat(64) + ".com",
    `${"a.".repeat(125)}com`,
    `${"a.".repeat(126)}com`,
    "é.com",
  ])
    cases.push({
      name: `validAllowHost ${JSON.stringify(host.length > 40 ? `${host.slice(0, 20)}…(${host.length})` : host)}`,
      input: host,
      output: validAllowHost(host),
    });

  // Data directory resolution against the home directory.
  for (const override of [
    undefined,
    "",
    "/srv/nv",
    "relative/nv",
    "/srv/nv/",
    "/srv/../srv/nv",
    "~/nv",
    ".",
  ])
    cases.push({
      name: `dataDirectory ${JSON.stringify(override ?? null)}`,
      input: {
        override: override ?? null,
        home: "<home>",
        cwd: "<repository>",
      },
      output: dataDirectory(override),
    });
  placeholders
    .add(repository, "<repository>")
    .add(process.env.HOME ?? "", "<home>");

  // Release channels.
  for (const version of [
    "0.2.0",
    "1.0.0-beta.1",
    "1.0.0-beta.0",
    "01.0.0",
    "1.0",
    "1.0.0-beta",
    "1.0.0-beta.01",
    "1.0.0-rc.1",
    "v1.0.0",
    "1.0.0+build",
    "10.20.30",
    " 1.0.0",
    "1.0.0-beta.1.2",
  ])
    cases.push({
      name: `releaseChannel ${version}`,
      input: version,
      output: releaseChannel(version) ?? null,
    });

  // The desktop opens only HTTPS provider pages on these hosts (safeExternal in
  // src/desktop/main.ts, not importable without Electron): the allowlist, and how
  // the WHATWG URL parser reads each candidate.
  const hosts = [
    ...new Set(
      Object.values<ProviderEntry>(providers).flatMap((p) => p.hosts ?? []),
    ),
  ];
  cases.push({ name: "external hosts", input: null, output: hosts });
  for (const raw of [
    "https://claude.ai/login",
    "https://CLAUDE.AI/x",
    "http://claude.ai/",
    "https://user@claude.ai/",
    "https://claude.ai.evil.com/",
    "https://evil.com/?https://claude.ai",
    "https://claude.ai:8443/x",
    "https://xn--clude-gra.ai/",
    "https://chatgpt.com/#a",
    "javascript:alert(1)",
    "not a url",
    "https://accounts.google.com/o/oauth2",
  ])
    cases.push(
      await outcome(`external url ${raw}`, raw, () => {
        const url = new URL(raw);
        return {
          protocol: url.protocol,
          username: url.username,
          password: url.password,
          hostname: url.hostname,
          href: url.href,
          allowed:
            url.protocol === "https:" &&
            !url.username &&
            !url.password &&
            hosts.includes(url.hostname),
        };
      }),
    );
  cases.push({ name: "providers table", input: null, output: providers });
  return placeholders.apply(cases);
}
