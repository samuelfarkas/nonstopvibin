// Builds a data directory with the TypeScript server's own modules, as an existing
// install holds it, for the Rust port's migration test. Run from the repository root:
//   bun tests/fixtures/ts-data-dir/generate.ts
// It builds under a fixed /tmp path, so paths inside files are the same on every run,
// then copies the result beside this script and writes expected.json.
// Every secret is synthetic and low-entropy. No core runs: OAuth credential files are
// written in the shape Credentials' reconnect path writes, and the core's management
// answers are stubbed where Accounts.sync and Usage.collect read them.
import type { ChildProcess } from "node:child_process";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { randomUUID } from "node:crypto";
import type {
  AgentSetupInput,
  Json,
  Profile,
} from "../../../src/shared/types.ts";
import type { ApiAccountInput } from "../../../src/shared/api.ts";
import { Accounts, type AccountSource } from "../../../src/server/accounts.ts";
import { AgentSetup } from "../../../src/server/agent-setup.ts";
import { apiAccount } from "../../../src/server/api-accounts.ts";
import { ConfigWriter } from "../../../src/server/config.ts";
import type { CorePool } from "../../../src/server/core.ts";
import { recordCore } from "../../../src/server/core-processes.ts";
import {
  credentialDirectory,
  databasePath,
  vaultKeyPath,
} from "../../../src/server/data-directory.ts";
import { parseQuota, Quotas } from "../../../src/server/quota.ts";
import { secretIds, Store } from "../../../src/server/store.ts";
import { Usage } from "../../../src/server/usage.ts";
import { fileKeyCodec } from "../../../src/server/vault.ts";

const root = "/tmp/nsv-ts-fixture";
const data = join(root, "data");
const agentHome = join(root, "agent-home");
const output = dirname(fileURLToPath(import.meta.url));
const repository = join(output, "../../..");
const gatewayPort = 4318;
const quotaTime = Date.parse("2026-11-22T12:00:00.000Z");

rmSync(root, { recursive: true, force: true });
mkdirSync(data, { recursive: true, mode: 0o700 });
mkdirSync(agentHome, { mode: 0o700 });
// Written before fileKeyCodec, which would otherwise create a random one.
const vaultKey = Buffer.from(Array.from({ length: 32 }, (_, i) => i + 1));
writeFileSync(vaultKeyPath(data), vaultKey, { mode: 0o600, flag: "wx" });
const store = new Store(data, fileKeyCodec(data));

// Profiles: an accented and punctuated name, a duplicate name for the `-2` slug,
// and a name with no slug characters at all.
const profileSpecs = [
  {
    name: "Équipe Café: R&D (été)",
    letter: "a",
    strategy: "round-robin",
    sessionAffinity: false,
    enabled: true,
    createdAt: "2026-01-05T09:00:00.000Z",
    corePort: 8317,
  },
  {
    name: "Personal",
    letter: "b",
    strategy: "fill-first",
    sessionAffinity: true,
    enabled: false,
    createdAt: "2026-02-10T12:30:00.000Z",
    corePort: 8318,
  },
  {
    name: "Personal",
    letter: "c",
    strategy: "round-robin",
    sessionAffinity: true,
    enabled: true,
    createdAt: "2026-03-15T18:45:30.500Z",
    corePort: 8319,
  },
  {
    name: "★ ★ ★",
    letter: "d",
    strategy: "fill-first",
    sessionAffinity: false,
    enabled: false,
    createdAt: "2026-04-01T00:00:00.000Z",
    corePort: 8320,
  },
] as const;
const profiles: Profile[] = profileSpecs.map((spec) => {
  const created = store.createProfile(spec.name);
  const profile: Profile = {
    ...created,
    strategy: spec.strategy,
    sessionAffinity: spec.sessionAffinity,
    enabled: spec.enabled,
    createdAt: spec.createdAt,
  };
  store.saveProfile(profile);
  store.setSecret(secretIds.client(profile.id), `nv_${spec.letter.repeat(43)}`);
  store.setSecret(secretIds.core(profile.id), `core-${spec.letter.repeat(32)}`);
  store.setSecret(
    secretIds.management(profile.id),
    `management-${spec.letter.repeat(32)}`,
  );
  return profile;
});
const [team, personal, personal2] = profiles;
if (!team || !personal || !personal2) throw new Error("Missing profiles");

