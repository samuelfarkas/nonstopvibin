import { createHash } from "node:crypto";
import { z } from "zod";
import type { Json, PricingTokens, UsageRecord } from "../shared/types.ts";
import { quotaRefreshMs, usagePageSize } from "../shared/vocabulary.ts";
import type { Accounts } from "./accounts.ts";
import type { CorePool } from "./core.ts";
import { AppError, errorMessage } from "./errors.ts";
import { number, record, text, unwrap } from "./json.ts";
import type { Quotas } from "./quota.ts";
import type { Store } from "./store.ts";

// The core's usage queue: what each record holds, how it is drained, and when.

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

/** One usage-queue entry as stored: accounting metadata only, never keys, prompts or upstream error bodies. */
function usageRecord(profileId: string, item: Json): UsageRecord {
  const r = record(unwrap(item));
  const t = record(r.tokens);
  const fail = record(r.fail);
  const n = (key: string) => Math.max(0, number(t[key]) ?? 0);
  const timestamp = text(r.timestamp) ?? new Date().toISOString();
  const requestId = String(
    r.request_id ||
      createHash("sha256").update(JSON.stringify(r)).digest("hex"),
  );
  return {
    id: `${profileId}:${requestId}`,
    profileId,
    timestamp,
    provider: String(r.provider ?? "unknown"),
    model: String(r.alias || r.model || "unknown"),
    upstreamModel: text(r.model),
    pricingTokens: pricingTokens(r.token_breakdown),
    account: String(r.auth_index ?? ""),
    inputTokens: n("input_tokens"),
    outputTokens: n("output_tokens"),
    cachedTokens: Math.max(
      0,
      number(t.cache_read_tokens) ?? n("cached_tokens"),
    ),
    reasoningTokens: n("reasoning_tokens"),
    cacheWriteTokens: n("cache_creation_tokens"),
    totalTokens: n("total_tokens"),
    latencyMs: Math.max(0, number(r.latency_ms) ?? 0),
    failed: r.failed === true,
    statusCode: number(fail.status_code) || (r.failed ? 502 : 200),
    stream: r.stream === true,
  };
}

/** Scheduler periods in milliseconds. */
export interface UsageTiming {
  /** Account status refresh for running profiles. */
  accounts: number;
  /** Shortest wait between scheduler wakeups. */
  coalesce: number;
  /** Longest wait between retries of a usage drain that keeps failing. */
  usageBackoff: number;
  /** How long after gateway activity the queue keeps being drained. */
  activity: number;
}
export const usageTiming: UsageTiming = {
  accounts: 20_000,
  coalesce: 2000,
  usageBackoff: 60_000,
  activity: 10_000,
};
interface Cadence {
  nextAt: number;
  activeUntil: number;
  /** Consecutive failed drains. */
  failures: number;
}
export interface UsageOptions {
  /** Read on every wakeup, so a caller may change it while the scheduler runs. */
  timing?: UsageTiming;
  /** Gateway requests or WebSockets still open for a profile; their usage arrives later. */
  inFlight?: (profileId: string) => number;
}

