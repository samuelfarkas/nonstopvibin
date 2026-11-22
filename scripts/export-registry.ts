// `bun scripts/export-registry.ts [--check]`: writes the provider, agent and vocabulary data
// the Rust server embeds to crates/domain/data/registry.json. src/shared stays the source;
// `--check` fails instead of writing when the committed file is stale.
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { agents } from "../src/shared/agents.ts";
import { protocols, providers } from "../src/shared/providers.ts";
import {
  allHistoryDays,
  credentialFileBytes,
  nameLength,
  prefixLength,
  priorityLimit,
  quotaRefreshMs,
  quotaStaleMs,
  resetResultCodes,
  retentionDays,
  strategies,
  usagePageSize,
} from "../src/shared/vocabulary.ts";

const target = join(import.meta.dirname, "../crates/domain/data/registry.json");
const registry = {
  providers,
  agents,
  protocols,
  strategies,
  retentionDays,
  limits: {
    nameLength,
    prefixLength,
    priorityLimit,
    credentialFileBytes,
    allHistoryDays,
  },
  usagePageSize,
  quota: { refreshMs: quotaRefreshMs, staleMs: quotaStaleMs },
  resetResultCodes,
};
const contents = `${JSON.stringify(registry, null, 2)}\n`;

if (process.argv.includes("--check")) {
  const committed = await readFile(target, "utf8").catch(() => "");
  if (committed !== contents) {
    console.error(
      "crates/domain/data/registry.json is stale: run `bun scripts/export-registry.ts`.",
    );
    process.exit(1);
  }
} else {
  await mkdir(dirname(target), { recursive: true });
  await writeFile(target, contents);
}