// Retention first: setting it prunes against the wall clock, before any usage exists.
store.setUsageRetentionDays(365);

// API-key accounts through apiAccount, with models entered so nothing is discovered.
async function addApi(
  profile: Profile,
  input: ApiAccountInput,
  disabled = false,
): Promise<string> {
  const account = await apiAccount(input);
  store.saveApiAccount(profile.id, { ...account, disabled }, input.apiKey);
  return account.id;
}
const localMixed = await addApi(team, {
  name: "Local Mixed",
  provider: "custom",
  baseUrl: "http://127.0.0.1:11434/v1/",
  apiKey: "synthetic api key one",
  protocol: "openai",
  prefix: "local",
  models: [
    { id: "qwen3-coder:30b", protocol: "openai" },
    { id: "local-claude-proxy", protocol: "anthropic" },
    { id: "local-responses", protocol: "responses" },
  ],
});
const anthropicApi = await addApi(team, {
  name: "Anthropic API",
  provider: "anthropic",
  baseUrl: "https://api.anthropic.com/v1",
  apiKey: "synthetic api key two",
  protocol: "anthropic",
  prefix: "anthropic",
  models: [{ id: "claude-sonnet-4-5", protocol: "anthropic" }],
});
await addApi(
  team,
  {
    name: "OpenAI Team",
    provider: "openai",
    baseUrl: "https://api.openai.com/v1",
    apiKey: "synthetic api key three",
    protocol: "openai",
    prefix: "oai",
    models: [
      { id: "gpt-5", protocol: "openai" },
      { id: "gpt-5-codex", protocol: "responses" },
    ],
  },
  true,
);
await addApi(personal, {
  name: "Gemini",
  provider: "gemini",
  baseUrl: "https://generativelanguage.googleapis.com/v1beta/openai",
  apiKey: "synthetic api key six",
  protocol: "openai",
  prefix: "gem",
  models: [{ id: "gemini-2.5-pro", protocol: "openai" }],
});
// OpenCode Go picks each model's protocol; the second account shares the first's
// OpenAI-protocol entry, so the config lists both keys under one provider.
const goAccount = await addApi(personal2, {
  name: "OpenCode Go",
  provider: "opencode-go",
  baseUrl: "https://opencode.ai/zen/go/v1",
  apiKey: "synthetic api key four",
  protocol: "openai",
  prefix: "go",
  models: [
    { id: "glm-4.6", protocol: "openai" },
    { id: "qwen3-coder", protocol: "openai" },
    { id: "gpt-5-mini", protocol: "openai" },
  ],
});
await addApi(personal2, {
  name: "OpenCode Go spare",
  provider: "opencode-go",
  baseUrl: "https://opencode.ai/zen/go/v1",
  apiKey: "synthetic api key five",
  protocol: "openai",
  prefix: "go",
  models: [{ id: "glm-4.6", protocol: "openai" }],
});

// config.yaml for every profile, as before a core start; this also creates auth/.
const configs = new ConfigWriter(store);
for (const [index, profile] of profiles.entries())
  await configs.write(profile.id, profileSpecs[index]?.corePort ?? 0);

