import { UsagePrices } from "./usage-pricing.ts";
import http, { type IncomingMessage, type ServerResponse } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { existsSync } from "node:fs";
import { extname, resolve } from "node:path";
import { randomBytes, randomUUID } from "node:crypto";
import { z } from "zod";
import type { AppState, Json, ProfileState } from "../shared/types.ts";
import type {
  ApiError,
  RouteBody,
  RouteKey,
  RouteParamName,
  RouteQuery,
  RouteResponse,
  SessionRefused,
} from "../shared/api.ts";
import {
  apiKeyProviders,
  oauthProviders,
  protocols,
} from "../shared/providers.ts";
import { agents } from "../shared/agents.ts";
import {
  allHistoryDays,
  nameLength,
  prefixLength,
  priorityLimit,
  retentionDays,
  strategies,
} from "../shared/vocabulary.ts";
import { Store, sameSecret } from "./store.ts";
import { ownerOnlyDirectory } from "./data-directory.ts";
import { offeredModels } from "./agents.ts";
import { profileEndpoint } from "./agent-connection.ts";
import { fileKeyCodec } from "./vault.ts";
import { Cores, type CoresOptions } from "./cores.ts";
import { coreVersion, verifyCore } from "./core-binary.ts";
import { Gateway } from "./gateway.ts";
import { ProfileModels } from "./model-catalog.ts";
import { devHost } from "./host-defaults.ts";
import { AppError, errorMessage } from "./errors.ts";
import { version } from "./config.ts";
import { parse, record, responseJson, text } from "./json.ts";
import { ImportCatalog } from "./imports.ts";
import {
  AgentSetup,
  agentSelectionSchema,
  agentSetupSchema,
} from "./agent-setup.ts";

const nameSchema = z.string().trim().min(1).max(nameLength);
const profileSchema = z.object({ name: nameSchema }).strict();
const patchSchema = z
  .object({
    name: nameSchema.optional(),
    strategy: z.enum(strategies).optional(),
    sessionAffinity: z.boolean().optional(),
  })
  .strict();
const apiSchema = z
  .object({
    name: nameSchema,
    provider: z.enum(apiKeyProviders),
    baseUrl: z.string().url().max(500),
    apiKey: z.string().trim().min(1).max(20_000),
    protocol: z.enum(protocols).default("openai"),
    prefix: z
      .string()
      .regex(/^[a-z0-9-]*$/)
      .max(prefixLength),
    models: z
      .array(
        z.object({
          id: z.string().trim().min(1).max(200),
          protocol: z.enum(protocols),
        }),
      )
      .max(500),
  })
  .strict();
