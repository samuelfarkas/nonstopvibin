import { existsSync } from "node:fs";
import { chmod, lstat, readFile, readdir } from "node:fs/promises";
import { join } from "node:path";
import type { Account } from "../shared/types.ts";
import { providerLabel } from "../shared/providers.ts";
import { credentialFileBytes } from "../shared/vocabulary.ts";
import type { CorePool } from "./core.ts";
import { credentialDirectory } from "./data-directory.ts";
import { AppError } from "./errors.ts";
import { claudeIdentity } from "./identity.ts";
import { number, parse, record, text, type JsonObject } from "./json.ts";
import type { Store } from "./store.ts";

/** What reading a profile's accounts needs from its core. */
export type AccountSource = Pick<
  CorePool,
  "usable" | "management" | "runtimes"
>;

/** The accounts each profile's core holds, and the credential files behind them. */
export class Accounts {
  readonly store: Store;
  private readonly pool: AccountSource;
  /** OAuth accounts as last read from the core or rewritten on reconnect; the store keeps them across restarts. */
  private readonly oauthLists = new Map<string, Account[]>();
  /** ChatGPT account IDs by credential file; Codex quota and reset calls need them. */
  private readonly chatgptAccountIds = new Map<string, Map<string, string>>();
  /** Parsed Claude display identity per profile and credential file, keyed by file version. */
  private readonly claudeIdentities = new Map<
    string,
    Map<
      string,
      {
        mtimeMs: number;
        size: number;
        identity: ReturnType<typeof claudeIdentity>;
      }
    >
  >();
  constructor(pool: AccountSource, store: Store) {
    this.pool = pool;
    this.store = store;
  }
  /** A profile's OAuth accounts, without quota. */
  oauth(profileId: string): Account[] {
    let accounts = this.oauthLists.get(profileId);
    if (!accounts) {
      accounts = this.store.accountMetadata(profileId);
      this.oauthLists.set(profileId, accounts);
    }
    return accounts;
  }
  private save(profileId: string, accounts: Account[]): void {
    this.oauthLists.set(profileId, accounts);
    this.store.saveAccountMetadata(profileId, accounts);
  }
  /** Every account of a profile, OAuth then API key, each with its stored quota. */
  list(profileId: string): Account[] {
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
    return [...this.oauth(profileId), ...api].map((account) => ({
      ...account,
      quota: this.store.quota(profileId, account.id),
    }));
  }
  chatgptAccountId(profileId: string, accountId: string): string | undefined {
    return this.chatgptAccountIds.get(profileId)?.get(accountId);
  }
  /** Re-reads the accounts from a usable core; does nothing otherwise. */
  async sync(profileId: string, signal?: AbortSignal): Promise<void> {
    if (!this.pool.usable(profileId)) return;
    const core = this.pool.runtimes.get(profileId);
    const response = record(
      await this.pool.management(
        profileId,
        "/auth-files",
        "GET",
        undefined,
        undefined,
        signal,
      ),
    );
    if (!Array.isArray(response.files))
      throw new AppError("The proxy returned an unexpected account list.", 502);
    const identities = new Map<string, ReturnType<typeof claudeIdentity>>();
    const known = this.claudeIdentities.get(profileId);
    const parsed: NonNullable<typeof known> = new Map();
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
        // Stat before reading: a later rewrite always shows a newer version.
        const { mtimeMs, size } = await lstat(
          this.credentialPath(profileId, name),
        );
        const cached = known?.get(name);
        const identity =
          cached?.mtimeMs === mtimeMs && cached.size === size
            ? cached.identity
            : claudeIdentity(await this.readCredential(profileId, name));
        parsed.set(name, { mtimeMs, size, identity });
        identities.set(name, identity);
      } catch {
        // Display enrichment is optional: refresh can replace a file during a read.
        // The row explicitly shows unverified identity until a later sync succeeds.
        identities.set(name, {});
      }
    }
    this.claudeIdentities.set(profileId, parsed);
    const dir = this.credentialDirectory(profileId);
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      if (!entry.isFile()) continue;
      const path = join(dir, entry.name);
      // A no-op chmod still writes inode metadata; repair only what differs.
      if (((await lstat(path)).mode & 0o777) !== 0o600)
        await chmod(path, 0o600);
    }
    const chatgptAccountIds = new Map<string, string>();
    const accounts = response.files.flatMap((value) => {
      const file = record(value);
      if (file.runtime_only || file.account_type === "api_key") return [];
      const id = String(file.name ?? file.id ?? "");
      if (!id) return [];
      const provider = String(file.provider ?? file.type ?? "unknown");
      const chatgptAccountId = text(record(file.id_token).chatgpt_account_id);
      if (chatgptAccountId) chatgptAccountIds.set(id, chatgptAccountId);
      const account: Account = {
        id,
        name: String(file.note || providerLabel(provider)),
        ...identities.get(id),
        provider,
        kind: "oauth",
        email: text(file.email),
        disabled: file.disabled === true,
        // Core status messages: "disabled (invalid grant)", "invalid grant (retrying)", "unauthorized", "token expired".
        status: file.disabled
          ? "Paused"
          : file.status === "error"
            ? /invalid grant|unauthorized/i.test(
                String(file.status_message ?? ""),
              )
              ? "Reconnect needed"
              : "Needs attention"
            : file.unavailable
              ? "Cooling down"
              : "Ready",
        priority: number(file.priority) ?? 0,
        authIndex: String(file.auth_index ?? ""),
      };
      return [account];
    });
    // A listing from a core that was replaced meanwhile must not overwrite its successor's.
    if (this.pool.runtimes.get(profileId) !== core) return;
    this.chatgptAccountIds.set(profileId, chatgptAccountIds);
    this.save(profileId, accounts);
  }
  /** A credential file was replaced without the core: its account shows the file's seat and email. */
  replaced(profileId: string, accountId: string, credential: JsonObject): void {
    this.save(
      profileId,
      this.oauth(profileId).map((account) =>
        account.id === accountId
          ? {
              ...account,
              ...claudeIdentity(credential),
              email: text(credential.email) ?? account.email,
            }
          : account,
      ),
    );
  }
  private credentialDirectory(profileId: string): string {
    this.store.profile(profileId);
    return credentialDirectory(this.store.directory, profileId);
  }
  credentialPath(profileId: string, name: string): string {
    if (
      !name.endsWith(".json") ||
      name.includes("/") ||
      name.includes("\\") ||
      name === ".json"
    )
      throw new AppError("Unsupported credential filename.");
    return join(this.credentialDirectory(profileId), name);
  }
  async readCredential(profileId: string, name: string): Promise<JsonObject> {
    const path = this.credentialPath(profileId, name);
    const file = await lstat(path);
    if (!file.isFile() || file.size > credentialFileBytes)
      throw new AppError("The credential is not a supported regular file.");
    try {
      return record(parse(await readFile(path, "utf8")));
    } catch {
      throw new AppError(
        "A saved credential could not be read. Reconnect that subscription.",
      );
    }
  }
  /** The profile's credential files, by file name. */
  async credentialFiles(
    profileId: string,
  ): Promise<Array<{ id: string; raw: JsonObject }>> {
    const files = [];
    const directory = this.credentialDirectory(profileId);
    if (!existsSync(directory)) return [];
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      if (!entry.isFile() || !entry.name.endsWith(".json")) continue;
      files.push({
        id: entry.name,
        raw: await this.readCredential(profileId, entry.name),
      });
    }
    return files;
  }
}
