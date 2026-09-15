import { spawn, type ChildProcess } from "node:child_process";
import {
  mkdir,
  chmod,
  writeFile,
  rename,
  readdir,
  readFile,
  mkdtemp,
  rm,
  lstat,
} from "node:fs/promises";
import { createReadStream, existsSync } from "node:fs";
import { join } from "node:path";
import { createServer } from "node:net";
import {
  createServer as createHttpServer,
  type Server as HttpServer,
} from "node:http";
import { randomUUID, createHash } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import YAML from "yaml";
import type {
  Account,
  Json,
  JsonObject,
  Model,
  OAuthProvider,
  OAuthSession,
  OAuthReview,
  OAuthStatus,
  ProfileState,
  Quota,
  UsageRecord,
} from "../shared/types.ts";
import { oauthProviders, providerLabel } from "../shared/providers.ts";
import { coreConfiguration, type CoreConfiguration } from "./config.ts";
import { AppError, errorMessage } from "./errors.ts";
import { number, parse, record, responseJson, text, unwrap } from "./json.ts";
import { pricingTokens } from "./usage-pricing.ts";
import { resetCreditsSchema, resetResultSchema } from "./reset-credits.ts";
import { parseQuota } from "./quota.ts";
import { Store } from "./store.ts";
import { claudeIdentity, sameSeat, verifyClaudeIdentity } from "./identity.ts";

