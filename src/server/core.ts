import { spawn, type ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import { createServer } from "node:net";
import { setTimeout as delay } from "node:timers/promises";
import type { Json, Model, ProfileState } from "../shared/types.ts";
import { ConfigWriter, applied } from "./config.ts";
import { AppError, errorMessage, type Report } from "./errors.ts";
import { record, responseJson, text } from "./json.ts";
import type { Store } from "./store.ts";
import {
  forgetCore,
  killGraceMs,
  reapOrphanedCores,
  recordCore,
  watchCore,
} from "./core-processes.ts";
import { coreArguments, profileDirectory } from "./data-directory.ts";

export interface CoreRuntime {
  child: ChildProcess;
  port: number;
  state: ProfileState["runtime"];
  error?: string;
  stopping: boolean;
  /** Kills the core if this process dies without stopping it. */
  watchdog?: ChildProcess;
  /** Ledger entry naming the core for the next instance if both die. */
  ledger?: string;
}
export interface ManagementRequest {
  profileId: string;
  port: number;
  key: string;
  path: string;
  method: string;
  body?: Json;
  timeout: number;
  signal?: AbortSignal;
}
export type Management = (request: ManagementRequest) => Promise<Json>;
export const httpManagement: Management = async (request) => {
  const headers = new Headers({ Authorization: `Bearer ${request.key}` });
  if (request.body !== undefined)
    headers.set("Content-Type", "application/json");
  const timeout = AbortSignal.timeout(request.timeout);
  const response = await fetch(
    `http://127.0.0.1:${request.port}/v0/management${request.path}`,
    {
      method: request.method,
      headers,
      body:
        request.body === undefined ? undefined : JSON.stringify(request.body),
      signal: request.signal
        ? AbortSignal.any([request.signal, timeout])
        : timeout,
      redirect: "error",
    },
  );
  if (!response.ok)
    throw new AppError(
      `Proxy management returned HTTP ${response.status}. ${response.status === 429 ? "Wait a moment and try again." : "Check the profile is running and try again."}`,
      502,
    );
  if (response.status === 204) return {};
  return responseJson(response);
};
/** Lifecycle periods in milliseconds. */
export interface CoreTiming {
  /** Overall bound on a new core answering its management API. */
  ready: number;
  kill: number;
}
export const coreTiming: CoreTiming = { ready: 15_000, kill: killGraceMs };
export interface CorePoolOptions {
  /** How management calls reach a core; tests answer some of them. */
  management?: Management;
  timing?: CoreTiming;
  /** Runs once a new core answers, before start resolves; a failure retries until the deadline. */
  ready?: (profileId: string, signal?: AbortSignal) => Promise<void>;
  beforeStop?: (profileId: string) => Promise<void>;
  /** A core became running, failed, exited, or applied new settings. */
  onStatusChange?: () => void;
}
/** The process exists and has not exited; a failed spawn has no PID and may never emit exit. */
function alive(child: ChildProcess): boolean {
  return (
    child.pid !== undefined &&
    child.exitCode === null &&
    child.signalCode === null
  );
}
async function unusedPort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!(address instanceof Object))
    throw new Error("Could not allocate core port.");
  await new Promise<void>((resolve, reject) =>
    server.close((e) => (e ? reject(e) : resolve())),
  );
  return address.port;
}
export class CorePool {
  readonly store: Store;
  readonly binary: string;
  private readonly processes = new Map<string, CoreRuntime>();
  /** Each profile's current core. A restart makes a new record, so an unchanged record means the same core. */
  readonly runtimes: ReadonlyMap<string, Readonly<CoreRuntime>> =
    this.processes;
  private readonly starts = new Map<string, Promise<void>>();
  readonly starting: ReadonlyMap<string, Promise<void>> = this.starts;
  private readonly stops = new Map<string, Promise<void>>();
  /** Profiles whose core is held stopped while its files are replaced. */
  private readonly held = new Set<string>();
  private readonly config: ConfigWriter;
  private closing = false;
  private readonly options: CorePoolOptions;
  private readonly transport: Management;
  private readonly timing: CoreTiming;
  constructor(store: Store, binary: string, options: CorePoolOptions = {}) {
    this.store = store;
    this.binary = binary;
    this.options = options;
    this.transport = options.management ?? httpManagement;
    this.timing = options.timing ?? coreTiming;
    this.config = new ConfigWriter(store);
  }
  /** A pool on another store with this pool's binary, transport and timing, and no features. */
  isolated(store: Store): CorePool {
    const { management, timing } = this.options;
    return new CorePool(store, this.binary, { management, timing });
  }
  async start(profileId: string, signal?: AbortSignal): Promise<void> {
    signal?.throwIfAborted();
    const stopping = this.stops.get(profileId);
    if (stopping) await stopping;
    if (this.closing) throw new AppError("nonstopvibin is shutting down.", 503);
    if (this.held.has(profileId))
      throw new AppError(
        "This profile is reconnecting a subscription. Try again in a moment.",
        409,
      );
    if (this.starts.has(profileId)) return this.starts.get(profileId);
    if (this.running(profileId)) return;
    const operation = this.startProcess(profileId, signal);
    this.starts.set(profileId, operation);
    try {
      await operation;
    } finally {
      this.starts.delete(profileId);
    }
  }
  private async startProcess(
    profileId: string,
    signal?: AbortSignal,
  ): Promise<void> {
    this.store.profile(profileId);
    if (!existsSync(this.binary))
      throw new AppError(
        "The bundled proxy core is missing. Run bun run core:install, then start the profile.",
        503,
      );
    const port = await unusedPort();
    await this.config.write(profileId, port);
    // Do not inherit API keys, proxy settings, storage backends, or arbitrary provider environment variables.
    const env = Object.fromEntries(
      ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "SystemRoot"].flatMap(
        (key) => (process.env[key] ? [[key, process.env[key]!]] : []),
      ),
    );
    signal?.throwIfAborted();
    const child = spawn(
      this.binary,
      coreArguments(this.store.directory, profileId),
      {
        cwd: profileDirectory(this.store.directory, profileId),
        env,
        stdio: ["ignore", "ignore", "ignore"],
      },
    );
    const runtime: CoreRuntime = {
      child,
      port,
      state: "starting",
      stopping: false,
      // Same tick as spawn: the core cannot have been reaped and its PID reused yet.
      watchdog: watchCore(child),
    };
    this.processes.set(profileId, runtime);
    child.once("error", (error) => {
      runtime.state = "error";
      runtime.error = `Could not launch proxy: ${error.message}`;
      this.options.onStatusChange?.();
    });
    child.once("exit", (code, signal) => {
      // The core is gone, so its watchdog (our own child) has nothing left to guard.
      runtime.watchdog?.kill("SIGKILL");
      forgetCore(runtime.ledger);
      runtime.ledger = undefined;
      runtime.state = runtime.stopping ? "stopped" : "error";
      if (!runtime.stopping)
        runtime.error = `Proxy exited (${signal ?? code}). Profiles with subscriptions restart automatically, or use Restart.`;
      this.options.onStatusChange?.();
    });
    try {
      runtime.ledger = await recordCore(this.store.directory, child);
    } catch {
      // The watchdog still guards this core; only the restart backstop is lost.
    }
    let lastError = "";
    const deadline = Date.now() + this.timing.ready;
    while (Date.now() < deadline) {
      signal?.throwIfAborted();
      if (runtime.state === "error") break;
      try {
        await this.management(
          profileId,
          "/config",
          "GET",
          undefined,
          // The clock can pass the deadline after the loop check; a negative timeout throws.
          Math.max(0, deadline - Date.now()),
          signal,
        );
        signal?.throwIfAborted();
        // An answering core has finished its exec: record it if the first read came too early.
        runtime.ledger ??= await recordCore(this.store.directory, child).catch(
          () => undefined,
        );
        runtime.state = "running";
        const profile = this.store.profile(profileId);
        this.store.saveProfile({ ...profile, enabled: true });
        await this.options.ready?.(profileId, signal);
        signal?.throwIfAborted();
        this.options.onStatusChange?.();
        return;
      } catch (error) {
        signal?.throwIfAborted();
        lastError = errorMessage(error);
        await delay(100, undefined, { signal });
      }
    }
    runtime.stopping = true;
    // A later start replaces this runtime entry, so shutdown could no longer reach the child.
    await this.terminate(child);
    runtime.state = "error";
    runtime.error = runtime.error ?? `Proxy did not become ready. ${lastError}`;
    throw new AppError(runtime.error, 503);
  }
  async stop(profileId: string): Promise<void> {
    const starting = this.starts.get(profileId);
    if (starting) await starting.catch(() => undefined);
    let operation = this.stops.get(profileId);
    if (!operation) {
      operation = this.stopProcess(profileId);
      this.stops.set(profileId, operation);
    }
    try {
      await operation;
    } finally {
      if (this.stops.get(profileId) === operation) this.stops.delete(profileId);
    }
  }
  private async stopProcess(profileId: string): Promise<void> {
    const runtime = this.processes.get(profileId);
    // A failed spawn emits error/close, not exit; there is no child to reap.
    if (runtime && alive(runtime.child)) {
      runtime.stopping = true;
      if (this.running(profileId)) await this.options.beforeStop?.(profileId);
      // The process can exit while the final usage request is in flight.
      await this.terminate(runtime.child);
      runtime.state = "stopped";
    }
  }
  private async terminate(child: ChildProcess): Promise<void> {
    if (!alive(child)) return;
    const exited = new Promise<void>((resolve) =>
      child.once("exit", () => resolve()),
    );
    child.kill("SIGTERM");
    const force = setTimeout(() => child.kill("SIGKILL"), this.timing.kill);
    await exited;
    clearTimeout(force);
  }
  /**
   * Stops the core, runs `replace`, then starts the core again if it was running or
   * starting. Nothing can start it in between: its own token refresh could overwrite
   * a file being replaced.
   */
  async whileStopped(
    profileId: string,
    replace: () => Promise<void>,
  ): Promise<void> {
    const wasRunning = this.running(profileId) || this.starts.has(profileId);
    this.held.add(profileId);
    try {
      await this.stop(profileId);
      await replace();
    } finally {
      this.held.delete(profileId);
      if (wasRunning && !this.closing) await this.start(profileId);
    }
  }
  /** A stop or a credential swap is in flight: it decides whether the core runs next. */
  holding(profileId: string): boolean {
    return this.stops.has(profileId) || this.held.has(profileId);
  }
  /** The core answered and has not exited. It may be stopping. */
  running(profileId: string): boolean {
    return this.processes.get(profileId)?.state === "running";
  }
  /** Requests may be sent: the core runs, is not stopping, and the pool is not shutting down. */
  usable(profileId: string): boolean {
    const runtime = this.processes.get(profileId);
    return runtime?.state === "running" && !runtime.stopping && !this.closing;
  }
  /** Whether this profile's core process exists and has not exited. */
  alive(profileId: string): boolean {
    const runtime = this.processes.get(profileId);
    return runtime !== undefined && alive(runtime.child);
  }
  port(profileId: string): number {
    const runtime = this.processes.get(profileId);
    if (!runtime || !this.usable(profileId))
      throw new AppError(
        "This profile is stopped. Start it in nonstopvibin.",
        503,
      );
    return runtime.port;
  }
  /**
   * Rewrites a running core's config and waits until the core has applied it. A core that
   * is starting may have read the settings before this change, so it is applied once ready.
   * If the core stops or is replaced meanwhile, this ends quietly: every start writes the
   * saved settings.
   */
  async configure(profileId: string): Promise<void> {
    // A failed start leaves no core to configure.
    await this.starts.get(profileId)?.catch(() => undefined);
    const runtime = this.processes.get(profileId);
    // Checked right before each write and request. A start can only follow once this core
    // stops, so a write enqueued while it holds lands before the successor's own write.
    const current = () =>
      this.processes.get(profileId) === runtime &&
      this.usable(profileId) &&
      !this.held.has(profileId);
    if (!runtime || !current()) return;
    try {
      let expected = await this.config.write(profileId, runtime.port);
      // The upstream file watcher reloads asynchronously; acknowledge only the applied configuration.
      for (let attempt = 0; attempt < 60; attempt++) {
        if (!current()) return;
        if (applied(expected, await this.management(profileId, "/config"))) {
          this.options.onStatusChange?.();
          return;
        }
        // A core that has only just answered can miss a write for good; another write reaches it.
        if (attempt % 10 === 9) {
          if (!current()) return;
          expected = await this.config.write(profileId, runtime.port);
        }
        await delay(100);
      }
    } catch (error) {
      // A stop marks its core before ending it: a failure from a core that went away is not one.
      if (!current()) return;
      throw error;
    }
    if (!current()) return;
    throw new AppError(
      "The proxy has not applied the settings yet. Stop and start this profile to reload them.",
      503,
    );
  }
  async management(
    profileId: string,
    path: string,
    method = "GET",
    body?: Json,
    timeout = 15_000,
    signal?: AbortSignal,
  ): Promise<Json> {
    const runtime = this.processes.get(profileId);
    if (!runtime) throw new AppError("Start this profile first.", 409);
    return this.transport({
      profileId,
      port: runtime.port,
      key: this.store.managementKey(profileId),
      path,
      method,
      body,
      timeout,
      signal,
    });
  }
  async models(profileId: string): Promise<Model[]> {
    const response = await fetch(
      `http://127.0.0.1:${this.port(profileId)}/v1/models`,
      {
        headers: {
          Authorization: `Bearer ${this.store.coreKey(profileId)}`,
        },
        signal: AbortSignal.timeout(15_000),
      },
    );
    if (!response.ok)
      throw new AppError("Could not load the model catalog.", 502);
    const body = record(await responseJson(response));
    return Array.isArray(body.data)
      ? body.data.flatMap((raw) => {
          const r = record(raw);
          const id = text(r.id);
          return id === undefined ? [] : [{ id, owned_by: text(r.owned_by) }];
        })
      : [];
  }
  async reapOrphans(): Promise<void> {
    await reapOrphanedCores(this.store.directory);
  }
  /** Starts every enabled profile; a profile that fails is reported, not thrown. */
  async restore(report: Report, signal?: AbortSignal): Promise<void> {
    for (const profile of this.store.profiles().filter((p) => p.enabled)) {
      signal?.throwIfAborted();
      try {
        await this.start(profile.id, signal);
      } catch (error) {
        signal?.throwIfAborted();
        report(`${profile.name}: ${errorMessage(error)}`);
      }
    }
  }
  /** From now on nothing starts and no request may be sent to a core. */
  beginShutdown(): void {
    this.closing = true;
  }
  get shuttingDown(): boolean {
    return this.closing;
  }
  /** Shuts down a pool that runs no features, such as a sign-in's. */
  async shutdown(): Promise<void> {
    this.beginShutdown();
    await this.stopAll();
  }
  async stopAll(): Promise<void> {
    // Together, so quitting takes the slowest core's time rather than the sum. Settle
    // every stop, even after a failure, so no drain outlives the caller closing the store.
    const failures = (
      await Promise.allSettled(
        this.store.profiles().map((profile) => this.stop(profile.id)),
      )
    ).flatMap((result) =>
      result.status === "rejected" ? [result.reason] : [],
    );
    if (failures.length > 1)
      throw new AggregateError(
        failures,
        `Profiles did not stop cleanly: ${failures.map(errorMessage).join(" ")}`,
      );
    if (failures.length) throw failures[0];
  }
}
