import { setImmediate as yieldToLoop } from "node:timers/promises";
import type { Store } from "./store.ts";
import { z } from "zod";
import type {
  Json,
  PricingTokens,
  UsagePricing,
  UsageRecord,
  UsageTotals,
  UsageSummary,
} from "../shared/types.ts";
import type { CatalogModel, ModelCatalog } from "./model-catalog.ts";
import { usagePageSize } from "../shared/vocabulary.ts";

interface UsageHistory {
  records: UsageRecord[];
  summary: UsageSummary;
  pricing: UsagePricing;
}

const counter = z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER);
const breakdown = z.object({
  schema_version: z.literal(2),
  quality: z.literal("complete"),
  total_tokens: counter,
  unclassified_tokens: z.literal(0),
  input: z.object({
    total_tokens: counter,
    uncached_tokens: counter,
    cache_read_tokens: counter,
    cache_write_tokens: counter,
  }),
  output: z.object({
    total_tokens: counter,
    non_reasoning_tokens: counter,
    reasoning_tokens: counter,
  }),
});

export function pricingTokens(value: Json | undefined): PricingTokens | null {
  const parsed = breakdown.safeParse(value);
  if (!parsed.success) return null;
  const { input: i, output: o, total_tokens: total } = parsed.data;
  if (
    i.total_tokens !==
      i.uncached_tokens + i.cache_read_tokens + i.cache_write_tokens ||
    o.total_tokens !== o.non_reasoning_tokens + o.reasoning_tokens ||
    total !== i.total_tokens + o.total_tokens
  )
    return null;
  return {
    input: i.uncached_tokens,
    cached: i.cache_read_tokens,
    cacheWrite: i.cache_write_tokens,
    output: o.non_reasoning_tokens,
    reasoning: o.reasoning_tokens,
  };
}

function legacyTokens(r: UsageRecord): PricingTokens | null {
  const t = {
    input: r.inputTokens,
    cached: r.cachedTokens,
    cacheWrite: r.cacheWriteTokens ?? 0,
    output: r.outputTokens,
    reasoning: r.reasoningTokens,
  };
  // Pinned core's raw Claude output includes reasoning; Gemini reports it separately.
  if (["claude", "anthropic"].includes(r.provider)) t.output -= t.reasoning;
  else if (["gemini", "antigravity", "google"].includes(r.provider))
    t.input -= t.cached + t.cacheWrite;
  else if (["openai", "codex", "xai", "kimi"].includes(r.provider)) {
    t.input -= t.cached + t.cacheWrite;
    t.output -= t.reasoning;
  } else return null;
  const values = Object.values(t);
  return values.every((n) => Number.isSafeInteger(n) && n >= 0) &&
    values.reduce((sum, n) => sum + n, 0) === r.totalTokens
    ? t
    : null;
}

export function normalizedTokens(r: UsageRecord): PricingTokens | null {
  if (r.pricingTokens === undefined) return legacyTokens(r);
  const tokens = r.pricingTokens;
  if (!tokens) return null;
  const values = Object.values(tokens);
  return values.every((n) => Number.isSafeInteger(n) && n >= 0) &&
    values.reduce((sum, n) => sum + n, 0) === r.totalTokens
    ? tokens
    : null;
}

export function estimateUsd(
  r: UsageRecord,
  cost: NonNullable<CatalogModel["metadata"]>["cost"],
  t = normalizedTokens(r),
): number | null {
  if (!cost || !t) return null;
  const context = t.input + t.cached + t.cacheWrite;
  let rate = cost;
  let threshold = -1;
  for (const tier of cost.tiers ?? []) {
    if (context > tier.tier.size && tier.tier.size > threshold) {
      rate = tier;
      threshold = tier.tier.size;
    }
  }
  if (
    (t.cached > 0 && rate.cache_read === undefined) ||
    (t.cacheWrite > 0 && rate.cache_write === undefined)
  )
    return null;
  const usd =
    (t.input * rate.input +
      t.output * rate.output +
      t.reasoning * (rate.reasoning ?? rate.output) +
      t.cached * (rate.cache_read ?? 0) +
      t.cacheWrite * (rate.cache_write ?? 0)) /
    1_000_000;
  return Number.isFinite(usd) ? usd : null;
}

