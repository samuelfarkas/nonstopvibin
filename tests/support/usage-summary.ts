import type { Store } from "../../src/server/store.ts";
import type { UsageSummary } from "../../src/shared/types.ts";

/** Stored totals since an ISO time, summed in SQL: the tests' accounting oracle, independent of UsagePrices. */
export function usageSummary(
  store: Store,
  profileId: string,
  since = "",
): UsageSummary {
  const row = store.db
    .prepare(
      `SELECT count(*) AS requests,
      coalesce(sum(json_extract(body,'$.failed')),0) AS failed,
      coalesce(sum(json_extract(body,'$.inputTokens')),0) AS inputTokens,
      coalesce(sum(json_extract(body,'$.outputTokens')),0) AS outputTokens,
      coalesce(sum(json_extract(body,'$.cachedTokens')),0) AS cachedTokens,
      coalesce(sum(json_extract(body,'$.reasoningTokens')),0) AS reasoningTokens,
      coalesce(sum(json_extract(body,'$.totalTokens')),0) AS totalTokens
      FROM usage WHERE profile_id=? AND timestamp>=?`,
    )
    .get(profileId, since);
  return {
    requests: Number(row?.requests ?? 0),
    failed: Number(row?.failed ?? 0),
    inputTokens: Number(row?.inputTokens ?? 0),
    outputTokens: Number(row?.outputTokens ?? 0),
    cachedTokens: Number(row?.cachedTokens ?? 0),
    reasoningTokens: Number(row?.reasoningTokens ?? 0),
    totalTokens: Number(row?.totalTokens ?? 0),
  };
}
