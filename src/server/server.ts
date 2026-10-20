import { agentModels } from "./model-catalog.ts";
import { UsagePrices } from "./usage-pricing.ts";
import http, { type IncomingMessage, type ServerResponse } from "node:http";
import { readFile, mkdir, stat } from "node:fs/promises";
import { existsSync } from "node:fs";
import { extname, resolve } from "node:path";
import { randomBytes, randomUUID } from "node:crypto";
import { z } from "zod";
import coreRelease from "../../scripts/core-release.json";
import { version } from "../../package.json";
import type {
  ApiAccount,
  AppState,
  Json,
  ProfileState,
} from "../shared/types.ts";
import { goModelProtocol, modelsFor } from "../shared/providers.ts";
import { Store } from "./store.ts";
import { fileKeyCodec } from "./vault.ts";
import { CorePool } from "./core.ts";
import { Gateway, sameSecret } from "./gateway.ts";
import { AppError, errorMessage } from "./errors.ts";
import { parse, record, responseJson, text } from "./json.ts";
import { ImportCatalog } from "./imports.ts";
import {
  AgentSetup,
  agentSelectionSchema,
  agentSetupSchema,
} from "./agent-setup.ts";

const nameSchema = z.string().trim().min(1).max(60);
const profileSchema = z
  .object({
    name: nameSchema,
    color: z.enum(["forest", "blue", "clay", "plum"]).default("forest"),
  })
  .strict();
const patchSchema = z
  .object({
    name: nameSchema.optional(),
    color: z.enum(["forest", "blue", "clay", "plum"]).optional(),
    strategy: z.enum(["round-robin", "fill-first"]).optional(),
    sessionAffinity: z.boolean().optional(),
  })
  .strict();
const apiSchema = z
  .object({
    name: nameSchema,
    provider: z.enum([
      "opencode-go",
      "openai",
      "anthropic",
      "gemini",
      "custom",
    ]),
    baseUrl: z.string().url().max(500),
    apiKey: z.string().trim().min(1).max(20_000),
    protocol: z.enum(["openai", "anthropic", "responses"]).default("openai"),
    prefix: z
      .string()
      .regex(/^[a-z0-9-]*$/)
      .max(30),
    models: z
      .array(
        z.object({
          id: z.string().trim().min(1).max(200),
          protocol: z.enum(["openai", "anthropic", "responses"]),
        }),
      )
      .max(500),
  })
  .strict();
