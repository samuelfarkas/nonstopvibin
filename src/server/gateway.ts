import http, { type IncomingMessage, type ServerResponse } from "node:http";
import { randomUUID } from "node:crypto";
import { pipeline } from "node:stream/promises";
import type { Duplex } from "node:stream";
import type { CorePool } from "./core.ts";
import type { Usage } from "./usage.ts";
import { Store, clientDigest, sameSecret } from "./store.ts";
import { AppError } from "./errors.ts";
import type { ProfileModels } from "./model-catalog.ts";
import { agentDefinitions, agentModels, offeredModels } from "./agents.ts";
import { catalogRequest } from "./agent-connection.ts";
import { isAgent, type Agent } from "../shared/agents.ts";
import type { Profile } from "../shared/types.ts";

const permitted =
  /^\/(?:v1\/(?:models|chat\/completions|responses(?:\/.*)?|messages(?:\/count_tokens)?|completions|embeddings|images\/(?:generations|edits)|audio\/(?:speech|transcriptions)|ws)|v1beta\/models(?:\/.*)?|v1interactions(?:\/.*)?)$/;
function keyOwner(store: Store, key: string): Profile | undefined {
  const id = store.clientIndex().get(clientDigest(key));
  try {
    // The live rows decide, so deletion and rotation fail closed even past the index.
    if (id && sameSecret(key, store.clientKey(id))) return store.profile(id);
  } catch (error) {
    if (!(error instanceof AppError)) throw error;
  }
  return undefined;
}
function routeRequest(store: Store, req: IncomingMessage) {
  const url = new URL(req.url ?? "/", "http://127.0.0.1");
  const bearer = req.headers.authorization?.match(/^Bearer (\S+)$/i)?.[1];
  // Node yields an array when a header repeats; a repeated key is not a credential.
  const apiKey = req.headers["x-api-key"];
  const headerKey = Array.isArray(apiKey) ? undefined : apiKey;
  if (bearer && headerKey && !sameSecret(bearer, headerKey))
    throw new AppError("Conflicting client credentials.", 401);
  const key = bearer ?? headerKey;
  if (!key) throw new AppError("A profile API key is required.", 401);
  const profile = keyOwner(store, key);
  if (!profile) throw new AppError("Invalid profile API key.", 401);
  let path = url.pathname;
  if (path.startsWith("/p/")) {
    const match = /^\/p\/([a-z0-9-]+)(\/.*)$/.exec(path);
    if (!match || match[1] !== profile.slug)
      throw new AppError(
        "This API key does not belong to the requested profile.",
        403,
      );
    path = match[2];
  }
  if (
    !permitted.test(path) ||
    path.includes("..") ||
    path.includes("%") ||
    url.searchParams.has("key") ||
    url.searchParams.has("api_key")
  )
    throw new AppError("Unsupported proxy endpoint.", 404);
  return { profileId: profile.id, slug: profile.slug, path: path + url.search };
}
function responseHeaders(
  upstream: http.IncomingHttpHeaders,
  slug: string,
): http.OutgoingHttpHeaders {
  const headers: http.OutgoingHttpHeaders = { ...upstream };
  delete headers["access-control-allow-origin"];
  delete headers["set-cookie"];
  headers["x-nonstopvibin-profile"] = slug;
  return headers;
}
/** The agent asking for its curated catalog, by the mechanism its definition names. */
function curatedCatalog(req: IncomingMessage, url: URL): Agent | undefined {
  const header = req.headers[catalogRequest.header];
  if (
    header !== undefined &&
    !Array.isArray(header) &&
    isAgent(header) &&
    agentDefinitions[header].catalog === "header"
  )
    return header;
  const query = url.searchParams.get(catalogRequest.query);
  if (query && isAgent(query) && agentDefinitions[query].catalog === "query")
    return query;
  return undefined;
}
function forwardedHeaders(
  req: IncomingMessage,
  secret: string,
): http.OutgoingHttpHeaders {
  const headers = { ...req.headers };
  for (const key of [
    "host",
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "cookie",
    "origin",
    "referer",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "proxy-authorization",
    catalogRequest.header,
  ])
    delete headers[key];
  headers.authorization = `Bearer ${secret}`;
  // Each caller session stays stable for Go prompt caching. Distinct sessions do not share a synthetic global ID.
  headers["x-opencode-session"] =
    req.headers["x-opencode-session"] ??
    req.headers["x-pi-session-id"] ??
    req.headers["session_id"] ??
    req.headers["x-session-id"] ??
    randomUUID();
  return headers;
}
export class Gateway {
  readonly store: Store;
  readonly core: CorePool;
  private readonly usage: Usage;
  readonly active = new Map<string, number>();
  private readonly models: ProfileModels;
  private readonly onActivityChange?: () => void;
  constructor(
    store: Store,
    core: CorePool,
    usage: Usage,
    options: {
      models: ProfileModels;
      /** A profile's first request opened or its last one closed. */
      onActivityChange?: () => void;
    },
  ) {
    this.store = store;
    this.core = core;
    this.usage = usage;
    this.models = options.models;
    this.onActivityChange = options.onActivityChange;
  }
  private acquire(profileId: string): () => void {
    const count = this.active.get(profileId) ?? 0;
    if (count >= 32)
      throw new AppError(
        "This profile has 32 requests in flight. Retry shortly.",
        429,
      );
    this.active.set(profileId, count + 1);
    this.usage.markActivity(profileId);
    if (count === 0) this.onActivityChange?.();
    let released = false;
    return () => {
      if (released) return;
      released = true;
      this.usage.markActivity(profileId);
      this.active.set(
        profileId,
        Math.max(0, (this.active.get(profileId) ?? 1) - 1),
      );
      if (this.active.get(profileId) === 0) this.onActivityChange?.();
    };
  }
  async proxy(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const { profileId, slug, path } = routeRequest(this.store, req);
    const port = this.core.port(profileId);
    const url = new URL(path, "http://localhost");
    const agent = curatedCatalog(req, url);
    if (req.method === "GET" && url.pathname === "/v1/models" && agent) {
      const release = this.acquire(profileId);
      try {
        const models = await offeredModels(
          agent,
          await this.core.models(profileId),
          false,
          (models) => this.models.enriched(profileId, models),
        );
        const generated = agentDefinitions[agent].catalog === "query";
        // The agent's own request gets the OpenAI list; generated code gets its entries.
        const payload = generated
          ? { models: agentModels(models, agent) }
          : {
              object: "list",
              data: models.map((model) => ({ object: "model", ...model })),
            };
        res.writeHead(200, {
          "Content-Type": "application/json",
          "Cache-Control": "no-store",
          ...(generated && { "x-nonstopvibin-profile": slug }),
        });
        res.end(JSON.stringify(payload));
      } finally {
        release();
      }
      return;
    }
    const release = this.acquire(profileId);
    const upstream = http.request({
      hostname: "127.0.0.1",
      port,
      method: req.method,
      path,
      headers: forwardedHeaders(req, this.store.coreKey(profileId)),
    });
    let abandoned = false;
    const close = () => {
      abandoned = !res.writableFinished;
      upstream.destroy();
    };
    res.once("close", close);
    try {
      await new Promise<void>((resolve, reject) => {
        upstream.once("error", reject);
        upstream.once("response", (response) => {
          res.writeHead(
            response.statusCode ?? 502,
            responseHeaders(response.headers, slug),
          );
          pipeline(response, res).then(resolve, reject);
        });
        pipeline(req, upstream)
          .then(() => {
            // Bun can leave native reads paused after unpiping a complete body,
            // hiding a peer FIN during idle SSE. Restore socket reads once.
            req.socket.resume();
          })
          .catch(reject);
      });
    } catch (error) {
      // The agent hung up; there is no socket to answer and nothing failed in the app.
      if (abandoned || res.destroyed) return;
      if (res.headersSent) throw error;
      // Raw socket errors name local ports and internals; the agent needs only the remedy.
      throw new AppError(
        "The profile proxy is unavailable. Restart the profile from the sidebar if NonstopVibin offers it; otherwise quit and reopen NonstopVibin.",
        502,
      );
    } finally {
      res.off("close", close);
      release();
      upstream.destroy();
    }
  }
  upgrade(req: IncomingMessage, socket: Duplex, head: Buffer): void {
    let release = () => {};
    socket.on("error", () => socket.destroy());
    try {
      const { profileId, slug, path } = routeRequest(this.store, req);
      const port = this.core.port(profileId);
      release = this.acquire(profileId);
      const upstream = http.request({
        hostname: "127.0.0.1",
        port,
        method: "GET",
        path,
        headers: forwardedHeaders(req, this.store.coreKey(profileId)),
      });
      socket.once("close", () => {
        release();
        upstream.destroy();
      });
      upstream.once("upgrade", (response, remote, remoteHead) => {
        const lines = Object.entries(
          responseHeaders(response.headers, slug),
        ).flatMap(([key, value]) =>
          [value ?? []].flat().map((item) => `${key}: ${item}\r\n`),
        );
        socket.write(
          `HTTP/1.1 101 Switching Protocols\r\n${lines.join("")}\r\n`,
        );
        if (remoteHead.length) socket.write(remoteHead);
        if (head.length) remote.write(head);
        socket.once("close", () => remote.destroy());
        remote.once("close", () => socket.destroy());
        socket.on("error", () => remote.destroy());
        remote.on("error", () => socket.destroy());
        socket.pipe(remote).pipe(socket);
      });
      upstream.once("response", (response) => {
        socket.end(
          `HTTP/1.1 ${response.statusCode ?? 502} Proxy error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`,
        );
        response.resume();
        release();
      });
      upstream.once("error", () => {
        socket.destroy();
        release();
      });
      upstream.end();
    } catch (error) {
      release();
      const status = error instanceof AppError ? error.status : 502;
      socket.end(
        `HTTP/1.1 ${status} Proxy error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`,
      );
    }
  }
}
