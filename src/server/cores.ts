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
  /** How management calls reach a core; tests answer some of them. */
  management?: Management;
  timing?: CoreTiming;
  usageTiming?: UsageTiming;
  /** Gateway requests or WebSockets still open for a profile; their usage arrives later. */
  inFlight?: (profileId: string) => number;
  /** The sign-in clock. */
  now?: () => number;
  /** Where the features report errors the UI shows. */
  report: Report;
  /** A core became running, failed, exited, or applied new settings. */
  onStatusChange?: () => void;
  /** A browser sign-in callback was accepted. */
  onSignInCallback?: () => void;
}

/** The profile cores and the features that run on them, wired together. */
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
      // A new core's drain cadence starts over, and its accounts are read before start resolves.
      ready: async (profileId, signal) => {
        this.usage.started(profileId);
        await this.accounts.sync(profileId, signal);
      },
      // The usage queue lives only in the core's memory.
      beforeStop: (profileId) => this.usage.drain(profileId),
      onStatusChange: options.onStatusChange,
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
    await this.signIn.discardPending();
    await this.pool.restore(this.report, signal);
    signal?.throwIfAborted();
    this.usage.begin();
  }
  /**
   * Refuses new work, lets sign-in actions and then credential changes finish, drops the
   * pending sign-in, lets the scheduler's running pass finish, then stops every core.
   * The caller closes the store only after this: each stop drains usage into it.
   */
  async shutdown(): Promise<void> {
    this.pool.beginShutdown();
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
