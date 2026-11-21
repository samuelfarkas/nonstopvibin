import { mkdir, mkdtemp, readdir, rm } from "node:fs/promises";
import {
  createServer as createHttpServer,
  type Server as HttpServer,
} from "node:http";
import { join } from "node:path";
import type {
  Account,
  OAuthReview,
  OAuthSession,
  OAuthStatus,
} from "../shared/types.ts";
import {
  hasOrganizations,
  providerEntry,
  providerLabel,
  type OAuthProvider,
  type OrganizationProvider,
} from "../shared/providers.ts";
import { Accounts } from "./accounts.ts";
import type { Credentials } from "./credentials.ts";
import type { CorePool } from "./core.ts";
import { reapOrphanedCores } from "./core-processes.ts";
import { oauthPendingDirectory } from "./data-directory.ts";
import { AppError, type Report } from "./errors.ts";
import { claudeIdentity, verifySeat } from "./identity.ts";
import { number, record, text, type JsonObject } from "./json.ts";
import type { Quotas } from "./quota.ts";
import { Store } from "./store.ts";

/**
 * A sign-in for a provider with organizations runs in a throwaway data directory with its
 * own core, so a credential enters a profile only after its organization and seat are reviewed.
 */
interface Sandbox {
  provider: OrganizationProvider;
  pool: CorePool;
  accounts: Accounts;
  profileId: string;
  authorized?: boolean;
  credentialId?: string;
  credential?: JsonObject;
  review?: OAuthReview;
}

export interface SignInOptions {
  /** The clock sessions expire by. */
  now?: () => number;
  report: Report;
  /** A browser sign-in callback was accepted; the desktop can bring the dialog forward. */
  onCallback?: () => void;
}