async function jsonBody(req: IncomingMessage): Promise<Json> {
  if (!req.headers["content-type"]?.startsWith("application/json"))
    throw new AppError("Content-Type must be application/json.", 415);
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > 2_097_152) throw new AppError("Payload exceeds 2 MB.", 413);
    chunks.push(Buffer.from(chunk));
  }
  try {
    return parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    throw new AppError("Invalid JSON.");
  }
}
const mime = new Map([
  [".html", "text/html"],
  [".js", "text/javascript"],
  [".css", "text/css"],
  [".png", "image/png"],
  [".svg", "image/svg+xml"],
  [".woff2", "font/woff2"],
]);
function json<T>(res: ServerResponse, value: T, status = 200): void {
  res.writeHead(status, {
    "Content-Type": "application/json",
    "Cache-Control": "no-store",
    "X-Content-Type-Options": "nosniff",
  });
  res.end(JSON.stringify(value));
}
type Awaitable<T> = T | Promise<T>;
interface RouteCall<K extends RouteKey> {
  param(name: RouteParamName<K>): string;
  query: RouteQuery<K>;
  body: RouteBody<K>;
}
/** Parses a route's query, reading each parameter (its first value, or null) by the contract's name. */
type QueryParser<K extends RouteKey> = (
  get: (name: Extract<keyof RouteQuery<K>, string>) => string | null,
) => RouteQuery<K>;
interface RouteDefinition<K extends RouteKey> {
  body: z.ZodType<RouteBody<K>>;
  /** Runs after the body is parsed, before the handler. */
  query: QueryParser<K>;
  handle: (call: RouteCall<K>) => Awaitable<RouteResponse<K>>;
  status?: number;
}
type RouteTable = { [K in RouteKey]: RouteDefinition<K> };
/** The body schema of a route that takes none; its request body is never read. */
const noBody: z.ZodType<undefined> = z.undefined();
/** The query parser of a route that takes none; its query is ignored. */
const noQuery = () => undefined;
interface Endpoint {
  method: string;
  match(
    pathname: string,
    segments: string[],
  ): Record<string, string> | undefined;
  serve(
    req: IncomingMessage,
    res: ServerResponse,
    params: Record<string, string>,
    search: URLSearchParams,
  ): Promise<void>;
}
function endpoint<K extends RouteKey>(key: K, table: RouteTable): Endpoint {
  const definition = table[key];
  const [method, path] = key.split(" ");
  const pattern = ["api", ...path.split("/").filter(Boolean)];
  return {
    method,
    // A route without parameters matches its raw path exactly; one with
    // parameters matches the decoded segments, which skip empty ones.
    match: pattern.some((part) => part.startsWith(":"))
      ? (_, segments) => {
          if (segments.length !== pattern.length) return undefined;
          const params: Record<string, string> = {};
          for (const [index, part] of pattern.entries()) {
            if (part.startsWith(":")) params[part.slice(1)] = segments[index];
            else if (part !== segments[index]) return undefined;
          }
          return params;
        }
      : (pathname) => (pathname === `/api${path}` ? {} : undefined),
    async serve(req, res, params, search) {
      const body = definition.body.parse(
        definition.body === noBody ? undefined : await jsonBody(req),
      );
      const query = definition.query((name) => search.get(name));
      json(
        res,
        await definition.handle({ param: (name) => params[name], query, body }),
        definition.status,
      );
    },
  };
}
// An exact DNS name such as a Tailscale Serve host: lowercase labels, at least
// one dot, an alphabetic top-level label (so no IP literals), no port or wildcard.
export function validAllowHost(host: string): boolean {
  return (
    host.length <= 253 &&
    /^(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(
      host,
    )
  );
}
interface ServerOptions {
  directory: string;
  binary: string;
  clientDirectory: string;
  port: number;
  development?: boolean;
  agentHome?: string;
  // Extra Host names for the management UI only (HTTPS reverse proxy such as
  // Tailscale Serve). The agent gateway stays loopback-only.
  allowHosts?: string[];
  requirePinnedCore?: boolean;
  /** Starts without a core binary, so development and tests run without one; profiles then cannot start. */
  allowMissingCore?: boolean;
  /** Runs once after the application closed, even if closing failed: the host's own resources. */
  afterClose?: () => Promise<void>;
  /** How the profile cores are reached and timed; tests answer and shorten them. */
  cores?: Omit<CoresOptions, "report" | "onStatusChange" | "onSignInCallback">;
}
export interface HostStatus {
  /** Names of the profiles whose core runs, in profile order. */
  running: string[];
  /** Whether any running profile has a gateway request or WebSocket open. */
  streaming: boolean;
}
/** `status`: `status()` may have changed. `sign-in`: a browser sign-in callback was accepted. */
export type HostEvent = "status" | "sign-in";
export class Application {
  readonly options: ServerOptions;
  readonly store: Store;
  readonly cores: Cores;
  readonly gateway: Gateway;
  readonly server: http.Server;
  readonly token = randomBytes(32).toString("base64url");
  // ASAR timestamps are fabricated; revalidate unhashed files after each launch.
  private readonly assetVersion = randomUUID();
  readonly imports = new ImportCatalog();
  readonly usagePrices = new UsagePrices();
  readonly models: ProfileModels;
  readonly agentSetup: AgentSetup;
  private readonly endpoints: Endpoint[];
  port = 0;
  closing = false;
  private closePromise?: Promise<void>;
  private reported: string[] = [];
  private readonly listeners: Record<HostEvent, Set<() => void>> = {
    status: new Set(),
    "sign-in": new Set(),
  };
  constructor(options: ServerOptions, store: Store) {
    this.options = options;
    this.store = store;
    this.cores = new Cores(store, options.binary, {
      ...options.cores,
      // The scheduler drains slowly while the gateway still has requests open.
      inFlight: (profileId) => this.gateway.active.get(profileId) ?? 0,
      report: (message) => this.report(message),
      onStatusChange: () => this.emit("status"),
      onSignInCallback: () => this.emit("sign-in"),
    });
    this.models = new ProfileModels(store);
    this.gateway = new Gateway(store, this.cores.pool, this.cores.usage, {
      models: this.models,
      onActivityChange: () => this.emit("status"),
    });
    this.agentSetup = new AgentSetup(
      options.directory,
      (profileId) => {
        this.store.profile(profileId);
        if (this.closing || !this.cores.pool.running(profileId))
          throw new AppError(
            "Start this profile before connecting an agent.",
            409,
          );
        return { key: this.store.clientKey(profileId), port: this.port };
      },
      options.agentHome ? { home: options.agentHome } : undefined,
    );
    const routes = this.routes();
    this.endpoints = Object.keys(routes)
      .filter((key): key is RouteKey => Object.hasOwn(routes, key))
      .map((key) => endpoint(key, routes));
    this.server = http.createServer((req, res) => {
      this.handle(req, res).catch((error) => {
        // The client hung up mid-body, as the gateway treats it: nothing failed in the app.
        if (req.readableAborted && !req.complete) {
          res.destroy();
          return;
        }
        if (res.headersSent) {
          res.destroy();
          return;
        }
        const status =
          error instanceof AppError
            ? error.status
            : error instanceof z.ZodError
              ? 400
              : 500;
        if (status === 429) res.setHeader("Retry-After", "2");
        const message =
          error instanceof z.ZodError
            ? error.issues
                .map((i) => `${i.path.join(".")}: ${i.message}`)
                .join("; ")
            : error instanceof AppError
              ? error.message
              : "The operation failed. Check the profile status and try again.";
        if (status === 500) this.report(errorMessage(error));
        if (res.destroyed) return;
        json<ApiError>(
          res,
          { error: { message, type: "nonstopvibin_error", code: status } },
          status,
        );
      });
    });
    this.server.requestTimeout = 0;
    this.server.headersTimeout = 60_000;
    this.server.on("upgrade", (req, socket, head) => {
      if (!this.validHost(req) || this.allowedHost(req) || req.headers.origin) {
        socket.end(
          "HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        );
        return;
      }
      this.gateway.upgrade(req, socket, head);
    });
  }
  static async create(
    options: ServerOptions,
    signal?: AbortSignal,
  ): Promise<Application> {
    signal?.throwIfAborted();
    for (const host of options.allowHosts ?? [])
      if (!validAllowHost(host))
        throw new Error(`Invalid allowed host: ${JSON.stringify(host)}`);
    ownerOnlyDirectory(options.directory);
    signal?.throwIfAborted();
    const store = new Store(options.directory, fileKeyCodec(options.directory));
    const app = new Application(options, store);
    try {
      store.pruneUsage();
      await new Promise<void>((resolve, reject) => {
        app.server.once("error", reject);
        app.server.listen(options.port, "127.0.0.1", resolve);
      });
      signal?.throwIfAborted();
      const address = app.server.address();
      if (!(address instanceof Object))
        throw new Error("Gateway failed to bind.");
      app.port = address.port;
      if (!options.allowMissingCore || existsSync(options.binary))
        await verifyCore(options.binary, options.requirePinnedCore);
      signal?.throwIfAborted();
      await app.cores.restore(signal);
      signal?.throwIfAborted();
      try {
        for (const failure of await app.agentSetup.restore(
          store.profiles(),
          app.port,
        ))
          app.report(failure);
      } catch {
        app.report(
          "Agent connections are unavailable. Open Connect agents and reconnect the agent.",
        );
      }
      signal?.throwIfAborted();
      return app;
    } catch (error) {
      try {
        await app.close();
      } catch (cleanupError) {
        throw new AggregateError(
          [error, cleanupError],
          `${errorMessage(error)} Cleanup failed: ${errorMessage(cleanupError)}`,
          { cause: cleanupError },
        );
      }
      throw error;
    }
  }
  /** The last five distinct errors, oldest first, for the UI. */
  get errors(): readonly string[] {
    return this.reported;
  }
  report(message: string): void {
    this.reported = [
      ...this.reported.filter((e) => e !== message),
      message,
    ].slice(-5);
  }
  on(event: HostEvent, listener: () => void): () => void {
    this.listeners[event].add(listener);
    return () => {
      this.listeners[event].delete(listener);
    };
  }
  private emit(event: HostEvent): void {
    for (const listener of this.listeners[event]) listener();
  }
  /** Read on every gateway activity edge: no query once the profile names are cached. */
  status(): HostStatus {
    const running: string[] = [];
    let streaming = false;
    for (const [id, name] of this.store.profileNames()) {
      if (!this.cores.pool.running(id)) continue;
      running.push(name);
      if ((this.gateway.active.get(id) ?? 0) > 0) streaming = true;
    }
    return { running, streaming };
  }
  hasProfile(profileId: string): boolean {
    return this.store.profileNames().has(profileId);
  }
  get origin(): string {
    return `http://127.0.0.1:${this.port}`;
  }
  endpoint(profileId: string): string {
    return profileEndpoint(this.port, this.store.profile(profileId).slug);
  }
  state(): AppState {
    const profiles: ProfileState[] = this.store.profiles().map((profile) => {
      const runtime = this.cores.pool.runtimes.get(profile.id);
      return {
        ...profile,
        runtime: runtime?.state ?? "stopped",
        error: runtime?.error,
        accounts: this.cores.accounts.list(profile.id),
        endpoint: this.endpoint(profile.id),
      };
    });
    return {
      profiles,
      coreVersion,
      gateway: this.origin,
      usageRetentionDays: this.store.usageRetentionDays(),
      version,
      errors: [...this.errors],
    };
  }
  validHost(req: IncomingMessage): boolean {
    return (
      req.headers.host === `127.0.0.1:${this.port}` ||
      req.headers.host === `localhost:${this.port}` ||
      (Boolean(this.options.development) && req.headers.host === devHost) ||
      this.allowedHost(req)
    );
  }
  private allowedHost(req: IncomingMessage): boolean {
    return this.options.allowHosts?.includes(req.headers.host ?? "") ?? false;
  }
  private async handle(
    req: IncomingMessage,
    res: ServerResponse,
  ): Promise<void> {
    if (this.closing) throw new AppError("nonstopvibin is shutting down.", 503);
    if (!this.validHost(req)) throw new AppError("Unrecognized host.", 403);
    const url = new URL(req.url ?? "/", this.origin);
    if (url.pathname.startsWith("/api/")) {
      const origin = req.headers.origin;
      if (
        origin &&
        (this.allowedHost(req)
          ? origin !== `https://${req.headers.host}`
          : origin !== this.origin &&
            !(this.options.development && origin === `http://${devHost}`))
      )
        throw new AppError(
          "Cross-origin management requests are not allowed.",
          403,
        );
      if (!sameSecret(req.headers.authorization ?? "", `Bearer ${this.token}`))
        throw new AppError(
          "Open NonstopVibin from the desktop app, or run nonstopvibin url on the server for a new session link.",
          401 satisfies SessionRefused,
        );
      await this.api(req, res, url);
      return;
    }
    if (url.pathname.startsWith("/v1") || url.pathname.startsWith("/p/")) {
      if (this.allowedHost(req))
        throw new AppError(
          "The agent API is only available on the loopback address.",
          403,
        );
      if (req.headers.origin)
        throw new AppError(
          "The agent API accepts local agent clients, not browser origins.",
          403,
        );
      await this.gateway.proxy(req, res);
      return;
    }
    if (req.method !== "GET" && req.method !== "HEAD")
      throw new AppError("Method not allowed.", 405);
    let requested;
    try {
      requested = decodeURIComponent(url.pathname);
    } catch {
      throw new AppError("Not found.", 404);
    }
    const relative =
      requested === "/" || !extname(requested)
        ? "index.html"
        : requested.replace(/^\/+/, "");
    const path = resolve(this.options.clientDirectory, relative);
    if (!path.startsWith(resolve(this.options.clientDirectory) + "/"))
      throw new AppError("Not found.", 404);
    let metadata;
    try {
      metadata = await stat(path);
      if (!metadata.isFile()) throw new Error("Not a file");
    } catch {
      throw new AppError(
        "Build the interface with bun run build before opening this page.",
        404,
      );
    }
    const etag = `W/"${this.assetVersion}-${metadata.size}-${metadata.mtimeMs}"`;
    const unchanged = req.headers["if-none-match"]
      ?.split(",")
      .some(
        (value) =>
          value.trim() === "*" ||
          value.trim().replace(/^W\//, "") === etag.slice(2),
      );
    const bytes =
      unchanged || req.method === "HEAD" ? undefined : await readFile(path);
    res.writeHead(unchanged ? 304 : 200, {
      "Content-Type": mime.get(extname(path)) ?? "application/octet-stream",
      "X-Content-Type-Options": "nosniff",
      "Referrer-Policy": "no-referrer",
      // Only Vite's fingerprinted assets survive an update without revalidation.
      "Cache-Control": /^assets\/[^/]+-[\w-]{8,}\.(js|css|woff2)$/.test(
        relative,
      )
        ? "public, max-age=31536000, immutable"
        : "no-cache",
      ETag: etag,
      "Content-Security-Policy":
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'",
    });
    res.end(bytes);
  }
  private async api(
    req: IncomingMessage,
    res: ServerResponse,
    url: URL,
  ): Promise<void> {
    // Split on raw "/" first so an encoded slash stays inside its segment.
    const segments = url.pathname
      .split("/")
      .filter(Boolean)
      .map((segment) => {
        try {
          return decodeURIComponent(segment);
        } catch {
          throw new AppError("Malformed request path.", 400);
        }
      });
    // Any profile path names a real profile before its route is chosen.
    if (segments[1] === "profiles" && segments[2])
      this.store.profile(z.string().uuid().parse(segments[2]));
    const allowed: string[] = [];
    for (const endpoint of this.endpoints) {
      const params = endpoint.match(url.pathname, segments);
      if (!params) continue;
      if (endpoint.method === req.method) {
        await endpoint.serve(req, res, params, url.searchParams);
        return;
      }
      allowed.push(endpoint.method);
    }
    if (allowed.length) {
      // RFC 9110 requires a 405 to list the methods the path does serve.
      res.setHeader("Allow", allowed.join(", "));
      throw new AppError("Method not allowed.", 405);
    }
    throw new AppError("Not found.", 404);
  }
  private routes(): RouteTable {
    return {
      "GET /state": {
        body: noBody,
        query: noQuery,
        handle: () => this.state(),
      },
      "PUT /usage-retention": {
        body: z
          .object({
            days: z.union(retentionDays.map((days) => z.literal(days))),
          })
          .strict(),
        query: noQuery,
        handle: ({ body }) => {
          this.store.setUsageRetentionDays(body.days);
          return this.state();
        },
      },
      "GET /import-sources": {
        body: noBody,
        query: noQuery,
        handle: () => this.imports.scan(),
      },
      "POST /profiles": {
        body: profileSchema,
        status: 201,
        query: noQuery,
        handle: ({ body }) => this.store.createProfile(body.name),
      },
      "PATCH /profiles/:profileId": {
        body: patchSchema,
        query: noQuery,
        handle: async ({ param, body }) => {
          const id = param("profileId");
          this.store.saveProfile({ ...this.store.profile(id), ...body });
          await this.cores.pool.configure(id);
          return this.state();
        },
      },
      "POST /profiles/:profileId/start": {
        body: noBody,
        query: noQuery,
        handle: async ({ param }) => {
          await this.cores.pool.start(param("profileId"));
          return this.state();
        },
      },
      "POST /profiles/:profileId/refresh": {
        body: noBody,
        query: noQuery,
        handle: async ({ param }) => {
          const id = param("profileId");
          await this.cores.quotas.refreshAccounts(
            id,
            (account) => !account.disabled,
          );
          return this.state();
        },
      },
      "GET /profiles/:profileId/key": {
        body: noBody,
        query: noQuery,
        handle: ({ param }) => ({
          key: this.store.clientKey(param("profileId")),
        }),
      },
      "GET /profiles/:profileId/models": {
        body: noBody,
        query: (get) => ({
          metadata: get("metadata") === "1" ? "1" : undefined,
        }),
        handle: async ({ param, query }) => {
          const id = param("profileId");
          const models = await this.cores.pool.models(id);
          return query.metadata === "1"
            ? this.models.enriched(id, models)
            : models;
        },
      },
      "GET /profiles/:profileId/usage": {
        body: noBody,
        query: (get) => ({
          days: z.coerce
            .number()
            .int()
            .min(1)
            .max(allHistoryDays)
            .optional()
            .parse(get("days") ?? undefined),
        }),
        handle: ({ param, query: { days = 7 } }) => {
          const since =
            days === allHistoryDays
              ? ""
              : new Date(Date.now() - days * 86_400_000).toISOString();
          return this.usagePrices.history(
            this.store,
            param("profileId"),
            since,
            this.models.catalog,
          );
        },
      },
      "GET /profiles/:profileId/agent-setup": {
        body: noBody,
        query: (get) =>
          agentSelectionSchema.parse({
            agent: get("agent"),
            projectDirectory: get("projectDirectory") ?? undefined,
          }),
        handle: ({ param, query }) =>
          this.agentSetup.status(
            this.store.profile(param("profileId")),
            query.agent,
            query.projectDirectory,
          ),
      },
      "POST /profiles/:profileId/agent-setup": {
        body: agentSetupSchema,
        query: noQuery,
        handle: async ({ param, body }) => {
          const id = param("profileId");
          const models = await this.cores.pool.models(id);
          if (!models.length)
            throw new AppError("This profile has no available models.");
          const offered = await offeredModels(
            body.agent,
            models,
            body.otherProviders ?? true,
            (models) => this.models.enriched(id, models),
          );
          // Only an agent with a native vendor can be left without models.
          if (!offered.length)
            throw new AppError(
              `This profile has no ${agents[body.agent].native} models. Add one, or offer models from other providers.`,
            );
          return this.agentSetup.install(
            this.store.profile(id),
            body,
            offered.map((model) => model.id),
          );
        },
      },
      "DELETE /profiles/:profileId/agent-setup": {
        body: agentSelectionSchema,
        query: noQuery,
        handle: async ({ param, body }) => {
          await this.agentSetup.remove(
            this.store.profile(param("profileId")),
            body.agent,
            body.projectDirectory,
          );
          return { ok: true };
        },
      },
      "POST /profiles/:profileId/agent-check": {
        body: z.object({}).strict(),
        query: noQuery,
        handle: async ({ param }) => {
          const id = param("profileId");
          const response = await fetch(`${this.endpoint(id)}/models`, {
            headers: {
              Authorization: `Bearer ${this.store.clientKey(id)}`,
            },
            redirect: "error",
            signal: AbortSignal.timeout(10_000),
          });
          if (!response.ok)
            throw new AppError(
              "The profile connection failed. Check that this profile is running.",
              502,
            );
          const catalog = record(await responseJson(response));
          if (
            !Array.isArray(catalog.data) ||
            !catalog.data.some((entry) => text(record(entry).id) !== undefined)
          )
            throw new AppError(
              "No models are available. Refresh the profile catalog.",
            );
          return { ok: true };
        },
      },
      "POST /profiles/:profileId/oauth": {
        body: z
          .object({
            provider: z.enum(oauthProviders.map(({ id }) => id)),
            reconnectAccountId: z.string().min(1).max(255).optional(),
          })
          .strict(),
        query: noQuery,
        handle: ({ param, body }) =>
          this.cores.signIn.begin(
            param("profileId"),
            body.provider,
            body.reconnectAccountId,
          ),
      },
      "GET /profiles/:profileId/oauth": {
        body: noBody,
        query: (get) => ({ state: z.string().min(1).parse(get("state")) }),
        handle: ({ param, query }) =>
          this.cores.signIn.status(param("profileId"), query.state),
      },
      "DELETE /profiles/:profileId/oauth": {
        body: noBody,
        query: (get) => ({ state: z.string().min(1).parse(get("state")) }),
        handle: async ({ param, query }) => {
          await this.cores.signIn.cancel(param("profileId"), query.state);
          return { ok: true };
        },
      },
      "POST /profiles/:profileId/oauth-confirm": {
        body: z.object({ state: z.string().min(1).max(500) }).strict(),
        query: noQuery,
        handle: ({ param, body }) =>
          this.cores.signIn.confirm(param("profileId"), body.state),
      },
      "GET /profiles/:profileId/oauth-session": {
        body: noBody,
        query: noQuery,
        handle: ({ param }) =>
          this.cores.signIn.session?.profileId === param("profileId")
            ? this.cores.signIn.session
            : null,
      },
      "POST /profiles/:profileId/callback": {
        body: z.object({ url: z.string().url().max(10_000) }).strict(),
        query: noQuery,
        handle: async ({ param, body }) => {
          await this.cores.signIn.callback(param("profileId"), body.url);
          return { ok: true };
        },
      },
      "POST /profiles/:profileId/import": {
        body: z.object({ contents: z.record(z.string(), z.json()) }).strict(),
        query: noQuery,
        handle: async ({ param, body }) => {
          await this.cores.credentials.import(
            param("profileId"),
            body.contents,
          );
          return this.state();
        },
      },
      "POST /profiles/:profileId/import-source": {
        body: z
          .object({ sourceId: z.string().regex(/^[a-f0-9]{64}$/) })
          .strict(),
        query: noQuery,
        handle: async ({ param, body }) => {
          await this.cores.credentials.import(
            param("profileId"),
            await this.imports.contents(body.sourceId),
          );
          return this.state();
        },
      },
      "PATCH /profiles/:profileId/accounts/:accountId": {
        body: z
          .object({
            disabled: z.boolean().optional(),
            priority: z
              .number()
              .int()
              .min(-priorityLimit)
              .max(priorityLimit)
              .optional(),
            name: nameSchema.optional(),
          })
          .strict(),
        query: noQuery,
        handle: async ({ param, body }) => {
          await this.cores.credentials.setAccount(
            param("profileId"),
            param("accountId"),
            body,
          );
          return this.state();
        },
      },
      "DELETE /profiles/:profileId/accounts/:accountId": {
        body: noBody,
        query: noQuery,
        handle: async ({ param }) => {
          await this.cores.credentials.removeAccount(
            param("profileId"),
            param("accountId"),
          );
          return this.state();
        },
      },
      "GET /profiles/:profileId/accounts/:accountId/reset-credits": {
        body: noBody,
        query: noQuery,
        handle: ({ param }) =>
          this.cores.resets.credits(param("profileId"), param("accountId")),
      },
      "POST /profiles/:profileId/accounts/:accountId/reset-credits": {
        body: z.object({ creditId: z.string().min(1).max(500) }).strict(),
        query: noQuery,
        handle: ({ param, body }) =>
          this.cores.resets.consume(
            param("profileId"),
            param("accountId"),
            body.creditId,
          ),
      },
      "POST /profiles/:profileId/api-account": {
        body: apiSchema,
        status: 201,
        query: noQuery,
        handle: async ({ param, body }) => {
          await this.cores.credentials.addApiAccount(param("profileId"), body);
          return this.state();
        },
      },
    };
  }
  close(): Promise<void> {
    return (this.closePromise ??= this.closeResources());
  }
  private async closeResources(): Promise<void> {
    this.closing = true;
    this.server.close();
    this.server.closeIdleConnections();
    try {
      await this.agentSetup.close();
    } finally {
      try {
        await this.cores.shutdown();
      } finally {
        try {
          await this.usagePrices.close();
        } finally {
          try {
            this.server.closeAllConnections();
            this.store.close();
          } finally {
            await this.options.afterClose?.();
          }
        }
      }
    }
  }
}
