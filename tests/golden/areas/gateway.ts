// Gateway routing, header rewriting and curated catalogs, through the real Gateway.proxy
// on a loopback server. routeRequest, forwardedHeaders, responseHeaders and
// curatedCatalog are private, so each case is a raw HTTP request: the verdict is the
// AppError the gateway throws, or what a raw stub core received and what the client got.
import http from "node:http";
import net from "node:net";
import type { CorePool } from "../../../src/server/core.ts";
import { Gateway } from "../../../src/server/gateway.ts";
import {
  ModelCatalog,
  ProfileModels,
} from "../../../src/server/model-catalog.ts";
import { secretIds, Store } from "../../../src/server/store.ts";
import type { Usage } from "../../../src/server/usage.ts";
import { fileKeyCodec } from "../../../src/server/vault.ts";
import type { Model } from "../../../src/shared/types.ts";
import {
  type Case,
  caseError,
  modelsDevFetch,
  Placeholders,
  scratch,
  stub,
  syntheticKey,
} from "../lib.ts";

export type Header = [string, string];
export interface Request {
  method: string;
  target: string;
  headers: Header[];
  body?: string;
}

const keys = {
  work: syntheticKey("a"),
  personal: syntheticKey("b"),
  team: syntheticKey("c"),
  rotatedOld: syntheticKey("d"),
  rotatedNew: syntheticKey("e"),
  unknown: syntheticKey("z"),
};
/** What the stub core lists at GET /v1/models. */
export const coreModels: Model[] = [
  { id: "claude-sonnet-4-5", owned_by: "claude" },
  { id: "claude-opus-4-1", owned_by: "anthropic" },
  { id: "gpt-5-codex", owned_by: "codex" },
  { id: "gpt-5", owned_by: "openai" },
  { id: "gpt-image-1", owned_by: "codex" },
  { id: "gemini-2.5-pro", owned_by: "gemini" },
  { id: "gemini-free", owned_by: "gemini" },
  { id: "kimi-k2", owned_by: "kimi" },
  { id: "local/qwen3-coder", owned_by: "openai-compatible-0123abcd" },
  { id: "no-owner" },
];
// Response header sets the stub core answers with, chosen by the forwarded
// x-golden-response request header.
const responseSets: Array<{ status: number; headers: Header[] }> = [
  { status: 200, headers: [["Content-Type", "application/json"]] },
  {
    status: 200,
    headers: [
      ["Content-Type", "application/json"],
      ["Access-Control-Allow-Origin", "*"],
      ["Set-Cookie", "a=1; Path=/"],
      ["Set-Cookie", "b=2"],
      ["X-Request-Id", "req-1"],
    ],
  },
  {
    status: 200,
    headers: [
      ["Content-Type", "application/json"],
      ["X-Nonstopvibin-Profile", "spoofed"],
    ],
  },
  {
    status: 200,
    headers: [
      ["content-type", "text/event-stream"],
      ["cache-control", "no-cache"],
      ["access-control-allow-credentials", "true"],
      ["access-control-expose-headers", "x-request-id"],
    ],
  },
  {
    status: 429,
    headers: [
      ["Content-Type", "application/json"],
      ["Retry-After", "7"],
      ["anthropic-ratelimit-requests-remaining", "0"],
    ],
  },
  {
    status: 500,
    headers: [
      ["Content-Type", "text/plain"],
      ["WWW-Authenticate", 'Bearer realm="core"'],
    ],
  },
  {
    status: 200,
    headers: [
      ["Content-Type", "application/json"],
      ["X-Multi", "1"],
      ["X-Multi", "2"],
    ],
  },
  { status: 404, headers: [["Content-Type", "application/json"]] },
];

