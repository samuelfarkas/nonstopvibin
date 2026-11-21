import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rename,
  rm,
} from "node:fs/promises";
import { join } from "node:path";
import type { CorePhase, CoreState } from "../shared/types.ts";
import {
  compareVersions,
  coreLayout,
  corePinSchema,
  coreVersion,
  fileSha256,
  verifyCore,
  type CorePin,
} from "./core-binary.ts";
import { installCore } from "./core-install.ts";
import type { CorePool } from "./core.ts";
import type { Credentials } from "./credentials.ts";
import {
  coresDirectory,
  oauthPendingDirectory,
  ownerOnlyDirectory,
} from "./data-directory.ts";
import { AppError, errorMessage, type Report } from "./errors.ts";
import { version as appVersion } from "./config.ts";
import { parse, record, type JsonObject } from "./json.ts";
import { isMissing } from "./safe-files.ts";
import type { SignIn } from "./sign-in.ts";
import { Store } from "./store.ts";

// Reviewed core updates for installed builds. The feed is the core pin on `main`, so a core
// runs only if its archive and executable match a pin someone reviewed. Each update is kept
// as `<data>/cores/<version>/`, the executable beside its manifest. Startup selects the newest
// intact one above the bundled core, so an app update bundling a newer or equal core wins.

/** A core profiles can start from. */
export interface ActiveCore {
  version: string;
  binary: string;
}

const versionName = /^\d+\.\d+\.\d+$/;

async function entries(directory: string): Promise<string[]> {
  try {
    return await readdir(directory);
  } catch (error) {
    if (isMissing(error)) return [];
    throw error;
  }
}
/** Deletes every entry of `directory` except `keep`. */
async function prune(directory: string, keep?: string): Promise<void> {
  for (const name of await entries(directory))
    if (name !== keep)
      await rm(join(directory, name), { recursive: true, force: true });
}
/** A crash or a hand edit can leave anything in `cores/`: such an entry is no core. */
function unreadable(cause: unknown): boolean {
  return (
    cause instanceof SyntaxError ||
    isMissing(cause) ||
    (cause instanceof Error &&
      "code" in cause &&
      (cause.code === "ENOTDIR" || cause.code === "EISDIR"))
  );
}
/** Whether `directory` holds an intact `version` core for this target. */
async function intact(
  directory: string,
  version: string,
  platform: string,
  arch: string,
): Promise<boolean> {
  try {
    const manifest: JsonObject = record(
      parse(await readFile(join(directory, coreLayout.manifest), "utf8")),
    );
    return (
      manifest.version === version &&
      manifest.platform === platform &&
      manifest.arch === arch &&
      manifest.binarySha256 ===
        (await fileSha256(join(directory, coreLayout.binary)))
    );
  } catch (error) {
    if (unreadable(error)) return false;
    throw error;
  }
}

/**
 * The newest intact core in `directory` above `bundled` for `platform`/`arch`. Every other
 * entry is deleted: older or equal versions, damaged ones, and staging a crash left behind.
 */
export async function selectInstalledCore(
  directory: string,
  bundled: string,
  platform: string,
  arch: string,
): Promise<ActiveCore | undefined> {
  const newest = (await entries(directory))
    .filter(
      (name) => versionName.test(name) && compareVersions(name, bundled) > 0,
    )
    .sort((a, b) => compareVersions(b, a));
  let selected: ActiveCore | undefined;
  for (const version of newest)
    if (await intact(join(directory, version), version, platform, arch)) {
      selected = {
        version,
        binary: join(directory, version, coreLayout.binary),
      };
      break;
    }
  await prune(directory, selected?.version);
  return selected;
}

export interface CoreUpdatesOptions {
  /** The reviewed pin feed; without one only the bundled core runs. */
  feed?: string;
  /** Where release archives download from; tests serve their own. */
  archives?: string;
  /** Whether the bundled core must also match the pin compiled into this build. */
  requirePinnedCore?: boolean;
  report: Report;
}