function providerURL(raw: string): URL {
  const url = new URL(raw);
  if (
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    (url.protocol !== "https:" &&
      !(
        url.protocol === "http:" &&
        ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)
      ))
  )
    throw new AppError(
      "Use an HTTPS base URL, or HTTP for a local provider. Do not include credentials or query parameters in the URL.",
    );
  return url;
}
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
export function json<T>(res: ServerResponse, value: T, status = 200): void {
  res.writeHead(status, {
    "Content-Type": "application/json",
    "Cache-Control": "no-store",
    "X-Content-Type-Options": "nosniff",
  });
  res.end(JSON.stringify(value));
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
  port?: number;
  desktop?: boolean;
  development?: boolean;
  agentHome?: string;
  // Extra Host names for the management UI only (HTTPS reverse proxy such as
  // Tailscale Serve). The agent gateway stays loopback-only.
  allowHosts?: string[];
  requirePinnedCore?: boolean;
}
export class Application {
  readonly options: ServerOptions;
  readonly store: Store;
  readonly core: CorePool;
  readonly gateway: Gateway;
  readonly server: http.Server;
  readonly token = randomBytes(32).toString("base64url");
  // ASAR timestamps are fabricated; revalidate unhashed files after each launch.
  private readonly assetVersion = randomUUID();
  readonly imports = new ImportCatalog();
  readonly usagePrices = new UsagePrices();
  readonly agentSetup: AgentSetup;
  port = 0;
  closing = false;
  private closePromise?: Promise<void>;
  constructor(options: ServerOptions, store: Store) {
    this.options = options;
    this.store = store;
    this.core = new CorePool(store, options.binary);
    this.gateway = new Gateway(store, this.core);
    this.agentSetup = new AgentSetup(
      options.directory,
      (profileId) => {
        this.store.profile(profileId);
        if (
          this.closing ||
          this.core.runtimes.get(profileId)?.state !== "running"
        )
          throw new AppError(
            "Start this profile before connecting an agent.",
            409,
          );
        return {
          key: this.store.secret(`${profileId}:client`),
          port: this.port,
        };
      },
      options.agentHome ? { home: options.agentHome } : undefined,
    );
    this.server = http.createServer((req, res) => {
      this.handle(req, res).catch((error) => {
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
        if (status === 500) this.core.report(errorMessage(error));
        json(
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
    await mkdir(options.directory, { recursive: true, mode: 0o700 });
    signal?.throwIfAborted();
    const store = new Store(options.directory, fileKeyCodec(options.directory));
    const app = new Application(options, store);
    try {
      store.pruneUsage();
      await new Promise<void>((resolve, reject) => {
        app.server.once("error", reject);
        app.server.listen(options.port ?? 4318, "127.0.0.1", resolve);
      });
      signal?.throwIfAborted();
      const address = app.server.address();
      if (!(address instanceof Object))
        throw new Error("Gateway failed to bind.");
      app.port = address.port;
      if (existsSync(options.binary))
        await app.core.verifyBinary(options.requirePinnedCore);
      signal?.throwIfAborted();
      await app.core.restore(signal);
      signal?.throwIfAborted();
      try {
        for (const failure of await app.agentSetup.restore(
          store.profiles(),
          app.port,
        ))
          app.core.report(failure);
      } catch {
        app.core.report(
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
  get origin(): string {
    return `http://127.0.0.1:${this.port}`;
  }
  endpoint(profileId: string): string {
    return `${this.origin}/p/${this.store.profile(profileId).slug}/v1`;
  }
  state(): AppState {
    const profiles: ProfileState[] = this.store.profiles().map((profile) => {
      const runtime = this.core.runtimes.get(profile.id);
      return {
        ...profile,
        runtime: runtime?.state ?? "stopped",
        error: runtime?.error,
        accounts: this.core.accounts(profile.id),
        usage: this.store.summary(profile.id),
        endpoint: this.endpoint(profile.id),
        lastSyncedAt: runtime?.lastSyncedAt,
      };
    });
    return {
      profiles,
      coreVersion: coreRelease.version,
      coreAvailable: existsSync(this.options.binary),
      gateway: this.origin,
      storage: this.store.codec.label,
      usageRetentionDays: this.store.usageRetentionDays(),
      version,
      desktop: Boolean(this.options.desktop),
      errors: this.core.errors,
    };
  }
  validHost(req: IncomingMessage): boolean {
    return (
      req.headers.host === `127.0.0.1:${this.port}` ||
      req.headers.host === `localhost:${this.port}` ||
      (Boolean(this.options.development) &&
        req.headers.host === "127.0.0.1:5173") ||
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
            !(this.options.development && origin === "http://127.0.0.1:5173"))
      )
        throw new AppError(
          "Cross-origin management requests are not allowed.",
          403,
        );
      if (!sameSecret(req.headers.authorization ?? "", `Bearer ${this.token}`))
        throw new AppError(
          "Open NonstopVibin from the desktop app, or run nonstopvibin url on the server for a new session link.",
          401,
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
    const requested = decodeURIComponent(url.pathname);
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
    const method = req.method;
    const segments = url.pathname.split("/").filter(Boolean);
    if (url.pathname === "/api/state" && method === "GET") {
      json(res, this.state());
      return;
    }
    if (url.pathname === "/api/usage-retention" && method === "PUT") {
      const input = z
        .object({
          days: z.union([
            z.literal(0),
            z.literal(30),
            z.literal(90),
            z.literal(365),
          ]),
        })
        .strict()
        .parse(await jsonBody(req));
      this.store.setUsageRetentionDays(input.days);
      json(res, this.state());
      return;
    }
    if (url.pathname === "/api/import-sources" && method === "GET") {
      json(res, await this.imports.scan());
      return;
    }
    if (url.pathname === "/api/profiles" && method === "POST") {
      const input = profileSchema.parse(await jsonBody(req));
      json(res, this.store.createProfile(input.name, input.color), 201);
      return;
    }
    if (segments[1] !== "profiles" || !segments[2])
      throw new AppError("Not found.", 404);
    const id = z.string().uuid().parse(segments[2]);
    const profile = this.store.profile(id);
    const action = segments[3];
    if (!action && method === "PATCH") {
      this.store.saveProfile({
        ...profile,
        ...patchSchema.parse(await jsonBody(req)),
      });
      await this.core.configure(id);
      json(res, this.state());
      return;
    }
    if (action === "start" && method === "POST") {
      await this.core.start(id);
      json(res, this.state());
      return;
    }
    if (action === "stop" && method === "POST") {
      await this.core.stop(id);
      json(res, this.state());
      return;
    }
    if (action === "refresh" && method === "POST") {
      await this.core.syncAccounts(id);
      for (const account of this.core.accounts(id))
        if (!account.disabled) await this.core.refreshQuota(id, account.id);
      json(res, this.state());
      return;
    }
    if (action === "key" && method === "GET") {
      json(res, { key: this.store.secret(`${id}:client`) });
      return;
    }
    if (action === "models" && method === "GET") {
      const models = await this.core.models(id);
      json(
        res,
        url.searchParams.get("metadata") === "1"
          ? await this.gateway.catalog.models(
              models,
              this.store.apiAccounts(id),
            )
          : models,
      );
      return;
    }
    if (action === "usage" && method === "GET") {
      const days = z.coerce
        .number()
        .int()
        .min(1)
        .max(3650)
        .parse(url.searchParams.get("days") ?? 7);
      const since =
        days === 3650
          ? ""
          : new Date(Date.now() - days * 86_400_000).toISOString();
      json(
        res,
        await this.usagePrices.history(
          this.store,
          id,
          since,
          this.gateway.catalog,
        ),
      );
      return;
    }
    if (action === "agent-setup" && segments.length === 4) {
      if (method === "GET") {
        const selection = agentSelectionSchema.parse({
          agent: url.searchParams.get("agent"),
          projectDirectory:
            url.searchParams.get("projectDirectory") ?? undefined,
        });
        json(
          res,
          await this.agentSetup.status(
            profile,
            selection.agent,
            selection.projectDirectory,
          ),
        );
        return;
      }
      if (method === "POST") {
        const input = agentSetupSchema.parse(await jsonBody(req));
        const models = await this.core.models(id);
        if (!models.length)
          throw new AppError("This profile has no available models.");
        const offered =
          input.agent === "pi" || input.agent === "opencode"
            ? agentModels(
                await this.gateway.catalog.models(
                  models,
                  this.store.apiAccounts(id),
                ),
                input.agent,
              )
            : modelsFor(input.agent, models, input.otherProviders ?? true);
        if (!offered.length)
          throw new AppError(
            `This profile has no ${input.agent === "claude" ? "Claude" : "OpenAI"} models. Add one, or offer models from other providers.`,
          );
        json(
          res,
          await this.agentSetup.install(
            profile,
            input,
            offered.map((model) => model.id),
          ),
        );
        return;
      }
      if (method === "DELETE") {
        const selection = agentSelectionSchema.parse(await jsonBody(req));
        await this.agentSetup.remove(
          profile,
          selection.agent,
          selection.projectDirectory,
        );
        json(res, { ok: true });
        return;
      }
    }
    if (
      action === "agent-check" &&
      method === "POST" &&
      segments.length === 4
    ) {
      z.object({})
        .strict()
        .parse(await jsonBody(req));
      const response = await fetch(`${this.endpoint(id)}/models`, {
        headers: {
          Authorization: `Bearer ${this.store.secret(`${id}:client`)}`,
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
      json(res, { ok: true });
      return;
    }
    if (action === "oauth" && method === "POST") {
      const { provider, reconnectAccountId } = z
        .object({
          provider: z.enum(["codex", "claude", "antigravity", "kimi", "xai"]),
          reconnectAccountId: z.string().min(1).max(255).optional(),
        })
        .strict()
        .parse(await jsonBody(req));
      json(res, await this.core.beginOAuth(id, provider, reconnectAccountId));
      return;
    }
    if (
      action === "oauth-confirm" &&
      method === "POST" &&
      segments.length === 4
    ) {
      const { state } = z
        .object({ state: z.string().min(1).max(500) })
        .strict()
        .parse(await jsonBody(req));
      json(res, await this.core.confirmOAuth(id, state));
      return;
    }
    if (action === "oauth-session" && method === "GET") {
      json(res, this.core.oauth?.profileId === id ? this.core.oauth : null);
      return;
    }
    if (action === "oauth" && method === "GET") {
      json(
        res,
        await this.core.oauthStatus(
          id,
          z.string().min(1).parse(url.searchParams.get("state")),
        ),
      );
      return;
    }
    if (action === "oauth" && method === "DELETE") {
      await this.core.cancelOAuth(
        id,
        z.string().min(1).parse(url.searchParams.get("state")),
      );
      json(res, { ok: true });
      return;
    }
    if (action === "callback" && method === "POST") {
      const { url } = z
        .object({ url: z.string().url().max(10_000) })
        .strict()
        .parse(await jsonBody(req));
      await this.core.oauthCallback(id, url);
      json(res, { ok: true });
      return;
    }
    if (action === "import" && method === "POST") {
      const { contents } = z
        .object({ contents: z.record(z.string(), z.json()) })
        .strict()
        .parse(await jsonBody(req));
      await this.core.importAuth(id, contents);
      json(res, this.state());
      return;
    }
    if (action === "import-source" && method === "POST") {
      const { sourceId } = z
        .object({ sourceId: z.string().regex(/^[a-f0-9]{64}$/) })
        .strict()
        .parse(await jsonBody(req));
      await this.core.importAuth(id, await this.imports.contents(sourceId));
      json(res, this.state());
      return;
    }
    if (
      action === "accounts" &&
      segments[4] &&
      segments[5] === "reset-credits" &&
      segments.length === 6
    ) {
      const accountId = decodeURIComponent(segments[4]);
      if (method === "GET") {
        json(res, await this.core.resetCredits(id, accountId));
        return;
      }
      if (method === "POST") {
        const { creditId } = z
          .object({ creditId: z.string().min(1).max(500) })
          .strict()
          .parse(await jsonBody(req));
        json(res, await this.core.consumeResetCredit(id, accountId, creditId));
        return;
      }
      throw new AppError("Method not allowed.", 405);
    }
    if (action === "accounts" && segments[4] && method === "PATCH") {
      const accountId = decodeURIComponent(segments[4]);
      const patch = z
        .object({
          disabled: z.boolean().optional(),
          priority: z.number().int().min(-1000).max(1000).optional(),
          name: nameSchema.optional(),
        })
        .strict()
        .parse(await jsonBody(req));
      await this.core.setAccount(id, accountId, patch);
      json(res, this.state());
      return;
    }
    if (action === "accounts" && segments[4] && method === "DELETE") {
      await this.core.removeAccount(id, decodeURIComponent(segments[4]));
      json(res, this.state());
      return;
    }
    if (action === "api-account" && method === "POST") {
      const input = apiSchema.parse(await jsonBody(req));
      providerURL(input.baseUrl);
      if (
        input.provider === "opencode-go" &&
        input.baseUrl.replace(/\/$/, "") !== "https://opencode.ai/zen/go/v1"
      )
        throw new AppError("OpenCode Go must use its official endpoint.");
      let models =
        input.provider === "opencode-go"
          ? input.models.map((m) => ({ ...m, protocol: goModelProtocol(m.id) }))
          : input.models;
      if (!models.length) {
        const response = await fetch(
          `${input.baseUrl.replace(/\/$/, "")}/models`,
          {
            headers: {
              ...(input.provider === "anthropic" ||
              input.protocol === "anthropic"
                ? {
                    "x-api-key": input.apiKey,
                    "anthropic-version": "2023-06-01",
                  }
                : { Authorization: `Bearer ${input.apiKey}` }),
              "User-Agent": "nonstopvibin/0.1.1",
            },
            redirect: "error",
            signal: AbortSignal.timeout(20_000),
          },
        );
        if (!response.ok)
          throw new AppError(
            `Model discovery returned HTTP ${response.status}. Verify the API key, or enter model IDs manually.`,
            502,
          );
        const data = record(await responseJson(response));
        models = Array.isArray(data.data)
          ? data.data
              .flatMap((raw) => {
                const r = record(raw);
                const id = text(r.id);
                return id === undefined
                  ? []
                  : [
                      {
                        id,
                        protocol:
                          input.provider === "opencode-go"
                            ? goModelProtocol(id, text(r.endpoint))
                            : input.provider === "anthropic"
                              ? ("anthropic" as const)
                              : input.protocol,
                      },
                    ];
              })
              .slice(0, 500)
          : [];
      }
      if (!models.length)
        throw new AppError(
          "No models were found. Enter the model IDs your provider supports.",
        );
      const account: ApiAccount = {
        id: randomUUID(),
        name: input.name,
        provider: input.provider,
        baseUrl: input.baseUrl.replace(/\/$/, ""),
        prefix: input.prefix,
        models,
        disabled: false,
      };
      for (const existing of this.store.apiAccounts(id)) {
        if (
          existing.baseUrl === account.baseUrl &&
          sameSecret(this.store.secret(`${existing.id}:api`), input.apiKey)
        )
          throw new AppError(
            "This API key is already connected to this profile.",
            409,
          );
      }
      this.store.saveApiAccount(id, account, input.apiKey);
      await this.core.start(id);
      await this.core.configure(id);
      await this.core.refreshQuota(id, account.id);
      json(res, this.state(), 201);
      return;
    }
    throw new AppError("Not found.", 404);
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
        await this.core.shutdown();
      } finally {
        try {
          await this.usagePrices.close();
        } finally {
          this.server.closeAllConnections();
          this.store.close();
        }
      }
    }
  }
}
