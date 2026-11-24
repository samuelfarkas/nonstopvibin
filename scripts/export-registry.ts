// `bun scripts/export-registry.ts [--check]`: writes the provider, agent and vocabulary fields
// the Rust server reads to crates/domain/data/registry.json, and fails when relations the Rust
// types cannot state disagree. src/shared stays the source; `--check` fails instead of writing
// when the committed file is stale.
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { agents } from "../src/shared/agents.ts";
import { providers, type ProviderEntry } from "../src/shared/providers.ts";
import {
  allHistoryDays,
  credentialFileBytes,
  nameLength,
  pollMs,
  prefixLength,
  priorityLimit,
  quotaRefreshMs,
  usagePageSize,
} from "../src/shared/vocabulary.ts";

const failures: string[] = [];
// An agent's native vendor and the providers whose nativeAgent it is encode one relation.
for (const [agent, entry] of Object.entries(agents)) {
  const vendor = Object.values<ProviderEntry>(providers).some(
    (provider) => provider.nativeAgent === agent,
  );
  if (Boolean(entry.native) !== vendor)
    failures.push(
      `agents.${agent}.native and the providers' nativeAgent disagree.`,
    );
}
for (const [id, entry] of Object.entries<ProviderEntry>(providers)) {
  const upstream = entry.preset?.upstream;
  if (upstream !== undefined && upstream !== id)
    failures.push(`providers.${id}.preset.upstream must name ${id} itself.`);
}
if (failures.length > 0) {
  console.error(failures.join("\n"));
  process.exit(1);
}

const provider = (entry: ProviderEntry) => ({
  label: entry.label,
  catalog: entry.catalog,
  nativeAgent: entry.nativeAgent,
  hosts: entry.hosts,
  credentialFile: entry.credentialFile,
  signIn: entry.signIn && { core: entry.signIn.core },
  apiKey: entry.apiKey,
  preset: entry.preset && {
    name: entry.preset.name,
    baseUrl: entry.preset.baseUrl,
    fixedEndpoint: entry.preset.fixedEndpoint,
    upstream: entry.preset.upstream,
  },
  bankedResets: entry.bankedResets,
  organizations: entry.organizations,
  reconnect: entry.reconnect,
});
const target = join(import.meta.dirname, "../crates/domain/data/registry.json");
const registry = {
  providers: Object.fromEntries(
    Object.entries<ProviderEntry>(providers).map(([id, entry]) => [
      id,
      provider(entry),
    ]),
  ),
  agents: Object.fromEntries(
    Object.entries(agents).map(([id, { title, project, native, catalog }]) => [
      id,
      { title, project, native, catalog },
    ]),
  ),
  limits: {
    nameLength,
    prefixLength,
    priorityLimit,
    credentialFileBytes,
    allHistoryDays,
  },
  usagePageSize,
  quotaRefreshMs,
  pollMs,
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