/** A raw HTTP/1.1 core: answers with the request line and header lines it received. */
function stubCore(): net.Server {
  return net.createServer((socket) => {
    let buffer = Buffer.alloc(0);
    socket.on("data", (chunk) => {
      buffer = Buffer.concat([buffer, chunk]);
      const end = buffer.indexOf("\r\n\r\n");
      if (end < 0) return;
      const lines = buffer.subarray(0, end).toString("latin1").split("\r\n");
      const headers = lines.slice(1).map((line): Header => {
        const colon = line.indexOf(":");
        return [line.slice(0, colon), line.slice(colon + 1).trim()];
      });
      const get = (name: string) =>
        headers.find(([key]) => key.toLowerCase() === name)?.[1];
      const body = buffer.subarray(end + 4).toString("latin1");
      const length = Number(get("content-length") ?? 0);
      if (get("transfer-encoding") === "chunked") {
        if (!body.endsWith("0\r\n\r\n")) return;
      } else if (Buffer.byteLength(body, "latin1") < length) return;
      socket.removeAllListeners("data");
      const set =
        responseSets[Number(get("x-golden-response") ?? 0)] ?? responseSets[0];
      const payload =
        set.status === 204
          ? ""
          : JSON.stringify({ requestLine: lines[0], headers, body });
      socket.end(
        `HTTP/1.1 ${set.status} Stub\r\n${set.headers.map(([k, v]) => `${k}: ${v}\r\n`).join("")}Content-Length: ${Buffer.byteLength(payload)}\r\nConnection: close\r\n\r\n${payload}`,
      );
    });
    socket.on("error", () => socket.destroy());
  });
}

export interface Response {
  status: number;
  headers: Header[];
  body: string;
}
function parseResponse(raw: Buffer): Response {
  const end = raw.indexOf("\r\n\r\n");
  const head = raw.subarray(0, end < 0 ? raw.length : end).toString("latin1");
  const [statusLine = "", ...lines] = head.split("\r\n");
  const headers = lines.map((line): Header => {
    const colon = line.indexOf(":");
    return [line.slice(0, colon).toLowerCase(), line.slice(colon + 1).trim()];
  });
  let body = end < 0 ? Buffer.alloc(0) : raw.subarray(end + 4);
  if (headers.some(([k, v]) => k === "transfer-encoding" && v === "chunked")) {
    const parts: Buffer[] = [];
    let rest = body;
    for (;;) {
      const lineEnd = rest.indexOf("\r\n");
      const size = parseInt(rest.subarray(0, lineEnd).toString("latin1"), 16);
      if (!size) break;
      parts.push(rest.subarray(lineEnd + 2, lineEnd + 2 + size));
      rest = rest.subarray(lineEnd + 2 + size + 2);
    }
    body = Buffer.concat(parts);
  }
  return {
    status: Number(statusLine.split(" ")[1]),
    headers,
    body: body.toString("utf8"),
  };
}

/** One raw HTTP/1.1 request; Host is 127.0.0.1:<port> unless the request names one. */
export function send(port: number, request: Request): Promise<Response> {
  return new Promise((resolve, reject) => {
    const socket = net.connect(port, "127.0.0.1");
    const chunks: Buffer[] = [];
    socket.on("data", (chunk) => chunks.push(chunk));
    socket.on("error", reject);
    socket.on("close", () => resolve(parseResponse(Buffer.concat(chunks))));
    const body = request.body ?? "";
    socket.write(
      `${request.method} ${request.target} HTTP/1.1\r\n${request.headers.some(([k]) => k.toLowerCase() === "host") ? "" : `Host: 127.0.0.1:${port}\r\n`}${request.headers
        .map(([k, v]) => `${k}: ${v}\r\n`)
        .join(
          "",
        )}${body && !request.headers.some(([k]) => k.toLowerCase() === "transfer-encoding") ? `Content-Length: ${Buffer.byteLength(body)}\r\n` : ""}Connection: close\r\n\r\n${body}`,
    );
  });
}

const bearer = (key: string): Header => ["Authorization", `Bearer ${key}`];
const apiKey = (key: string): Header => ["x-api-key", key];