const fxSource = "https://api.frankfurter.dev/v2/providers/ecb/rate/usd/eur";
const fxSchema = z.object({
  base: z.literal("USD"),
  quote: z.literal("EUR"),
  date: z.iso.date(),
  rate: z.number().finite().positive(),
});

export class UsagePrices {
  private totals = new Map<
    string,
    {
      since: string;
      sequence: number;
      generation: number;
      fetchedAt: string | null;
      total: UsageTotals;
      summary: UsageSummary;
    }
  >();
  private histories = new Map<string, Promise<UsageHistory>>();
  private closed = false;
  private eur: UsagePricing["eur"] = null;
  private nextRefresh = 0;
  private pending?: Promise<void>;
  private fetcher: typeof fetch;
  constructor(fetcher: typeof fetch = fetch) {
    this.fetcher = fetcher;
  }

  snapshot(records: UsageRecord[], catalog: ModelCatalog): UsagePricing {
    if (records.length && !this.pending && Date.now() >= this.nextRefresh) {
      this.nextRefresh = Date.now() + 60_000;
      this.pending = this.refreshEuro()
        .catch(() => {})
        .finally(() => {
          this.pending = undefined;
        });
    }
    const key = (r: UsageRecord) =>
      JSON.stringify([r.provider, r.upstreamModel || r.model]);
    const unique = new Map(
      records.map((r) => [
        key(r),
        { id: r.upstreamModel || r.model, owned_by: r.provider },
      ]),
    );
    const snapshot = records.length
      ? catalog.snapshot([...unique.values()])
      : { models: [], fetchedAt: null, refreshing: false };
    const costs = new Map(
      snapshot.models.map((model) => [
        JSON.stringify([model.owned_by, model.id]),
        model.metadata?.cost,
      ]),
    );
    return {
      usd: Object.fromEntries(
        records.map((r) => [r.id, estimateUsd(r, costs.get(key(r)))]),
      ),
      fetchedAt: snapshot.fetchedAt,
      refreshing: snapshot.refreshing,
      eur: this.eur,
    };
  }

  async history(
    store: Store,
    profileId: string,
    since: string,
    catalog: ModelCatalog,
  ): Promise<UsageHistory> {
    // Serialize only callers for this profile; retain one bounded aggregate, not history rows.
    // A predecessor's failure is its own caller's; waiters still run their own request.
    while (this.histories.has(profileId))
      await this.histories.get(profileId)?.catch(() => {});
    if (this.closed) throw new Error("Activity pricing is shutting down");
    const operation = this.summarize(store, profileId, since, catalog);
    this.histories.set(profileId, operation);
    try {
      return await operation;
    } finally {
      if (this.histories.get(profileId) === operation)
        this.histories.delete(profileId);
    }
  }

