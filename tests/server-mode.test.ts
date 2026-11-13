import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { chmod, mkdtemp, mkdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createHash } from "node:crypto";
import { Application, validAllowHost } from "../src/server/server.ts";
import { CorePool } from "../src/server/core.ts";
import {
  parseHost,
  readSession,
  removeSession,
  renderUnit,
  sessionPath,
  startServer,
  stopServer,
  tunnelCommand,
  updateUnit,
} from "../src/server/cli.ts";

const HOST = "box.tailnet.ts.net";
const binary = resolve(".vendor/core/cli-proxy-api");
let directory: string;
let client: string;
let app: Application;

before(async () => {
  directory = await mkdtemp(join(tmpdir(), "nv-server-mode-"));
  client = join(directory, "client");
  await mkdir(client);
  await writeFile(join(client, "index.html"), "fixture page");
  app = await Application.create({
    directory: join(directory, "data"),
    binary,
    clientDirectory: client,
    port: 0,
    allowHosts: [HOST],
  });
});
after(async () => {
  await app?.close();
  await rm(directory, { recursive: true, force: true });
});

// fetch() owns the Host header; raw HTTP puts the proxy's Host on the wire.
function send(
  target: Application,
  path: string,
  headers: http.OutgoingHttpHeaders,
  method = "GET",
  body?: string,
): Promise<number> {
  return new Promise((resolve, reject) => {
    const request = http.request(
      { host: "127.0.0.1", port: target.port, path, method, headers },
      (response) => {
        response.resume();
        resolve(response.statusCode!);
      },
    );
    request.once("upgrade", (response, socket) => {
      socket.destroy();
      resolve(response.statusCode!);
    });
    request.once("error", reject);
    request.end(body);
  });
}
const auth = () => ({ Authorization: `Bearer ${app.token}` });

test("an allowed host reaches the UI and management only over its HTTPS origin", async () => {
  assert.equal(await send(app, "/", { Host: HOST }), 200);
  assert.equal(await send(app, "/api/state", { Host: HOST, ...auth() }), 200);
  assert.equal(await send(app, "/api/state", { Host: HOST }), 401);
  const create = (origin: string) =>
    send(
      app,
      "/api/profiles",
      {
        Host: HOST,
        Origin: origin,
        "Content-Type": "application/json",
        ...auth(),
      },
      "POST",
      JSON.stringify({ name: "Over HTTPS" }),
    );
  assert.equal(await create(`https://${HOST}`), 201);
  assert.equal(await create(`http://${HOST}`), 403);
  assert.equal(await create("https://evil.example"), 403);
  assert.equal(await create(app.origin), 403);
  assert.equal(await create(`https://${HOST}:8443`), 403);
});

test("lookalike, suffixed, ported and differently cased hosts are rejected", async () => {
  for (const host of [
    `${HOST}.evil.example`,
    `evil${HOST}`,
    `x.${HOST}`,
    `${HOST}:443`,
    HOST.toUpperCase(),
  ])
    assert.equal(
      await send(app, "/api/state", { Host: host, ...auth() }),
      403,
      host,
    );
});

test("the agent gateway and WebSockets stay loopback-only for an allowed host", async () => {
  const key = { Authorization: "Bearer anything" };
  assert.equal(await send(app, "/v1/models", { Host: HOST, ...key }), 403);
  assert.equal(
    await send(app, "/p/some-profile/v1/models", { Host: HOST, ...key }),
    403,
  );
  assert.equal(
    await send(app, "/p/some-profile/v1/responses", {
      Host: HOST,
      Connection: "Upgrade",
      Upgrade: "websocket",
      "Sec-WebSocket-Key": Buffer.alloc(16).toString("base64"),
      "Sec-WebSocket-Version": "13",
      ...key,
    }),
    403,
  );
  // The loopback gateway still answers (and rejects the unknown key itself).
  assert.equal(
    await send(app, "/v1/models", { Host: `127.0.0.1:${app.port}`, ...key }),
    401,
  );
});

test("without --allow-host the proxy host is rejected", async () => {
  const plain = await Application.create({
    directory: join(directory, "plain"),
    binary,
    clientDirectory: client,
    port: 0,
  });
  try {
    assert.equal(await send(plain, "/", { Host: HOST }), 403);
    assert.equal(
      await send(plain, "/api/state", {
        Host: HOST,
        Authorization: `Bearer ${plain.token}`,
      }),
      403,
    );
  } finally {
    await plain.close();
  }
});

test("allowed hosts must be exact DNS names", () => {
  assert.ok(validAllowHost(HOST));
  for (const host of [
    "1.2.3.4",
    "0x7f.1",
    "localhost",
    "*.ts.net",
    "box.ts.net:443",
    "[::1]",
    "Box.ts.net",
    "box..ts.net",
    "-box.ts.net",
    "box.ts.net.",
    "https://box.ts.net",
  ])
    assert.equal(validAllowHost(host), false, host);
  assert.equal(parseHost(" Box.Tailnet.TS.net "), HOST);
  assert.throws(() => parseHost("box.ts.net:443"), /exact DNS name/);
});