// OAuth credential files, as Credentials' reconnect path writes them.
type Credential = {
  profile: Profile;
  payload: Record<string, Json>;
  listing: Record<string, Json>;
};
const credentials: Credential[] = [
  {
    profile: team,
    payload: {
      type: "claude",
      email: "ada@example.com",
      access_token: "synthetic-access-token-claude",
      refresh_token: "synthetic-refresh-token-claude",
      id_token: "",
      expired: "2026-11-23T10:00:00Z",
      last_refresh: "2026-11-23T02:00:00Z",
      account_uuid: "aaaaaaaa-0000-4000-8000-000000000001",
      organization_uuid: "bbbbbbbb-0000-4000-8000-000000000001",
      organization_name: "Acme Research",
      disabled: false,
      note: "Work Claude",
      priority: 2,
    },
    listing: { status: "active", auth_index: "0000000000000001" },
  },
  {
    profile: team,
    payload: {
      type: "claude",
      email: "ada.personal@example.com",
      access_token: "synthetic-access-token-claude-personal",
      refresh_token: "synthetic-refresh-token-claude-personal",
      expired: "2026-11-20T10:00:00Z",
      last_refresh: "2026-11-20T02:00:00Z",
      account_uuid: "aaaaaaaa-0000-4000-8000-000000000002",
      disabled: true,
    },
    listing: { status: "disabled", auth_index: "0000000000000002" },
  },
  {
    profile: team,
    payload: {
      type: "codex",
      email: "ada@example.com",
      access_token: "synthetic-access-token-codex",
      refresh_token: "synthetic-refresh-token-codex",
      id_token: "synthetic-id-token-codex",
      account_id: "11111111-2222-4333-8444-555555555555",
      expired: "2026-11-30T10:00:00Z",
      last_refresh: "2026-11-22T10:00:00Z",
      disabled: false,
      note: "Codex Pro",
      priority: 1,
    },
    listing: {
      status: "error",
      status_message: "invalid grant (retrying)",
      unavailable: true,
      auth_index: "0000000000000003",
      id_token: { chatgpt_account_id: "11111111-2222-4333-8444-555555555555" },
    },
  },
  {
    profile: personal2,
    payload: {
      type: "claude",
      email: "grace@example.com",
      access_token: "synthetic-access-token-claude-cafe",
      refresh_token: "synthetic-refresh-token-claude-cafe",
      expired: "2026-11-23T11:00:00Z",
      last_refresh: "2026-11-23T03:00:00Z",
      // Upper case on disk; claudeIdentity lowercases it.
      account_uuid: "CCCCCCCC-0000-4000-8000-000000000003",
      organization_uuid: "DDDDDDDD-0000-4000-8000-000000000003",
      organization_name: "Café Ltd",
      disabled: false,
    },
    listing: {
      status: "active",
      unavailable: true,
      auth_index: "0000000000000004",
    },
  },
];
const credentialNames = new Map<Credential, string>();
for (const credential of credentials) {
  const provider = String(credential.payload.type);
  const name = `${provider}-${randomUUID()}.json`;
  credentialNames.set(credential, name);
  writeFileSync(
    join(credentialDirectory(data, credential.profile.id), name),
    JSON.stringify(credential.payload),
    { mode: 0o600, flag: "wx" },
  );
}

// Account metadata: Accounts.sync on a stubbed /auth-files listing, as the core lists them.
const listings = new Map<string, Json[]>();
for (const credential of credentials) {
  const { payload, listing, profile } = credential;
  const files = listings.get(profile.id) ?? [];
  files.push({
    name: credentialNames.get(credential) ?? "",
    provider: payload.type,
    type: payload.type,
    account_type: "oauth",
    email: payload.email,
    note: payload.note ?? "",
    priority: payload.priority ?? 0,
    disabled: payload.disabled ?? false,
    ...listing,
  });
  listings.set(profile.id, files);
}
// The core lists API-key entries too; sync skips them.
listings.get(team.id)?.push({
  name: "claude-api-key-1",
  provider: "claude",
  account_type: "api_key",
  auth_index: "0000000000000005",
});
const usageQueues = new Map<string, Json[]>();
const pool: AccountSource = {
  usable: () => true,
  runtimes: new Map(),
  management: async (profileId: string, path: string): Promise<Json> => {
    if (path === "/auth-files") return { files: listings.get(profileId) ?? [] };
    if (path.startsWith("/usage-queue?")) {
      const queue = usageQueues.get(profileId) ?? [];
      usageQueues.delete(profileId);
      return queue;
    }
    throw new Error(`Unexpected management call ${path}`);
  },
};
const accounts = new Accounts(pool, store);
for (const profileId of listings.keys()) await accounts.sync(profileId);

