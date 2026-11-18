import { createHash } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { coreLayout } from "../../src/server/core-binary.ts";

/**
 * Stays alive until SIGTERM, as a core does; with nothing else it is running but unreachable.
 * The watchdog only guards a core whose command line is its binary's, which a script run by
 * Bun is not, so the fake ends itself once the test process that started it is gone.
 */
export const idleCore = `process.on("SIGTERM", () => process.exit(0));
setInterval(() => process.ppid === 1 && process.exit(0), 1000);
`;
/**
 * A core that listens on the port its config names and answers management as a running
 * core with no subscriptions and an empty usage queue. `handle` answers every other
 * request; `server` is in scope for `extra`.
 */
export const listeningCore = (
  handle = "(_req, res) => res.writeHead(404).end()",
  extra = "",
) => `${idleCore}const config = process.argv[process.argv.indexOf("--config") + 1];
const port = Number(/^port: (\\d+)$/m.exec(require("node:fs").readFileSync(config, "utf8"))[1]);
const server = require("node:http").createServer((req, res) => {
  const path = req.url.replace("/v0/management", "");
  if (path === req.url) return (${handle})(req, res);
  res.setHeader("Content-Type", "application/json");
  res.end(JSON.stringify(path === "/auth-files" ? { files: [] } : path.startsWith("/usage-queue") ? [] : {}));
});
${extra}
server.listen(port, "127.0.0.1");
`;
/**
 * Writes `source` as a core binary run by this Bun, with the manifest an unpinned core's
 * integrity check compares; returns the binary's path.
 */
export async function installFakeCore(
  directory: string,
  source: string,
): Promise<string> {
  await mkdir(directory, { recursive: true });
  const binary = join(directory, coreLayout.binary);
  const script = `#!${process.execPath}\n${source}`;
  await writeFile(binary, script, { mode: 0o700 });
  await writeFile(
    join(directory, coreLayout.manifest),
    JSON.stringify({
      binarySha256: createHash("sha256").update(script).digest("hex"),
    }),
  );
  return binary;
}
