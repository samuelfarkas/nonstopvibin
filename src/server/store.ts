import { DatabaseSync, type SQLOutputValue } from "node:sqlite";
import { chmodSync, mkdirSync } from "node:fs";
import { createHash, randomUUID, randomBytes } from "node:crypto";
import { join } from "node:path";
import type {
  Account,
  ApiAccount,
  Profile,
  Quota,
  UsageRecord,
  UsageSummary,
} from "../shared/types.ts";
import type { SecretCodec } from "./vault.ts";
import { AppError } from "./errors.ts";

/** One-way index key for a client key; the index never holds the key itself. */
export function clientDigest(key: string): string {
  return createHash("sha256").update(key).digest("base64");
}
function stored<T>(body: SQLOutputValue): T {
  // SAFETY: bodies are written only by this Store via JSON.stringify of the
  // typed value each reader requests, so decoding yields that type back.
  return JSON.parse(String(body)) as T;
}
export class Store {
  readonly db: DatabaseSync;
  readonly directory: string;
  readonly codec: SecretCodec;
  usageGeneration = 0;
  /** Bumped on every profile write, so readers can cache profile rows. */
  profileGeneration = 0;
  private clients?: Map<string, string>;
  constructor(directory: string, codec: SecretCodec) {
    this.directory = directory;
    this.codec = codec;
    mkdirSync(directory, { recursive: true, mode: 0o700 });
    chmodSync(directory, 0o700);
    this.db = new DatabaseSync(join(directory, "nonstopvibin.sqlite"));
    chmodSync(join(directory, "nonstopvibin.sqlite"), 0o600);
    this.db
      .exec(`PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;
      CREATE TABLE IF NOT EXISTS profiles (id TEXT PRIMARY KEY, slug TEXT UNIQUE NOT NULL, body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS secrets (id TEXT PRIMARY KEY, encrypted TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS api_accounts (id TEXT PRIMARY KEY, profile_id TEXT NOT NULL REFERENCES profiles(id), body TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS quotas (profile_id TEXT NOT NULL, account_id TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(profile_id,account_id));
      CREATE TABLE IF NOT EXISTS usage (id TEXT PRIMARY KEY, profile_id TEXT NOT NULL, timestamp TEXT NOT NULL, body TEXT NOT NULL);
      CREATE INDEX IF NOT EXISTS usage_profile_sequence ON usage(profile_id);
      CREATE INDEX IF NOT EXISTS usage_profile_time ON usage(profile_id,timestamp);
      CREATE TABLE IF NOT EXISTS settings (id TEXT PRIMARY KEY, value TEXT NOT NULL);
    `);
  }
  private transaction<T>(work: () => T): T {
    this.db.exec("BEGIN IMMEDIATE");
    try {
      const result = work();
      this.db.exec("COMMIT");
      return result;
    } catch (error) {
      this.db.exec("ROLLBACK");
      throw error;
    }
  }
  profiles(): Profile[] {
    return this.db
      .prepare("SELECT body FROM profiles ORDER BY rowid")
      .all()
      .map((r) => stored<Profile>(r.body));
  }
  profile(id: string): Profile {
    const row = this.db.prepare("SELECT body FROM profiles WHERE id=?").get(id);
    if (!row) throw new AppError("Profile not found.", 404);
    return stored<Profile>(row.body);
  }
  createProfile(name: string, color: string): Profile {
    const stem =
      name
        .normalize("NFKD")
        .replace(/[\u0300-\u036f]/g, "")
        .toLowerCase()
        .replace(/[^a-z0-9]+/g, "-")
        .slice(0, 40)
        .replace(/^-|-$/g, "") || "profile";
    const slugs = new Set(this.profiles().map((p) => p.slug));
    let slug = stem;
    let suffix = 2;
    while (slugs.has(slug)) slug = `${stem}-${suffix++}`;
    const profile: Profile = {
      id: randomUUID(),
      slug,
      name,
      color,
      strategy: "fill-first",
      sessionAffinity: true,
      enabled: false,
      createdAt: new Date().toISOString(),
    };
    this.transaction(() => {
      this.db
        .prepare("INSERT INTO profiles VALUES (?,?,?)")
        .run(profile.id, slug, JSON.stringify(profile));
      this.setSecret(
        `${profile.id}:client`,
        `nv_${randomBytes(32).toString("base64url")}`,
      );
      this.setSecret(
        `${profile.id}:core`,
        randomBytes(32).toString("base64url"),
      );
      this.setSecret(
        `${profile.id}:management`,
        randomBytes(32).toString("base64url"),
      );
    });
    this.profileGeneration++;
    return profile;
  }
  saveProfile(profile: Profile): void {
    this.clients = undefined;
    this.profileGeneration++;
    this.db
      .prepare("UPDATE profiles SET body=? WHERE id=?")
      .run(JSON.stringify(profile), profile.id);
  }
  saveAccountMetadata(profileId: string, accounts: Account[]): void {
    this.db
      .prepare(`INSERT INTO settings VALUES (?,?) ON CONFLICT(id) DO UPDATE
        SET value=excluded.value WHERE value!=excluded.value`)
      .run(`accounts:${profileId}`, JSON.stringify(accounts));
  }
  accountMetadata(profileId: string): Account[] {
    const row = this.db
      .prepare("SELECT value FROM settings WHERE id=?")
      .get(`accounts:${profileId}`);
    return row ? stored<Account[]>(row.value) : [];
  }
  secret(id: string): string {
    const row = this.db
      .prepare("SELECT encrypted FROM secrets WHERE id=?")
      .get(id);
    if (!row) throw new AppError("Credential not found.", 404);
    return this.codec.decrypt(String(row.encrypted));
  }
  /**
   * Client-key digest to profile id, so a request costs one lookup instead of a
   * decrypt per profile. A hint only: callers verify the live rows. Every profile
   * or secret write (including createProfile, via setSecret) drops it.
   */
  clientIndex(): Map<string, string> {
    this.clients ??= new Map(
      this.profiles().map((profile) => [
        clientDigest(this.secret(`${profile.id}:client`)),
        profile.id,
      ]),
    );
    return this.clients;
  }
  setSecret(id: string, value: string): void {
    this.clients = undefined;
    this.db
      .prepare("INSERT OR REPLACE INTO secrets VALUES (?,?)")
      .run(id, this.codec.encrypt(value));
  }
  resetRequestId(identity: string, creditId: string): string {
    const key = `codex-reset:${JSON.stringify([identity, creditId])}`;
    this.db
      .prepare("INSERT OR IGNORE INTO settings VALUES (?,?)")
      .run(key, randomUUID());
    const row = this.db
      .prepare("SELECT value FROM settings WHERE id=?")
      .get(key);
    return String(row?.value);
  }
  clearResetRequest(identity: string, creditId: string): void {
    this.db
      .prepare("DELETE FROM settings WHERE id=?")
      .run(`codex-reset:${JSON.stringify([identity, creditId])}`);
  }
  apiAccounts(profileId: string): ApiAccount[] {
    return this.db
      .prepare(
        "SELECT body FROM api_accounts WHERE profile_id=? ORDER BY rowid",
      )
      .all(profileId)
      .map((r) => stored<ApiAccount>(r.body));
  }
  saveApiAccount(profileId: string, account: ApiAccount, key?: string): void {
    this.transaction(() => {
      this.db
        .prepare("INSERT OR REPLACE INTO api_accounts VALUES (?,?,?)")
        .run(account.id, profileId, JSON.stringify(account));
      if (key !== undefined) this.setSecret(`${account.id}:api`, key);
    });
  }
  removeApiAccount(profileId: string, accountId: string): void {
    this.transaction(() => {
      this.db
        .prepare("DELETE FROM api_accounts WHERE profile_id=? AND id=?")
        .run(profileId, accountId);
      this.db.prepare("DELETE FROM secrets WHERE id=?").run(accountId + ":api");
      this.db
        .prepare("DELETE FROM quotas WHERE profile_id=? AND account_id=?")
        .run(profileId, accountId);
    });
  }
  quota(profileId: string, accountId: string): Quota | undefined {
    const row = this.db
      .prepare("SELECT body FROM quotas WHERE profile_id=? AND account_id=?")
      .get(profileId, accountId);
    return row ? stored<Quota>(row.body) : undefined;
  }
  saveQuota(profileId: string, accountId: string, quota: Quota): void {
    this.db
      .prepare("INSERT OR REPLACE INTO quotas VALUES (?,?,?)")
      .run(profileId, accountId, JSON.stringify(quota));
  }
  addUsage(records: UsageRecord[]): void {
    if (!records.length) return;
    const stmt = this.db.prepare(
      "INSERT OR IGNORE INTO usage VALUES (?,?,?,?)",
    );
    // One batch commits or rolls back whole; duplicate IDs are ignored.
    this.transaction(() => {
      for (const r of records)
        stmt.run(r.id, r.profileId, r.timestamp, JSON.stringify(r));
    });
  }
  usage(profileId: string, since = "", limit = 300): UsageRecord[] {
    return this.db
      .prepare(
        "SELECT body FROM usage WHERE profile_id=? AND timestamp>=? ORDER BY timestamp DESC LIMIT ?",
      )
      .all(profileId, since, limit)
      .map((r) => stored<UsageRecord>(r.body));
  }
  usageSequence(): number {
    return Number(
      this.db
        .prepare("SELECT coalesce(max(rowid),0) AS sequence FROM usage")
        .get()?.sequence,
    );
  }
  usageBatch(
    profileId: string,
    after: number,
    through: number,
  ): Array<{ sequence: number; record: UsageRecord }> {
    return this.db
      .prepare(`SELECT rowid AS sequence, body FROM usage INDEXED BY usage_profile_sequence
      WHERE profile_id=? AND rowid>? AND rowid<=? ORDER BY rowid LIMIT 500`)
      .all(profileId, after, through)
      .map((row) => ({
        sequence: Number(row.sequence),
        record: stored<UsageRecord>(row.body),
      }));
  }
  usageTimeBatch(
    profileId: string,
    since: string,
    before: string,
    through: number,
    timestamp = since,
    sequence = 0,
  ): Array<{ sequence: number; record: UsageRecord }> {
    // Seek ties by rowid, then later timestamps; a tuple filter rescans the prefix on each page.
    if (timestamp < since) {
      timestamp = since;
      sequence = 0;
    }
    return this.db
      .prepare(`SELECT rowid AS sequence, body, timestamp FROM usage INDEXED BY usage_profile_time
      WHERE profile_id=? AND timestamp=? AND timestamp<? AND rowid>? AND rowid<=?
      UNION ALL
      SELECT rowid AS sequence, body, timestamp FROM usage INDEXED BY usage_profile_time
      WHERE profile_id=? AND timestamp>? AND timestamp<? AND rowid<=?
      ORDER BY timestamp,sequence LIMIT 500`)
      .all(
        profileId,
        timestamp,
        before,
        sequence,
        through,
        profileId,
        timestamp,
        before,
        through,
      )
      .map((row) => ({
        sequence: Number(row.sequence),
        record: stored<UsageRecord>(row.body),
      }));
  }
  /** Stored totals since an ISO time. Tests' accounting oracle; the app aggregates in UsagePrices. */
  summary(profileId: string, since = ""): UsageSummary {
    const row = this.db
      .prepare(
        `SELECT count(*) AS requests,
      coalesce(sum(json_extract(body,'$.failed')),0) AS failed,
      coalesce(sum(json_extract(body,'$.inputTokens')),0) AS inputTokens,
      coalesce(sum(json_extract(body,'$.outputTokens')),0) AS outputTokens,
      coalesce(sum(json_extract(body,'$.cachedTokens')),0) AS cachedTokens,
      coalesce(sum(json_extract(body,'$.reasoningTokens')),0) AS reasoningTokens,
      coalesce(sum(json_extract(body,'$.totalTokens')),0) AS totalTokens
      FROM usage WHERE profile_id=? AND timestamp>=?`,
      )
      .get(profileId, since);
    return {
      requests: Number(row?.requests ?? 0),
      failed: Number(row?.failed ?? 0),
      inputTokens: Number(row?.inputTokens ?? 0),
      outputTokens: Number(row?.outputTokens ?? 0),
      cachedTokens: Number(row?.cachedTokens ?? 0),
      reasoningTokens: Number(row?.reasoningTokens ?? 0),
      totalTokens: Number(row?.totalTokens ?? 0),
    };
  }
  usageRetentionDays(): number {
    const row = this.db
      .prepare("SELECT value FROM settings WHERE id='usage-retention-days'")
      .get();
    return Number(row?.value ?? 0);
  }
  setUsageRetentionDays(days: number): void {
    if (![0, 30, 90, 365].includes(days))
      throw new AppError("Unsupported history retention.");
    this.transaction(() => {
      this.db
        .prepare(
          "INSERT OR REPLACE INTO settings VALUES ('usage-retention-days',?)",
        )
        .run(String(days));
      this.pruneUsage();
    });
  }
  pruneUsage(now = Date.now()): void {
    const days = this.usageRetentionDays();
    if (!days) return;
    const cutoff = new Date(now - days * 86_400_000).toISOString();
    const remove = this.db.prepare(
      "DELETE FROM usage WHERE profile_id=? AND timestamp<?",
    );
    for (const profile of this.profiles()) {
      if (remove.run(profile.id, cutoff).changes) this.usageGeneration++;
    }
  }
  close(): void {
    this.db.close();
  }
}
