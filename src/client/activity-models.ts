import type { UsageRecord } from "../shared/types.ts";

// The selected-period API sample also feeds the account table and request log.
export function activityModels(
  records: UsageRecord[],
  costs: Record<string, number | null> = {},
) {
  const groups = new Map<
    string,
    {
      requests: number;
      tokens: number;
      failed: number;
      latencies: number[];
      usd: number;
      priced: number;
    }
  >();
  for (const record of records) {
    const group = groups.get(record.model) ?? {
      requests: 0,
      tokens: 0,
      failed: 0,
      latencies: [],
      usd: 0,
      priced: 0,
    };
    const cost = costs[record.id] ?? null;
    group.requests += 1;
    group.tokens += record.totalTokens;
    group.usd += cost ?? 0;
    group.priced += Number(cost !== null);
    if (record.failed) group.failed += 1;
    group.latencies.push(record.latencyMs);
    groups.set(record.model, group);
  }
  return [...groups].sort((a, b) => b[1].requests - a[1].requests);
}

export type ActivityModel = ReturnType<typeof activityModels>[number];