test("server.json is owner-only, discoverable, guarded and pid-scoped", async () => {
  const data = join(directory, "session");
  const options = {
    directory: data,
    binary,
    clientDirectory: client,
    port: 0,
    allowHosts: [HOST],
  };
  const server = await startServer(options);
  try {
    assert.equal((await stat(sessionPath(data))).mode & 0o777, 0o600);
    const session = await readSession(data);
    assert.equal(session?.pid, process.pid);
    assert.equal(session?.port, server.port);
    assert.equal(session?.token, server.token);
    assert.deepEqual(session?.allowHosts, [HOST]);
    await chmod(sessionPath(data), 0o644);
    await assert.rejects(readSession(data), /owner-only/);
    await chmod(sessionPath(data), 0o600);
    await assert.rejects(
      startServer(options),
      new RegExp(`already running on 127\\.0\\.0\\.1:${server.port}`),
    );
    await removeSession(data, process.pid + 1);
    assert.ok(await readSession(data), "another pid must not remove it");
  } finally {
    await stopServer(server, data);
  }
  assert.equal(await readSession(data), undefined);
  await assert.rejects(
    startServer({ ...options, binary: join(directory, "missing-core") }),
    /proxy core is missing/,
  );
});

test("unit files quote paths and environment values for systemd", () => {
  const unit = renderUnit(
    "/home/a b/.local/lib/nonstopvibin/nonstopvibin",
    ["serve", "--port", "4320", "--allow-host", HOST],
    [["CODEX_HOME", `/home/a b/50%$HOME"\\x`]],
  );
  assert.match(
    unit,
    /^ExecStart="\/home\/a b\/\.local\/lib\/nonstopvibin\/nonstopvibin" "serve" "--port" "4320" "--allow-host" "box\.tailnet\.ts\.net"$/m,
  );
  assert.ok(
    unit.includes(`Environment="CODEX_HOME=/home/a b/50%%$HOME\\"\\\\x"\n`),
  );
  assert.doesNotMatch(unit, /^(PrivateTmp|ProtectHome|ProtectSystem)=/m);
  assert.throws(() => renderUnit("/bin/x", ["a\nb"], []), /control character/);
});

test("tunnel commands use the server's own port on both ends", () => {
  assert.equal(
    tunnelCommand("203.0.113.9 50000 192.0.2.10 22", "nv", 4320),
    "ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:4320:127.0.0.1:4320 nv@192.0.2.10",
  );
  assert.equal(
    tunnelCommand("2001:db8::9 50000 2001:db8::10 2222", "nv", 4321),
    "ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:4321:127.0.0.1:4321 -p 2222 nv@2001:db8::10",
  );
  assert.equal(
    tunnelCommand("::ffff:198.51.100.1 1 ::ffff:192.0.2.10 22", "nv", 4320),
    "ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:4320:127.0.0.1:4320 nv@192.0.2.10",
  );
  assert.equal(tunnelCommand("", "nv", 4320), undefined);
});

test("a core matching only its adjacent manifest fails the compiled-in pin", async () => {
  const core = join(directory, "swapped-core");
  await mkdir(core);
  const bytes = Buffer.from("#!/bin/sh\necho replaced\n");
  await writeFile(join(core, "cli-proxy-api"), bytes);
  await writeFile(
    join(core, "manifest.json"),
    JSON.stringify({
      binarySha256: createHash("sha256").update(bytes).digest("hex"),
    }),
  );
  await assert.rejects(
    new CorePool(app.store, join(core, "cli-proxy-api")).verifyBinary(true),
    /integrity check/,
  );
  // Desktop signing legitimately rewrites the binary and the adjacent
  // manifest; preserve that pre-existing validation path explicitly.
  await new CorePool(app.store, join(core, "cli-proxy-api")).verifyBinary();
  await assert.rejects(
    startServer({
      directory: join(directory, "pinned-server"),
      binary: join(core, "cli-proxy-api"),
      clientDirectory: client,
      port: 0,
      allowHosts: [],
    }),
    /integrity check/,
  );
  await writeFile(join(core, "cli-proxy-api"), "different bytes");
  await assert.rejects(
    new CorePool(app.store, join(core, "cli-proxy-api")).verifyBinary(),
    /integrity check/,
  );
  await new CorePool(app.store, binary).verifyBinary(true);
});

test("service updates preserve custom paths, arguments and agent directories", () => {
  const previous = renderUnit(
    "/opt/custom $&path/50%/nonstopvibin",
    ["serve", "--port", "4399", "--allow-host", HOST],
    [
      ["NONSTOPVIBIN_DATA_DIR", "/home/nv/custom data"],
      ["CODEX_HOME", "/home/nv/custom codex"],
    ],
  );
  assert.equal(updateUnit(previous, undefined, undefined, []), previous);
  const portOnly = updateUnit(previous, 4400, undefined, []);
  assert.ok(
    portOnly.includes('ExecStart="/opt/custom $&path/50%%/nonstopvibin"'),
  );
  assert.ok(portOnly.includes(`"--allow-host" "${HOST}"`));
  assert.ok(portOnly.includes('"--port" "4400"'));
  assert.ok(
    portOnly.includes(
      'Environment="NONSTOPVIBIN_DATA_DIR=/home/nv/custom data"',
    ),
  );
  assert.ok(
    portOnly.includes('Environment="CODEX_HOME=/home/nv/custom codex"'),
  );
  const hostOnly = updateUnit(
    previous,
    undefined,
    ["other.ts.net"],
    [["CODEX_HOME", "/new/codex"]],
  );
  assert.ok(hostOnly.includes('"--port" "4399"'));
  assert.ok(hostOnly.includes('"--allow-host" "other.ts.net"'));
  assert.ok(!hostOnly.includes(HOST));
  assert.ok(hostOnly.includes('Environment="CODEX_HOME=/new/codex"'));
});
