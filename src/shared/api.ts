// The management API under /api, types only. Its keys are the server's route table (`ROUTES`
// in crates/domain/src/wire/api.rs), and its bodies and responses are the server's wire types,
// both as `bun run wire:export` writes them to ./generated/domain.
import type { AccountPatch } from "./generated/domain/AccountPatch.ts";
import type { AgentSelection } from "./generated/domain/AgentSelection.ts";
import type { ApiAccountInput } from "./generated/domain/ApiAccountInput.ts";
import type { CallbackBody } from "./generated/domain/CallbackBody.ts";
import type { ClientKeyResponse } from "./generated/domain/ClientKeyResponse.ts";
import type { CoreInstallBody } from "./generated/domain/CoreInstallBody.ts";
import type { CreateProfileBody } from "./generated/domain/CreateProfileBody.ts";
import type { CreditBody } from "./generated/domain/CreditBody.ts";
import type { ImportBody } from "./generated/domain/ImportBody.ts";
import type { ImportSourceBody } from "./generated/domain/ImportSourceBody.ts";
import type { OAuthStartBody } from "./generated/domain/OAuthStartBody.ts";
import type { Ok } from "./generated/domain/Ok.ts";
import type { ProfilePatch } from "./generated/domain/ProfilePatch.ts";
import type { RetentionBody } from "./generated/domain/RetentionBody.ts";
import type { RouteKey } from "./generated/domain/RouteKey.ts";
import type { StateBody } from "./generated/domain/StateBody.ts";
import type { UsageHistory } from "./generated/domain/UsageHistory.ts";
import type {
  AgentSetup,
  AgentSetupInput,
  AppState,
  CatalogModel,
  ImportSource,
  OAuthSession,
  OAuthStatus,
  Profile,
  ResetCredits,
  ResetResult,
} from "./types.ts";

export type { AccountPatch, RouteKey };

/**
 * Keyed "METHOD /path" below /api; `:name` segments are path parameters.
 * Routes answering `AppState` return the whole state after their change.
 */
interface RouteTypes {
  /** `watch=1`: a UI that shows this state, so the cores' status is polled and dormant ones start. */
  "GET /state": { query: { watch?: "1" }; response: AppState };
  "PUT /usage-retention": {
    body: RetentionBody;
    response: AppState;
  };
  /** Reads the reviewed core pin; `core.available` names a newer core this app may run. */
  "POST /core/check": { response: AppState };
  /** Installs the offered core `version` and restarts running profiles on it. */
  "POST /core/install": { body: CoreInstallBody; response: AppState };
  /** Deletes installed core updates and restarts running profiles on the bundled core. */
  "DELETE /core/install": { response: AppState };
  "GET /import-sources": { response: ImportSource[] };
  "POST /profiles": { body: CreateProfileBody; response: Profile };
  "PATCH /profiles/:profileId": { body: ProfilePatch; response: AppState };
  "POST /profiles/:profileId/start": { response: AppState };
  /** Re-reads the accounts, then the quota of every enabled account. */
  "POST /profiles/:profileId/refresh": { response: AppState };
  "GET /profiles/:profileId/key": { response: ClientKeyResponse };
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
    body: OAuthStartBody;
    response: OAuthSession;
  };
  "GET /profiles/:profileId/oauth": {
    query: StateBody;
    response: OAuthStatus;
  };
  "DELETE /profiles/:profileId/oauth": {
    query: StateBody;
    response: Ok;
  };
  "POST /profiles/:profileId/oauth-confirm": {
    body: StateBody;
    response: OAuthStatus;
  };
  "GET /profiles/:profileId/oauth-session": { response: OAuthSession | null };
  "POST /profiles/:profileId/callback": { body: CallbackBody; response: Ok };
  "POST /profiles/:profileId/import": {
    body: ImportBody;
    response: AppState;
  };
  "POST /profiles/:profileId/import-source": {
    body: ImportSourceBody;
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
    body: CreditBody;
    response: ResetResult;
  };
  "POST /profiles/:profileId/api-account": {
    body: ApiAccountInput;
    response: AppState;
  };
  /** A second desktop launch; only the desktop app serves it. */
  "POST /desktop/show": { response: Ok };
}
/** One entry per route: a missing entry fails to index `RouteTypes`, and an extra one is no key. */
export type Routes = { [K in RouteKey]: RouteTypes[K] };
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

/**
 * A missing or stale session token, or a profile key. Checked before routing,
 * so every route can answer it and nothing else under /api does. Its message
 * already tells the user how to open a new session.
 */
export type SessionRefused = 401;
