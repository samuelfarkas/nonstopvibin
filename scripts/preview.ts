// Explicit visual/agent simulator. Its data and credentials never enter the installed app.
import http from "node:http";
import { mkdtemp, writeFile, mkdir } from "node:fs/promises";
import { resolve, join } from "node:path";
import { Application } from "../src/server/server.ts";
import { runHost } from "../src/server/host.ts";
import { sessionUrl } from "../src/shared/renderer-url.ts";

await mkdir(".test-runtime", { recursive: true });
const directory = await mkdtemp(resolve(".test-runtime/preview-"));
const upstream = http.createServer(async (req, res) => {
  const chunks: Buffer[] = [];
  for await (const c of req) chunks.push(Buffer.from(c));
  let body: { model?: string; stream?: boolean };
  try {
    body = JSON.parse(Buffer.concat(chunks).toString() || "{}");
  } catch {
    res.writeHead(400).end();
    return;
  }
  if (req.url === "/v1/models") {
    res.setHeader("Content-Type", "application/json");
    res.end(
      JSON.stringify({
        data: [
          { id: "fixture-model", owned_by: "anthropic" },
          { id: "fixture-opus", owned_by: "anthropic" },
          { id: "fixture-gpt", owned_by: "openai" },
          { id: "fixture-codex", owned_by: "openai" },
        ],
      }),
    );
    return;
  }
  const model = body.model || "fixture-model";
  const answer = "The local nonstopvibin connection is working.";
  const usage = { prompt_tokens: 18, completion_tokens: 9, total_tokens: 27 };
  if (body.stream) {
    res.writeHead(200, { "Content-Type": "text/event-stream" });
    for (const delta of [{ role: "assistant", content: answer }, {}])
      res.write(
        `data: ${JSON.stringify({ id: "chatcmpl-fixture", object: "chat.completion.chunk", model, created: Math.floor(Date.now() / 1000), choices: [{ index: 0, delta, finish_reason: "content" in delta ? null : "stop" }], ...(!("content" in delta) && { usage }) })}\n\n`,
      );
    res.end("data: [DONE]\n\n");
  } else {
    res.setHeader("Content-Type", "application/json");
    res.end(
      JSON.stringify({
        id: "chatcmpl-fixture",
        object: "chat.completion",
        model,
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: answer },
            finish_reason: "stop",
          },
        ],
        usage,
      }),
    );
  }
});
await new Promise<void>((resolve) =>
  upstream.listen(4322, "127.0.0.1", resolve),
);
const app = await runHost(({ binary, clientDirectory }, signal) =>
  Application.create(
    {
      directory,
      agentHome: join(directory, "agent-home"),
      binary,
      clientDirectory,
      port: 4321,
    },
    signal,
  ),
);
const work = app.store.createProfile("Work");
const personal = app.store.createProfile("Personal");
for (const [profile, accounts] of [
  [
    work,
    [
      ["claude", "Claude Max", 72, 46],
      ["codex", "Codex Team", 84, 61],
      ["codex", "Codex Pro", 68, 82],
    ],
  ],
  [personal, [["claude", "Claude Pro", 91, 65]]],
] as const) {
  for (const [index, [provider, name, session, week]] of accounts.entries()) {
    const id = `${profile.id}-${index}`;
    app.store.saveApiAccount(
      profile.id,
      {
        id,
        name,
        provider,
        baseUrl: "http://127.0.0.1:4322/v1",
        prefix: "",
        disabled: false,
        models: [{ id: "fixture-model", protocol: "openai" }],
      },
      `simulated-${id}`,
    );
    app.store.saveQuota(profile.id, id, {
      status: "available",
      checkedAt: new Date().toISOString(),
      windows: [
        {
          label: "5-hour",
          remainingPercent: session,
          resetsAt: new Date(Date.now() + 8_000_000).toISOString(),
        },
        {
          label: "Weekly",
          remainingPercent: week,
          resetsAt: new Date(Date.now() + 230_000_000).toISOString(),
        },
      ],
    });
  }
  await app.cores.pool.start(profile.id);
}
app.store.createProfile("Side projects");
let seed = 7;
const random = () => {
  seed = (seed * 48271) % 2147483647;
  return seed / 2147483647;
};
const models = [
  "claude-sonnet-4-5",
  "gpt-5-codex",
  "claude-opus-4-1",
  "claude-haiku-4-5",
];
const usageAccounts = [`${work.id}-0`, `${work.id}-1`, `${work.id}-2`];
app.store.addUsage(
  Array.from({ length: 480 }, (_, i) => {
    const input = Math.round(400 + random() * 9000);
    const output = Math.round(50 + random() * 1800);
    const cached = Math.round(random() * input * 1.4);
    const reasoning = random() < 0.3 ? Math.round(random() * 900) : 0;
    const failed = random() < 0.04;
    return {
      id: `sim-${i}`,
      profileId: work.id,
      timestamp: new Date(
        Date.now() - Math.floor(random() * 7 * 86_400_000),
      ).toISOString(),
      provider: random() < 0.6 ? "claude" : "codex",
      model: models[Math.floor(random() * models.length)],
      account: usageAccounts[Math.floor(random() * usageAccounts.length)],
      inputTokens: input,
      outputTokens: failed ? 0 : output,
      cachedTokens: cached,
      reasoningTokens: reasoning,
      cacheWriteTokens: 0,
      totalTokens: input + output + cached + reasoning,
      latencyMs: Math.round(300 + random() * 6000),
      failed,
      statusCode: failed ? 429 : 200,
      stream: random() < 0.8,
    };
  }),
);
// Keep simulated quota readings stable; the simulator never contacts provider quota services.
app.cores.quotas.refresh = async (profileId, accountId) =>
  app.store.quota(profileId, accountId)!;
await writeFile(
  join(directory, "connection.json"),
  JSON.stringify({
    endpoint: app.endpoint(work.id),
    key: app.store.clientKey(work.id),
    provider: `nonstopvibin-${work.slug}`,
  }),
  { mode: 0o600 },
);
console.log(`SIMULATOR: ${sessionUrl(app.origin, app.token)}`);
console.log(`Test directory: ${directory}`);
