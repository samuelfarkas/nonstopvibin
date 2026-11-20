import { Accounts } from "./accounts.ts";
import { Credentials } from "./credentials.ts";
import { CorePool, type CoreTiming, type Management } from "./core.ts";
import { Quotas } from "./quota.ts";
import { CodexResets } from "./reset-credits.ts";
import { SignIn } from "./sign-in.ts";
import type { Report } from "./errors.ts";
import type { Store } from "./store.ts";
import { Usage, type UsageTiming } from "./usage.ts";

export interface CoresOptions {
  management?: Management;
  timing?: CoreTiming;
  usageTiming?: UsageTiming;
  inFlight?: (profileId: string) => number;
  /** The sign-in clock. */
  now?: () => number;
  report: Report;
  onStatusChange?: () => void;
  onSignInCallback?: () => void;
}

export class Cores {
  readonly pool: CorePool;
  readonly accounts: Accounts;
  readonly quotas: Quotas;
  readonly credentials: Credentials;
  readonly signIn: SignIn;
  readonly resets: CodexResets;
  readonly usage: Usage;
  private readonly report: Report;
  constructor(store: Store, binary: string, options: CoresOptions) {
    this.report = options.report;
    this.pool = new CorePool(store, binary, {
      management: options.management,
      timing: options.timing,
      // Runs on each readiness attempt: usage starts once per core; a failed account sync is retried.
      ready: async (profileId, signal) => {
        this.usage.started(profileId);
        await this.accounts.sync(profileId, signal);
      },
      // The usage queue lives only in the core's memory.
      beforeStop: (profileId) => this.usage.drain(profileId),
      onStatusChange: options.onStatusChange,
      // The scheduler hears what happens to each profile's core as it happens.
      onCoreEvent: (event) => this.usage.coreEvent(event),
    });
    this.accounts = new Accounts(this.pool, store);
    this.quotas = new Quotas(this.pool, this.accounts);
    this.credentials = new Credentials(this.pool, this.accounts, this.quotas);
    this.signIn = new SignIn(
      this.pool,
      this.accounts,
      this.credentials,
      this.quotas,
      {
        now: options.now,
        report: options.report,
        onCallback: options.onSignInCallback,
      },
    );
    this.resets = new CodexResets(this.pool, this.accounts, this.quotas);
    this.usage = new Usage(this.pool, this.accounts, this.quotas, {
      timing: options.usageTiming,
      inFlight: options.inFlight,
      report: options.report,
    });
  }
  async restore(signal?: AbortSignal): Promise<void> {
    signal?.throwIfAborted();
    // Orphans first: if the pending sign-ins cannot be removed, startup fails without leaving one running.
    await this.pool.reapOrphans();
    await this.signIn.discardPending();
    const reported = await this.pool.restore(this.report, signal);
    signal?.throwIfAborted();
    this.usage.begin(reported);
  }
  /**
   * Refuses new work and abandons an automatic start, lets sign-in actions and then
   * credential changes finish, drops the pending sign-in, lets the scheduler's running
   * work finish, then stops every core.
   * The caller closes the store only after this: each stop drains usage into it.
   */
  async shutdown(): Promise<void> {
    this.pool.beginShutdown();
    this.usage.abandonRestarts();
    await this.signIn.settled();
    await this.credentials.settled();
    try {
      await this.signIn.clear();
    } finally {
      await this.usage.settled();
      await this.pool.stopAll();
    }
  }
}