/** Checks the feed, installs a reviewed core, or goes back to the bundled one: one at a time. */
export class CoreUpdates {
  private readonly pool: CorePool;
  private readonly signIn: SignIn;
  private readonly credentials: Credentials;
  private readonly options: CoreUpdatesOptions;
  private readonly directory: string;
  private readonly bundled: ActiveCore;
  private active: ActiveCore;
  /** What the last check fetched. */
  private pin?: CorePin;
  private phase?: CorePhase;
  private operation: Promise<void> = Promise.resolve();
  private checking?: Promise<void>;
  private readonly abort = new AbortController();
  constructor(
    pool: CorePool,
    signIn: SignIn,
    credentials: Credentials,
    options: CoreUpdatesOptions,
  ) {
    this.pool = pool;
    this.signIn = signIn;
    this.credentials = credentials;
    this.options = options;
    this.directory = coresDirectory(pool.store.directory);
    this.bundled = { version: coreVersion, binary: pool.binary };
    this.active = this.bundled;
  }
  get enabled(): boolean {
    return this.options.feed !== undefined;
  }
  /** The version of the core profiles start from. */
  get version(): string {
    return this.active.version;
  }
  state(): CoreState {
    return {
      bundled: this.bundled.version,
      updates: this.enabled,
      available: this.offers(this.pin) ? this.pin.version : undefined,
      phase: this.phase,
    };
  }
  /**
   * Whether `pin` is newer than the core in use and this app may run it. The schema requires
   * every target, so each valid pin covers this one.
   */
  private offers(pin: CorePin | undefined): pin is CorePin {
    return (
      pin !== undefined &&
      compareVersions(pin.version, this.active.version) > 0 &&
      (pin.minApp === undefined || compareVersions(appVersion, pin.minApp) >= 0)
    );
  }
  /**
   * At startup, before any core runs: profiles start from the newest intact installed core
   * above the bundled one, if any. Answers whether one was selected.
   */
  async select(): Promise<boolean> {
    if (!this.enabled) return false;
    const core = await selectInstalledCore(
      this.directory,
      this.bundled.version,
      process.platform,
      process.arch,
    );
    if (!core) return false;
    this.activate(core);
    return true;
  }
  private activate(core: ActiveCore): void {
    this.active = core;
    this.pool.use(core.binary);
  }
  private async run(
    phase: CorePhase,
    action: (feed: string, signal: AbortSignal) => Promise<void>,
  ): Promise<void> {
    const feed = this.options.feed;
    if (feed === undefined)
      throw new AppError("Only installed builds update the core.", 409);
    if (this.pool.shuttingDown)
      throw new AppError("nonstopvibin is shutting down.", 503);
    if (this.phase)
      throw new AppError("A core update is already in progress.", 409);
    this.phase = phase;
    const operation = action(feed, this.abort.signal);
    this.operation = operation;
    try {
      await operation;
    } finally {
      this.phase = undefined;
    }
  }
  /** A check already under way, such as the one at startup, answers this one too. */
  check(): Promise<void> {
    this.checking ??= this.run("checking", async (feed, signal) => {
      this.pin = await this.fetchPin(feed, signal);
    }).finally(() => {
      this.checking = undefined;
    });
    return this.checking;
  }
  /** A check is short and its answer is about to be replaced, so an install or revert waits for it. */
  private async afterCheck(): Promise<void> {
    await this.checking?.catch(() => undefined);
  }
  private async fetchPin(feed: string, signal: AbortSignal): Promise<CorePin> {
    const failed = (reason: string) =>
      new AppError(`Could not check for core updates: ${reason}`, 502);
    let body: unknown;
    try {
      const response = await fetch(feed, {
        signal: AbortSignal.any([signal, AbortSignal.timeout(15_000)]),
      });
      if (!response.ok) throw failed(`HTTP ${response.status}.`);
      body = await response.json();
    } catch (error) {
      signal.throwIfAborted();
      if (error instanceof AppError) throw error;
      throw failed(errorMessage(error));
    }
    const pin = corePinSchema.safeParse(body);
    if (!pin.success) throw failed("the feed is not a core pin.");
    return pin.data;
  }
  /**
   * Installs `version` if the feed still offers it, checks that it starts, then restarts
   * running profiles on it. Nothing changes unless every step before the restart succeeds.
   */
  async install(version: string): Promise<void> {
    await this.afterCheck();
    return this.run("checking", async (feed, signal) => {
      const pin = await this.fetchPin(feed, signal);
      this.pin = pin;
      if (pin.version !== version || !this.offers(pin))
        throw new AppError(`CLIProxyAPI ${version} is no longer offered.`, 409);
      this.phase = "downloading";
      ownerOnlyDirectory(this.directory);
      const staging = await mkdtemp(join(this.directory, ".staging-"));
      const target = join(this.directory, pin.version);
      try {
        await installCore(pin, process.platform, process.arch, staging, {
          archives: this.options.archives,
          signal,
        });
        this.phase = "testing";
        await this.preflight(
          join(staging, coreLayout.binary),
          pin.version,
          signal,
        );
        this.phase = "restarting";
        await this.switchTo(
          { version: pin.version, binary: join(target, coreLayout.binary) },
          async () => {
            await rm(target, { recursive: true, force: true });
            await rename(staging, target);
          },
        );
      } finally {
        await rm(staging, { recursive: true, force: true });
      }
      await prune(this.directory, pin.version);
    });
  }
  /** Restarts running profiles on the bundled core, then deletes installed updates. */
  async useBundled(): Promise<void> {
    await this.afterCheck();
    return this.run("restarting", async () => {
      if (this.active === this.bundled)
        throw new AppError("NonstopVibin already runs its bundled core.", 409);
      // Startup checked the installed core instead; keep it unless the bundled one is intact.
      await verifyCore(this.bundled.binary, this.options.requirePinnedCore);
      await this.switchTo(this.bundled);
      // Only now does no start still read an installed executable. A crash before this
      // leaves the update in place, and the next launch runs it.
      await prune(this.directory);
    });
  }
  /**
   * Starts `binary` on a throwaway profile, as a sign-in sandbox does, and lists its models
   * through the client listener and key. Startup reaps and deletes what a crash leaves.
   */
  private async preflight(
    binary: string,
    version: string,
    signal: AbortSignal,
  ): Promise<void> {
    const store = this.pool.store;
    const parent = oauthPendingDirectory(store.directory);
    await mkdir(parent, { recursive: true, mode: 0o700 });
    const directory = await mkdtemp(join(parent, "core-"));
    const sandbox = new Store(directory, store.codec);
    const pool = this.pool.isolated(sandbox, binary);
    try {
      const profile = sandbox.createProfile("Core check");
      await pool.start(profile.id, signal);
      await pool.models(profile.id);
    } catch (error) {
      signal.throwIfAborted();
      throw new AppError(
        `CLIProxyAPI ${version} did not start: ${errorMessage(error)} Nothing was changed.`,
        502,
      );
    } finally {
      try {
        await pool.shutdown();
      } finally {
        sandbox.close();
        await rm(directory, { recursive: true, force: true });
      }
    }
  }
  /**
   * Runs `change` if given, puts `core` in use, and restarts each running profile on it. It
   * waits for sign-in actions and credential changes and refuses while a sign-in is pending,
   * so a restart interrupts none. A profile that does not start again is reported.
   */
  private switchTo(
    core: ActiveCore,
    change?: () => Promise<void>,
  ): Promise<void> {
    return this.signIn.whileIdle(() =>
      this.credentials.queue(async () => {
        await change?.();
        this.activate(core);
        for (const profile of this.pool.store.profiles()) {
          // Starts are refused once shutdown begins; that is no failure of the core.
          if (this.pool.shuttingDown) return;
          // A start already under way may have read the previous core.
          await this.pool.starting.get(profile.id)?.catch(() => undefined);
          const runtime = this.pool.runtimes.get(profile.id);
          if (
            !runtime ||
            runtime.binary === core.binary ||
            !this.pool.alive(profile.id)
          )
            continue;
          try {
            await this.pool.stop(profile.id);
            await this.pool.start(profile.id);
          } catch (error) {
            if (this.pool.shuttingDown) return;
            this.options.report(`${profile.name}: ${errorMessage(error)}`);
          }
        }
      }),
    );
  }
  /** Aborts a download or start check, then waits for the operation in progress to end. */
  async shutdown(): Promise<void> {
    this.abort.abort(new AppError("nonstopvibin is shutting down.", 503));
    await this.operation.catch(() => undefined);
  }
}