  private async summarize(
    store: Store,
    profileId: string,
    since: string,
    catalog: ModelCatalog,
  ): Promise<UsageHistory> {
    // Capture one storage snapshot after queued callers finish, before yielding.
    const records = store.usage(profileId, since, usagePageSize);
    const pricing = this.snapshot(records, catalog);
    // Freeze rates for the entire calculation, even if a network refresh finishes between batches.
    const snapshot = records.length
      ? catalog.snapshot([])
      : {
          enrich: (models: CatalogModel[]) => models,
          fetchedAt: null,
        };
    const generation = store.usageGeneration;
    const through = store.usageSequence();
    const cached = this.totals.get(profileId);
    const previous =
      cached &&
      cached.generation === generation &&
      cached.fetchedAt === snapshot.fetchedAt &&
      since >= cached.since
        ? cached
        : undefined;
    const total: UsageTotals = previous
      ? { ...previous.total, tokens: { ...previous.total.tokens } }
      : {
          requests: 0,
          pricedRequests: 0,
          usd: 0,
          normalizedRequests: 0,
          tokens: {
            input: 0,
            cached: 0,
            cacheWrite: 0,
            output: 0,
            reasoning: 0,
          },
        };
    const summary: UsageSummary = previous
      ? { ...previous.summary }
      : {
          requests: 0,
          failed: 0,
          inputTokens: 0,
          outputTokens: 0,
          cachedTokens: 0,
          reasoningTokens: 0,
          totalTokens: 0,
        };
    const costs = new Map<
      string,
      NonNullable<CatalogModel["metadata"]>["cost"]
    >();
    const include = (r: UsageRecord, direction: number) => {
      const id = r.upstreamModel || r.model;
      const key = JSON.stringify([r.provider, id]);
      if (!costs.has(key))
        costs.set(
          key,
          snapshot.enrich([{ id, owned_by: r.provider }])[0]?.metadata?.cost,
        );
      const tokens = normalizedTokens(r);
      const usd = estimateUsd(r, costs.get(key), tokens);
      summary.requests += direction;
      summary.failed += direction * Number(r.failed);
      summary.inputTokens += direction * r.inputTokens;
      summary.outputTokens += direction * r.outputTokens;
      summary.cachedTokens += direction * r.cachedTokens;
      summary.reasoningTokens += direction * r.reasoningTokens;
      summary.totalTokens += direction * r.totalTokens;
      total.requests += direction;
      if (usd !== null) {
        total.pricedRequests += direction;
        total.usd += direction * usd;
      }
      if (tokens) {
        total.normalizedRequests += direction;
        total.tokens.input += direction * tokens.input;
        total.tokens.cached += direction * tokens.cached;
        total.tokens.cacheWrite += direction * tokens.cacheWrite;
        total.tokens.output += direction * tokens.output;
        total.tokens.reasoning += direction * tokens.reasoning;
      }
    };
    if (previous && since > previous.since) {
      let timestamp = previous.since;
      let sequence = 0;
      for (;;) {
        if (this.closed) throw new Error("Activity pricing is shutting down");
        const batch = store.usageTimeBatch(
          profileId,
          previous.since,
          since,
          previous.sequence,
          timestamp,
          sequence,
        );
        for (const item of batch) include(item.record, -1);
        if (batch.length < usagePageSize) break;
        timestamp = batch[batch.length - 1].record.timestamp;
        sequence = batch[batch.length - 1].sequence;
        await yieldToLoop();
      }
    }
    let sequence = previous?.sequence ?? 0;
    let timestamp = since;
    for (;;) {
      if (this.closed) throw new Error("Activity pricing is shutting down");
      const batch = previous
        ? store.usageBatch(profileId, sequence, through)
        : store.usageTimeBatch(
            profileId,
            since,
            "\uffff",
            through,
            timestamp,
            sequence,
          );
      for (const item of batch)
        if (item.record.timestamp >= since) include(item.record, 1);
      if (batch.length < usagePageSize) break;
      sequence = batch[batch.length - 1].sequence;
      timestamp = batch[batch.length - 1].record.timestamp;
      await yieldToLoop();
    }
    // Retention may run while yielding; never publish a total containing deleted records.
    if (generation !== store.usageGeneration)
      return this.summarize(store, profileId, since, catalog);
    total.usd = Math.max(0, total.usd);
    this.totals.set(profileId, {
      since,
      sequence: through,
      generation,
      fetchedAt: snapshot.fetchedAt,
      total,
      summary,
    });
    return { records, summary, pricing: { ...pricing, totals: total } };
  }

  async close(): Promise<void> {
    this.closed = true;
    await Promise.allSettled(this.histories.values());
    this.totals.clear();
  }

  private async refreshEuro() {
    const response = await this.fetcher(fxSource, {
      redirect: "error",
      signal: AbortSignal.timeout(5000),
    });
    if (!response.ok || !response.body)
      throw new Error("Exchange rate unavailable");
    const reader = response.body.getReader();
    const chunks: Uint8Array[] = [];
    let size = 0;
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        size += value.length;
        if (size > 4096) throw new Error("Exchange rate response too large");
        chunks.push(value);
      }
    } finally {
      await reader.cancel();
    }
    const data = fxSchema.parse(
      JSON.parse(Buffer.concat(chunks).toString("utf8")),
    );
    this.eur = { rate: data.rate, date: data.date };
    this.nextRefresh = Date.now() + 86_400_000;
  }
}
