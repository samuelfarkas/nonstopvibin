// The management API under /api: one entry per route, types only. The server's
// route table and the client's `api()` are both checked against it.
import type { Provider, Protocol, OAuthProvider } from "./providers.ts";
import type { RetentionDays } from "./vocabulary.ts";
import type {
  AgentSetup,
  AgentSetupInput,
  AppState,
  CatalogModel,
  ImportSource,
  Json,
  OAuthSession,
  OAuthStatus,
  Profile,
  ResetCredits,
  ResetResult,
  UsagePricing,
  UsageRecord,
  UsageSummary,
} from "./types.ts";

export interface Ok {
  ok: true;
}
export interface ClientKey {
  key: string;
}
export interface UsageHistory {
  records: UsageRecord[];
  summary: UsageSummary;
  pricing: UsagePricing;
}
/** Which connection of an agent: Claude Code also takes a project. */
export type AgentSelection = Pick<
  AgentSetupInput,
  "agent" | "projectDirectory"
>;
export type ProfilePatch = Partial<
  Pick<Profile, "name" | "strategy" | "sessionAffinity">
>;
export interface AccountPatch {
  disabled?: boolean;
  priority?: number;
  name?: string;
}
export interface ApiAccountInput {
  name: string;
  provider: Provider;
  baseUrl: string;
  apiKey: string;
  protocol: Protocol;
  prefix: string;
  /** Empty asks the server to discover the provider's models. */
  models: Array<{ id: string; protocol: Protocol }>;
}

/**
 * Keyed "METHOD /path" below /api; `:name` segments are path parameters.
 * Routes answering `AppState` return the whole state after their change.
 */
export interface Routes {
  "GET /state": { response: AppState };
  "PUT /usage-retention": {
    body: { days: RetentionDays };
    response: AppState;
  };
  "GET /import-sources": { response: ImportSource[] };
  "POST /profiles": { body: { name: string }; response: Profile };
  "PATCH /profiles/:profileId": { body: ProfilePatch; response: AppState };
  "POST /profiles/:profileId/start": { response: AppState };
  /** Re-reads the accounts, then the quota of every enabled account. */
  "POST /profiles/:profileId/refresh": { response: AppState };
  "GET /profiles/:profileId/key": { response: ClientKey };
  /** `metadata=1` adds the models.dev limits and prices. */
  "GET /profiles/:profileId/models": {
    query: { metadata?: "1" };
    response: CatalogModel[];
  };
  "GET /profiles/:profileId/usage": {
    query: { days?: number };
    response: UsageHistory;
  };
  /** Null when the agent is not connected. */
  "GET /profiles/:profileId/agent-setup": {
    query: AgentSelection;
    response: AgentSetup | null;
  };
  "POST /profiles/:profileId/agent-setup": {
    body: AgentSetupInput;
    response: AgentSetup;
  };
  "DELETE /profiles/:profileId/agent-setup": {
    body: AgentSelection;
    response: Ok;
  };
  /** Lists models through the profile endpoint with its client key. */
  "POST /profiles/:profileId/agent-check": {
    body: Record<string, never>;
    response: Ok;
  };
  "POST /profiles/:profileId/oauth": {
    body: { provider: OAuthProvider; reconnectAccountId?: string };
    response: OAuthSession;
  };
  "GET /profiles/:profileId/oauth": {
    query: { state: string };
    response: OAuthStatus;
  };
  "DELETE /profiles/:profileId/oauth": {
    query: { state: string };
    response: Ok;
  };
  "POST /profiles/:profileId/oauth-confirm": {
    body: { state: string };
    response: OAuthStatus;
  };
  "GET /profiles/:profileId/oauth-session": { response: OAuthSession | null };
  "POST /profiles/:profileId/callback": { body: { url: string }; response: Ok };
  "POST /profiles/:profileId/import": {
    body: { contents: Record<string, Json> };
    response: AppState;
  };
  "POST /profiles/:profileId/import-source": {
    body: { sourceId: string };
    response: AppState;
  };
  "PATCH /profiles/:profileId/accounts/:accountId": {
    body: AccountPatch;
    response: AppState;
  };
  "DELETE /profiles/:profileId/accounts/:accountId": { response: AppState };
  "GET /profiles/:profileId/accounts/:accountId/reset-credits": {
    response: ResetCredits;
  };
  "POST /profiles/:profileId/accounts/:accountId/reset-credits": {
    body: { creditId: string };
    response: ResetResult;
  };
  "POST /profiles/:profileId/api-account": {
    body: ApiAccountInput;
    response: AppState;
  };
}
export type RouteKey = keyof Routes;
type ParamNames<Path extends string> =
  Path extends `${string}/:${infer Name}/${infer Rest}`
    ? Name | ParamNames<`/${Rest}`>
    : Path extends `${string}/:${infer Name}`
      ? Name
      : never;
export type RouteParamName<K extends RouteKey> = ParamNames<K>;
export type RouteParams<K extends RouteKey> = Record<ParamNames<K>, string>;
export type RouteBody<K extends RouteKey> = Routes[K] extends {
  body: infer Body;
}
  ? Body
  : undefined;
export type RouteQuery<K extends RouteKey> = Routes[K] extends {
  query: infer Query;
}
  ? Query
  : undefined;
export type RouteResponse<K extends RouteKey> = Routes[K]["response"];

/** The body of every error this server answers, under /api and elsewhere. */
export interface ApiError {
  error: { message: string; type: "nonstopvibin_error"; code: number };
}
/**
 * A missing or stale session token, or a profile key. Checked before routing,
 * so every route can answer it and nothing else under /api does. Its message
 * already tells the user how to open a new session.
 */
export type SessionRefused = 401;
