// Setup the server suites share: the verified core, a temporary store, an application and a
// synthetic API account, loopback servers, management requests, and an application
// teardown that names stuck cores.
import type { Server } from "node:http";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { Json } from "../../src/shared/types.ts";
import { coreLayout } from "../../src/server/core-binary.ts";
import type { CorePool } from "../../src/server/core.ts";
import { checkoutLayout } from "../../src/server/host-defaults.ts";
import { Application } from "../../src/server/server.ts";
import { Store, type ApiAccount } from "../../src/server/store.ts";
import { fileKeyCodec } from "../../src/server/vault.ts";

/** The pinned core `bun run core:install` verified into the checkout. */
export const coreBinary = resolve(coreLayout.checkout, coreLayout.binary);
/** The client `bun run build` writes. */
export const clientDirectory = resolve(checkoutLayout.client);

/** A store in a fresh temporary directory; `remove` closes it and deletes the directory. */
export async function temporaryStore(prefix: string) {
  const directory = await mkdtemp(join(tmpdir(), prefix));
  const store = new Store(directory, fileKeyCodec(directory));
  return {
    directory,
    store,
    remove: async () => {
      store.close();
      await rm(directory, { recursive: true, force: true });
    },
  };
}

/** An application on `directory` with the verified core and the built client on a free port. */
export function testApplication(
  directory: string,
  options: Partial<Parameters<typeof Application.create>[0]> = {},
): Promise<Application> {
  return Application.create({
    directory,
    agentHome: join(directory, "agent-home"),
    binary: coreBinary,
    clientDirectory,
    port: 0,
    ...options,
  });
}

/** A compatible-API account without a prefix, named by its ID unless `name` is given. */
export function customAccount(
  id: string,
  baseUrl: string,
  models: ApiAccount["models"],
  name = id,
): ApiAccount {
  return {
    id,
    name,
    provider: "custom",
    baseUrl,
    prefix: "",
    disabled: false,
    models,
  };
}

/** Listens on a free loopback port and returns it. */
export async function listen(server: Server): Promise<number> {
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  const address = server.address();
  if (!(address instanceof Object)) throw new Error("The server has no port.");
  return address.port;
}
export async function closeServer(server: Server): Promise<void> {
  await new Promise<void>((done) => {
    server.close(() => done());
    server.closeAllConnections();
  });
}

/** A management API request with the application's token unless another is given. */
export function management(
  app: Application,
  path: string,
  {
    method = "GET",
    body,
    token = app.token,
  }: {
    method?: string;
    body?: Json;
    token?: string;
  } = {},
): Promise<Response> {
  return fetch(`${app.origin}/api${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

/**
 * Closes `app` and removes its agent socket directory. A core may need its usage drain
 * plus the 5 s SIGTERM grace (timing.kill) to stop; this fails after 25 s naming the cores
 * still running, below the 30 s hooks that call it, whose own timeout names nothing.
 */
export async function closeApplication(app: Application): Promise<void> {
  await settle("app.close()", app.close(), app.cores.pool, 25_000);
  await rm(app.agentSetup.socketDirectory, { recursive: true, force: true });
}

async function settle(
  step: string,
  operation: Promise<unknown>,
  core: CorePool,
  limit: number,
): Promise<void> {
  let timer: NodeJS.Timeout | undefined;
  const expired = new Promise<never>((_, reject) => {
    timer = setTimeout(
      () =>
        reject(
          new Error(
            `${step} was still waiting after ${limit} ms: ${pending(core)}`,
          ),
        ),
      limit,
    );
  });
  try {
    await Promise.race([operation, expired]);
  } finally {
    clearTimeout(timer);
  }
}

function pending(core: CorePool): string {
  const alive = [...core.runtimes].filter(([id]) => core.alive(id));
  if (!alive.length) return "every core has exited, so not on a core.";
  return alive
    .map(([id, runtime]) => {
      let name = id;
      try {
        name = core.store.profile(id).name;
      } catch {
        // A deleted profile or a closed store leaves the ID.
      }
      const phase = runtime.child.killed
        ? "told to exit, waiting for it to"
        : "not told to exit yet (usage drain, or an earlier shutdown step)";
      return `${name} core pid ${runtime.child.pid} (${runtime.state}${runtime.stopping ? ", stopping" : ""}): ${phase}`;
    })
    .join("; ");
}
