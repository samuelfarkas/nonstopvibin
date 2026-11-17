import { chmod, mkdir, writeFile } from "node:fs/promises";
import YAML from "yaml";
import type { Json, Profile } from "../shared/types.ts";
import { protocols } from "../shared/providers.ts";
import type { ApiAccount, Store } from "./store.ts";
import { createHash } from "node:crypto";
import { version } from "../../package.json";
import {
  coreConfigPath,
  credentialDirectory,
  profileDirectory,
} from "./data-directory.ts";
import { record } from "./json.ts";

export { version };
export const userAgent = `nonstopvibin/${version}`;

// Upstream CLIProxyAPI config entries. Type aliases keep them assignable to Json.
type ModelEntry = { name: string; alias: string };
type CompatibleEntry = {
  name: string;
  prefix: string;
  "base-url": string;
  headers: Record<string, string>;
  "api-key-entries": Array<{ "api-key": string }>;
  models: ModelEntry[];
};
type KeyEntry = {
  "api-key": string;
  prefix: string;
  "base-url": string;
  headers: Record<string, string>;
  models: ModelEntry[];
};
export type CoreConfiguration = ReturnType<typeof coreConfiguration>;

export function coreConfiguration(
  profile: Profile,
  port: number,
  authDirectory: string,
  keys: { core: string; management: string },
  accounts: ApiAccount[],
  getApiKey: (id: string) => string,
) {
  const compatible: CompatibleEntry[] = [];
  const claude: KeyEntry[] = [];
  const responses: KeyEntry[] = [];
  for (const account of accounts.filter((a) => !a.disabled)) {
    const headers: Record<string, string> =
      account.provider === "opencode-go"
        ? {
            "User-Agent": `${userAgent} (coding-agent-gateway)`,
            "x-opencode-session": "$x-opencode-session",
          }
        : {};
    for (const protocol of protocols) {
      const models = account.models
        .filter((m) => m.protocol === protocol)
        .map((m) => ({ name: m.id, alias: m.id }));
      if (!models.length) continue;
      const key = getApiKey(account.id);
      if (protocol === "openai") {
        const identity = createHash("sha256")
          .update(JSON.stringify([account.baseUrl, account.prefix, models]))
          .digest("hex");
        // The name hashes the base URL, prefix and models, so a match shares all three.
        const name = `${account.provider}-${identity}`;
        const existing = compatible.find((p) => p.name === name);
        if (existing) existing["api-key-entries"].push({ "api-key": key });
        else
          compatible.push({
            name,
            prefix: account.prefix,
            "base-url": account.baseUrl,
            headers,
            "api-key-entries": [{ "api-key": key }],
            models,
          });
      } else {
        const entry = {
          "api-key": key,
          prefix: account.prefix,
          "base-url":
            protocol === "anthropic"
              ? account.baseUrl.replace(/\/v1\/?$/, "")
              : account.baseUrl,
          headers,
          models,
        };
        (protocol === "anthropic" ? claude : responses).push(entry);
      }
    }
  }
  return {
    host: "127.0.0.1",
    port,
    "auth-dir": authDirectory,
    "api-keys": [keys.core],
    "remote-management": {
      "allow-remote": false,
      "secret-key": keys.management,
      "disable-control-panel": true,
      "disable-auto-update-panel": true,
    },
    debug: false,
    // Skip body-capturing middleware, including error-only capture. This is
    // an upstream performance option, unrelated to the application's license.
    "commercial-mode": true,
    "logging-to-file": false,
    "request-log": false,
    "error-logs-max-files": 10,
    "usage-statistics-enabled": true,
    "redis-usage-queue-retention-seconds": 86400,
    "request-retry": 1,
    "max-retry-interval": 5,
    "ws-auth": true,
    "quota-exceeded": {
      "switch-project": true,
      "switch-preview-model": false,
      "antigravity-credits": false,
    },
    routing: {
      strategy: profile.strategy,
      "session-affinity": profile.sessionAffinity,
      "session-affinity-ttl": "1h",
    },
    plugins: { enabled: false },
    "openai-compatibility": compatible,
    "claude-api-key": claude,
    "codex-api-key": responses,
  };
}

/** Writes each profile's core config from the store, one write per profile at a time. */
export class ConfigWriter {
  private readonly store: Store;
  private readonly writes = new Map<string, Promise<CoreConfiguration>>();
  constructor(store: Store) {
    this.store = store;
  }
  async write(profileId: string, port: number): Promise<CoreConfiguration> {
    const previous = this.writes.get(profileId) ?? Promise.resolve();
    const operation = previous
      .catch(() => undefined)
      .then(() => this.writeFile(profileId, port));
    this.writes.set(profileId, operation);
    try {
      return await operation;
    } finally {
      if (this.writes.get(profileId) === operation)
        this.writes.delete(profileId);
    }
  }
  private async writeFile(
    profileId: string,
    port: number,
  ): Promise<CoreConfiguration> {
    // An unknown profile fails before any directory is created.
    this.store.profile(profileId);
    const directory = profileDirectory(this.store.directory, profileId);
    const authDir = credentialDirectory(this.store.directory, profileId);
    await mkdir(authDir, { recursive: true, mode: 0o700 });
    await chmod(directory, 0o700);
    await chmod(authDir, 0o700);
    // Read after the awaits above, so a settings change saved meanwhile is written.
    const config = coreConfiguration(
      this.store.profile(profileId),
      port,
      authDir,
      {
        core: this.store.coreKey(profileId),
        management: this.store.managementKey(profileId),
      },
      this.store.apiAccounts(profileId),
      (id) => this.store.apiKey(id),
    );
    const path = coreConfigPath(this.store.directory, profileId);
    // The core watches this inode; atomic replacement drops its Linux watch.
    // This derived config is regenerated from SQLite before every core start.
    await writeFile(path, YAML.stringify(config), { mode: 0o600 });
    await chmod(path, 0o600);
    return config;
  }
}

// The entries a running core reloads from a changed file; the rest apply at its next start.
const liveFields = [
  "routing",
  "openai-compatibility",
  "claude-api-key",
  "codex-api-key",
] as const;
// The Go core omits zero values, so "", false, 0, and [] read back as absent.
function matches(
  expected: Json | undefined,
  actual: Json | undefined,
): boolean {
  if (Array.isArray(expected))
    return expected.length === 0
      ? actual == null || (Array.isArray(actual) && actual.length === 0)
      : Array.isArray(actual) &&
          expected.length === actual.length &&
          expected.every((value, index) => matches(value, actual[index]));
  if (expected instanceof Object)
    return Object.entries(expected).every(([key, value]) =>
      matches(value, record(actual)[key]),
    );
  return (
    expected === actual ||
    ((expected === "" || expected === false || expected === 0) &&
      actual === undefined)
  );
}
/** Whether a core's reported config shows every live entry of `expected`. */
export function applied(expected: CoreConfiguration, actual: Json): boolean {
  const reported = record(actual);
  return liveFields.every((key) => matches(expected[key], reported[key]));
}
