// Limits, values and rules the server and the renderer must agree on. Data and pure
// functions only: the renderer loads this module, so it imports nothing at run time.

export const strategies = ["round-robin", "fill-first"] as const;
/** Days of usage history kept; 0 keeps everything. */
export const retentionDays = [0, 30, 90, 365] as const;
export type RetentionDays = (typeof retentionDays)[number];
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
 * Whether the app keeps a profile's core running, starting it again when it is not: the
 * profile has run before (`enabled`) and has a subscription, whose quota and routing need it.
 */
export const keepsRunning = (
  profile: { enabled: boolean },
  subscriptions: number,
): boolean => profile.enabled && subscriptions > 0;
/** How often the server refreshes every account's quota. */
export const quotaRefreshMs = 120_000;
export const quotaStaleMs = 2 * quotaRefreshMs + 60_000;
export const resetResultCodes = [
  "reset",
  "already_redeemed",
  "nothing_to_reset",
  "no_credit",
] as const;
export type ResetResultCode = (typeof resetResultCodes)[number];