/** Request targets: permitted, near-miss and hostile paths. */
const paths = [
  "/v1/models",
  "/v1/models/",
  "/v1/models/claude-sonnet-4-5",
  "/v1/chat/completions",
  "/v1/chat/completions/",
  "/v1/responses",
  "/v1/responses/resp_123",
  "/v1/responses/resp_123/cancel",
  "/v1/responses/",
  "/v1/messages",
  "/v1/messages/count_tokens",
  "/v1/messages/batches",
  "/v1/completions",
  "/v1/embeddings",
  "/v1/images/generations",
  "/v1/images/edits",
  "/v1/images/variations",
  "/v1/audio/speech",
  "/v1/audio/transcriptions",
  "/v1/audio/translations",
  "/v1/ws",
  "/v1/ws/",
  "/v1beta/models",
  "/v1beta/models/gemini-2.5-pro:generateContent",
  "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
  "/v1beta/files",
  "/v1interactions",
  "/v1interactions/abc",
  "/v1",
  "/v1/",
  "/v1x/models",
  "/v0/management/config",
  "/V1/models",
  "/v1/Models",
  "/v1//models",
  "//v1/models",
  "/v1/models;a=b",
  "/v1/responses/../../v0/management/config",
  "/v1/responses/..",
  "/v1/responses/a..b",
  "/v1/responses/%2e%2e/x",
  "/v1/responses/a%2Fb",
  "/v1/responses/%41",
  "/v1/chat%2Fcompletions",
  "/v1/models%00",
  "/v1beta/models/../../v0/management/usage",
  "/v1/responses/ünïcode",
  "/healthz",
  "/",
];
const prefixes = [
  "",
  "/p/work",
  "/p/personal",
  "/p/WORK",
  "/p/work-",
  "/p/team-2",
  "/p/",
  "/p",
  "/p/work/p/work",
  "/p/w%6Frk",
];
const queries = [
  "",
  "?beta=true",
  "?key=x",
  "?api_key=x",
  "?KEY=x",
  "?Api_Key=x",
  "?k%65y=x",
  "?api%5Fkey=x",
  "?key",
  "?key=",
  "?a=1&key=2",
  "?a=1&api_key=2",
  "?keys=1",
  "?x_api_key=1",
  "?key%3D1",
  "?%6B%65%79=1",
  "?nonstopvibin=pi",
  "#key=x",
  "?a=1#frag",
  "?q=%zz",
  "?a=%2e%2e",
];

