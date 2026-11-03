// Isolated Activity fixture server. No accounts, core processes, or external fetches.
// Run beside NONSTOPVIBIN_PORT=14318 bun run dev:web -- --port 15173.
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Application } from "../src/server/server.ts";

// Constructors capture this offline fetcher for pricing and model metadata.
globalThis.fetch = async () => new Response("Offline fixture", { status: 503 });
const directory = await mkdtemp(join(tmpdir(), "nv-activity-preview-"));
const app = await Application.create({
  directory,
  agentHome: join(directory, "agent-home"),
  binary: join(directory, "no-core"),
  clientDirectory: resolve("dist/client"),
  development: true,
  port: 14318,
});
const multiple = app.store.createProfile("Chart multiple", "forest");
const single = app.store.createProfile("Chart single", "blue");
app.store.createProfile("Chart empty", "clay");
const now = Date.now();
for (const [index, [age, model, totalTokens]] of [
  [0.5, "fixture-large", 1200],
  [0.25, "fixture-large", 300],
  [0.2, "fixture-zero", 0],
  [2, "fixture-small", 500],
  [10, "fixture-older", 2000],
  [4000, "fixture-archive", 4000],
].entries()) {
  app.store.addUsage([
    {
      id: `preview-${index}`,
      profileId: multiple.id,
      timestamp: new Date(now - Number(age) * 86_400_000).toISOString(),
      model: String(model),
      provider: "custom",
      account: "synthetic",
      inputTokens: Number(totalTokens),
      outputTokens: 0,
      cachedTokens: 0,
      cacheWriteTokens: 0,
      reasoningTokens: 0,
      totalTokens: Number(totalTokens),
      latencyMs: 100,
      failed: totalTokens === 0,
      statusCode: totalTokens === 0 ? 429 : 200,
      stream: false,
    },
  ]);
}
app.store.addUsage([
  {
    ...app.store.usage(multiple.id)[0],
    id: "preview-single",
    profileId: single.id,
    model: "fixture-single",
    totalTokens: 1234,
    inputTokens: 1234,
    failed: false,
    statusCode: 200,
  },
]);
// Do not print management tokens; the browser reads this owner-only local URL file.
await writeFile(
  join(directory, "browser-url"),
  `http://127.0.0.1:15173/#session=${app.token}`,
  { mode: 0o600 },
);
console.log(`Activity fixtures ready; isolated directory: ${directory}`);
for (const signal of ["SIGTERM", "SIGINT"] as const) {
  process.once(signal, () => {
    void app.close().then(() => process.exit(0));
  });
}
