import type { ApiAccount, Profile } from "../shared/types.ts";
import { createHash } from "node:crypto";
import { version } from "../../package.json";

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
    for (const protocol of ["openai", "anthropic", "responses"] as const) {
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