interface Runtime {
  child: ChildProcess;
  port: number;
  state: ProfileState["runtime"];
  error?: string;
  stopping: boolean;
  accounts: Account[];
  /** ChatGPT account IDs by credential file; Codex quota calls need them. */
  chatgptAccountIds: Map<string, string>;
  lastSyncedAt?: string;
  nextUsageAt: number;
  usageActiveUntil: number;
}
const quotaEndpoints: Record<OAuthProvider, string> = {
  claude: "https://api.anthropic.com/api/oauth/usage",
  codex: "https://chatgpt.com/backend-api/wham/usage",
  kimi: "https://api.kimi.com/coding/v1/usages",
  antigravity:
    "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
  xai: "https://cli-chat-proxy.grok.com/v1/billing",
};
// Sent to the core's /api-call, which substitutes $TOKEN$ and drops undefined values.
type QuotaHeader = {
  Authorization: string;
  "Content-Type": string;
  "anthropic-beta"?: string;
  "User-Agent"?: string;
  "Chatgpt-Account-Id"?: string;
};
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
  readonly runtimes = new Map<string, Runtime>();
  readonly starting = new Map<string, Promise<void>>();
  readonly busy = new Set<string>();
  private configWrites = new Map<string, Promise<CoreConfiguration>>();
  private imports: Promise<unknown> = Promise.resolve();
  private reconnectingProfile?: string;
  oauth: OAuthSession | undefined;
  private oauthActions: Promise<unknown> = Promise.resolve();
  private oauthExpiry?: NodeJS.Timeout;
  private claudeSignIn?: {
    core: CorePool;
    profileId: string;
    credentialId?: string;
    credential?: JsonObject;
    review?: OAuthReview;
  };
  private callbackServer?: HttpServer;
  private polling?: Promise<void>;
  private maintenance?: Promise<void>;
  private stops = new Map<string, Promise<void>>();
  private usageCollections = new Map<string, Promise<number>>();
  onStatusChange?: () => void;
  private shuttingDown = false;
  private schedulePolling?: () => void;
  errors: string[] = [];
  timer?: NodeJS.Timeout;
  constructor(store: Store, binary: string) {
    this.store = store;
    this.binary = binary;
  }
  directory(profileId: string): string {
    this.store.profile(profileId);
    return join(this.store.directory, "profiles", profileId);
  }
  async writeConfig(
    profileId: string,
    port: number,
  ): Promise<CoreConfiguration> {
    const previous = this.configWrites.get(profileId) ?? Promise.resolve();
    const operation = previous
      .catch(() => undefined)
      .then(() => this.writeConfigFile(profileId, port));
    this.configWrites.set(profileId, operation);
    try {
      return await operation;
    } finally {
      if (this.configWrites.get(profileId) === operation)
        this.configWrites.delete(profileId);
    }
  }
  private async writeConfigFile(
    profileId: string,
    port: number,
  ): Promise<CoreConfiguration> {
    const directory = this.directory(profileId);
    const authDir = join(directory, "auth");
    await mkdir(authDir, { recursive: true, mode: 0o700 });
    await chmod(directory, 0o700);
    await chmod(authDir, 0o700);
    const config = coreConfiguration(
      this.store.profile(profileId),
      port,
      authDir,
      {
        core: this.store.secret(`${profileId}:core`),
        management: this.store.secret(`${profileId}:management`),
      },
      this.store.apiAccounts(profileId),
      (id) => this.store.secret(`${id}:api`),
    );
    const path = join(directory, "config.yaml");
    // The core watches this inode; atomic replacement drops its Linux watch.
    // This derived config is regenerated from SQLite before every core start.
    await writeFile(path, YAML.stringify(config), { mode: 0o600 });
    await chmod(path, 0o600);
    return config;
  }
  async start(profileId: string): Promise<void> {
    const stopping = this.stops.get(profileId);
    if (stopping) await stopping;
    if (this.shuttingDown)
      throw new AppError("nonstopvibin is shutting down.", 503);
    if (this.reconnectingProfile === profileId)
      throw new AppError(
        "This profile is reconnecting a subscription. Try again in a moment.",
        409,
      );
    if (this.starting.has(profileId)) return this.starting.get(profileId);
    if (this.runtimes.get(profileId)?.state === "running") return;
    const operation = this.startProcess(profileId);
    this.starting.set(profileId, operation);
    try {
      await operation;
    } finally {
      this.starting.delete(profileId);
    }
  }
  private async startProcess(profileId: string): Promise<void> {
    this.store.profile(profileId);
    if (!existsSync(this.binary))
      throw new AppError(
        "The bundled proxy core is missing. Run bun run core:install, then start the profile.",
        503,
      );
    const port = await unusedPort();
    await this.writeConfig(profileId, port);
    const directory = this.directory(profileId);
    // Do not inherit API keys, proxy settings, storage backends, or arbitrary provider environment variables.
    const env = Object.fromEntries(
      ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "SystemRoot"].flatMap(
        (key) => (process.env[key] ? [[key, process.env[key]!]] : []),
      ),
    );
    const child = spawn(
      this.binary,
      ["--config", join(directory, "config.yaml"), "--local-model"],
      { cwd: directory, env, stdio: ["ignore", "ignore", "ignore"] },
    );
    const runtime: Runtime = {
      child,
      port,
      state: "starting",
      stopping: false,
      accounts: [],
      chatgptAccountIds: new Map(),
      nextUsageAt: 0,
      usageActiveUntil: 0,
    };
    this.runtimes.set(profileId, runtime);
    child.once("error", (error) => {
      runtime.state = "error";
      runtime.error = `Could not launch proxy: ${error.message}`;
      this.onStatusChange?.();
    });
    child.once("exit", (code, signal) => {
      runtime.state = runtime.stopping ? "stopped" : "error";
      if (!runtime.stopping)
        runtime.error = `Proxy exited (${signal ?? code}). Restart this profile to reconnect.`;
      this.onStatusChange?.();
    });
    let lastError = "";
    for (let attempt = 0; attempt < 80; attempt++) {
      if (runtime.state === "error") break;
      try {
        await this.management(profileId, "/config");
        runtime.state = "running";
        this.schedulePolling?.();
        const profile = this.store.profile(profileId);
        this.store.saveProfile({ ...profile, enabled: true });
        await this.syncAccounts(profileId);
        this.onStatusChange?.();
        return;
      } catch (error) {
        lastError = errorMessage(error);
        await delay(100);
      }
    }
    runtime.stopping = true;
    child.kill("SIGTERM");
    runtime.state = "error";
    runtime.error = runtime.error ?? `Proxy did not become ready. ${lastError}`;
    throw new AppError(runtime.error, 503);
  }
  async stop(profileId: string, persist = true): Promise<void> {
    if (persist && this.reconnectingProfile === profileId)
      throw new AppError(
        "This profile is reconnecting a subscription. Try again in a moment.",
        409,
      );
    const starting = this.starting.get(profileId);
    if (starting) await starting.catch(() => undefined);
    let operation = this.stops.get(profileId);
    if (!operation) {
      operation = this.stopProcess(profileId);
      this.stops.set(profileId, operation);
    }
    try {
      await operation;
      if (persist)
        this.store.saveProfile({
          ...this.store.profile(profileId),
          enabled: false,
        });
    } finally {
      if (this.stops.get(profileId) === operation) this.stops.delete(profileId);
    }
  }
  private async stopProcess(profileId: string): Promise<void> {
    const runtime = this.runtimes.get(profileId);
    if (
      runtime &&
      runtime.child.exitCode === null &&
      runtime.child.signalCode === null
    ) {
      runtime.stopping = true;
      if (runtime.state === "running") {
        try {
          while ((await this.collectUsage(profileId)) === 500) {
            // Full destructive queue pages must be persisted before terminating the core.
          }
        } catch (error) {
          this.report(`Usage before stop: ${errorMessage(error)}`);
        }
      }
      // The process can exit while the final usage request is in flight.
      if (
        runtime.child.exitCode === null &&
        runtime.child.signalCode === null
      ) {
        const exited = new Promise<void>((resolve) =>
          runtime.child.once("exit", () => resolve()),
        );
        runtime.child.kill("SIGTERM");
        const force = setTimeout(() => runtime.child.kill("SIGKILL"), 5000);
        await exited;
        clearTimeout(force);
      }
      runtime.state = "stopped";
    }
  }
  port(profileId: string): number {
    const runtime = this.runtimes.get(profileId);
    if (
      !runtime ||
      runtime.state !== "running" ||
      runtime.stopping ||
      this.shuttingDown
    )
      throw new AppError(
        "This profile is stopped. Start it in nonstopvibin.",
        503,
      );
    return runtime.port;
  }
  async configure(profileId: string): Promise<void> {
    const runtime = this.runtimes.get(profileId);
    if (runtime?.state !== "running") return;
    const expected = await this.writeConfig(profileId, runtime.port);
    // The upstream file watcher reloads asynchronously; acknowledge only the applied configuration.
    const fields = [
      "routing",
      "openai-compatibility",
      "claude-api-key",
      "codex-api-key",
    ] as const;
    // The Go core omits zero values, so "", false, 0, and [] read back as absent.
    const matches = (
      expected: Json | undefined,
      actual: Json | undefined,
    ): boolean => {
      if (Array.isArray(expected))
        return expected.length === 0
          ? actual == null || (Array.isArray(actual) && actual.length === 0)
          : Array.isArray(actual) &&
              expected.length === actual.length &&
              expected.every((value, index) => matches(value, actual[index]));
      if (expected instanceof Object)
        return Object.entries(expected).every(([key, value]) =>
          matches(value, record(actual)[key]),
        );
      return (
        expected === actual ||
        ((expected === "" || expected === false || expected === 0) &&
          actual === undefined)
      );
    };
    for (let attempt = 0; attempt < 60; attempt++) {
      const actual = record(await this.management(profileId, "/config"));
      if (fields.every((key) => matches(expected[key], actual[key]))) {
        this.onStatusChange?.();
        return;
      }
      await delay(100);
    }
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
  ): Promise<Json> {
    const runtime = this.runtimes.get(profileId);
    if (!runtime) throw new AppError("Start this profile first.", 409);
    const headers = new Headers({
      Authorization: `Bearer ${this.store.secret(`${profileId}:management`)}`,
    });
    if (body !== undefined) headers.set("Content-Type", "application/json");
    const response = await fetch(
      `http://127.0.0.1:${runtime.port}/v0/management${path}`,
      {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(15_000),
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
  }
  async syncAccounts(profileId: string): Promise<void> {
    const runtime = this.runtimes.get(profileId);
    if (
      !runtime ||
      runtime.state !== "running" ||
      runtime.stopping ||
      this.shuttingDown
    )
      return;
    const response = record(await this.management(profileId, "/auth-files"));
    if (!Array.isArray(response.files))
      throw new AppError("The proxy returned an unexpected account list.", 502);
    const identities = new Map<string, ReturnType<typeof claudeIdentity>>();
    for (const value of response.files) {
      const file = record(value);
      const name = text(file.name);
      if (
        (file.provider ?? file.type) !== "claude" ||
        file.runtime_only ||
        !name
      )
        continue;
      try {
        identities.set(
          name,
          claudeIdentity(await this.readCredential(profileId, name)),
        );
      } catch {
        // Display enrichment is optional: refresh can replace a file during a read.
        // The row explicitly shows unverified identity until a later sync succeeds.
        identities.set(name, {});
      }
    }
    const dir = join(this.directory(profileId), "auth");
    for (const entry of await readdir(dir, { withFileTypes: true }))
      if (entry.isFile()) await chmod(join(dir, entry.name), 0o600);
    runtime.chatgptAccountIds.clear();
    runtime.accounts = response.files.flatMap((value) => {
      const file = record(value);
      if (file.runtime_only || file.account_type === "api_key") return [];
      const id = String(file.name ?? file.id ?? "");
      if (!id) return [];
      const provider = String(file.provider ?? file.type ?? "unknown");
      const chatgptAccountId = text(record(file.id_token).chatgpt_account_id);
      if (chatgptAccountId) runtime.chatgptAccountIds.set(id, chatgptAccountId);
      const account: Account = {
        id,
        name: String(file.note || providerLabel(provider)),
        ...identities.get(id),
        provider,
        kind: "oauth",
        email: text(file.email),
        disabled: file.disabled === true,
        status: file.disabled
          ? "Paused"
          : file.unavailable
            ? "Cooling down"
            : file.status === "error"
              ? "Needs attention"
              : "Ready",
        priority: number(file.priority) ?? 0,
        authIndex: String(file.auth_index ?? ""),
      };
      return [account];
    });
    runtime.lastSyncedAt = new Date().toISOString();
    this.store.saveAccountMetadata(profileId, runtime.accounts);
  }
  accounts(profileId: string): Account[] {
    const oauth =
      this.runtimes.get(profileId)?.accounts ??
      this.store.accountMetadata(profileId);
    const api: Account[] = this.store.apiAccounts(profileId).map((a) => ({
      id: a.id,
      name: a.name,
      provider: a.provider,
      kind: "api-key",
      disabled: a.disabled,
      status: a.disabled ? "Paused" : "Configured",
      priority: 0,
      prefix: a.prefix,
      modelCount: a.models.length,
    }));
    return [...oauth, ...api].map((account) => ({
      ...account,
      quota: this.store.quota(profileId, account.id),
    }));
  }
  async models(profileId: string): Promise<Model[]> {
    const response = await fetch(
      `http://127.0.0.1:${this.port(profileId)}/v1/models`,
      {
        headers: {
          Authorization: `Bearer ${this.store.secret(`${profileId}:core`)}`,
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
  private serializeOAuth<T>(action: () => Promise<T>): Promise<T> {
    const operation = this.oauthActions.catch(() => undefined).then(action);
    this.oauthActions = operation;
    return operation;
  }
  beginOAuth(
    profileId: string,
    provider: OAuthProvider,
    reconnectAccountId?: string,
  ): Promise<OAuthSession> {
    return this.serializeOAuth(async () => {
      if (this.shuttingDown)
        throw new AppError("nonstopvibin is shutting down.", 503);
      this.store.profile(profileId);
      if (
        this.oauth &&
        Date.now() < (this.oauth.expiresAt ?? this.oauth.startedAt + 300_000)
      )
        throw new AppError(
          "Finish or cancel the current sign-in before starting another.",
          409,
        );
      await this.clearOAuth();
      if (reconnectAccountId) {
        const account = this.accounts(profileId).find(
          (a) => a.id === reconnectAccountId,
        );
        if (
          provider !== "claude" ||
          account?.provider !== "claude" ||
          account.kind !== "oauth"
        )
          throw new AppError(
            "Choose a Claude subscription in this profile to reconnect.",
            404,
          );
      }
      try {
        if (provider === "claude") {
          const parent = join(this.store.directory, "oauth-pending");
          await mkdir(parent, { recursive: true, mode: 0o700 });
          const directory = await mkdtemp(join(parent, "claude-"));
          const store = new Store(directory, this.store.codec);
          const core = new CorePool(store, this.binary);
          const pending = store.createProfile("Pending sign-in", "forest");
          this.claudeSignIn = { core, profileId: pending.id };
          const session = await core.beginDirectOAuth(pending.id, provider);
          this.oauth = { ...session, profileId, reconnectAccountId };
        } else {
          this.oauth = await this.beginDirectOAuth(profileId, provider);
        }
        const session = this.oauth;
        this.oauthExpiry = setTimeout(
          () => {
            void this.serializeOAuth(async () => {
              if (this.oauth === session) await this.clearOAuth();
            }).catch(() =>
              this.report(
                "Could not clean up an expired sign-in. Restart the app.",
              ),
            );
          },
          Math.max(
            1,
            (session.expiresAt ?? session.startedAt + 300_000) - Date.now(),
          ),
        );
        this.oauthExpiry.unref();
        return session;
      } catch (error) {
        await this.clearOAuth();
        throw error;
      }
    });
  }
  private async beginDirectOAuth(
    profileId: string,
    provider: OAuthProvider,
  ): Promise<OAuthSession> {
    await this.start(profileId);
    const path = provider === "claude" ? "anthropic" : provider;
    const response = record(
      await this.management(profileId, `/${path}-auth-url`),
    );
    const url = text(response.url);
    const state = text(response.state);
    if (url === undefined || state === undefined)
      throw new AppError("The provider did not return a sign-in link.", 502);
    const session: OAuthSession = {
      profileId,
      provider,
      url,
      state,
      startedAt: Date.now(),
      userCode: text(response.user_code),
      expiresAt:
        Date.now() + Math.max(1, number(response.expires_in) ?? 300) * 1000,
    };
    const redirect = new URL(url).searchParams.get("redirect_uri");
    if (redirect) {
      const callback = new URL(redirect);
      if (
        callback.protocol !== "http:" ||
        !["localhost", "127.0.0.1"].includes(callback.hostname) ||
        !callback.port
      )
        throw new AppError(
          "The provider returned an unsupported callback address.",
          502,
        );
      this.closeCallback();
      const server = createHttpServer((req, res) => {
        let incoming: URL;
        try {
          incoming = new URL(req.url ?? "/", callback.origin);
        } catch {
          res.writeHead(400, { "Cache-Control": "no-store" });
          res.end("Malformed sign-in callback.");
          return;
        }
        if (
          req.method !== "GET" ||
          incoming.pathname !== callback.pathname ||
          incoming.searchParams.get("state") !== session.state
        ) {
          res.writeHead(400);
          res.end("This sign-in callback does not match the pending session.");
          return;
        }
        void this.oauthCallback(profileId, incoming.href).then(
          () => {
            res.writeHead(200, {
              "Content-Type": "text/html; charset=utf-8",
              "Cache-Control": "no-store",
              "Content-Security-Policy":
                "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
            });
            res.end(
              '<!doctype html><title>nonstopvibin</title><body style="font:16px system-ui;padding:64px;background:#fbfcf9;color:#275b42"><h1>Sign-in received.</h1><p>Return to nonstopvibin to finish connecting your subscription. You can close this tab.</p>',
            );
            this.closeCallback(false);
          },
          () => {
            res.writeHead(400);
            res.end(
              "Sign-in could not be completed. Return to nonstopvibin and start again.",
            );
          },
        );
      });
      server.requestTimeout = 10_000;
      server.headersTimeout = 10_000;
      try {
        await new Promise<void>((resolve, reject) => {
          server.once("error", reject);
          server.listen(Number(callback.port), "127.0.0.1", resolve);
        });
      } catch {
        await this.management(
          profileId,
          `/oauth-session?state=${encodeURIComponent(session.state)}`,
          "DELETE",
        );
        throw new AppError(
          `Sign-in port ${callback.port} is in use. Close any pending sign-in in another app, then try again.`,
          409,
        );
      }
      this.callbackServer = server;
    }
    this.oauth = session;
    return session;
  }
  private requireOAuth(profileId: string, state: string): OAuthSession {
    if (
      !this.oauth ||
      this.oauth.profileId !== profileId ||
      this.oauth.state !== state
    )
      throw new AppError("Sign-in session not found for this profile.", 404);
    if (Date.now() >= (this.oauth.expiresAt ?? this.oauth.startedAt + 300_000))
      throw new AppError("This sign-in expired. Start a new sign-in.", 410);
    return this.oauth;
  }
  cancelOAuth(profileId: string, state: string): Promise<void> {
    return this.serializeOAuth(async () => {
      this.requireOAuth(profileId, state);
      await this.clearOAuth();
    });
  }
  private async clearOAuth(): Promise<void> {
    clearTimeout(this.oauthExpiry);
    const pending = this.claudeSignIn;
    if (pending) {
      await pending.core.shutdown();
      pending.core.store.close();
      await rm(pending.core.store.directory, { recursive: true, force: true });
      this.claudeSignIn = undefined;
    } else if (this.oauth) {
      try {
        await this.management(
          this.oauth.profileId,
          `/oauth-session?state=${encodeURIComponent(this.oauth.state)}`,
          "DELETE",
        );
      } catch {
        // A stopped or unreachable core cannot acknowledge cancellation. Stop it
        // locally so it cannot complete this authorization after cleanup.
        await this.stop(this.oauth.profileId, false);
      }
    }
    this.closeCallback();
    this.oauth = undefined;
  }
  private closeCallback(force = true): void {
    const server = this.callbackServer;
    this.callbackServer = undefined;
    if (!server) return;
    // Stop accepting callbacks now. Do not wait for handlers queued behind this OAuth action.
    server.close();
    if (force) server.closeAllConnections();
    else server.closeIdleConnections();
  }
  oauthStatus(profileId: string, state: string): Promise<OAuthStatus> {
    return this.serializeOAuth(async () => {
      const session = this.requireOAuth(profileId, state);
      const pending = this.claudeSignIn;
      if (pending) {
        try {
          if (!pending.credentialId) {
            const result = await pending.core.oauthStatus(
              pending.profileId,
              state,
            );
            if (result.status === "wait") return result;
            if (result.status === "error") {
              await this.clearOAuth();
              return result;
            }
            const accounts = pending.core.accounts(pending.profileId);
            if (accounts.length !== 1)
              throw new AppError(
                "Claude did not return one subscription. Sign in again.",
                502,
              );
            pending.credentialId = accounts[0]!.id;
          }
          if (!pending.credential) {
            // Stop refresh before reading and verifying the token that will be transferred.
            await pending.core.stop(pending.profileId, false);
            pending.credential = await pending.core.readCredential(
              pending.profileId,
              pending.credentialId,
            );
          }
          if (!pending.review) {
            const identity = await verifyClaudeIdentity(pending.credential);
            const account: Account = {
              id: "pending",
              provider: "claude",
              kind: "oauth",
              name: "Claude",
              ...identity,
              email: text(pending.credential.email),
              disabled: false,
              status: "Ready",
              priority: 0,
            };
            const match = await this.matchClaudeSeat(
              profileId,
              pending.credential,
              session.reconnectAccountId,
            );
            pending.review = { account, ...match };
          }
          return { status: "review", review: pending.review };
        } catch (error) {
          // Retain the isolated credential so a temporary profile-service failure can be retried.
          throw error instanceof AppError
            ? error
            : new AppError(
                "Could not finish verifying this subscription. Try again.",
                502,
              );
        }
      }
      const result = record(
        await this.management(
          profileId,
          `/get-auth-status?state=${encodeURIComponent(state)}`,
        ),
      );
      if (result.status === "ok" || result.status === "error") {
        this.oauth = undefined;
        clearTimeout(this.oauthExpiry);
        this.closeCallback();
        await this.syncAccounts(profileId);
        if (result.status === "ok" && session.provider !== "claude")
          for (const account of this.accounts(profileId))
            if (!account.disabled && account.provider === session.provider)
              await this.refreshQuota(profileId, account.id);
        return result.status === "ok"
          ? { status: "ok" }
          : {
              status: "error",
              error: "Sign-in did not complete. Start a new sign-in.",
            };
      }
      return { status: "wait" };
    });
  }
  confirmOAuth(profileId: string, state: string): Promise<OAuthStatus> {
    return this.serializeOAuth(async () => {
      const session = this.requireOAuth(profileId, state);
      const pending = this.claudeSignIn;
      if (!pending?.credential || !pending.review)
        throw new AppError(
          "Wait for Claude to verify the organization before connecting.",
          409,
        );
      const credential = pending.credential;
      const result = await this.queueImport(async () => {
        const match = await this.matchClaudeSeat(
          profileId,
          credential,
          session.reconnectAccountId,
        );
        if (match.action === "blocked") throw new AppError(match.message!, 409);
        const account = await this.importCredential(
          profileId,
          credential,
          match.accountId,
        );
        return {
          status: "ok" as const,
          account,
          reconnected: match.action === "reconnect",
        };
      });
      await this.clearOAuth();
      return result;
    });
  }
  oauthCallback(profileId: string, redirectUrl: string): Promise<void> {
    return this.serializeOAuth(async () => {
      const url = new URL(redirectUrl);
      const session = this.requireOAuth(
        profileId,
        url.searchParams.get("state") ?? "",
      );
      if (this.claudeSignIn) {
        await this.claudeSignIn.core.oauthCallback(
          this.claudeSignIn.profileId,
          redirectUrl,
        );
        return;
      }
      await this.management(profileId, "/oauth-callback", "POST", {
        provider:
          session.provider === "claude" ? "anthropic" : session.provider,
        redirect_url: redirectUrl,
      });
    });
  }
  private async readCredential(
    profileId: string,
    name: string,
  ): Promise<JsonObject> {
    if (
      !name.endsWith(".json") ||
      name.includes("/") ||
      name.includes("\\") ||
      name === ".json"
    )
      throw new AppError("Unsupported credential filename.");
    const path = join(this.directory(profileId), "auth", name);
    const file = await lstat(path);
    if (!file.isFile() || file.size > 2_000_000)
      throw new AppError("The credential is not a supported regular file.");
    try {
      return record(parse(await readFile(path, "utf8")));
    } catch {
      throw new AppError(
        "A saved credential could not be read. Reconnect that subscription.",
      );
    }
  }
  private async credentialFiles(profileId: string): Promise<
    Array<{
      profileId: string;
      profileName: string;
      id: string;
      raw: JsonObject;
    }>
  > {
    const files = [];
    const profile = this.store.profile(profileId);
    const directory = join(this.directory(profile.id), "auth");
    if (!existsSync(directory)) return [];
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      if (!entry.isFile() || !entry.name.endsWith(".json")) continue;
      files.push({
        profileId: profile.id,
        profileName: profile.name,
        id: entry.name,
        raw: await this.readCredential(profile.id, entry.name),
      });
    }
    return files;
  }
  private async matchClaudeSeat(
    profileId: string,
    credential: JsonObject,
    reconnectAccountId?: string,
  ): Promise<
    Pick<OAuthReview, "action" | "message" | "existingProfileId"> & {
      accountId?: string;
    }
  > {
    const identity = claudeIdentity(credential);
    const files = await this.credentialFiles(profileId);
    if (reconnectAccountId) {
      const target = files.find(
        (f) =>
          f.profileId === profileId &&
          f.id === reconnectAccountId &&
          f.raw.type === "claude",
      );
      if (!target)
        return {
          action: "blocked",
          message: "This subscription was removed. Start a new connection.",
        };
      if (!sameSeat(identity, claudeIdentity(target.raw)))
        return {
          action: "blocked",
          message:
            "This is not the organization and account saved for this subscription, or its old identity is unavailable. Choose the original organization, or connect this seat separately.",
        };
    }
    const matches = files.filter(
      (f) =>
        f.raw.type === "claude" && sameSeat(identity, claudeIdentity(f.raw)),
    );
    if (matches.length > 1)
      return {
        action: "blocked",
        message:
          "This profile already has multiple copies of this seat. Remove the extra copies before reconnecting.",
      };
    const existing = matches[0];
    return existing
      ? { action: "reconnect", accountId: existing.id }
      : { action: "add" };
  }
  // ponytail: one credential mutation at a time; keyed locks only if large account pools make this a bottleneck.
  private queueImport<T>(action: () => Promise<T>): Promise<T> {
    const operation = this.imports.catch(() => undefined).then(action);
    this.imports = operation;
    return operation;
  }
  importAuth(profileId: string, contents: JsonObject): Promise<Account> {
    return this.queueImport(() => this.importCredential(profileId, contents));
  }
  private async importCredential(
    profileId: string,
    raw: JsonObject,
    reconnectAccountId?: string,
  ): Promise<Account> {
    const provider = String(raw.type ?? "");
    const accessToken = text(raw.access_token);
    const refreshToken = text(raw.refresh_token);
    if (
      !["codex", "claude", "antigravity", "kimi", "xai", "gemini"].includes(
        provider,
      ) ||
      (accessToken === undefined && !(raw.token instanceof Object))
    )
      throw new AppError(
        "Choose a CLIProxyAPI OAuth JSON file. It must contain a supported type and token.",
      );
    const files = await this.credentialFiles(profileId);
    const prior = reconnectAccountId
      ? files.find(
          (f) => f.profileId === profileId && f.id === reconnectAccountId,
        )
      : undefined;
    if (reconnectAccountId && !prior)
      throw new AppError(
        "The subscription to reconnect no longer exists.",
        409,
      );
    const identity = provider === "claude" ? claudeIdentity(raw) : undefined;
    for (const existing of files) {
      if (existing === prior) continue;
      if (
        existing.raw.type === provider &&
        ((refreshToken && existing.raw.refresh_token === refreshToken) ||
          (accessToken && existing.raw.access_token === accessToken) ||
          (identity && sameSeat(identity, claudeIdentity(existing.raw))))
      )
        throw new AppError(
          `This subscription is already connected to ${existing.profileName}. Use its existing connection instead of importing another copy.`,
          409,
        );
    }
    const name = reconnectAccountId ?? `${provider}-${randomUUID()}.json`;
    // Reconnecting keeps the label, priority, and prefix the user set on the old file.
    const payload: JsonObject = {
      ...raw,
      disabled: prior ? prior.raw.disabled === true : raw.disabled === true,
      ...(prior && {
        note: prior.raw.note,
        priority: prior.raw.priority,
        prefix: prior.raw.prefix,
      }),
    };
    if (prior) {
      const runtime = this.runtimes.get(profileId);
      const wasRunning =
        runtime?.state === "running" || this.starting.has(profileId);
      const path = join(this.directory(profileId), "auth", name);
      const temporary = `${path}.${randomUUID()}.tmp`;
      this.reconnectingProfile = profileId;
      try {
        // An in-flight core refresh can overwrite a management upload's tokens.
        // Stop the writer before swapping the file; keep its path for stable accounting.
        await this.stop(profileId, false);
        await writeFile(temporary, JSON.stringify(payload), {
          mode: 0o600,
          flag: "wx",
        });
        await rename(temporary, path);
        const accounts = this.store.accountMetadata(profileId).map((account) =>
          account.id === name
            ? {
                ...account,
                ...claudeIdentity(payload),
                email: text(raw.email) ?? account.email,
              }
            : account,
        );
        this.store.saveAccountMetadata(profileId, accounts);
        if (runtime) runtime.accounts = accounts;
      } catch (error) {
        await rm(temporary, { force: true });
        throw error;
      } finally {
        this.reconnectingProfile = undefined;
        if (wasRunning && !this.shuttingDown) await this.start(profileId);
      }
    } else {
      await this.start(profileId);
      await this.management(
        profileId,
        `/auth-files?name=${encodeURIComponent(name)}`,
        "POST",
        payload,
      );
      await this.syncAccounts(profileId);
    }
    const account = this.accounts(profileId).find((a) => a.id === name);
    if (!account)
      throw new AppError(
        "The proxy has not registered this subscription yet. Refresh the profile.",
        503,
      );
    if (!account.disabled)
      account.quota = await this.refreshQuota(profileId, account.id);
    return account;
  }
  setAccount(
    profileId: string,
    accountId: string,
    patch: { disabled?: boolean; priority?: number; name?: string },
  ): Promise<void> {
    return this.queueImport(async () => {
      const api = this.store
        .apiAccounts(profileId)
        .find((a) => a.id === accountId);
      if (api) {
        this.store.saveApiAccount(profileId, {
          ...api,
          disabled: patch.disabled ?? api.disabled,
          name: patch.name || api.name,
        });
        await this.configure(profileId);
        return;
      }
      const account = this.accounts(profileId).find((a) => a.id === accountId);
      if (!account)
        throw new AppError("Account not found in this profile.", 404);
      if (patch.disabled !== undefined)
        await this.management(profileId, "/auth-files/status", "PATCH", {
          name: account.id,
          disabled: patch.disabled,
        });
      if (patch.priority !== undefined || patch.name !== undefined)
        // JSON.stringify drops undefined, so untouched fields are not sent.
        await this.management(profileId, "/auth-files/fields", "PATCH", {
          name: account.id,
          priority: patch.priority,
          note: patch.name,
        });
      await this.syncAccounts(profileId);
    });
  }
  removeAccount(profileId: string, accountId: string): Promise<void> {
    return this.queueImport(async () => {
      const account = this.accounts(profileId).find((a) => a.id === accountId);
      if (!account)
        throw new AppError("Account not found in this profile.", 404);
      if (account.kind === "api-key") {
        this.store.removeApiAccount(profileId, accountId);
        await this.configure(profileId);
      } else {
        await this.start(profileId);
        await this.management(
          profileId,
          `/auth-files?name=${encodeURIComponent(accountId)}`,
          "DELETE",
        );
        await this.syncAccounts(profileId);
      }
    });
  }
  private codexResetAccount(profileId: string, accountId: string) {
    const runtime = this.runtimes.get(profileId);
    if (!runtime || runtime.state !== "running" || runtime.stopping)
      throw new AppError("Start this profile first.", 409);
    const account = runtime.accounts.find((a) => a.id === accountId);
    if (!account) throw new AppError("Account not found in this profile.", 404);
    if (account.provider !== "codex" || account.kind !== "oauth")
      throw new AppError("Banked resets require a Codex subscription.");
    const identity = runtime.chatgptAccountIds.get(accountId);
    if (!identity || !account.authIndex)
      throw new AppError(
        "Reconnect this subscription to verify its account identity.",
        409,
      );
    return { account, identity };
  }
  private async codexResetCall(
    profileId: string,
    accountId: string,
    creditId?: string,
  ) {
    const { account, identity } = this.codexResetAccount(profileId, accountId);
    const response = record(
      await this.management(profileId, "/api-call", "POST", {
        auth_index: account.authIndex,
        method: creditId ? "POST" : "GET",
        url: `https://chatgpt.com/backend-api/wham/rate-limit-reset-credits${creditId ? "/consume" : ""}`,
        header: {
          Authorization: "Bearer $TOKEN$",
          "Content-Type": "application/json",
          "Chatgpt-Account-Id": identity,
          "User-Agent": "nonstopvibin/0.1.1",
        },
        data: creditId
          ? JSON.stringify({
              credit_id: creditId,
              redeem_request_id: this.store.resetRequestId(identity, creditId),
            })
          : undefined,
      }),
    );
    if (number(response.status_code) !== 200)
      throw new AppError(
        `Codex returned HTTP ${number(response.status_code) ?? "unknown"}. Retry the same reset; reconnect if authorization has expired.`,
        502,
      );
    try {
      return unwrap(response.body);
    } catch {
      throw new AppError(
        "Codex returned an unreadable reset response. Retry the same reset.",
        502,
      );
    }
  }
  async resetCredits(profileId: string, accountId: string) {
    const result = resetCreditsSchema.safeParse(
      await this.codexResetCall(profileId, accountId),
    );
    if (!result.success)
      throw new AppError("Codex reset information is unavailable.", 502);
    return result.data;
  }
  async consumeResetCredit(
    profileId: string,
    accountId: string,
    creditId: string,
  ) {
    const { identity } = this.codexResetAccount(profileId, accountId);
    const key = `codex-reset:${identity}`;
    if (this.busy.has(key))
      throw new AppError(
        "A reset is already in progress for this subscription.",
        409,
      );
    this.busy.add(key);
    try {
      const result = resetResultSchema.safeParse(
        await this.codexResetCall(profileId, accountId, creditId),
      );
      if (!result.success)
        throw new AppError(
          "Reset outcome is unknown. Retry the same reset.",
          502,
        );
      // Keep the identity after success or ambiguity, including across app restarts.
      if (
        result.data.code === "nothing_to_reset" ||
        result.data.code === "no_credit"
      )
        this.store.clearResetRequest(identity, creditId);
      let quotaRefreshed = false;
      try {
        quotaRefreshed =
          (await this.refreshQuota(profileId, accountId)).status ===
          "available";
      } catch {
        // Redemption is settled even if a concurrent refresh or account removal wins.
      }
      return { ...result.data, quotaRefreshed };
    } finally {
      this.busy.delete(key);
    }
  }
  async refreshQuota(profileId: string, accountId: string): Promise<Quota> {
    const account = this.accounts(profileId).find((a) => a.id === accountId);
    if (!account) throw new AppError("Account not found.", 404);
    const busyKey = `${profileId}:${accountId}:quota`;
    const prior = this.store.quota(profileId, accountId);
    if (this.busy.has(busyKey) && prior) return prior;
    if (this.busy.has(busyKey))
      throw new AppError("Quota refresh is already in progress.", 409);
    this.busy.add(busyKey);
    let quota: Quota;
    try {
      let payload: Json | undefined;
      if (account.provider === "opencode-go") {
        const response = await fetch("https://opencode.ai/zen/go/v1/usage", {
          headers: {
            Authorization: `Bearer ${this.store.secret(`${accountId}:api`)}`,
            "User-Agent": "nonstopvibin/0.1.1",
          },
          signal: AbortSignal.timeout(15_000),
          redirect: "error",
        });
        if (!response.ok)
          throw new Error(
            `OpenCode Go returned HTTP ${response.status}. Check your API key or try again later.`,
          );
        try {
          payload = await responseJson(response);
        } catch {
          throw new Error("Quota response was not valid JSON.");
        }
      } else {
        const provider = oauthProviders
          .map((p) => p.id)
          .find((id) => id === account.provider);
        if (!provider || account.kind !== "oauth") {
          quota = {
            status: "unavailable",
            windows: [],
            checkedAt: new Date().toISOString(),
          };
          this.store.saveQuota(profileId, accountId, quota);
          return quota;
        }
        const url = quotaEndpoints[provider];
        const header: QuotaHeader = {
          Authorization: "Bearer $TOKEN$",
          "Content-Type": "application/json",
        };
        if (provider === "claude")
          header["anthropic-beta"] = "oauth-2025-04-20";
        if (provider === "codex") {
          header["User-Agent"] = "nonstopvibin/0.1.1";
          header["Chatgpt-Account-Id"] = this.runtimes
            .get(profileId)
            ?.chatgptAccountIds.get(accountId);
        }
        const response = record(
          await this.management(profileId, "/api-call", "POST", {
            auth_index: account.authIndex,
            method: provider === "antigravity" ? "POST" : "GET",
            url,
            header,
            data: provider === "antigravity" ? "{}" : undefined,
          }),
        );
        const status = number(response.status_code);
        if (status !== 200)
          throw new Error(
            `${providerLabel(provider)} returned HTTP ${status ?? "unknown"}. ${status === 401 || status === 403 ? "Reconnect this subscription." : "Try again later."}`,
          );
        payload = response.body;
        if (account.provider === "xai") {
          const weekly = record(
            await this.management(profileId, "/api-call", "POST", {
              auth_index: account.authIndex,
              method: "GET",
              url: `${url}?format=credits`,
              header,
            }),
          );
          if (number(weekly.status_code) !== 200)
            throw new Error("Grok weekly quota check failed. Try again later.");
          try {
            payload = { monthly: unwrap(payload), weekly: unwrap(weekly.body) };
          } catch {
            throw new Error("Quota response was not valid JSON.");
          }
        }
      }
      quota = parseQuota(account.provider, payload);
    } catch (error) {
      quota = {
        status: "error",
        windows: prior?.windows ?? [],
        checkedAt: prior?.checkedAt ?? new Date().toISOString(),
        error: errorMessage(error),
      };
    } finally {
      this.busy.delete(busyKey);
    }
    this.store.saveQuota(profileId, accountId, quota);
    return quota;
  }
  markUsageActivity(profileId: string): void {
    const runtime = this.runtimes.get(profileId);
    if (!runtime) return;
    runtime.usageActiveUntil = Date.now() + 10_000;
    runtime.nextUsageAt = 0;
    this.schedulePolling?.();
  }
  collectUsage(profileId: string): Promise<number> {
    const pending = this.usageCollections.get(profileId);
    if (pending) return pending;
    const collection = (async () => {
      const payload = await this.management(
        profileId,
        "/usage-queue?count=500",
      );
      if (!Array.isArray(payload))
        throw new AppError("Unexpected usage queue format.", 502);
      const records: UsageRecord[] = payload.map((item) => {
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
      });
      // Store only accounting metadata. Never persist API keys, prompt bodies, or upstream error bodies.
      this.store.addUsage(records);
      return records.length;
    })().finally(() => {
      this.usageCollections.delete(profileId);
    });
    this.usageCollections.set(profileId, collection);
    return collection;
  }
  report(message: string): void {
    this.errors = [...this.errors.filter((e) => e !== message), message].slice(
      -5,
    );
  }
  beginPolling(): void {
    clearTimeout(this.timer);
    let nextAccounts = Date.now() + 20_000;
    let nextQuotas = Date.now() + 120_000;
    let nextPrune = Date.now() + 86_400_000;
    let scheduledAt = Infinity;
    const schedule = () => {
      if (this.shuttingDown) return;
      const running = [...this.runtimes.values()].filter(
        (runtime) => runtime.state === "running",
      );
      const due = Math.min(
        this.maintenance ? Infinity : nextPrune,
        this.maintenance || !running.length ? Infinity : nextAccounts,
        ...(this.polling ? [] : running.map((runtime) => runtime.nextUsageAt)),
      );
      // Coalesce active work at two seconds; idle work sleeps until its deadline.
      const at = Math.max(Date.now() + 2000, due);
      if (at >= scheduledAt) return;
      clearTimeout(this.timer);
      scheduledAt = at;
      this.timer = setTimeout(tick, at - Date.now());
      this.timer.unref();
    };
    const tick = () => {
      scheduledAt = Infinity;
      this.timer = undefined;
      if (this.shuttingDown) return;
      if (!this.polling)
        this.polling = (async () => {
          for (const [profileId, runtime] of this.runtimes) {
            if (this.shuttingDown) return;
            if (runtime.state !== "running" || Date.now() < runtime.nextUsageAt)
              continue;
            // Reserve the idle deadline before awaiting: activity during collection
            // must be able to bring the next drain forward again.
            runtime.nextUsageAt = Date.now() + 30_000;
            try {
              const count = await this.collectUsage(profileId);
              if (count === 500 || Date.now() < runtime.usageActiveUntil)
                runtime.nextUsageAt = 0;
            } catch (error) {
              runtime.nextUsageAt = 0;
              this.report(
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
        nextAccounts = Date.now() + 20_000;
        if (quotasDue) nextQuotas = Date.now() + 120_000;
        this.maintenance = (async () => {
          if (Date.now() >= nextPrune) {
            nextPrune = Date.now() + 86_400_000;
            try {
              this.store.pruneUsage();
            } catch (error) {
              this.report(`History cleanup: ${errorMessage(error)}`);
            }
          }
          for (const [profileId, runtime] of this.runtimes) {
            if (this.shuttingDown) return;
            if (runtime.state !== "running") continue;
            try {
              await this.syncAccounts(profileId);
              if (quotasDue)
                for (const account of this.accounts(profileId)) {
                  if (this.shuttingDown || runtime.state !== "running") break;
                  if (
                    !account.disabled &&
                    account.quota?.status !== "unavailable"
                  )
                    await this.refreshQuota(profileId, account.id);
                }
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
      schedule();
    };
    this.schedulePolling = schedule;
    schedule();
  }
  async restore(): Promise<void> {
    // Pending sign-ins never survive an app restart or enter an active profile.
    await rm(join(this.store.directory, "oauth-pending"), {
      recursive: true,
      force: true,
    });
    for (const profile of this.store.profiles().filter((p) => p.enabled)) {
      try {
        await this.start(profile.id);
      } catch (error) {
        this.report(`${profile.name}: ${errorMessage(error)}`);
      }
    }
    this.beginPolling();
  }
  async shutdown(): Promise<void> {
    this.shuttingDown = true;
    clearTimeout(this.timer);
    await this.oauthActions.catch(() => undefined);
    await this.imports.catch(() => undefined);
    try {
      await this.clearOAuth();
    } finally {
      await this.polling;
      await this.maintenance;
      for (const profile of this.store.profiles())
        await this.stop(profile.id, false);
    }
  }
  async verifyBinary(): Promise<void> {
    const manifest = record(
      parse(await readFile(join(this.binary, "..", "manifest.json"), "utf8")),
    );
    const digest = createHash("sha256");
    for await (const chunk of createReadStream(this.binary))
      digest.update(chunk);
    const hash = digest.digest("hex");
    if (manifest.binarySha256 !== hash)
      throw new AppError("The bundled core failed its integrity check.", 503);
  }
}