function corpus(): Array<{ name: string; request: Request }> {
  const cases: Array<{ name: string; request: Request }> = [];
  const seen = new Set<string>();
  // Prefix and path pairs can spell the same target; the first one is kept.
  const add = (name: string, request: Request) => {
    if (seen.has(name)) return;
    seen.add(name);
    cases.push({ name, request });
  };
  // Paths under every prefix, with the work profile's key.
  for (const prefix of prefixes)
    for (const path of paths)
      for (const method of ["GET", "POST"])
        add(`route ${method} ${prefix}${path}`, {
          method,
          target: `${prefix}${path}`,
          headers: [bearer(keys.work)],
        });
  // Query names on permitted and refused paths.
  for (const path of [
    "/v1/models",
    "/p/work/v1/messages",
    "/v1beta/models/x",
    "/v0/x",
  ])
    for (const query of queries)
      add(`query ${path}${query}`, {
        method: "GET",
        target: `${path}${query}`,
        headers: [bearer(keys.work)],
      });
  // Absolute-form and odd request targets.
  for (const target of [
    "http://127.0.0.1/v1/models",
    "http://evil.example/v1/models",
    "http://evil.example/p/work/v1/messages?key=1",
    "*",
    "v1/models",
  ])
    add(`target ${target}`, {
      method: "GET",
      target,
      headers: [bearer(keys.work)],
    });
  // Credentials.
  const credentialSets: Array<[string, Header[]]> = [
    ["none", []],
    ["bearer work", [bearer(keys.work)]],
    ["bearer personal", [bearer(keys.personal)]],
    ["bearer unknown", [bearer(keys.unknown)]],
    ["bearer rotated old", [bearer(keys.rotatedOld)]],
    ["bearer rotated new", [bearer(keys.rotatedNew)]],
    ["lowercase bearer", [["Authorization", `bearer ${keys.work}`]]],
    ["uppercase BEARER", [["Authorization", `BEARER ${keys.work}`]]],
    ["bearer two spaces", [["Authorization", `Bearer  ${keys.work}`]]],
    ["bearer trailing space", [["Authorization", `Bearer ${keys.work} `]]],
    ["bearer with suffix", [["Authorization", `Bearer ${keys.work} extra`]]],
    ["bearer empty", [["Authorization", "Bearer "]]],
    ["bearer only", [["Authorization", "Bearer"]]],
    ["basic", [["Authorization", `Basic ${keys.work}`]]],
    ["raw key in authorization", [["Authorization", keys.work]]],
    ["bearer tab", [["Authorization", `Bearer\t${keys.work}`]]],
    ["x-api-key work", [apiKey(keys.work)]],
    ["X-API-KEY header case", [["X-API-KEY", keys.work]]],
    ["x-api-key empty", [apiKey("")]],
    ["x-api-key unknown", [apiKey(keys.unknown)]],
    ["x-api-key with bearer prefix", [apiKey(`Bearer ${keys.work}`)]],
    ["x-api-key repeated same", [apiKey(keys.work), apiKey(keys.work)]],
    [
      "x-api-key repeated different",
      [apiKey(keys.work), apiKey(keys.personal)],
    ],
    ["authorization repeated", [bearer(keys.work), bearer(keys.personal)]],
    ["authorization repeated same", [bearer(keys.work), bearer(keys.work)]],
    ["bearer and x-api-key same", [bearer(keys.work), apiKey(keys.work)]],
    [
      "bearer and x-api-key different",
      [bearer(keys.work), apiKey(keys.personal)],
    ],
    [
      "bearer unknown and x-api-key work",
      [bearer(keys.unknown), apiKey(keys.work)],
    ],
    [
      "lowercase bearer and different x-api-key",
      [["Authorization", `bearer ${keys.work}`], apiKey(keys.personal)],
    ],
    [
      "basic and x-api-key",
      [["Authorization", "Basic abc"], apiKey(keys.work)],
    ],
    [
      "bearer and repeated x-api-key",
      [bearer(keys.work), apiKey(keys.personal), apiKey(keys.personal)],
    ],
    ["x-goog-api-key only", [["x-goog-api-key", keys.work]]],
    ["key too short", [bearer("nv_aaaa")]],
    ["key wrong prefix", [bearer(`xx_${"a".repeat(43)}`)]],
    ["team key", [bearer(keys.team)]],
  ];
  for (const [label, headers] of credentialSets)
    for (const target of [
      "/v1/models",
      "/p/work/v1/messages",
      "/p/personal/v1/models",
      "/p/team-2/v1/chat/completions",
      "/v0/management/config",
    ])
      add(`credentials ${label} ${target}`, {
        method: "POST",
        target,
        headers,
      });
  // Forwarded headers: stripped, kept, rewritten and the session header chain.
  const forward: Array<[string, Header[]]> = [
    ["plain", [bearer(keys.work)]],
    [
      "strips credentials and proxy headers",
      [
        bearer(keys.work),
        apiKey(keys.work),
        ["x-goog-api-key", "goog"],
        ["Cookie", "a=1"],
        ["Referer", "http://example.com/"],
        ["Forwarded", "for=1.2.3.4"],
        ["X-Forwarded-For", "1.2.3.4"],
        ["X-Forwarded-Host", "example.com"],
        ["X-Forwarded-Proto", "https"],
        ["Proxy-Authorization", "Basic abc"],
        ["x-nonstopvibin-agent", "codex"],
      ],
    ],
    [
      "keeps agent headers",
      [
        bearer(keys.work),
        ["anthropic-version", "2023-06-01"],
        ["anthropic-beta", "oauth-2025-04-20,context-1m"],
        ["User-Agent", "claude-cli/2.0"],
        ["Accept", "text/event-stream"],
        ["Content-Type", "application/json"],
        ["X-Stainless-Lang", "js"],
        ["openai-beta", "responses=v1"],
        ["chatgpt-account-id", "acct"],
      ],
    ],
    [
      "x-opencode-session kept",
      [bearer(keys.work), ["x-opencode-session", "oc-1"]],
    ],
    ["x-pi-session-id used", [bearer(keys.work), ["x-pi-session-id", "pi-1"]]],
    ["session_id used", [bearer(keys.work), ["session_id", "codex-1"]]],
    ["x-session-id used", [bearer(keys.work), ["x-session-id", "s-1"]]],
    [
      "session precedence opencode first",
      [
        bearer(keys.work),
        ["x-session-id", "s-1"],
        ["session_id", "codex-1"],
        ["x-pi-session-id", "pi-1"],
        ["x-opencode-session", "oc-1"],
      ],
    ],
    [
      "session precedence pi over session_id",
      [
        bearer(keys.work),
        ["x-session-id", "s-1"],
        ["session_id", "codex-1"],
        ["x-pi-session-id", "pi-1"],
      ],
    ],
    [
      "session precedence session_id over x-session-id",
      [bearer(keys.work), ["x-session-id", "s-1"], ["session_id", "codex-1"]],
    ],
    ["empty session header", [bearer(keys.work), ["x-opencode-session", ""]]],
    [
      "repeated session header",
      [
        bearer(keys.work),
        ["x-opencode-session", "oc-1"],
        ["x-opencode-session", "oc-2"],
      ],
    ],
    [
      "repeated custom header",
      [bearer(keys.work), ["X-Custom", "1"], ["X-Custom", "2"]],
    ],
    ["mixed case custom", [bearer(keys.work), ["X-MiXeD-CaSe", "Value"]]],
    [
      "connection keep-alive",
      [bearer(keys.work), ["Upgrade-Insecure-Requests", "1"]],
    ],
  ];
  for (const [label, headers] of forward)
    add(`forward ${label}`, {
      method: "POST",
      target: "/p/work/v1/messages?beta=true",
      headers,
      body: '{"model":"claude-sonnet-4-5"}',
    });
  add("forward GET without body", {
    method: "GET",
    target: "/v1/models?x=1",
    headers: [bearer(keys.work)],
  });
  add("forward chunked body", {
    method: "POST",
    target: "/v1/responses",
    headers: [bearer(keys.work), ["Transfer-Encoding", "chunked"]],
    body: "5\r\nhello\r\n0\r\n\r\n",
  });
  // Response headers the stub answers with.
  for (const [index] of responseSets.entries())
    add(`response set ${index}`, {
      method: "POST",
      target: "/p/work/v1/chat/completions",
      headers: [bearer(keys.work), ["x-golden-response", String(index)]],
      body: "{}",
    });
  // Curated catalogs: header for Codex, query for pi and OpenCode.
  const agentValues = [
    "codex",
    "pi",
    "opencode",
    "claude",
    "Codex",
    "",
    "nope",
  ];
  for (const value of agentValues) {
    add(`catalog header ${JSON.stringify(value)}`, {
      method: "GET",
      target: "/v1/models",
      headers: [bearer(keys.work), ["x-nonstopvibin-agent", value]],
    });
    add(`catalog query ${JSON.stringify(value)}`, {
      method: "GET",
      target: `/p/work/v1/models?nonstopvibin=${value}`,
      headers: [bearer(keys.work)],
    });
  }
  add("catalog header repeated", {
    method: "GET",
    target: "/v1/models",
    headers: [
      bearer(keys.work),
      ["x-nonstopvibin-agent", "codex"],
      ["x-nonstopvibin-agent", "codex"],
    ],
  });
  add("catalog POST with header", {
    method: "POST",
    target: "/v1/models",
    headers: [bearer(keys.work), ["x-nonstopvibin-agent", "codex"]],
  });
  add("catalog query on other path", {
    method: "GET",
    target: "/v1/models/gpt-5?nonstopvibin=pi",
    headers: [bearer(keys.work)],
  });
  add("catalog query repeated first wins", {
    method: "GET",
    target: "/v1/models?nonstopvibin=pi&nonstopvibin=opencode",
    headers: [bearer(keys.work)],
  });
  add("catalog query encoded", {
    method: "GET",
    target: "/v1/models?nonstopvibin=%70i",
    headers: [bearer(keys.work)],
  });
  add("catalog header and query", {
    method: "GET",
    target: "/v1/models?nonstopvibin=pi",
    headers: [bearer(keys.work), ["x-nonstopvibin-agent", "codex"]],
  });
  add("catalog query for codex", {
    method: "GET",
    target: "/v1/models?nonstopvibin=codex",
    headers: [bearer(keys.work)],
  });
  add("catalog header for pi", {
    method: "GET",
    target: "/v1/models",
    headers: [bearer(keys.work), ["x-nonstopvibin-agent", "pi"]],
  });
  add("catalog query personal profile without priced models", {
    method: "GET",
    target: "/p/personal/v1/models?nonstopvibin=pi",
    headers: [bearer(keys.personal)],
  });
  return cases;
}