// Quotas in the shapes parseQuota and Quotas.refresh store.
const oauthId = (index: number) => {
  const credential = credentials[index];
  if (!credential) throw new Error(`No credential ${index}`);
  return credentialNames.get(credential) ?? "";
};
store.saveQuota(
  team.id,
  oauthId(0),
  parseQuota(
    "claude",
    {
      five_hour: { utilization: 25, resets_at: "2026-11-22T14:00:00Z" },
      seven_day: { utilization: 60.5, resets_at: "2026-11-27T00:00:00Z" },
      seven_day_opus: { utilization: 90, resets_at: "2026-11-27T00:00:00Z" },
      limits: [
        {
          kind: "weekly_scoped",
          scope: { model: { display_name: "Fable" } },
          percent: 5,
          resets_at: "2026-11-27T00:00:00Z",
        },
      ],
    },
    quotaTime,
  ),
);
store.saveQuota(
  team.id,
  oauthId(2),
  parseQuota(
    "codex",
    {
      plan_type: "pro",
      rate_limit: {
        primary_window: {
          used_percent: 40,
          limit_window_seconds: 18_000,
          reset_after_seconds: 3600,
        },
        secondary_window: {
          used_percent: 70,
          limit_window_seconds: 604_800,
          resets_at: 1_795_000_000,
        },
      },
      code_review_rate_limit: {
        primary_window: {
          used_percent: 0,
          limit_window_seconds: 604_800,
          reset_after_seconds: 86_400,
        },
      },
      rate_limit_reset_credits: { available_count: 2 },
    },
    quotaTime,
  ),
);
store.saveQuota(
  personal2.id,
  oauthId(3),
  parseQuota(
    "claude",
    {
      five_hour: { utilization: 100, resets_at: "2026-11-22T13:00:00Z" },
      seven_day: { utilization: 100 },
    },
    quotaTime,
  ),
);
store.saveQuota(personal2.id, goAccount, {
  ...parseQuota(
    "opencode-go",
    {
      usage: {
        rolling: { usagePercent: 12, resetInSec: 3600 },
        weekly: { used: 30, limit: 120, resetInSec: 172_800 },
        monthly: { usedPercent: 55 },
      },
    },
    quotaTime,
  ),
});
store.saveQuota(team.id, localMixed, {
  status: "unavailable",
  windows: [],
  checkedAt: "2026-11-22T12:00:00.000Z",
});
store.saveQuota(team.id, anthropicApi, {
  status: "error",
  windows: [],
  checkedAt: "2026-11-21T12:00:00.000Z",
  error: "Anthropic API returned HTTP 401. Reconnect this subscription.",
});