/** Browser sign-in through a core, one at a time across all profiles. */
export class SignIn {
  session: OAuthSession | undefined;
  private readonly onCallback?: () => void;
  private readonly report: Report;
  private readonly pool: CorePool;
  private readonly accounts: Accounts;
  private readonly credentials: Credentials;
  private readonly quotas: Quotas;
  private readonly now: () => number;
  private actions: Promise<unknown> = Promise.resolve();
  private expiry?: NodeJS.Timeout;
  private server?: HttpServer;
  private sandbox?: Sandbox;
  constructor(
    pool: CorePool,
    accounts: Accounts,
    credentials: Credentials,
    quotas: Quotas,
    options: SignInOptions,
  ) {
    this.pool = pool;
    this.accounts = accounts;
    this.credentials = credentials;
    this.quotas = quotas;
    this.now = options.now ?? Date.now;
    this.report = options.report;
    this.onCallback = options.onCallback;
  }
  /** The loopback server receiving the provider's redirect, while one is open. */
  get callbackServer(): HttpServer | undefined {
    return this.server;
  }
  private serialize<T>(action: () => Promise<T>): Promise<T> {
    const operation = this.actions.catch(() => undefined).then(action);
    this.actions = operation;
    return operation;
  }
  async settled(): Promise<void> {
    await this.actions.catch(() => undefined);
  }
  /**
   * Runs `action` between sign-in actions, and only while no sign-in is pending: one on a
   * profile's own core would not survive that core restarting.
   */
  whileIdle<T>(action: () => Promise<T>): Promise<T> {
    return this.serialize(async () => {
      if (this.session && this.now() < this.session.expiresAt)
        throw new AppError("Finish or cancel the sign-in first.", 409);
      // An expired one is dropped now, while the core that holds it still runs.
      if (this.session) await this.clear();
      return action();
    });
  }
  begin(
    profileId: string,
    provider: OAuthProvider,
    reconnectAccountId?: string,
  ): Promise<OAuthSession> {
    return this.serialize(async () => {
      if (this.pool.shuttingDown)
        throw new AppError("nonstopvibin is shutting down.", 503);
      const store = this.pool.store;
      store.profile(profileId);
      if (this.session && this.now() < this.session.expiresAt)
        throw new AppError(
          "Finish or cancel the current sign-in before starting another.",
          409,
        );
      await this.clear();
      if (reconnectAccountId) {
        const account = this.accounts
          .list(profileId)
          .find((a) => a.id === reconnectAccountId);
        if (
          !providerEntry(provider).reconnect ||
          account?.provider !== provider ||
          account.kind !== "oauth"
        )
          throw new AppError(
            `Choose a ${providerLabel(provider)} subscription in this profile to reconnect.`,
            404,
          );
      }
      try {
        if (hasOrganizations(provider)) {
          const parent = oauthPendingDirectory(store.directory);
          await mkdir(parent, { recursive: true, mode: 0o700 });
          const directory = await mkdtemp(join(parent, `${provider}-`));
          const sandboxStore = new Store(directory, store.codec);
          const pool = this.pool.isolated(sandboxStore);
          const pending = sandboxStore.createProfile("Pending sign-in");
          this.sandbox = {
            provider,
            pool,
            accounts: new Accounts(pool, sandboxStore),
            profileId: pending.id,
          };
          this.session = {
            ...(await this.open(pool, pending.id, provider, profileId)),
            reconnectAccountId,
          };
        } else {
          this.session = await this.open(
            this.pool,
            profileId,
            provider,
            profileId,
          );
        }
        const session = this.session;
        this.expiry = setTimeout(
          () => {
            void this.serialize(async () => {
              if (this.session === session) await this.clear();
            }).catch(() =>
              this.report(
                "Could not clean up an expired sign-in. Restart the app.",
              ),
            );
          },
          Math.max(1, session.expiresAt - this.now()),
        );
        this.expiry.unref();
        return session;
      } catch (error) {
        await this.clear();
        throw error;
      }
    });
  }
  /** Asks `pool`'s core for a sign-in link for `coreProfileId`, the core that will hold the credential. */
  private async open(
    pool: CorePool,
    coreProfileId: string,
    provider: OAuthProvider,
    profileId: string,
  ): Promise<OAuthSession> {
    await pool.start(coreProfileId);
    const path = providerEntry(provider).signIn?.core ?? provider;
    const response = record(
      await pool.management(coreProfileId, `/${path}-auth-url`),
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
      startedAt: this.now(),
      userCode: text(response.user_code),
      expiresAt:
        this.now() + Math.max(1, number(response.expires_in) ?? 300) * 1000,
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
        void this.callback(profileId, incoming.href).then(
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
            this.onCallback?.();
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
        await pool.management(
          coreProfileId,
          `/oauth-session?state=${encodeURIComponent(session.state)}`,
          "DELETE",
        );
        throw new AppError(
          `Sign-in port ${callback.port} is in use. Close any pending sign-in in another app, then try again.`,
          409,
        );
      }
      this.server = server;
    }
    return session;
  }
  private requireSession(profileId: string, state: string): OAuthSession {
    if (
      !this.session ||
      this.session.profileId !== profileId ||
      this.session.state !== state
    )
      throw new AppError("Sign-in session not found for this profile.", 404);
    if (this.now() >= this.session.expiresAt)
      throw new AppError("This sign-in expired. Start a new sign-in.", 410);
    return this.session;
  }
  cancel(profileId: string, state: string): Promise<void> {
    return this.serialize(async () => {
      this.requireSession(profileId, state);
      await this.clear();
    });
  }
  /** Drops the sign-in in progress: its sandbox, or its pending authorization in the profile's core. */
  async clear(): Promise<void> {
    clearTimeout(this.expiry);
    const sandbox = this.sandbox;
    if (sandbox) {
      this.closeCallback();
      await sandbox.pool.shutdown();
      sandbox.pool.store.close();
      await rm(sandbox.pool.store.directory, { recursive: true, force: true });
      this.sandbox = undefined;
    } else if (this.session) {
      try {
        await this.pool.management(
          this.session.profileId,
          `/oauth-session?state=${encodeURIComponent(this.session.state)}`,
          "DELETE",
        );
      } catch {
        // A stopped or unreachable core cannot acknowledge cancellation. Stop it
        // locally so it cannot complete this authorization after cleanup.
        await this.pool.stop(this.session.profileId);
      }
    }
    this.closeCallback();
    this.session = undefined;
  }
  private closeCallback(force = true): void {
    const server = this.server;
    this.server = undefined;
    if (!server) return;
    // Stop accepting callbacks now. Do not wait for handlers queued behind this sign-in action.
    server.close();
    if (force) server.closeAllConnections();
    else server.closeIdleConnections();
  }
  /** The core's view of the authorization: finished, failed, or still waiting. */
  private async authorization(
    pool: CorePool,
    coreProfileId: string,
    state: string,
  ): Promise<"ok" | "error" | "wait"> {
    const result = record(
      await pool.management(
        coreProfileId,
        `/get-auth-status?state=${encodeURIComponent(state)}`,
      ),
    );
    return result.status === "ok" || result.status === "error"
      ? result.status
      : "wait";
  }
  status(profileId: string, state: string): Promise<OAuthStatus> {
    return this.serialize(async () => {
      const session = this.requireSession(profileId, state);
      const failed: OAuthStatus = {
        status: "error",
        error: "Sign-in did not complete. Start a new sign-in.",
      };
      const sandbox = this.sandbox;
      if (sandbox) {
        try {
          if (!sandbox.credentialId) {
            if (!sandbox.authorized) {
              const result = await this.authorization(
                sandbox.pool,
                sandbox.profileId,
                state,
              );
              if (result === "wait") return { status: "wait" };
              if (result === "error") {
                await this.clear();
                return failed;
              }
              sandbox.authorized = true;
              this.closeCallback();
            }
            await sandbox.accounts.sync(sandbox.profileId);
            const accounts = sandbox.accounts.list(sandbox.profileId);
            if (accounts.length !== 1)
              throw new AppError(
                `${providerLabel(sandbox.provider)} did not return one subscription. Sign in again.`,
                502,
              );
            sandbox.credentialId = accounts[0]!.id;
          }
          if (!sandbox.credential) {
            // Stop refresh before reading and verifying the token that will be transferred.
            await sandbox.pool.stop(sandbox.profileId);
            sandbox.credential = await sandbox.accounts.readCredential(
              sandbox.profileId,
              sandbox.credentialId,
            );
          }
          if (!sandbox.review) {
            const credential = await verifySeat[sandbox.provider](
              sandbox.credential,
            );
            sandbox.credential = credential;
            const account: Account = {
              id: "pending",
              provider: sandbox.provider,
              kind: "oauth",
              name: providerLabel(sandbox.provider),
              ...claudeIdentity(credential),
              email: text(credential.email),
              disabled: false,
              status: "Ready",
              priority: 0,
            };
            const match = await this.credentials.matchSeat(
              profileId,
              sandbox.provider,
              credential,
              session.reconnectAccountId,
            );
            sandbox.review = { account, ...match };
          }
          return { status: "review", review: sandbox.review };
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
      const result = await this.authorization(this.pool, profileId, state);
      if (result === "wait") return { status: "wait" };
      this.session = undefined;
      clearTimeout(this.expiry);
      this.closeCallback();
      if (result === "error") {
        await this.accounts.sync(profileId);
        return failed;
      }
      await this.quotas.refreshAccounts(
        profileId,
        (account) => !account.disabled && account.provider === session.provider,
      );
      return { status: "ok" };
    });
  }
  confirm(profileId: string, state: string): Promise<OAuthStatus> {
    return this.serialize(async () => {
      const session = this.requireSession(profileId, state);
      const sandbox = this.sandbox;
      if (!sandbox?.credential || !sandbox.review)
        throw new AppError(
          `Wait for ${providerLabel(session.provider)} to verify the organization before connecting.`,
          409,
        );
      const credential = sandbox.credential;
      const result = await this.credentials.queue(async () => {
        const match = await this.credentials.matchSeat(
          profileId,
          sandbox.provider,
          credential,
          session.reconnectAccountId,
        );
        if (match.action === "blocked") throw new AppError(match.message!, 409);
        const account = await this.credentials.importCredential(
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
      await this.clear();
      return result;
    });
  }
  callback(profileId: string, redirectUrl: string): Promise<void> {
    return this.serialize(async () => {
      const url = new URL(redirectUrl);
      const session = this.requireSession(
        profileId,
        url.searchParams.get("state") ?? "",
      );
      const sandbox = this.sandbox;
      // Its core already finished this authorization.
      if (sandbox?.authorized)
        throw new AppError("Sign-in session not found for this profile.", 404);
      await (sandbox?.pool ?? this.pool).management(
        sandbox?.profileId ?? profileId,
        "/oauth-callback",
        "POST",
        {
          provider:
            providerEntry(session.provider).signIn?.core ?? session.provider,
          redirect_url: redirectUrl,
        },
      );
    });
  }
  /**
   * Sign-ins pending when the app last stopped never resume or enter a profile, and core
   * update checks never finish: reap their cores and delete them.
   */
  async discardPending(): Promise<void> {
    const pending = oauthPendingDirectory(this.pool.store.directory);
    for (const entry of await readdir(pending).catch(() => []))
      await reapOrphanedCores(join(pending, entry));
    await rm(pending, { recursive: true, force: true });
  }
}
