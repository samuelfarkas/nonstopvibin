import { createHash } from "node:crypto";
import { z } from "zod";
import type { Json, PricingTokens, UsageRecord } from "../shared/types.ts";
import { quotaRefreshMs, usagePageSize } from "../shared/vocabulary.ts";
import type { Accounts } from "./accounts.ts";
import type { CoreEvent, CorePool, CoreRuntime } from "./core.ts";
import { AppError, errorMessage, type Report } from "./errors.ts";
import { number, record, text, unwrap } from "./json.ts";
import type { Quotas } from "./quota.ts";
import {
  initial,
  transition,
  type RestartEvent,
  type RestartState,
  type RestartTiming,
} from "./restart-policy.ts";
import type { Store } from "./store.ts";

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
export interface UsageTiming extends RestartTiming {
  /** Account status refresh for running profiles, while a UI watches. */
  accounts: number;
  /** How long one UI read of the state keeps account and quota polling on. */
  watch: number;
  /** Quota refresh for running profiles, while a UI watches. */
  quotas: number;
  /** Longest wait between retries of a usage drain that keeps failing. */
  usageBackoff: number;
  /** How long after gateway activity the queue keeps being drained. */
  activity: number;
}
export const usageTiming: UsageTiming = {
  accounts: 20_000,
  // Three of the UI's 4 s polls: a hidden or closed UI stops polling the cores.
  watch: 12_000,
  quotas: quotaRefreshMs,
  coalesce: 2000,
  usageBackoff: 60_000,
  activity: 10_000,
  restartBackoff: 300_000,
  restartLimit: 10,
};
/** How long the first UI read after a pause waits for fresh account status. */
const freshStatusWaitMs = 2000;
interface Cadence {
  core: Readonly<CoreRuntime> | undefined;
  nextAt: number;
  activeUntil: number;
  failures: number;
}
export interface UsageOptions {
  /** Read on every wakeup, so a caller may change it while the scheduler runs. */
  timing?: UsageTiming;
  /** Gateway requests or WebSockets still open for a profile; their usage arrives later. */
  inFlight?: (profileId: string) => number;
  report: Report;
}