/** Drains every running core's usage queue into the store, and runs account, quota and history maintenance. */
export class Usage {
  private readonly pool: CorePool;
  private readonly store: Store;
  private readonly accounts: Accounts;
  private readonly quotas: Quotas;
  private readonly timing: UsageTiming;
  private readonly inFlight: (profileId: string) => number;
  private readonly cadences = new Map<string, Cadence>();
  private readonly collections = new Map<string, Promise<number>>();
  private timer?: NodeJS.Timeout;
  private polling?: Promise<void>;
  private maintenance?: Promise<void>;
  private schedule?: () => void;
  constructor(
    pool: CorePool,
    accounts: Accounts,
    quotas: Quotas,
    options: UsageOptions = {},
  ) {
    this.pool = pool;
    this.store = accounts.store;
    this.accounts = accounts;
    this.quotas = quotas;
    this.timing = options.timing ?? { ...usageTiming };
    this.inFlight = options.inFlight ?? (() => 0);
  }
  /** A new core answered: its drain cadence starts over. */
  started(profileId: string): void {
    this.cadences.set(profileId, { nextAt: 0, activeUntil: 0, failures: 0 });
    this.schedule?.();
  }
  /** The scheduler's view of one profile. */
  cadence(profileId: string): Readonly<Cadence> | undefined {
    return this.cadences.get(profileId);
  }
  markActivity(profileId: string): void {
    const cadence = this.cadences.get(profileId);
    if (!cadence) return;
    cadence.activeUntil = Date.now() + this.timing.activity;
    cadence.nextAt = 0;
    this.schedule?.();
  }
  /** Reads one page of the queue. Concurrent reads share one: the queue is destructive. */
  collect(profileId: string): Promise<number> {
    const pending = this.collections.get(profileId);
    if (pending) return pending;
    const collection = (async () => {
      const payload = await this.pool.management(
        profileId,
        `/usage-queue?count=${usagePageSize}`,
      );
      if (!Array.isArray(payload))
        throw new AppError("Unexpected usage queue format.", 502);
      const records = payload.map((item) => usageRecord(profileId, item));
      this.store.addUsage(records);
      return records.length;
    })().finally(() => {
      this.collections.delete(profileId);
    });
    this.collections.set(profileId, collection);
    return collection;
  }
  /** Reads the queue until a short page. The queue lives only in the core's memory, so this runs before it stops. */
  async drain(profileId: string): Promise<void> {
    try {
      while ((await this.collect(profileId)) === usagePageSize) {
        // Full destructive queue pages must be persisted before terminating the core.
      }
    } catch (error) {
      this.pool.report(`Usage before stop: ${errorMessage(error)}`);
    }
  }
  /** Starts the scheduler. */
  begin(): void {
    clearTimeout(this.timer);
    let nextAccounts = Date.now() + this.timing.accounts;
    let nextQuotas = Date.now() + quotaRefreshMs;
    let nextPrune = Date.now() + 86_400_000;
    let scheduledAt = Infinity;
    const schedule = () => {
      if (this.pool.shuttingDown) return;
      const running = [...this.pool.runtimes.keys()].filter((profileId) =>
        this.pool.running(profileId),
      );
      const due = Math.min(
        this.maintenance ? Infinity : nextPrune,
        this.maintenance || !running.length ? Infinity : nextAccounts,
        ...(this.polling
          ? []
          : running.map(
              (profileId) => this.cadences.get(profileId)?.nextAt ?? Infinity,
            )),
      );
      // Coalesce active work; idle work sleeps until its deadline.
      const at = Math.max(Date.now() + this.timing.coalesce, due);
      if (at >= scheduledAt) return;
      clearTimeout(this.timer);
      scheduledAt = at;
      this.timer = setTimeout(tick, at - Date.now());
      this.timer.unref();
    };
    const tick = () => {
      scheduledAt = Infinity;
      this.timer = undefined;
      if (this.pool.shuttingDown) return;
      if (!this.polling)
        this.polling = (async () => {
          for (const profileId of this.pool.runtimes.keys()) {
            if (this.pool.shuttingDown) return;
            const cadence = this.cadences.get(profileId);
            // A stopping profile's own stop drains its queue until a short page.
            if (
              !cadence ||
              !this.pool.usable(profileId) ||
              Date.now() < cadence.nextAt
            )
              continue;
            // Usage only arises from gateway traffic, which marks activity. Once it has
            // been quiet for the whole window, this drain was the trailing one: sleep
            // until new activity or stop (which drains). Open requests keep a slow
            // cadence because their usage lands only as each response completes.
            // Set before awaiting so activity during collection can reset it to 0.
            cadence.nextAt = this.inFlight(profileId)
              ? Date.now() + 30_000
              : Infinity;
            try {
              const count = await this.collect(profileId);
              cadence.failures = 0;
              if (count === usagePageSize || Date.now() < cadence.activeUntil)
                cadence.nextAt = 0;
            } catch (error) {
              // Retry the first failure at once, then back off up to timing.usageBackoff.
              cadence.failures++;
              cadence.nextAt =
                cadence.failures === 1
                  ? 0
                  : Date.now() +
                    Math.min(
                      this.timing.coalesce * 2 ** (cadence.failures - 1),
                      this.timing.usageBackoff,
                    );
              this.pool.report(
                `${this.store.profile(profileId).name}: ${errorMessage(error)}`,
              );
            }
          }
        })().finally(() => {
          this.polling = undefined;
          schedule();
        });
      if (
        !this.maintenance &&
        (Date.now() >= nextAccounts || Date.now() >= nextPrune)
      ) {
        const quotasDue = Date.now() >= nextQuotas;
        nextAccounts = Date.now() + this.timing.accounts;
        if (quotasDue) nextQuotas = Date.now() + quotaRefreshMs;
        this.maintenance = (async () => {
          if (Date.now() >= nextPrune) {
            nextPrune = Date.now() + 86_400_000;
            try {
              this.store.pruneUsage();
            } catch (error) {
              this.pool.report(`History cleanup: ${errorMessage(error)}`);
            }
          }
          for (const profileId of this.pool.runtimes.keys()) {
            if (this.pool.shuttingDown) return;
            if (!this.pool.running(profileId)) continue;
            try {
              if (!quotasDue) await this.accounts.sync(profileId);
              else
                await this.quotas.refreshAccounts(
                  profileId,
                  // Each account is checked as its turn comes; a stop or shutdown ends the pass.
                  (account) =>
                    !this.pool.shuttingDown &&
                    this.pool.running(profileId) &&
                    !account.disabled &&
                    account.quota?.status !== "unavailable",
                );
            } catch (error) {
              this.pool.report(
                `${this.store.profile(profileId).name}: ${errorMessage(error)}`,
              );
            }
          }
        })().finally(() => {
          this.maintenance = undefined;
          schedule();
        });
      }
      schedule();
    };
    this.schedule = schedule;
    schedule();
  }
  /** Stops scheduling and waits for the drain pass and maintenance already running. */
  async settled(): Promise<void> {
    clearTimeout(this.timer);
    await this.polling;
    await this.maintenance;
  }
}