// Usage: Usage.collect on stubbed usage-queue pages, as the core queues them.
const breakdown = (
  uncached: number,
  cacheRead: number,
  cacheWrite: number,
  plain: number,
  reasoning: number,
) => ({
  schema_version: 2,
  quality: "complete",
  total_tokens: uncached + cacheRead + cacheWrite + plain + reasoning,
  unclassified_tokens: 0,
  input: {
    total_tokens: uncached + cacheRead + cacheWrite,
    uncached_tokens: uncached,
    cache_read_tokens: cacheRead,
    cache_write_tokens: cacheWrite,
  },
  output: {
    total_tokens: plain + reasoning,
    non_reasoning_tokens: plain,
    reasoning_tokens: reasoning,
  },
});
usageQueues.set(team.id, [
  {
    request_id: "req-0001",
    timestamp: "2026-11-16T09:15:00.000Z",
    provider: "claude",
    model: "claude-sonnet-4-5-20250929",
    alias: "claude-sonnet-4-5",
    auth_index: "0000000000000001",
    tokens: {
      input_tokens: 1200,
      output_tokens: 300,
      cache_read_tokens: 800,
      cache_creation_tokens: 100,
      total_tokens: 1500,
    },
    token_breakdown: breakdown(300, 800, 100, 300, 0),
    latency_ms: 2150,
    stream: true,
  },
  {
    request_id: "req-0002",
    timestamp: "2026-11-17T14:02:11.250Z",
    provider: "claude",
    model: "claude-opus-4-1",
    auth_index: "0000000000000001",
    tokens: { input_tokens: 5000, output_tokens: 1200, total_tokens: 6200 },
    // A breakdown whose parts disagree: stored as pricingTokens null.
    token_breakdown: {
      ...breakdown(5000, 0, 0, 1200, 0),
      total_tokens: 9999,
    },
    latency_ms: 8400,
    stream: true,
  },
  {
    request_id: "req-0003",
    timestamp: "2026-11-18T08:00:00.000Z",
    provider: "codex",
    model: "gpt-5-codex",
    auth_index: "0000000000000003",
    tokens: {
      input_tokens: 4000,
      output_tokens: 900,
      cached_tokens: 2500,
      reasoning_tokens: 400,
      total_tokens: 4900,
    },
    token_breakdown: breakdown(1500, 2500, 0, 500, 400),
    latency_ms: 5100,
    stream: false,
  },
  {
    request_id: "req-0004",
    timestamp: "2026-11-18T08:00:00.000Z",
    provider: "codex",
    model: "gpt-5-codex",
    auth_index: "0000000000000003",
    tokens: { input_tokens: 10, total_tokens: 10 },
    failed: true,
    fail: { status_code: 429 },
    latency_ms: 120,
    stream: false,
  },
  {
    // No request_id: the id is a SHA-256 of the entry's JSON.
    timestamp: "2026-11-19T21:30:45.999Z",
    provider: "openai-compatibility",
    model: "qwen3-coder:30b",
    alias: "local/qwen3-coder:30b",
    auth_index: "0000000000000006",
    tokens: { input_tokens: 700, output_tokens: 150, total_tokens: 850 },
    latency_ms: 980,
    stream: true,
  },
  {
    request_id: "req-0006",
    timestamp: "2026-11-20T10:00:00.123456789Z",
    provider: "claude",
    // Only an alias: upstreamModel stays absent.
    alias: "anthropic/claude-sonnet-4-5",
    auth_index: "0000000000000005",
    tokens: { input_tokens: 64, output_tokens: 16, total_tokens: 80 },
    failed: true,
    latency_ms: 30_000,
    stream: true,
  },
  {
    request_id: "req-0007",
    timestamp: "2026-11-21T16:45:00.000Z",
    provider: "claude",
    model: "claude-sonnet-4-5-20250929",
    alias: "claude-sonnet-4-5",
    auth_index: "0000000000000001",
    tokens: {
      input_tokens: 25_000,
      output_tokens: 2_000,
      cache_read_tokens: 20_000,
      cache_creation_tokens: 4_000,
      reasoning_tokens: 500,
      total_tokens: 27_000,
    },
    token_breakdown: breakdown(1_000, 20_000, 4_000, 1_500, 500),
    latency_ms: 12_345,
    stream: true,
  },
  {
    request_id: "req-0008",
    timestamp: "2026-11-22T07:59:59.000Z",
    provider: "codex",
    model: "gpt-5",
    auth_index: "0000000000000003",
    tokens: { input_tokens: 100, output_tokens: 50, total_tokens: 150 },
    token_breakdown: breakdown(100, 0, 0, 50, 0),
    latency_ms: 640,
    stream: false,
  },
]);
usageQueues.set(personal2.id, [
  {
    request_id: "req-0101",
    timestamp: "2026-11-19T12:00:00.000Z",
    provider: "claude",
    model: "claude-sonnet-4-5-20250929",
    alias: "claude-sonnet-4-5",
    auth_index: "0000000000000004",
    tokens: { input_tokens: 900, output_tokens: 90, total_tokens: 990 },
    token_breakdown: breakdown(900, 0, 0, 90, 0),
    latency_ms: 1500,
    stream: true,
  },
  {
    request_id: "req-0102",
    timestamp: "2026-11-20T12:00:00.000Z",
    provider: "openai-compatibility",
    model: "glm-4.6",
    alias: "go/glm-4.6",
    auth_index: "0000000000000007",
    tokens: { input_tokens: 2000, output_tokens: 400, total_tokens: 2400 },
    latency_ms: 2200,
    stream: true,
  },
  {
    request_id: "req-0103",
    timestamp: "2026-11-21T12:00:00.000Z",
    provider: "claude",
    model: "qwen3-coder",
    alias: "go/qwen3-coder",
    auth_index: "0000000000000008",
    tokens: { input_tokens: 3000, output_tokens: 600, total_tokens: 3600 },
    token_breakdown: breakdown(3000, 0, 0, 600, 0),
    latency_ms: 2600,
    stream: false,
  },
]);
// SAFETY: Usage.collect, the only method called, reads pool.management alone.
const corePool = pool as CorePool;
const usage = new Usage(corePool, accounts, new Quotas(corePool, accounts), {
  signingIn: () => false,
  report: () => undefined,
});
for (const profileId of [...usageQueues.keys()]) await usage.collect(profileId);
// Builds before pricingTokens stored records without it.
store.addUsage([
  {
    id: `${personal.id}:req-legacy-0001`,
    profileId: personal.id,
    timestamp: "2026-11-15T23:59:59.000Z",
    provider: "gemini",
    model: "gem/gemini-2.5-pro",
    account: "0000000000000009",
    inputTokens: 400,
    outputTokens: 100,
    cachedTokens: 0,
    reasoningTokens: 0,
    cacheWriteTokens: 0,
    totalTokens: 500,
    latencyMs: 700,
    failed: false,
    statusCode: 200,
    stream: false,
  },
]);

// A pending Codex banked-reset redemption, keyed by ChatGPT account and credit.
const resetIdentity = "11111111-2222-4333-8444-555555555555";
const resetCredit = "credit-synthetic-1";
const resetRequestId = store.resetRequestId(resetIdentity, resetCredit);