/** Listens on a free loopback port and returns it. */
async function listen(server: net.Server): Promise<number> {
  await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
  const address = server.address();
  if (!(address instanceof Object)) throw new Error("The server has no port.");
  return address.port;
}

export async function recordGateway(): Promise<Case[]> {
  const directory = scratch("gateway");
  const store = new Store(directory, fileKeyCodec(directory));
  const placeholders = new Placeholders().add(directory, "<data>");
  const profiles = new Map<string, string>();
  for (const [name, key] of [
    ["Work", keys.work],
    ["Personal", keys.personal],
    ["Team 2", keys.team],
    ["Rotated", keys.rotatedOld],
  ] as const) {
    const profile = store.createProfile(name);
    store.setSecret(secretIds.client(profile.id), key);
    store.setSecret(secretIds.core(profile.id), `core-${profile.slug}`);
    profiles.set(profile.slug, profile.id);
    placeholders.add(profile.id, `<profile:${profile.slug}>`);
  }
  // The index is built with the old key, then the live row changes.
  store.clientIndex();
  store.setSecret(
    secretIds.client(profiles.get("rotated") ?? ""),
    keys.rotatedNew,
  );
  // Personal's core lists only models models.dev cannot price for pi.
  const listed = (profileId: string) =>
    profileId === profiles.get("personal")
      ? coreModels.filter((m) => m.id === "gemini-free" || !m.owned_by)
      : coreModels;
  const core = stubCore();
  const corePort = await listen(core);
  // Gateway calls only port and models on the pool, and wake and markActivity on usage.
  const pool = stub<CorePool>({
    port: () => corePort,
    models: async (profileId: string) => listed(profileId),
  });
  const usage = stub<Usage>({
    markActivity: () => undefined,
    wake: async () => undefined,
  });
  const catalog = new ModelCatalog(modelsDevFetch);
  await catalog.models([], []);
  placeholders.add(catalog.snapshot([]).fetchedAt ?? "", "<fetched-at>");
  const gateway = new Gateway(store, pool, usage, {
    models: new ProfileModels(store, catalog),
  });
  const server = http.createServer((req, res) => {
    gateway.proxy(req, res).catch((cause) => {
      const verdict = caseError(cause);
      res.writeHead(verdict.status, { "x-golden-error": "1" });
      res.end(JSON.stringify(verdict));
    });
  });
  const port = await listen(server);
  placeholders.add(`127.0.0.1:${corePort}`, "127.0.0.1:<core-port>");
  const uuid =
    /\b[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\b/g;
  const cases: Case[] = [];
  try {
    for (const { name, request } of corpus()) {
      const response = await send(port, request);
      const header = (key: string) =>
        response.headers.find(([k]) => k === key)?.[1];
      if (header("x-golden-error")) {
        cases.push({ name, input: request, error: JSON.parse(response.body) });
        continue;
      }
      const responseHeaders = response.headers.filter(
        ([k]) => k !== "date" && k !== "x-golden-error",
      );
      if (response.body.startsWith('{"requestLine"')) {
        const received: {
          requestLine: string;
          headers: Header[];
          body: string;
        } = JSON.parse(response.body);
        const detailed =
          !name.startsWith("route ") &&
          !name.startsWith("query ") &&
          !name.startsWith("credentials ");
        cases.push({
          name,
          input: request,
          output: {
            kind: "proxied",
            profile: header("x-nonstopvibin-profile"),
            requestLine: received.requestLine,
            ...(detailed && {
              forwardedHeaders: received.headers.map(([k, v]): Header => [
                k,
                k.toLowerCase() === "x-opencode-session"
                  ? v.replace(uuid, "<uuid>")
                  : v,
              ]),
              forwardedBody: received.body,
              status: response.status,
              responseHeaders,
            }),
          },
        });
      } else
        cases.push({
          name,
          input: request,
          output: {
            kind: response.status === 200 ? "catalog" : "http",
            status: response.status,
            headers: responseHeaders,
            body:
              response.status === 200
                ? JSON.parse(response.body)
                : response.body,
          },
        });
    }
  } finally {
    await new Promise<void>((done) => server.close(() => done()));
    await new Promise<void>((done) => core.close(() => done()));
    store.close();
  }
  return placeholders.apply(cases);
}
