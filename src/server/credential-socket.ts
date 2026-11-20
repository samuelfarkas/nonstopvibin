import http from "node:http";
import { connect } from "node:net";
import { chmod, lstat, rm } from "node:fs/promises";
import { z } from "zod";
import type { Profile } from "../shared/types.ts";
import { AppError } from "./errors.ts";
import { isMissing } from "./safe-files.ts";
import { shellQuote } from "./agent-connection.ts";
import { clientKey, clientKeyPattern, clientKeySource } from "./store.ts";

// The private credential protocol between AgentSetup's Unix socket and the code it
// generates: GET /profiles/<profile id> with Host localhost, answered with the
// profile's client key and the gateway port on two lines. Never a shell program or
// a management token. The server and both clients below are built from these names.

const host = "localhost";
const profilesPath = "/profiles/";
const separator = "\n";

/** Serves the protocol on `path`, replacing a stale socket an earlier run left. */
export async function serveCredentials(
  path: string,
  connection: (profileId: string) => { key: string; port: number },
): Promise<http.Server> {
  await removeStaleSocket(path);
  const server = http.createServer({ maxHeaderSize: 4096 }, (req, res) => {
    res.setHeader("Cache-Control", "no-store");
    res.setHeader("Content-Type", "text/plain");
    res.setHeader("Connection", "close");
    const url = req.url ?? "";
    const id = url.startsWith(profilesPath)
      ? url.slice(profilesPath.length)
      : "";
    if (
      req.method !== "GET" ||
      req.headers.host !== host ||
      req.headers.origin ||
      !/^[a-f0-9-]{36}$/.test(id) ||
      !z.string().uuid().safeParse(id).success
    ) {
      res.writeHead(403).end("Agent connection request rejected.");
      return;
    }
    try {
      const { key, port } = connection(id);
      if (
        !clientKeyPattern.test(key) ||
        !Number.isInteger(port) ||
        port < 1 ||
        port > 65535
      )
        throw new Error("Invalid agent connection.");
      res.end(`${key}${separator}${port}`);
    } catch {
      res.writeHead(409).end("Open nonstopvibin and start this profile.");
    }
  });
  server.headersTimeout = 5000;
  server.requestTimeout = 5000;
  server.setTimeout(5000, (socket) => socket.destroy());
  try {
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(path, resolve);
    });
    await chmod(path, 0o600);
    return server;
  } catch (error) {
    server.close();
    throw error;
  }
}

async function removeStaleSocket(path: string): Promise<void> {
  let status;
  try {
    status = await lstat(path);
  } catch (error) {
    if (isMissing(error)) return;
    throw error;
  }
  if (!status.isSocket() || status.uid !== process.getuid?.())
    throw new AppError(
      "The agent connection path is occupied by an unexpected file.",
    );
  const active = await new Promise<boolean>((resolve, reject) => {
    const client = connect(path);
    client.setTimeout(1000, () => {
      client.destroy();
      reject(new AppError("The existing agent connection did not respond."));
    });
    client.once("connect", () => {
      client.destroy();
      resolve(true);
    });
    client.once("error", (error: NodeJS.ErrnoException) => {
      client.destroy();
      if (error.code === "ECONNREFUSED" || error.code === "ENOENT")
        resolve(false);
      else reject(error);
    });
  });
  if (active)
    throw new AppError(
      "Another nonstopvibin instance is serving these agent connections.",
      409,
    );
  await rm(path, { force: true });
}

// Called by the harness's native credential hook, never as an agent launcher.
// The installed URL stays fixed; a port change must fail before returning a key.
export function credentialHelper(
  profile: Profile,
  socket: string,
  port: number,
): string {
  return `#!/bin/sh
set -eu
nv_connection=$(curl -q --fail --silent --show-error --noproxy '*' --proxy '' --max-time 4 --unix-socket ${shellQuote(socket)} ${shellQuote(`http://${host}${profilesPath}${profile.id}`)}) || {
  printf '%s\\n' ${shellQuote(`Open nonstopvibin and start ${profile.slug}.`)} >&2
  exit 1
}
nv_key=\${nv_connection%%${shellQuote(separator)}*}
nv_port=\${nv_connection#*${shellQuote(separator)}}
case "$nv_key" in ${clientKey.prefix}*) ;; *) exit 1 ;; esac
case "$nv_key" in *[!${clientKey.characters}]*) exit 1 ;; esac
[ "\${#nv_key}" -eq ${clientKey.prefix.length + clientKey.length} ] || exit 1
[ "$nv_port" = ${shellQuote(String(port))} ] || {
  printf '%s\\n' 'The gateway port changed. Reconnect this agent in nonstopvibin.' >&2
  exit 1
}
printf '%s\\n' "$nv_key"
`;
}

/**
 * JavaScript source of `function credential()`, which resolves to the profile's key
 * or rejects with `failure` (a JavaScript expression) when the reply is refused,
 * malformed or for another port.
 */
export function credentialClient(
  profile: Profile,
  socket: string,
  port: number,
  failure: string,
): string {
  const reply = new RegExp(`^${clientKeySource}${separator}([0-9]{1,5})$`);
  return `function credential() {
  return new Promise((resolve, reject) => {
    const failure = () => ${failure};
    const request = http.get({
      socketPath: ${JSON.stringify(socket)},
      path: ${JSON.stringify(`${profilesPath}${profile.id}`)},
      headers: { Host: ${JSON.stringify(host)} },
    }, (response) => {
      let body = "";
      response.setEncoding("utf8");
      response.on("data", (chunk) => {
        body += chunk;
        if (body.length > 128) request.destroy(failure());
      });
      response.on("error", () => reject(failure()));
      response.on("end", () => {
        const match = ${reply}.exec(body);
        if (response.statusCode !== 200 || !match || match[1] !== ${JSON.stringify(String(port))}) reject(failure());
        else resolve(body.split(${JSON.stringify(separator)})[0]);
      });
    });
    const timer = setTimeout(() => request.destroy(failure()), 5000);
    request.on("close", () => clearTimeout(timer));
    request.on("error", () => reject(failure()));
  });
}`;
}
