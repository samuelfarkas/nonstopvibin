// Limits, values and rules the server and the renderer must agree on. Data and pure
// functions only: the renderer loads this module, so it imports only generated data.

export { retentionDays, strategies } from "./generated/domain/vocabulary.ts";
export type { RetentionDays } from "./generated/domain/RetentionDays.ts";
/** Profile and account names. */
export const nameLength = 60;
/** API-key model prefixes. */
export const prefixLength = 30;
/** Routing priority runs from -priorityLimit to priorityLimit. */
export const priorityLimit = 1000;
/** Largest credential file accepted for import or read from a profile. */
export const credentialFileBytes = 2_000_000;
/** The usage range that means the whole history. */
export const allHistoryDays = 3650;
/** Usage records per page: the core's drain, history pages and the Activity sample. */
export const usagePageSize = 500;
/**
 * Whether the app keeps a profile's core running, starting it again when it is not: it has
 * a subscription, whose quota and routing need the core. Every way to add one starts the
 * profile, so a profile with one is a profile the user meant to run.
 */
export const keepsRunning = (subscriptions: number): boolean =>
  subscriptions > 0;
/** How often the server refreshes every account's quota while the UI polls. */
export const quotaRefreshMs = 120_000;
/** How often the UI polls the state; the server keeps polling cores for three missed polls. */
export const pollMs = 4000;
export const quotaStaleMs = 2 * quotaRefreshMs + 60_000;
