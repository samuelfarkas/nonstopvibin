// Records the TypeScript server's behaviour as golden files for the Rust port:
//   bun tests/golden/record.ts [area ...]
// Each area calls the real src/server code and writes tests/golden/<area>.json.
// See README.md for the case format, placeholders and what is not recorded.
import { rmSync } from "node:fs";
import { setSystemTime } from "bun:test";
import { recordAccounts } from "./areas/accounts.ts";
import { recordAgents } from "./areas/agents.ts";
import { recordApi } from "./areas/api.ts";
import { recordCli } from "./areas/cli.ts";
import { recordConfig } from "./areas/config.ts";
import { recordGateway } from "./areas/gateway.ts";
import { recordRestart } from "./areas/restart.ts";
import { recordUsage } from "./areas/usage.ts";
import { type Case, writeArea } from "./lib.ts";

const areas = new Map<string, () => Promise<Case[]>>([
  ["gateway", recordGateway],
  ["config", recordConfig],
  ["usage", recordUsage],
  ["accounts", recordAccounts],
  ["agents", recordAgents],
  ["restart", recordRestart],
  ["api", recordApi],
  ["cli", recordCli],
]);
// Every clock the server reads says this instant, so timestamps it makes are stable.
setSystemTime(new Date("2026-11-22T12:00:00.000Z"));
const selected = process.argv.slice(2);
for (const name of selected.length ? selected : areas.keys()) {
  const record = areas.get(name);
  if (!record) throw new Error(`Unknown area ${name}`);
  console.log(`${name}: ${writeArea(name, await record())} cases`);
}
// Scratch directories live at fixed paths so recorded paths are stable; drop them.
rmSync("/tmp/nsv-golden", { recursive: true, force: true });
