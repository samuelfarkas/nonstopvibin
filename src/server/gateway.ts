import http, { type IncomingMessage, type ServerResponse } from "node:http";
import { timingSafeEqual, randomUUID } from "node:crypto";
import { pipeline } from "node:stream/promises";
import type { Duplex } from "node:stream";
import { CorePool } from "./core.ts";
import { Store } from "./store.ts";
import { AppError } from "./errors.ts";
import { ModelCatalog, agentModels } from "./model-catalog.ts";
import { modelsFor } from "../shared/providers.ts";

export function sameSecret(a: string, b: string): boolean {
  const left = Buffer.from(a);
  const right = Buffer.from(b);
  return left.length === right.length && timingSafeEqual(left, right);
}
const permitted =
  /^\/(?:v1\/(?:models|chat\/completions|responses(?:\/.*)?|messages(?:\/count_tokens)?|completions|embeddings|images\/(?:generations|edits)|audio\/(?:speech|transcriptions)|ws)|v1beta\/models(?:\/.*)?|v1interactions(?:\/.*)?)$/;
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
  const profile = store
    .profiles()
    .find((p) => sameSecret(key, store.secret(`${p.id}:client`)));
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
  return { profileId: profile.id, path: path + url.search };
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
    "x-nonstopvibin-agent",
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
  readonly active = new Map<string, number>();
  readonly catalog = new ModelCatalog();
  onActivityChange?: () => void;
  constructor(store: Store, core: CorePool) {
    this.store = store;
    this.core = core;
  }
  private acquire(profileId: string): () => void {
    const count = this.active.get(profileId) ?? 0;
    if (count >= 32)
      throw new AppError(
        "This profile has 32 requests in flight. Retry shortly.",
        429,
      );
    this.active.set(profileId, count + 1);
    this.core.markUsageActivity(profileId);
    if (count === 0) this.onActivityChange?.();
    let released = false;
    return () => {
      if (released) return;
      released = true;
      this.core.markUsageActivity(profileId);
      this.active.set(
        profileId,
        Math.max(0, (this.active.get(profileId) ?? 1) - 1),
      );
      if (this.active.get(profileId) === 0) this.onActivityChange?.();
    };
  }
  async proxy(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const { profileId, path } = routeRequest(this.store, req);
    const port = this.core.port(profileId);
    const url = new URL(path, "http://localhost");
    const agent = url.searchParams.get("nonstopvibin");
    if (
      req.method === "GET" &&
      url.pathname === "/v1/models" &&
      req.headers["x-nonstopvibin-agent"] === "codex"
    ) {
      // Codex installed without other providers: serve its own vendor's models.
      const release = this.acquire(profileId);
      try {
        const data = modelsFor(
          "codex",
          await this.core.models(profileId),
          false,
        );
        res.writeHead(200, {
          "Content-Type": "application/json",
          "Cache-Control": "no-store",
        });
        res.end(
          JSON.stringify({
            object: "list",
            data: data.map((model) => ({ object: "model", ...model })),
          }),
        );
      } finally {
        release();
      }
      return;
    }
    if (
      req.method === "GET" &&
      url.pathname === "/v1/models" &&
      (agent === "pi" || agent === "opencode")
    ) {
      const release = this.acquire(profileId);
      try {
        const models = await this.catalog.models(
          await this.core.models(profileId),
          this.store.apiAccounts(profileId),
        );
        const payload = { models: agentModels(models, agent) };
        res.writeHead(200, {
          "Content-Type": "application/json",
          "Cache-Control": "no-store",
          "x-nonstopvibin-profile": this.store.profile(profileId).slug,
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
      headers: forwardedHeaders(req, this.store.secret(`${profileId}:core`)),
    });
    const close = () => upstream.destroy();
    res.once("close", close);
    try {
      await new Promise<void>((resolve, reject) => {
        upstream.once("error", reject);
        upstream.once("response", (response) => {
          const headers = { ...response.headers };
          delete headers["access-control-allow-origin"];
          delete headers["set-cookie"];
          headers["x-nonstopvibin-profile"] =
            this.store.profile(profileId).slug;
          res.writeHead(response.statusCode ?? 502, headers);
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
      const { profileId, path } = routeRequest(this.store, req);
      const port = this.core.port(profileId);
      release = this.acquire(profileId);
      const upstream = http.request({
        hostname: "127.0.0.1",
        port,
        method: "GET",
        path,
        headers: forwardedHeaders(req, this.store.secret(`${profileId}:core`)),
      });
      socket.once("close", () => {
        release();
        upstream.destroy();
      });
      upstream.once("upgrade", (response, remote, remoteHead) => {
        socket.write(
          `HTTP/1.1 101 Switching Protocols\r\n${Object.entries(
            response.headers,
          )
            .map(([k, v]) => `${k}: ${v}`)
            .join("\r\n")}\r\n\r\n`,
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
