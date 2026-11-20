import { rename, rm, writeFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import type { ApiAccountInput } from "../shared/api.ts";
import type { Account, OAuthReview } from "../shared/types.ts";
import {
  hasOrganizations,
  isCredentialFile,
  type OrganizationProvider,
} from "../shared/providers.ts";
import type { Accounts } from "./accounts.ts";
import { apiAccount } from "./api-accounts.ts";
import type { CorePool } from "./core.ts";
import { AppError } from "./errors.ts";
import { claudeIdentity, sameSeat } from "./identity.ts";
import { text, type JsonObject } from "./json.ts";
import type { Quotas } from "./quota.ts";
import { sameSecret, type Store } from "./store.ts";

export class Credentials {
  private readonly pool: CorePool;
  private readonly store: Store;
  private readonly accounts: Accounts;
  private readonly quotas: Quotas;
  private changes: Promise<unknown> = Promise.resolve();
  constructor(pool: CorePool, accounts: Accounts, quotas: Quotas) {
    this.pool = pool;
    this.store = accounts.store;
    this.accounts = accounts;
    this.quotas = quotas;
  }
  // shortcut: one credential change at a time across all profiles; keyed locks if large account pools make this a bottleneck.
  queue<T>(action: () => Promise<T>): Promise<T> {
    const operation = this.changes.catch(() => undefined).then(action);
    this.changes = operation;
    return operation;
  }
  async settled(): Promise<void> {
    await this.changes.catch(() => undefined);
  }
  import(profileId: string, contents: JsonObject): Promise<Account> {
    return this.queue(() => this.importCredential(profileId, contents));
  }
  async matchSeat(
    profileId: string,
    provider: OrganizationProvider,
    credential: JsonObject,
    reconnectAccountId?: string,
  ): Promise<
    Pick<OAuthReview, "action" | "message"> & {
      accountId?: string;
    }
  > {
    const identity = claudeIdentity(credential);
    const files = await this.accounts.credentialFiles(profileId);
    if (reconnectAccountId) {
      const target = files.find(
        (f) => f.id === reconnectAccountId && f.raw.type === provider,
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
        f.raw.type === provider && sameSeat(identity, claudeIdentity(f.raw)),
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
  /** Adds a credential, or replaces `reconnectAccountId`'s; callers hold the queue. */
  async importCredential(
    profileId: string,
    raw: JsonObject,
    reconnectAccountId?: string,
  ): Promise<Account> {
    const provider = String(raw.type ?? "");
    const accessToken = text(raw.access_token);
    const refreshToken = text(raw.refresh_token);
    if (
      !isCredentialFile(provider) ||
      (accessToken === undefined && !(raw.token instanceof Object))
    )
      throw new AppError(
        "Choose a CLIProxyAPI OAuth JSON file. It must contain a supported type and token.",
      );
    const files = await this.accounts.credentialFiles(profileId);
    const prior = reconnectAccountId
      ? files.find((f) => f.id === reconnectAccountId)
      : undefined;
    if (reconnectAccountId && !prior)
      throw new AppError(
        "The subscription to reconnect no longer exists.",
        409,
      );
    const identity = hasOrganizations(provider)
      ? claudeIdentity(raw)
      : undefined;
    for (const existing of files) {
      if (existing === prior) continue;
      if (
        existing.raw.type === provider &&
        ((refreshToken && existing.raw.refresh_token === refreshToken) ||
          (accessToken && existing.raw.access_token === accessToken) ||
          (identity && sameSeat(identity, claudeIdentity(existing.raw))))
      )
        throw new AppError(
          `This subscription is already connected to ${this.store.profile(profileId).name}. Use its existing connection instead of importing another copy.`,
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
      const path = this.accounts.credentialPath(profileId, name);
      // An in-flight core refresh can overwrite a management upload's tokens, so the
      // file is swapped with the core stopped; its path stays for stable accounting.
      await this.pool.whileStopped(profileId, async () => {
        const temporary = `${path}.${randomUUID()}.tmp`;
        try {
          await writeFile(temporary, JSON.stringify(payload), {
            mode: 0o600,
            flag: "wx",
          });
          await rename(temporary, path);
        } catch (error) {
          await rm(temporary, { force: true });
          throw error;
        }
        this.accounts.replaced(profileId, name, payload);
      });
    } else {
      await this.pool.start(profileId);
      await this.pool.management(
        profileId,
        `/auth-files?name=${encodeURIComponent(name)}`,
        "POST",
        payload,
      );
      await this.accounts.sync(profileId);
    }
    const account = this.accounts.list(profileId).find((a) => a.id === name);
    if (!account)
      throw new AppError(
        "The proxy has not registered this subscription yet. Refresh the profile.",
        503,
      );
    if (!account.disabled)
      account.quota = await this.quotas.refresh(profileId, account.id);
    return account;
  }
  /**
   * Adds an API-key account. Its models are found before the queue and its quota is read
   * after it: neither changes credentials, and both wait on the provider.
   */
  async addApiAccount(
    profileId: string,
    input: ApiAccountInput,
  ): Promise<void> {
    const account = await apiAccount(input);
    await this.queue(async () => {
      for (const existing of this.store.apiAccounts(profileId)) {
        if (
          existing.baseUrl === account.baseUrl &&
          sameSecret(this.store.apiKey(existing.id), input.apiKey)
        )
          throw new AppError(
            "This API key is already connected to this profile.",
            409,
          );
      }
      this.store.saveApiAccount(profileId, account, input.apiKey);
      await this.pool.start(profileId);
      await this.pool.configure(profileId);
    });
    await this.quotas.refresh(profileId, account.id);
  }
  setAccount(
    profileId: string,
    accountId: string,
    patch: { disabled?: boolean; priority?: number; name?: string },
  ): Promise<void> {
    return this.queue(async () => {
      const api = this.store
        .apiAccounts(profileId)
        .find((a) => a.id === accountId);
      if (api) {
        this.store.saveApiAccount(profileId, {
          ...api,
          disabled: patch.disabled ?? api.disabled,
          name: patch.name || api.name,
        });
        await this.pool.configure(profileId);
        return;
      }
      const account = this.accounts
        .list(profileId)
        .find((a) => a.id === accountId);
      if (!account)
        throw new AppError("Account not found in this profile.", 404);
      // A stopped or exited core keeps its record and port; nothing answers there.
      if (!this.pool.usable(profileId))
        throw new AppError("Start this profile first.", 409);
      if (patch.disabled !== undefined)
        await this.pool.management(profileId, "/auth-files/status", "PATCH", {
          name: account.id,
          disabled: patch.disabled,
        });
      if (patch.priority !== undefined || patch.name !== undefined)
        // JSON.stringify drops undefined, so untouched fields are not sent.
        await this.pool.management(profileId, "/auth-files/fields", "PATCH", {
          name: account.id,
          priority: patch.priority,
          note: patch.name,
        });
      await this.accounts.sync(profileId);
    });
  }
  removeAccount(profileId: string, accountId: string): Promise<void> {
    return this.queue(async () => {
      const account = this.accounts
        .list(profileId)
        .find((a) => a.id === accountId);
      if (!account)
        throw new AppError("Account not found in this profile.", 404);
      if (account.kind === "api-key") {
        this.store.removeApiAccount(profileId, accountId);
        await this.pool.configure(profileId);
      } else {
        await this.pool.start(profileId);
        await this.pool.management(
          profileId,
          `/auth-files?name=${encodeURIComponent(accountId)}`,
          "DELETE",
        );
        await this.accounts.sync(profileId);
      }
    });
  }
}
