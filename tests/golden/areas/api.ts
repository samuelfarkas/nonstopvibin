// The real Application on a copy of tests/fixtures/ts-data-dir (data and agent home),
// with no core binary (allowMissingCore): what launch changes on disk, the JSON of the
// read routes, the management API's refusals, and static UI serving from a tiny client.
import {
  chmodSync,
  cpSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { ImportCatalog } from "../../../src/server/imports.ts";
import { Application } from "../../../src/server/server.ts";
import { type Header, type Request, send } from "./gateway.ts";
import {
  type Case,
  fetchStub,
  modelsDevFetch,
  Placeholders,
  type Recorded,
  repository,
  scratch,
} from "../lib.ts";

const fixture = join(repository, "tests/fixtures/ts-data-dir");
const expected: {
  profiles: Array<{ id: string; slug: string }>;
  originalPaths: { data: string; agentHome: string };
  files: Array<{ path: string; type: string; mode: string }>;
} = JSON.parse(readFileSync(join(fixture, "expected.json"), "utf8"));
const [team, personal, personal2, star] = expected.profiles;
export const apiPort = 24318;

/** Text files under `root` (no SQLite or vault key), path → content and mode. */
type Entry = { content: string; mode: string } | { type: string };
function tree(root: string, label: string): Map<string, Entry> {
  const files = new Map<string, Entry>();
  const visit = (directory: string) => {
    for (const name of readdirSync(directory).sort()) {
      const path = join(directory, name);
      const status = lstatSync(path);
      const key = path.replace(root, label);
      if (status.isDirectory()) visit(path);
      else if (/\.sqlite(-wal|-shm)?$|vault\.key$/.test(name))
        files.set(key, { type: "binary" });
      else if (status.isFile())
        files.set(key, {
          content: readFileSync(path, "utf8"),
          mode: `0${(status.mode & 0o777).toString(8)}`,
        });
      else files.set(key, { type: status.isSocket() ? "socket" : "other" });
    }
  };
  if (existsSync(root)) visit(root);
  return files;
}

function diff(before: Map<string, Entry>, after: Map<string, Entry>) {
  const changes = new Map<string, Entry | null>();
  for (const [path, file] of after)
    if (JSON.stringify(before.get(path)) !== JSON.stringify(file))
      changes.set(path, file);
  for (const path of before.keys())
    if (!after.has(path)) changes.set(path, null);
  return changes;
}

/** A client directory with an index, a fingerprinted asset and odd files. */
function clientDirectory(root: string): string {
  const client = join(root, "client");
  mkdirSync(join(client, "assets"), { recursive: true });
  mkdirSync(join(client, "folder.d"), { recursive: true });
  const files = {
    "index.html": "<!doctype html><title>golden</title>",
    "assets/index-AbCd1234.js": "console.log(1);",
    "assets/index-AbCd1234.css": "body{}",
    "assets/font-Zz_-9999.woff2": "font",
    "assets/short-abc.js": "short hash",
    "assets/nested/deep-AbCd1234.js": "nested",
    "logo.svg": "<svg/>",
    "icon.png": "png",
    "data.json": "{}",
    "file with space.txt": "space",
    "unknown.xyz": "x",
  } satisfies Record<string, string>;

  for (const [path, content] of Object.entries(files)) {
    mkdirSync(join(client, path, ".."), { recursive: true });
    writeFileSync(join(client, path), content);
    utimesSync(join(client, path), 1_700_000_000, 1_700_000_000);
  }
  writeFileSync(join(root, "secret.txt"), "outside the client");
  writeFileSync(join(root, "client-evil.txt"), "sibling prefix");
  return client;
}

export async function recordApi(): Promise<Case[]> {
  const root = scratch("api");
  const data = join(root, "data");
  const agentHome = join(root, "agent-home");
  const home = join(root, "home");
  cpSync(join(fixture, "data"), data, { recursive: true });
  cpSync(join(fixture, "agent-home"), agentHome, { recursive: true });
  // Git keeps no modes or empty directories; the fixture lists both (see its README).
  for (const file of expected.files) {
    if (file.type === "directory")
      mkdirSync(join(root, file.path), { recursive: true });
    chmodSync(join(root, file.path), Number.parseInt(file.mode, 8));
  }
  // Import sources are scanned under $HOME: synthetic CLIProxyAPI files only.
  mkdirSync(join(home, ".cli-proxy-api"), { recursive: true });
  writeFileSync(
    join(home, ".cli-proxy-api", "claude-one.json"),
    JSON.stringify({
      type: "claude",
      email: "ada@example.com",
      account_uuid: "AAAAAAAA-0000-4000-8000-000000000001",
      organization_uuid: "bbbbbbbb-0000-4000-8000-000000000001",
      organization_name: "Acme",
    }),
  );
  writeFileSync(
    join(home, ".cli-proxy-api", "codex-one.json"),
    JSON.stringify({ type: "codex", email: "ada@example.com" }),
  );
  writeFileSync(
    join(home, ".cli-proxy-api", "gemini.json"),
    JSON.stringify({ type: "gemini-cli", email: "x@example.com" }),
  );
  writeFileSync(join(home, ".cli-proxy-api", "broken.json"), "{");
  writeFileSync(join(home, ".cli-proxy-api", "notes.txt"), "ignored");
  mkdirSync(join(home, ".local/share/com.cpa.gui/oauth"), { recursive: true });
  writeFileSync(
    join(home, ".local/share/com.cpa.gui/oauth", "kimi.json"),
    JSON.stringify({ type: "kimi" }),
  );
  const client = clientDirectory(root);
  const before = new Map([
    ...tree(data, "<data>"),
    ...tree(agentHome, "<agent-home>"),
  ]);

  const originalFetch = globalThis.fetch;
  // models.dev answers with the fixed catalog; every other outside request fails.
  globalThis.fetch = fetchStub((url, init) =>
    url.startsWith("http://127.0.0.1:")
      ? originalFetch(url, init)
      : modelsDevFetch(url),
  );
  let app: Application | undefined;
  const cases: Case[] = [];
  try {
    app = await Application.create({
      directory: data,
      agentHome,
      binary: join(root, "no-core"),
      clientDirectory: client,
      // Fixed, so the port inside helper names and scripts is stable; must be free.
      port: apiPort,
      allowMissingCore: true,
    });
    const started = app;
    // Bun's homedir() ignores a changed $HOME, so the catalog scans the synthetic home's
    // known locations instead of the real user's; scan() itself is unchanged.
    Object.assign(started, {
      imports: new ImportCatalog([
        join(home, "Library/Application Support/com.cpa.gui/oauth"),
        join(home, ".local/share/com.cpa.gui/oauth"),
        join(home, ".cli-proxy-api"),
      ]),
    });
    // Warm the models.dev catalog so usage pricing does not depend on a background load.
    await started.models.catalog.models([], []);
    const placeholders = new Placeholders()
      .add(started.token, "<token>")
      .add(started.agentSetup.socketDirectory, "<socket-dir>")
      .add(data, "<data>")
      .add(agentHome, "<agent-home>")
      .add(home, "<home>")
      .add(root, "<root>");
    cases.push({
      name: "launch changes on disk",
      input: {
        fixture: "tests/fixtures/ts-data-dir copied to <data> and <agent-home>",
      },
      output: Object.fromEntries(
        diff(
          before,
          new Map([
            ...tree(data, "<data>"),
            ...tree(agentHome, "<agent-home>"),
          ]),
        ),
      ),
    });
    const port = started.port;
    const auth: Header = ["Authorization", `Bearer ${started.token}`];
    const json: Header = ["Content-Type", "application/json"];
    const call = async (name: string, request: Request) => {
      const response = await send(port, request);
      let body: Recorded = response.body;
      try {
        body = JSON.parse(response.body);
      } catch {
        // Not JSON: kept as text.
      }
      const large = (request.body?.length ?? 0) > 10_000;
      cases.push({
        name,
        input: large
          ? {
              ...request,
              body: `<${request.body?.length} bytes: {"name":"x…x"}>`,
            }
          : request,
        output: {
          status: response.status,
          headers: response.headers.filter(
            ([k]) => k !== "date" && k !== "etag" && k !== "keep-alive",
          ),
          etag: response.headers.find(([k]) => k === "etag")?.[1] ?? null,
          body,
        },
      });
    };
    const get = (name: string, path: string, headers: Header[] = [auth]) =>
      call(name, { method: "GET", target: path, headers });

    // Read routes.
    await get("GET /state", "/api/state");
    for (const profile of [team, personal, personal2, star]) {
      for (const days of ["7", "30", "3650"])
        await get(
          `GET usage ${profile.slug} days=${days}`,
          `/api/profiles/${profile.id}/usage?days=${days}`,
        );
      await get(
        `GET usage ${profile.slug} default days`,
        `/api/profiles/${profile.id}/usage`,
      );
      await get(`GET key ${profile.slug}`, `/api/profiles/${profile.id}/key`);
      await get(
        `GET oauth-session ${profile.slug}`,
        `/api/profiles/${profile.id}/oauth-session`,
      );
      for (const agent of ["claude", "codex", "opencode", "pi"])
        await get(
          `GET agent-setup ${profile.slug} ${agent}`,
          `/api/profiles/${profile.id}/agent-setup?agent=${agent}`,
        );
    }
    for (const project of ["projects/webapp", "projects/api server"])
      await get(
        `GET agent-setup team claude ${project}`,
        `/api/profiles/${team.id}/agent-setup?agent=claude&projectDirectory=${encodeURIComponent(join(expected.originalPaths.agentHome, project))}`,
      );
    for (const days of ["0", "3651", "abc", "1.5", "", "-1", "1"])
      await get(
        `GET usage days=${JSON.stringify(days)}`,
        `/api/profiles/${team.id}/usage?days=${days}`,
      );
    await get("GET import-sources", "/api/import-sources");
    await get(
      "GET agent-setup bad agent",
      `/api/profiles/${team.id}/agent-setup?agent=nope`,
    );
    await get(
      "GET agent-setup relative project",
      `/api/profiles/${team.id}/agent-setup?agent=claude&projectDirectory=rel`,
    );
    await get(
      "GET agent-setup project for pi",
      `/api/profiles/${team.id}/agent-setup?agent=pi&projectDirectory=/abs`,
    );
    await get("GET oauth without state", `/api/profiles/${team.id}/oauth`);
    await get(
      "GET oauth unknown state",
      `/api/profiles/${team.id}/oauth?state=nope`,
    );
    await get("GET models stopped profile", `/api/profiles/${team.id}/models`);
    await get(
      "GET reset-credits unknown account",
      `/api/profiles/${team.id}/accounts/nope.json/reset-credits`,
    );

    // Host, Origin and token checks.
    await get("bad host", "/api/state", [auth, ["Host", "evil.example"]]);
    await get("no host", "/api/state", [auth, ["Host", ""]]);
    await get("localhost host", "/api/state", [
      auth,
      ["Host", `localhost:${port}`],
    ]);
    await get("host without port", "/api/state", [auth, ["Host", "127.0.0.1"]]);
    await get("dev host when not development", "/api/state", [
      auth,
      ["Host", "127.0.0.1:5173"],
    ]);
    await get("same origin", "/api/state", [
      auth,
      ["Origin", `http://127.0.0.1:${port}`],
    ]);
    await get("cross origin", "/api/state", [
      auth,
      ["Origin", "http://evil.example"],
    ]);
    await get("localhost origin", "/api/state", [
      auth,
      ["Origin", `http://localhost:${port}`],
    ]);
    await get("null origin", "/api/state", [auth, ["Origin", "null"]]);
    await get("missing token", "/api/state", []);
    await get("wrong token", "/api/state", [["Authorization", "Bearer wrong"]]);
    await get("token without bearer", "/api/state", [
      ["Authorization", started.token],
    ]);
    await get("lowercase bearer", "/api/state", [
      ["Authorization", `bearer ${started.token}`],
    ]);
    await get("profile key as token", "/api/state", [
      ["Authorization", `Bearer nv_${"a".repeat(43)}`],
    ]);

    // Routing errors.
    await get("unknown route", "/api/nope");
    await get("unknown nested route", `/api/profiles/${team.id}/nope`);
    await call("wrong method on state", {
      method: "DELETE",
      target: "/api/state",
      headers: [auth],
    });
    await call("wrong method on agent-setup", {
      method: "PUT",
      target: `/api/profiles/${team.id}/agent-setup`,
      headers: [auth],
    });
    await call("wrong method on profiles", {
      method: "GET",
      target: "/api/profiles",
      headers: [auth],
    });
    await get("malformed path", "/api/profiles/%E0%A4%A/key");
    await get("bad profile id", "/api/profiles/not-a-uuid/key");
    await get(
      "unknown profile id",
      "/api/profiles/00000000-0000-4000-8000-000000000000/key",
    );
    await get(
      "upper-case profile id",
      `/api/profiles/${team.id.toUpperCase()}/key`,
    );
    await get("encoded slash in profile id", `/api/profiles/${team.id}%2Fkey`);
    await get("double slash", `/api//profiles//${team.id}//key`);
    await get("trailing slash state", "/api/state/");
    await get("query on state", "/api/state?x=1");
    await get("api without slash", "/api");

    // Bodies.
    const post = (
      name: string,
      target: string,
      headers: Header[],
      body?: string,
    ) =>
      call(name, {
        method: "POST",
        target,
        headers,
        ...(body !== undefined && { body }),
      });
    await post("bad body json", "/api/profiles", [auth, json], "{");
    await post("bad body schema", "/api/profiles", [auth, json], '{"name":""}');
    await post(
      "unknown body field",
      "/api/profiles",
      [auth, json],
      '{"name":"x","extra":1}',
    );
    await post(
      "wrong content type",
      "/api/profiles",
      [auth, ["Content-Type", "text/plain"]],
      '{"name":"x"}',
    );
    await post("missing content type", "/api/profiles", [auth], '{"name":"x"}');
    await call("json with charset", {
      method: "PUT",
      target: "/api/usage-retention",
      headers: [auth, ["Content-Type", "application/json; charset=utf-8"]],
      body: '{"days":365}',
    });
    await call("retention bad value", {
      method: "PUT",
      target: "/api/usage-retention",
      headers: [auth, json],
      body: '{"days":7}',
    });
    await post(
      "oversized body",
      "/api/profiles",
      [auth, json],
      `{"name":"${"x".repeat(2_097_200)}"}`,
    );
    await post(
      "body on a route without one",
      `/api/profiles/${star.id}/start`,
      [auth, json],
      "{}",
    );
    await call("patch bad strategy", {
      method: "PATCH",
      target: `/api/profiles/${star.id}`,
      headers: [auth, json],
      body: '{"strategy":"random"}',
    });
    await post(
      "api-account bad prefix",
      `/api/profiles/${star.id}/api-account`,
      [auth, json],
      JSON.stringify({
        name: "x",
        provider: "custom",
        baseUrl: "https://x.example/v1",
        apiKey: "k",
        prefix: "UPPER",
        models: [],
      }),
    );
    await post(
      "api-account unknown provider",
      `/api/profiles/${star.id}/api-account`,
      [auth, json],
      JSON.stringify({
        name: "x",
        provider: "claude",
        baseUrl: "https://x.example/v1",
        apiKey: "k",
        prefix: "",
        models: [],
      }),
    );
    await post(
      "oauth unknown provider",
      `/api/profiles/${star.id}/oauth`,
      [auth, json],
      '{"provider":"nope"}',
    );
    await post(
      "import-source bad id",
      `/api/profiles/${star.id}/import-source`,
      [auth, json],
      '{"sourceId":"abc"}',
    );
    await post(
      "import-source unknown id",
      `/api/profiles/${star.id}/import-source`,
      [auth, json],
      JSON.stringify({ sourceId: "0".repeat(64) }),
    );
    await post(
      "agent-check not running",
      `/api/profiles/${team.id}/agent-check`,
      [auth, json],
      "{}",
    );

    // The agent gateway on the same port.
    await get("gateway with origin", "/v1/models", [
      ["Authorization", `Bearer nv_${"a".repeat(43)}`],
      ["Origin", "http://localhost"],
    ]);
    await get("gateway stopped profile", "/v1/models", [
      ["Authorization", `Bearer nv_${"a".repeat(43)}`],
    ]);
    await get("gateway bad host", "/v1/models", [
      ["Authorization", `Bearer nv_${"a".repeat(43)}`],
      ["Host", "evil.example"],
    ]);

    // Static UI.
    for (const path of [
      "/",
      "/index.html",
      "/settings",
      "/profiles/abc",
      "/assets/index-AbCd1234.js",
      "/assets/index-AbCd1234.css",
      "/assets/font-Zz_-9999.woff2",
      "/assets/short-abc.js",
      "/assets/nested/deep-AbCd1234.js",
      "/logo.svg",
      "/icon.png",
      "/data.json",
      "/file%20with%20space.txt",
      "/unknown.xyz",
      "/missing.js",
      "/folder.d",
      "/../secret.txt",
      "/%2e%2e/secret.txt",
      "/..%2fsecret.txt",
      "/assets/..%2f..%2fsecret.txt",
      "/%2e%2e%2fclient-evil.txt",
      "/%E0%A4%A.js",
      "//logo.svg",
      "/logo.svg?x=1",
    ])
      await get(`static GET ${path}`, path, []);
    await call("static HEAD index", {
      method: "HEAD",
      target: "/",
      headers: [],
    });
    await call("static POST", { method: "POST", target: "/", headers: [] });
    const index = await send(port, { method: "GET", target: "/", headers: [] });
    const etag = index.headers.find(([k]) => k === "etag")?.[1] ?? "";
    // The ETag starts with a per-launch random asset version.
    placeholders.add(
      /^W\/"([0-9a-f-]{36})-/.exec(etag)?.[1],
      "<asset-version>",
    );
    await get("static If-None-Match exact", "/", [["If-None-Match", etag]]);
    await get("static If-None-Match strong form", "/", [
      ["If-None-Match", etag.replace(/^W\//, "")],
    ]);
    await get("static If-None-Match list", "/", [
      ["If-None-Match", `"other", ${etag}`],
    ]);
    await get("static If-None-Match star", "/", [["If-None-Match", "*"]]);
    await get("static If-None-Match other", "/", [
      ["If-None-Match", '"other"'],
    ]);
    await get("static bad host", "/", [["Host", "evil.example"]]);
    return placeholders.apply(cases);
  } finally {
    await app?.close();
    if (app)
      rmSync(app.agentSetup.socketDirectory, { recursive: true, force: true });
    globalThis.fetch = originalFetch;
  }
}
