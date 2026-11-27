// The server's wire types (crates/domain/src/wire), as `bun run wire:export` writes them to
// ./generated/domain. `bun run quality` fails while the committed export differs.
export type { Account } from "./generated/domain/Account.ts";
export type { AgentSetup } from "./generated/domain/AgentSetup.ts";
export type { AgentSetupInput } from "./generated/domain/AgentSetupInput.ts";
export type { AppState } from "./generated/domain/AppState.ts";
export type { CatalogModel } from "./generated/domain/CatalogModel.ts";
export type { CorePhase } from "./generated/domain/CorePhase.ts";
export type { CoreState } from "./generated/domain/CoreState.ts";
export type { ImportSource } from "./generated/domain/ImportSource.ts";
export type { Model } from "./generated/domain/Model.ts";
export type { OAuthReview } from "./generated/domain/OAuthReview.ts";
export type { OAuthSession } from "./generated/domain/OAuthSession.ts";
export type { OAuthStatus } from "./generated/domain/OAuthStatus.ts";
export type { PricingTokens } from "./generated/domain/PricingTokens.ts";
export type { Profile } from "./generated/domain/Profile.ts";
export type { ProfileState } from "./generated/domain/ProfileState.ts";
export type { Quota } from "./generated/domain/Quota.ts";
export type { QuotaWindow } from "./generated/domain/QuotaWindow.ts";
export type { ResetCredits } from "./generated/domain/ResetCredits.ts";
export type { ResetResult } from "./generated/domain/ResetResult.ts";
export type { UsagePricing } from "./generated/domain/UsagePricing.ts";
export type { UsageHub } from "./generated/domain/UsageHub.ts";
export type { ClaudeAgentStatus } from "./generated/domain/ClaudeAgentStatus.ts";
export type { UsageRecord } from "./generated/domain/UsageRecord.ts";
export type { UsageSummary } from "./generated/domain/UsageSummary.ts";

/** Decoded JSON. Object values may be undefined so partial records and optional fields fit without casts. */
export type Json =
  | string
  | number
  | boolean
  | null
  | Json[]
  | { [key: string]: Json | undefined };
