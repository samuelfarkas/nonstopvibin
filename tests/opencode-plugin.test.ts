import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";
import { openCodePlugin } from "../src/server/agent-opencode.ts";
import type { Json, Profile } from "../src/shared/types.ts";

// The subset of OpenCode's provider config the generated plugin reads and writes.
interface OpenCodeProvider {
  npm: string;
  options?: { fetch: typeof fetch; apiKey?: string };
  models: Record<string, Json>;
}
interface OpenCodeConfig {
  provider: Record<string, OpenCodeProvider>;
  small_model: string;
}
/** Output slot the small_model hook fills with the chosen provider model. */
interface SmallModelOutput {
  model?: Json;
}

test("OpenCode native plugin keeps live credentials and background requests in their profile", async () => {
  const directory = await mkdtemp("/tmp/nv-opencode-");
  const socket = join(directory, "bridge.sock");
  const profile: Profile = {
    id: randomUUID(),
    slug: "work",
    name: "Work's ${literal}",
    strategy: "round-robin",
    sessionAffinity: false,
    enabled: true,
    createdAt: "2026-09-06",
  };
  const providerID = `nonstopvibin-${profile.slug}`;
  let key = `nv_${"A".repeat(43)}`;
  let port = 4318;
  let status = 200;
  let bridgeRequests = 0;
  const server = http.createServer((request, response) => {
    bridgeRequests++;
    assert.equal(request.url, `/profiles/${profile.id}`);
    assert.equal(request.headers.host, "localhost");
    assert.equal(request.method, "GET");
    response.writeHead(status).end(`${key}\n${port}`);
  });
  await new Promise<void>((resolve) => server.listen(socket, resolve));
  const originalFetch = globalThis.fetch;
  let requests = 0;
  let redirect = false;
  globalThis.fetch = Object.assign(
    async (input: RequestInfo | URL, init?: RequestInit) => {
      if (String(input).endsWith("/models?nonstopvibin=opencode")) {
        assert.equal(
          new Headers(init?.headers).get("Authorization"),
          `Bearer ${key}`,
        );
        return Response.json({
          models: [
            {
              id: "vendor/{file:no-read}__model",
              name: "First",
              limit: { context: 64000, output: 8000 },
              cost: { input: 1, output: 2 },
            },
            {
              id: "second-model",
              name: "Second",
              limit: { context: 128000, output: 16000 },
              cost: { input: 3, output: 4 },
            },
          ],
        });
      }
      requests++;
      assert.equal(
        String(input instanceof Request ? input.url : input),
        "http://127.0.0.1:4318/p/work/v1/chat/completions",
      );
      assert.equal(
        new Headers(init?.headers).get("Authorization"),
        `Bearer ${key}`,
      );
      assert.equal(new Headers(init?.headers).get("x-fixture"), "preserved");
      assert.equal(init?.redirect, "error");
      return new Response("fixture", { status: redirect ? 302 : 200 });
    },
    originalFetch,
  );
  try {
    const code = openCodePlugin(profile, socket, port);
    assert.equal(code.includes(key), false);
    const path = join(directory, "plugin.mjs");
    await writeFile(path, code);
    const plugin = await (await import(pathToFileURL(path).href)).default();
    const existing = { npm: "existing", models: {} };
    const config: OpenCodeConfig = {
      provider: { unrelated: existing },
      small_model: "unrelated/model",
    };
    await plugin.config(config);
    assert.equal(config.provider.unrelated, existing);
    assert.equal(config.small_model, "unrelated/model");
    const provider = config.provider[providerID];
    assert.deepEqual(Object.keys(provider.models), [
      "vendor/{file:no-read}__model",
      "second-model",
    ]);
    assert.ok(provider.options);
    assert.equal(provider.options.apiKey, undefined);
    await assert.rejects(plugin.config(config), /conflicting/);
    const endpoint = "http://127.0.0.1:4318/p/work/v1/chat/completions";
    const init = {
      method: "POST",
      headers: { "x-fixture": "preserved", Authorization: "Bearer old" },
      body: "{}",
    };
    await provider.options.fetch(endpoint, init);
    key = `nv_${"B".repeat(43)}`;
    await provider.options.fetch(
      new Request(endpoint, { headers: init.headers }),
      { method: "POST" },
    );
    assert.equal(requests, 2);
    assert.equal(bridgeRequests, 3);
    for (const destination of [
      endpoint.replace("work", "personal"),
      endpoint.replace("4318", "4319"),
      endpoint.replace("127.0.0.1", "example.invalid"),
      endpoint + "?redirect=1",
      endpoint.replace("chat/completions", "../../api/state"),
    ]) {
      await assert.rejects(
        provider.options.fetch(destination, init),
        /outside this profile/,
      );
    }
    assert.equal(
      bridgeRequests,
      3,
      "destinations are checked before credentials are requested",
    );
    port = 4319;
    await assert.rejects(
      provider.options.fetch(endpoint, init),
      /port changed/,
    );
    port = 4318;
    key = "malformed";
    await assert.rejects(provider.options.fetch(endpoint, init), /start work/);
    key = `nv_${"C".repeat(43)}`;
    status = 409;
    await assert.rejects(provider.options.fetch(endpoint, init), /start work/);
    assert.equal(requests, 2);
    status = 200;
    redirect = true;
    await assert.rejects(provider.options.fetch(endpoint, init), /redirect/);
    const user = { model: { providerID } };
    await assert.rejects(
      plugin["chat.params"]({
        message: user,
        model: { providerID: "personal" },
      }),
      /background model/,
    );
    await plugin["chat.params"]({ message: user, model: { providerID } });
    await plugin["chat.params"]({
      message: { model: { providerID: "unrelated" } },
      model: { providerID: "unrelated" },
    });
    const background: SmallModelOutput = {};
    await plugin["experimental.provider.small_model"](
      { provider: { id: providerID, models: provider.models } },
      background,
    );
    assert.equal(
      background.model,
      provider.models["vendor/{file:no-read}__model"],
    );
    await new Promise<void>((resolve) => server.close(() => resolve()));
    await assert.rejects(provider.options.fetch(endpoint, init), /start work/);
    assert.equal(requests, 3);
  } finally {
    globalThis.fetch = originalFetch;
    server.close();
    await rm(directory, { recursive: true, force: true });
  }
});
