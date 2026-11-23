import { agents, type Agent } from "./agents.ts";

export const protocols = ["openai", "anthropic", "responses"] as const;
export type Protocol = (typeof protocols)[number];
/** Server rules an API-key preset can name: see `modelProtocols` (api-accounts.ts) and `upstreamHeaders` (config.ts). */
export type Upstream = "opencode-go";

interface ProviderFields {
  /** Short name in account lists, quotas and history. */
  label: string;
  /** The first release shows it; the server supports every entry. */
  visible: boolean;
  /** `public/providers/<icon>.svg`. */
  icon?: string;
  /** models.dev provider ID, where it differs. */
  catalog?: string;
  /** Its models are this agent's vendor's own and offered to it by default. */
  nativeAgent?: Agent;
  /** Pages the desktop app may open for it: sign-in, account and key pages. */
  hosts?: string[];
  /** CLIProxyAPI credential files of this type can be imported. */
  credentialFile?: boolean;
  /** Browser sign-in through the core. */
  signIn?: {
    name: string;
    detail: string;
    /** The core's name for the provider, where it differs. */
    core?: string;
    /** Quota windows it reports, shown before anything is connected. */
    limits?: string;
  };
  /** Accepted as an API-key account. */
  apiKey?: boolean;
  preset?: {
    name: string;
    detail: string;
    baseUrl: string;
    prefix: string;
    protocol: Protocol;
    /** Only the official endpoint is accepted. */
    fixedEndpoint?: boolean;
    /** The server's rules for this API: each model's protocol (the user picks none) and request headers. */
    upstream?: Upstream;
  };
  /** Banked rate-limit resets can be redeemed for its signed-in accounts. */
  bankedResets?: boolean;
}
export type ProviderEntry = ProviderFields &
  (
    | {
        /**
         * One email can hold seats in several organizations, each its own account: a sign-in
         * runs in a sandbox and its seat is reviewed before it joins a profile.
         */
        organizations: true;
        /** A sign-in can replace the credential of an existing account in place; only the review can. */
        reconnect?: boolean;
      }
    | { organizations?: false; reconnect?: false }
  );
// One entry per provider. Key order is the order of the app's pickers.
export const providers = {
  claude: {
    label: "Claude",
    visible: true,
    icon: "claude",
    catalog: "anthropic",
    nativeAgent: "claude",
    hosts: ["claude.ai", "console.anthropic.com", "platform.claude.com"],
    credentialFile: true,
    signIn: {
      name: "Claude",
      detail:
        "Personal or organization seats · the same email can connect more than once",
      core: "anthropic",
      limits: "5-hour · weekly · opus weekly",
    },
    reconnect: true,
    organizations: true,
  },
  codex: {
    label: "Codex",
    visible: true,
    icon: "openai",
    catalog: "openai",
    nativeAgent: "codex",
    hosts: ["auth.openai.com", "chatgpt.com"],
    credentialFile: true,
    signIn: {
      name: "ChatGPT / Codex",
      detail: "Plus, Pro, Business, or Enterprise",
      limits: "weekly · credits",
    },
    bankedResets: true,
  },
  antigravity: {
    label: "Antigravity",
    visible: false,
    icon: "antigravity",
    hosts: ["accounts.google.com"],
    credentialFile: true,
    signIn: { name: "Antigravity", detail: "Google account via CLIProxyAPI" },
  },
  kimi: {
    label: "Kimi Code",
    visible: false,
    icon: "kimi",
    catalog: "kimi-for-coding",
    hosts: ["auth.kimi.com", "www.kimi.com", "kimi.com"],
    credentialFile: true,
    signIn: { name: "Kimi Code", detail: "Kimi coding subscription" },
  },
  xai: {
    label: "Grok",
    visible: false,
    icon: "xai",
    hosts: ["accounts.x.ai", "auth.x.ai", "grok.com", "accounts.xai.com"],
    credentialFile: true,
    signIn: { name: "Grok", detail: "Grok Build subscription" },
  },
  "opencode-go": {
    label: "OpenCode Go",
    visible: true,
    icon: "opencode",
    hosts: ["opencode.ai"],
    apiKey: true,
    preset: {
      name: "OpenCode Go",
      detail: "Models are discovered from your subscription.",
      baseUrl: "https://opencode.ai/zen/go/v1",
      prefix: "go",
      protocol: "openai",
      fixedEndpoint: true,
      upstream: "opencode-go",
    },
  },
  openai: {
    label: "OpenAI",
    visible: false,
    icon: "openai",
    nativeAgent: "codex",
    apiKey: true,
    preset: {
      name: "OpenAI API",
      detail: "Discover models or enter the ones you want to expose.",
      baseUrl: "https://api.openai.com/v1",
      prefix: "openai",
      protocol: "openai",
    },
  },
  anthropic: {
    label: "Anthropic API",
    visible: false,
    icon: "claude",
    nativeAgent: "claude",
    apiKey: true,
    preset: {
      name: "Anthropic API",
      detail: "Enter model IDs enabled for your API key.",
      baseUrl: "https://api.anthropic.com/v1",
      prefix: "anthropic",
      protocol: "anthropic",
    },
  },
  gemini: {
    label: "Gemini",
    visible: false,
    catalog: "google",
    credentialFile: true,
    apiKey: true,
  },
  custom: {
    label: "Compatible API",
    visible: true,
    apiKey: true,
    preset: {
      name: "Other compatible API",
      detail: "OpenAI, Anthropic, or Responses compatible endpoints.",
      baseUrl: "",
      prefix: "",
      protocol: "openai",
    },
  },
} satisfies Record<string, ProviderEntry>;
type Entries = typeof providers;
export type Provider = keyof Entries;
export type OAuthProvider = {
  [K in Provider]: Entries[K] extends { signIn: object } ? K : never;
}[Provider];