/** Drains every running core's usage queue into the store, and runs account, quota and history maintenance. */
export class Usage {
  private readonly pool: CorePool;
  private readonly store: Store;
  private readonly accounts: Accounts;
  private readonly quotas: Quotas;
  private readonly timing: UsageTiming;
  private readonly inFlight: (profileId: string) => number;
  private readonly report: Report;
  private readonly cadences = new Map<string, Cadence>();
  private readonly collections = new Map<string, Promise<number>>();
  /** Each profile's restart state: `restart-policy.ts` decides, this carries it out. */
  private readonly restarts = new Map<string, RestartState>();
  /** When each profile's restart state wants to hear the time. */
  private readonly restartWakes = new Map<string, number>();
  /** What a profile shows once the scheduler stopped starting it. */
  private readonly shown = new Map<string, string>();
  /** Automatic starts in flight, each with the quota check after it. */
  private readonly work = new Set<Promise<void>>();
  /** Profiles whose quota check after an automatic start is in flight. */
  private readonly checking = new Set<string>();
  /** Ends an automatic start still waiting for its core once shutdown begins. */
  private readonly abandon = new AbortController();
  private scheduledAt = Infinity;
  /** Until when a UI read keeps account and quota polling on. */
  private watchedUntil = 0;
  private nextAccounts = Infinity;
  private nextQuotas = Infinity;
  private timer?: NodeJS.Timeout;
  private polling?: Promise<void>;
  private maintenance?: Promise<void>;
  private schedule?: () => void;
  constructor(
    pool: CorePool,
    accounts: Accounts,
    quotas: Quotas,
    options: UsageOptions,
  ) {
    this.pool = pool;
    this.store = accounts.store;
    this.accounts = accounts;
    this.quotas = quotas;
    this.timing = options.timing ?? { ...usageTiming };
    this.inFlight = options.inFlight ?? (() => 0);
    this.report = options.report;
  }
  /** A core answered: a new core's drain cadence starts over; a readiness retry of the same core keeps it. */
  started(profileId: string): void {
    const core = this.pool.runtimes.get(profileId);
    if (this.cadences.get(profileId)?.core !== core)
      this.cadences.set(profileId, {
        core,
        nextAt: 0,
        activeUntil: 0,
        failures: 0,
      });
    this.schedule?.();
  }
  cadence(profileId: string): Readonly<Cadence> | undefined {
    return this.cadences.get(profileId);
  }
  /** What happened to a profile's core, told by the pool as it happens. */
  coreEvent(event: CoreEvent): void {
    this.hear(event.profileId, event);
    this.schedule?.();
  }
  /** Subscriptions may have changed: whether each profile is kept running is read again. */
  changed(): void {
    for (const profileId of this.store.profileNames().keys())
      this.hear(profileId);
    this.schedule?.();
  }
  /** Shutdown began: an automatic start gives up now; the pool's stops end its core. */
  abandonRestarts(): void {
    this.abandon.abort(new AppError("nonstopvibin is shutting down.", 503));
  }
  /** What a profile shows once the scheduler stopped starting it. */
  gaveUp(profileId: string): string | undefined {
    return this.shown.get(profileId);
  }
  /** When the scheduler next wakes; Infinity while nothing is due. */
  get wakesAt(): number {
    return this.scheduledAt;
  }
  /**
   * Gives the profile's restart policy `event`, after whether the profile is kept running if
   * that changed. `reported`: a profile seen first, whose failure the launch restore reported.
   */
  private hear(
    profileId: string,
    event?: RestartEvent,
    reported = false,
  ): void {
    // Before `begin`, the launch restore is starting profiles; `begin` looks once it is done.
    if (!this.schedule || this.pool.shuttingDown) return;
    const kept =
      this.store.profileNames().has(profileId) &&
      this.accounts.keepsRunning(profileId);
    const known = this.restarts.get(profileId);
    let state =
      known ??
      initial({
        running: this.pool.running(profileId),
        reported,
        kept,
        starting: this.pool.starting.has(profileId),
        holding: this.pool.holding(profileId),
      });
    if (!known || state.kept !== kept)
      state = this.act(profileId, state, { type: "kept", kept });
    if (event) this.act(profileId, state, event);
  }
  /** One transition, carried out: its state is in place before a start can tell anything back. */
  private act(
    profileId: string,
    state: RestartState,
    event: RestartEvent,
  ): RestartState {
    const step = transition(state, event, Date.now(), this.timing);
    this.restarts.set(profileId, step.state);
    this.restartWakes.delete(profileId);
    for (const action of step.actions)
      if (action.type === "wake") this.restartWakes.set(profileId, action.at);
      else if (action.type === "report") this.reportFor(profileId, action.text);
      else if (action.type === "show")
        if (action.text === undefined) this.shown.delete(profileId);
        else this.shown.set(profileId, action.text);
    if (step.actions.some((action) => action.type === "start"))
      this.restart(profileId);
    return step.state;
  }
  private reportFor(profileId: string, message: string): void {
    const name = this.store.profileNames().get(profileId);
    if (name !== undefined) this.report(`${name}: ${message}`);
  }
  /** Starts one profile apart from maintenance and from other profiles' starts, then checks its quota. */
  private restart(profileId: string): void {
    const task = (async () => {
      try {
        // Shutdown must not wait out this core's readiness window.
        await this.pool.start(profileId, this.abandon.signal, true);
      } catch (error) {
        // The pool tells how a start it began ended; this tells of one it refused to begin.
        this.hear(profileId, {
          type: "started",
          automatic: true,
          running: false,
          error: errorMessage(error),
        });
        this.schedule?.();
        return;
      }
      // Restart decisions never wait for this check: it is not part of the policy.
      if (this.checking.has(profileId)) return;
      this.checking.add(profileId);
      try {
        // It missed its quota checks while down; a stop ends the checks, and shutdown
        // abandons the one in flight without storing it.
        await this.quotas.refreshAccounts(
          profileId,
          (account) =>
            this.pool.usable(profileId) &&
            !account.disabled &&
            account.quota?.status !== "unavailable",
          this.abandon.signal,
        );
      } catch (error) {
        if (!this.pool.shuttingDown)
          this.reportFor(profileId, errorMessage(error));
      } finally {
        this.checking.delete(profileId);
      }
    })()
      // One profile's failure must not end the scheduler or another profile's start.
      .catch((error) => this.report(errorMessage(error)))
      .finally(() => this.work.delete(task));
    this.work.add(task);
  }
  private watched(): boolean {
    return Date.now() < this.watchedUntil;
  }
  /**
   * A UI read the state. Account status and quota are polled only while one keeps reading,
   * and the first read after a pause waits briefly for every running profile's account status.
   */
  async watch(): Promise<void> {
    const resumed = !this.watched();
    this.watchedUntil = Date.now() + this.timing.watch;
    if (!resumed || !this.schedule) return;
    // Quota older than its period refreshes in the next maintenance pass, in the background.
    this.nextAccounts =
      Date.now() >= this.nextQuotas
        ? Date.now()
        : Date.now() + this.timing.accounts;
    this.schedule();
    const signal = AbortSignal.timeout(freshStatusWaitMs);
    await Promise.all(
      [...this.pool.runtimes.keys()]
        .filter((profileId) => this.pool.usable(profileId))
        .map((profileId) =>
          this.accounts.sync(profileId, signal).catch((error) => {
            // A core slower than the wait answers the next poll instead.
            if (!signal.aborted) this.reportFor(profileId, errorMessage(error));
          }),
        ),
    );
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
  async drain(profileId: string): Promise<void> {
    try {
      while ((await this.collect(profileId)) === usagePageSize) {
        // Full destructive queue pages must be persisted before terminating the core.
      }
    } catch (error) {
      this.report(`Usage before stop: ${errorMessage(error)}`);
    }
  }
  /** Starts scheduling; `reported`: profiles whose failed start the launch restore reported. */
  begin(reported: Iterable<string> = []): void {
    clearTimeout(this.timer);
    this.nextAccounts = Date.now() + this.timing.accounts;
    this.nextQuotas = Date.now() + this.timing.quotas;
    let nextPrune = Date.now() + 86_400_000;
    this.scheduledAt = Infinity;
    const schedule = () => {
      if (this.pool.shuttingDown) return;
      const running = [...this.pool.runtimes.keys()].filter((profileId) =>
        this.pool.running(profileId),
      );
      const due = Math.min(
        this.maintenance ? Infinity : nextPrune,
        // Account and quota polling only while a UI watches: nothing else reads them.
        this.maintenance || !running.length || !this.watched()
          ? Infinity
          : this.nextAccounts,
        // Only what the restart policy asked for: nothing for a profile that cannot start.
        ...this.restartWakes.values(),
        ...(this.polling
          ? []
          : running.map(
              (profileId) => this.cadences.get(profileId)?.nextAt ?? Infinity,
            )),
      );
      const at = Math.max(Date.now() + this.timing.coalesce, due);
      if (at >= this.scheduledAt) return;
      clearTimeout(this.timer);
      this.scheduledAt = at;
      this.timer = setTimeout(tick, at - Date.now());
      this.timer.unref();
    };
    const tick = () => {
      this.scheduledAt = Infinity;
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
              cadence.failures++;
              cadence.nextAt =
                cadence.failures === 1
                  ? 0
                  : Date.now() +
                    Math.min(
                      this.timing.coalesce * 2 ** (cadence.failures - 1),
                      this.timing.usageBackoff,
                    );
              this.report(
                `${this.store.profile(profileId).name}: ${errorMessage(error)}`,
              );
            }
          }
        })().finally(() => {
          this.polling = undefined;
          schedule();
        });
      const accountsDue = this.watched() && Date.now() >= this.nextAccounts;
      if (!this.maintenance && (accountsDue || Date.now() >= nextPrune)) {
        const quotasDue = accountsDue && Date.now() >= this.nextQuotas;
        if (accountsDue) this.nextAccounts = Date.now() + this.timing.accounts;
        if (quotasDue) this.nextQuotas = Date.now() + this.timing.quotas;
        this.maintenance = (async () => {
          if (Date.now() >= nextPrune) {
            nextPrune = Date.now() + 86_400_000;
            try {
              this.store.pruneUsage();
            } catch (error) {
              this.report(`History cleanup: ${errorMessage(error)}`);
            }
          }
          if (!accountsDue) return;
          for (const profileId of this.pool.runtimes.keys()) {
            if (this.pool.shuttingDown) return;
            if (!this.pool.usable(profileId)) continue;
            try {
              if (!quotasDue) await this.accounts.sync(profileId);
              else
                await this.quotas.refreshAccounts(
                  profileId,
                  // Each account is checked as its turn comes; a stop or shutdown ends the pass.
                  (account) =>
                    this.pool.usable(profileId) &&
                    !account.disabled &&
                    account.quota?.status !== "unavailable",
                );
            } catch (error) {
              this.report(
                `${this.store.profile(profileId).name}: ${errorMessage(error)}`,
              );
            }
          }
        })().finally(() => {
          this.maintenance = undefined;
          schedule();
        });
      }
      // After the drain pass and maintenance are under way, so a start's notices see them.
      for (const [profileId, at] of [...this.restartWakes])
        if (at <= Date.now()) this.hear(profileId, { type: "time" });
      schedule();
    };
    this.schedule = schedule;
    // Profiles the launch restore could not start are due at once.
    const launch = new Set(reported);
    for (const profileId of this.store.profileNames().keys())
      this.hear(profileId, undefined, launch.has(profileId));
    schedule();
  }
  /** Stops scheduling and waits for the drain pass, maintenance and automatic starts already running. */
  async settled(): Promise<void> {
    clearTimeout(this.timer);
    this.scheduledAt = Infinity;
    await this.polling;
    await this.maintenance;
    await Promise.all(this.work);
  }
}
