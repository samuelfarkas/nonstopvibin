// The Rust wire types (crates/domain/src/wire), as `bun run wire:export` writes them to
// ./generated, must stay mutually assignable with their twins in src/shared. `bun run check`
// fails here when either side drifts.
import type * as Api from "../../src/shared/api.ts";
import type * as Shared from "../../src/shared/types.ts";
import type { Agent } from "../../src/shared/agents.ts";
import type {
  OAuthProvider,
  Protocol,
  Provider,
} from "../../src/shared/providers.ts";
import type {
  ResetResultCode,
  RetentionDays,
  strategies,
} from "../../src/shared/vocabulary.ts";
import type { AccountPatch } from "./generated/AccountPatch";
import type { Agent as RustAgent } from "./generated/Agent";
import type { AgentSelection } from "./generated/AgentSelection";
import type { AgentSetup } from "./generated/AgentSetup";
import type { ApiAccountInput } from "./generated/ApiAccountInput";
import type { ApiError } from "./generated/ApiError";
import type { AppState } from "./generated/AppState";
import type { CallbackBody } from "./generated/CallbackBody";
import type { CatalogModel } from "./generated/CatalogModel";
import type { ClientKeyResponse } from "./generated/ClientKeyResponse";
import type { CoreInstallBody } from "./generated/CoreInstallBody";
import type { CreateProfileBody } from "./generated/CreateProfileBody";
import type { CreditBody } from "./generated/CreditBody";
import type { ImportBody } from "./generated/ImportBody";
import type { ImportSource } from "./generated/ImportSource";
import type { ImportSourceBody } from "./generated/ImportSourceBody";
import type { Model } from "./generated/Model";
import type { OAuthProvider as RustOAuthProvider } from "./generated/OAuthProvider";
import type { OAuthSession } from "./generated/OAuthSession";
import type { OAuthStartBody } from "./generated/OAuthStartBody";
import type { OAuthStatus } from "./generated/OAuthStatus";
import type { Ok } from "./generated/Ok";
import type { Profile } from "./generated/Profile";
import type { ProfilePatch } from "./generated/ProfilePatch";
import type { Protocol as RustProtocol } from "./generated/Protocol";
import type { Provider as RustProvider } from "./generated/Provider";
import type { ResetCredits } from "./generated/ResetCredits";
import type { ResetResult } from "./generated/ResetResult";
import type { ResetResultCode as RustResetResultCode } from "./generated/ResetResultCode";
import type { RetentionBody } from "./generated/RetentionBody";
import type { StateBody } from "./generated/StateBody";
import type { Strategy } from "./generated/Strategy";
import type { UsageHistory } from "./generated/UsageHistory";

type Mutual<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;
type Holds<T extends true> = T;
type Body<K extends Api.RouteKey> = Api.RouteBody<K>;
type Query<K extends Api.RouteKey> = Api.RouteQuery<K>;

export type WireTwins = [
  Holds<Mutual<Profile, Shared.Profile>>,
  Holds<Mutual<AppState, Shared.AppState>>,
  Holds<Mutual<ImportSource, Shared.ImportSource>>,
  Holds<Mutual<ResetCredits, Shared.ResetCredits>>,
  Holds<Mutual<ResetResult, Shared.ResetResult>>,
  Holds<Mutual<UsageHistory, Api.UsageHistory>>,
  Holds<Mutual<Model, Shared.Model>>,
  Holds<Mutual<CatalogModel, Shared.CatalogModel>>,
  Holds<Mutual<OAuthSession, Shared.OAuthSession>>,
  Holds<Mutual<OAuthStatus, Shared.OAuthStatus>>,
  Holds<Mutual<AgentSetup, Shared.AgentSetup>>,
  Holds<Mutual<AgentSelection, Api.AgentSelection>>,
  Holds<Mutual<Ok, Api.Ok>>,
  Holds<Mutual<ClientKeyResponse, Api.ClientKey>>,
  Holds<Mutual<ApiError, Api.ApiError>>,
  Holds<Mutual<ProfilePatch, Api.ProfilePatch>>,
  Holds<Mutual<AccountPatch, Api.AccountPatch>>,
  Holds<Mutual<ApiAccountInput, Api.ApiAccountInput>>,
  Holds<Mutual<CreateProfileBody, Body<"POST /profiles">>>,
  Holds<Mutual<RetentionBody, Body<"PUT /usage-retention">>>,
  Holds<Mutual<CoreInstallBody, Body<"POST /core/install">>>,
  Holds<Mutual<OAuthStartBody, Body<"POST /profiles/:profileId/oauth">>>,
  Holds<Mutual<StateBody, Body<"POST /profiles/:profileId/oauth-confirm">>>,
  Holds<Mutual<StateBody, Query<"GET /profiles/:profileId/oauth">>>,
  Holds<Mutual<CallbackBody, Body<"POST /profiles/:profileId/callback">>>,
  Holds<Mutual<ImportBody, Body<"POST /profiles/:profileId/import">>>,
  Holds<
    Mutual<ImportSourceBody, Body<"POST /profiles/:profileId/import-source">>
  >,
  Holds<
    Mutual<
      CreditBody,
      Body<"POST /profiles/:profileId/accounts/:accountId/reset-credits">
    >
  >,
  Holds<Mutual<RustProvider, Provider>>,
  Holds<Mutual<RustOAuthProvider, OAuthProvider>>,
  Holds<Mutual<RustProtocol, Protocol>>,
  Holds<Mutual<RustAgent, Agent>>,
  Holds<Mutual<Strategy, (typeof strategies)[number]>>,
  Holds<Mutual<RustResetResultCode, ResetResultCode>>,
  Holds<Mutual<AppState["usageRetentionDays"], RetentionDays>>,
];
