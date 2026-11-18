import type { Agent } from "./agents.ts";
import type { OAuthProvider } from "./providers.ts";
import type {
  ResetResultCode,
  RetentionDays,
  strategies,
} from "./vocabulary.ts";

/** Decoded JSON. Object values may be undefined so partial records and optional fields fit without casts. */
export type Json =
  | string
  | number
  | boolean
  | null
  | Json[]
  | { [key: string]: Json | undefined };
export interface Profile {
  id: string;
  slug: string;
  name: string;
  strategy: (typeof strategies)[number];
  sessionAffinity: boolean;
  enabled: boolean;
  createdAt: string;
}
export interface QuotaWindow {
  label: string;
  remainingPercent: number | null;
  resetsAt: string | null;
  /** Limits one model only; never decides whether the whole account is out. */
  scoped?: true;
}
export interface Quota {
  status: "available" | "unavailable" | "error";
  windows: QuotaWindow[];
  checkedAt: string;
  error?: string;
  plan?: string;
  bankedResets?: number;
}
export interface ResetCredits {
  available_count: number;
  credits: Array<{
    id: string;
    reset_type: string;
    status: string;
    expires_at: string | null;
    title?: string | null;
    description?: string | null;
  }>;
}
export interface ResetResult {
  code: ResetResultCode;
  quotaRefreshed: boolean;
}
export interface SeatIdentity {
  accountUuid?: string;
  organizationUuid?: string;
  organizationName?: string;
}
export interface Account extends SeatIdentity {
  id: string;
  name: string;
  provider: string;
  email?: string;
  kind: "oauth" | "api-key";
  disabled: boolean;
  status:
    | "Ready"
    | "Needs attention"
    | "Reconnect needed"
    | "Cooling down"
    | "Paused"
    | "Configured";
  priority: number;
  authIndex?: string;
  quota?: Quota;
  prefix?: string;
  modelCount?: number;
}
export interface UsageRecord {
  id: string;
  profileId: string;
  timestamp: string;
  provider: string;
  model: string;
  account: string;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
  reasoningTokens: number;
  cacheWriteTokens: number;
  totalTokens: number;
  latencyMs: number;
  failed: boolean;
  statusCode: number;
  stream: boolean;
  upstreamModel?: string;
  /** Non-overlapping core accounting; null means explicitly incomplete. */
  pricingTokens?: PricingTokens | null;
}
export interface PricingTokens {
  input: number;
  cached: number;
  cacheWrite: number;
  output: number;
  reasoning: number;
}
export interface UsageTotals {
  requests: number;
  pricedRequests: number;
  usd: number;
  normalizedRequests: number;
  tokens: PricingTokens;
}
export interface UsagePricing {
  totals?: UsageTotals;
  usd: Record<string, number | null>;
  fetchedAt: string | null;
  refreshing: boolean;
  eur: { rate: number; date: string } | null;
}
export interface UsageSummary {
  requests: number;
  failed: number;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
  reasoningTokens: number;
  totalTokens: number;
}
export interface ProfileState extends Profile {
  runtime: "stopped" | "starting" | "running" | "error";
  error?: string;
  accounts: Account[];
  endpoint: string;
}
export interface AppState {
  profiles: ProfileState[];
  coreVersion: string;
  gateway: string;
  usageRetentionDays: RetentionDays;
  version: string;
  errors: string[];
}
export interface Model {
  id: string;
  owned_by?: string;
}
/** A model with the models.dev limits and prices the agents and Activity need. */
export interface CatalogModel extends Model {
  metadata?: {
    name: string;
    context: number;
    output: number;
    inputLimit?: number;
    input: Array<"text" | "image">;
    reasoning: boolean;
    toolCall: boolean;
    cost?: {
      input: number;
      output: number;
      reasoning?: number;
      cache_read?: number;
      cache_write?: number;
      tiers?: Array<{
        input: number;
        output: number;
        reasoning?: number;
        cache_read?: number;
        cache_write?: number;
        tier: { type: "context"; size: number };
      }>;
    };
    source: string;
    fetchedAt: string;
  };
}
export interface ImportSource extends SeatIdentity {
  id: string;
  fileName: string;
  provider: string;
  email?: string;
  source: string;
}
export interface OAuthSession {
  profileId: string;
  provider: OAuthProvider;
  url: string;
  state: string;
  startedAt: number;
  userCode?: string;
  expiresAt: number;
  reconnectAccountId?: string;
}
export interface OAuthReview {
  account: Account;
  action: "add" | "reconnect" | "blocked";
  message?: string;
}
export type OAuthStatus =
  | { status: "wait" }
  | { status: "review"; review: OAuthReview }
  | { status: "ok"; account?: Account; reconnected?: boolean }
  | { status: "error"; error: string };
export interface AgentSetupInput {
  agent: Agent;
  projectDirectory?: string;
  /** Claude Code and Codex: also offer models from other providers. */
  otherProviders?: boolean;
}
export interface AgentSetup extends AgentSetupInput {
  models: string[];
  needsReconnect?: boolean;
  projects?: string[];
  command: string;
  files: string[];
  provider: string;
  instructions: string;
}