export function isProvider(id: string): id is Provider {
  return Object.hasOwn(providers, id);
}
/** Accounts and records carry the core's provider strings, which include unknown ones. */
export function providerEntry(id: Provider): ProviderEntry;
export function providerEntry(id: string): ProviderEntry | undefined;
export function providerEntry(id: string): ProviderEntry | undefined {
  return isProvider(id) ? providers[id] : undefined;
}
export function isOAuthProvider(id: string): id is OAuthProvider {
  return providerEntry(id)?.signIn !== undefined;
}
export type OrganizationProvider = {
  [K in Provider]: Entries[K] extends { organizations: true } ? K : never;
}[Provider];
export function hasOrganizations(id: string): id is OrganizationProvider {
  return providerEntry(id)?.organizations === true;
}
const providerIds = Object.keys(providers).filter(isProvider);
export const oauthProviders = providerIds.flatMap((id) => {
  const { signIn, ...entry } = providerEntry(id);
  return signIn && isOAuthProvider(id) ? [{ ...entry, ...signIn, id }] : [];
});
export const apiPresets = providerIds.flatMap((id) => {
  const { preset, visible } = providerEntry(id);
  return preset ? [{ ...preset, id, visible }] : [];
});

/** The agent whose vendor owns a model, from its `owned_by`. */
export function nativeAgent(owner: string | undefined): Agent | undefined {
  // owned_by is the vendor for OAuth models and "<provider>-<hash>" for
  // API-key accounts registered as compatible providers.
  const id = providerIds.find(
    (id) => owner === id || owner?.startsWith(`${id}-`),
  );
  return id && providerEntry(id).nativeAgent;
}
// Claude Code and Codex default to their own vendor's models; other
// providers are opt-in because CLIProxyAPI has to translate between them.
export function modelsFor<T extends { owned_by?: string }>(
  agent: Agent,
  models: T[],
  otherProviders: boolean,
): T[] {
  if (!agents[agent].native || otherProviders) return models;
  return models.filter(({ owned_by }) => nativeAgent(owned_by) === agent);
}
// The core names generated compatible providers "openai-compatible-<hash>".
export function providerLabel(provider: string): string {
  return (
    providerEntry(provider)?.label ??
    (provider.startsWith("openai-compatible-") ? "Compatible API" : provider)
  );
}