// The core ledger: cores that a crashed instance left running.
const ownerIdentity = "Sun Nov 22 08:00:00 2026 /opt/nonstopvibin/nonstopvibin";
const corePids = new Map([
  [4242, team],
  [4243, personal2],
]);
const identify = async (pid: number) => {
  if (pid === process.pid) return ownerIdentity;
  const profile = corePids.get(pid);
  return (
    profile &&
    `Sun Nov 22 08:00:01 2026 /opt/nonstopvibin/core/cli-proxy-api --config ${join(data, "profiles", profile.id, "config.yaml")}`
  );
};
const ledger = [];
for (const pid of corePids.keys()) {
  // SAFETY: recordCore reads only pid, exitCode and signalCode of the child.
  const child = { pid, exitCode: null, signalCode: null } as ChildProcess;
  const path = await recordCore(data, child, identify);
  if (!path) throw new Error(`recordCore skipped ${pid}`);
  ledger.push(path);
}

// Agent connections, installed into agentHome by AgentSetup. The user's own files
// come first so the merge keeps them.
const projects = {
  webapp: join(agentHome, "projects", "webapp"),
  api: join(agentHome, "projects", "api server"),
};
mkdirSync(join(projects.webapp, ".claude"), { recursive: true, mode: 0o755 });
mkdirSync(projects.api, { recursive: true, mode: 0o755 });
writeFileSync(
  join(projects.webapp, ".claude", "settings.local.json"),
  `${JSON.stringify({ permissions: { allow: ["Bash(ls:*)"] } }, null, 2)}\n`,
);
mkdirSync(join(agentHome, ".codex"), { mode: 0o700 });
writeFileSync(
  join(agentHome, ".codex", "config.toml"),
  'model = "gpt-5-codex"\n\n[projects."/srv/example"]\ntrust_level = "trusted"\n',
);
const setup = new AgentSetup(
  data,
  (profileId) => ({ key: store.clientKey(profileId), port: gatewayPort }),
  { home: agentHome },
);
const installs: Array<[Profile, AgentSetupInput, string[]]> = [
  [
    team,
    { agent: "claude", projectDirectory: projects.webapp },
    ["claude-sonnet-4-5", "claude-opus-4-1"],
  ],
  [
    team,
    {
      agent: "claude",
      projectDirectory: projects.api,
      otherProviders: true,
    },
    ["claude-sonnet-4-5", "gpt-5-codex", "local/qwen3-coder:30b"],
  ],
  [team, { agent: "codex", otherProviders: false }, ["gpt-5-codex", "gpt-5"]],
  [
    team,
    { agent: "opencode" },
    ["claude-sonnet-4-5", "gpt-5-codex", "local/qwen3-coder:30b"],
  ],
  [team, { agent: "pi" }, ["claude-sonnet-4-5", "gpt-5-codex"]],
  [
    personal2,
    { agent: "codex", otherProviders: true },
    ["go/glm-4.6", "claude-sonnet-4-5"],
  ],
];
const agentConnections = [];
for (const [profile, input, models] of installs) {
  const result = await setup.install(profile, input, models);
  agentConnections.push({ profileId: profile.id, input, models, result });
}
const socketDirectory = setup.socketDirectory;
await setup.close();
rmSync(socketDirectory, { recursive: true, force: true });

// What a reader must find, read back through the Store before it closes.
const rows = (sql: string) => store.db.prepare(sql).all();
const expectedStore = {
  profiles: store.profiles(),
  secrets: Object.fromEntries(
    rows("SELECT id, encrypted FROM secrets ORDER BY id").map((row) => [
      String(row.id),
      store.codec.decrypt(String(row.encrypted)),
    ]),
  ),
  apiAccounts: Object.fromEntries(
    profiles.map((p) => [p.id, store.apiAccounts(p.id)]),
  ),
  accountMetadata: Object.fromEntries(
    profiles.map((p) => [p.id, store.accountMetadata(p.id)]),
  ),
  // What Accounts.list shows: OAuth metadata, then API accounts, each with its quota.
  accountsList: Object.fromEntries(
    profiles.map((p) => [p.id, accounts.list(p.id)]),
  ),
  quotas: rows(
    "SELECT profile_id, account_id, body FROM quotas ORDER BY rowid",
  ).map((row) => ({
    profileId: row.profile_id,
    accountId: row.account_id,
    quota: JSON.parse(String(row.body)),
  })),
  usage: Object.fromEntries(
    profiles.map((p) => {
      const records = store.db
        .prepare(
          "SELECT rowid AS sequence, body FROM usage WHERE profile_id=? ORDER BY rowid",
        )
        .all(p.id)
        .map((row) => ({
          sequence: Number(row.sequence),
          record: JSON.parse(String(row.body)),
        }));
      return [p.id, { count: records.length, records }];
    }),
  ),
  settings: Object.fromEntries(
    rows("SELECT id, value FROM settings ORDER BY id").map((row) => [
      String(row.id),
      String(row.value),
    ]),
  ),
  retentionDays: store.usageRetentionDays(),
  resetRequest: {
    identity: resetIdentity,
    creditId: resetCredit,
    settingId: `codex-reset:${JSON.stringify([resetIdentity, resetCredit])}`,
    requestId: resetRequestId,
  },
  tables: rows(
    "SELECT type, name, sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY rowid",
  ),
};
store.close();
for (const suffix of ["-wal", "-shm"])
  if (existsSync(databasePath(data) + suffix))
    throw new Error(`The store left ${suffix} behind`);

// Copy the tree beside this script.
for (const name of ["data", "agent-home"]) {
  rmSync(join(output, name), { recursive: true, force: true });
  const copy = spawnSync("cp", ["-a", join(root, name), join(output, name)]);
  if (copy.status !== 0) throw new Error(`cp failed: ${copy.stderr}`);
}
function walk(
  directory: string,
): Array<{ path: string; type: string; mode: string }> {
  return readdirSync(directory)
    .sort()
    .flatMap((name) => {
      const path = join(directory, name);
      const status = lstatSync(path);
      const entry = {
        path: relative(output, path),
        type: status.isDirectory()
          ? "directory"
          : status.isSymbolicLink()
            ? "symlink"
            : "file",
        mode: `0${(status.mode & 0o777).toString(8)}`,
      };
      return status.isDirectory() ? [entry, ...walk(path)] : [entry];
    });
}
const files = [
  ...walk(join(output, "data")),
  ...walk(join(output, "agent-home")),
];
const ignored = spawnSync(
  "git",
  ["-C", repository, "check-ignore", "--no-index", "--stdin"],
  {
    input: files
      .map((f) => join("tests/fixtures/ts-data-dir", f.path))
      .join("\n"),
    encoding: "utf8",
  },
);
const relativeTo = (path: string) => relative(root, path);
const credentialFiles = Object.fromEntries(
  profiles.map((p) => [
    p.id,
    credentials
      .filter((c) => c.profile === p)
      .map((c) => ({
        path: relativeTo(
          join(credentialDirectory(data, p.id), credentialNames.get(c) ?? ""),
        ),
        content: c.payload,
      })),
  ]),
);
const expected = {
  generatedBy:
    "bun tests/fixtures/ts-data-dir/generate.ts at nonstopvibin main 4ea188a",
  originalPaths: { root, data, agentHome },
  vaultKeyHex: vaultKey.toString("hex"),
  gatewayPort,
  corePorts: Object.fromEntries(
    profiles.map((p, i) => [p.id, profileSpecs[i]?.corePort]),
  ),
  ...expectedStore,
  credentialFiles,
  coreLedger: ledger.map((path) => ({
    path: relativeTo(path),
    content: JSON.parse(readFileSync(path, "utf8")),
  })),
  agentConnections: agentConnections.map((connection) => ({
    ...connection,
    result: {
      ...connection.result,
      files: connection.result.files.map(relativeTo),
    },
  })),
  credentialSocket: socketDirectory,
  files,
  gitIgnored: ignored.stdout.split("\n").filter(Boolean),
};
writeFileSync(
  join(output, "expected.json"),
  `${JSON.stringify(expected, null, 2)}\n`,
);
// The repository's formatter checks tests/; data/ and agent-home/ stay byte-exact.
const format = spawnSync(
  "bunx",
  ["--no-install", "oxfmt", "--write", join(output, "expected.json")],
  { cwd: repository, encoding: "utf8" },
);
if (format.status !== 0) throw new Error(`oxfmt failed: ${format.stderr}`);
console.log(
  `Wrote ${files.length} entries under ${output}; ${expected.gitIgnored.length} are git-ignored.`,
);
