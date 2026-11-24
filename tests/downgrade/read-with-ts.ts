// Downgrade check: the TypeScript Store of the checkout in NSV_TS_CHECKOUT opens a data
// directory and finds what <dir>/expected.json says (the shape of
// tests/fixtures/ts-data-dir/expected.json). The Rust store's migration test leaves such a
// directory when NSV_KEEP_DIR is set.
//
//   NSV_KEEP_DIR=/tmp/nsv-downgrade cargo test -p nsv-store migration
//   NSV_TS_CHECKOUT=<checkout> bun tests/downgrade/read-with-ts.ts /tmp/nsv-downgrade
//
// The Store's shape is declared here, not imported, so this keeps type-checking after the
// TypeScript server leaves this repository.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";

/** A JSON value, as the store's bodies and expected.json hold them. */
type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
/** A column value as node:sqlite returns it. */
type SqlValue = null | number | bigint | string | Uint8Array;
interface SecretCodec {
  encrypt(value: string): string;
  decrypt(value: string): string;
}
interface TsStore {
  db: {
    prepare(sql: string): { all(): Array<Record<string, SqlValue>> };
  };
  profiles(): Json[];
  clientKey(profileId: string): string;
  coreKey(profileId: string): string;
  managementKey(profileId: string): string;
  apiKey(accountId: string): string;
  apiAccounts(profileId: string): Json[];
  accountMetadata(profileId: string): Json[];
  quota(profileId: string, accountId: string): Json | undefined;
  usage(profileId: string, since: string, limit: number): Json[];
  usageAfter(
    profileId: string,
    after: number,
    through: number,
  ): { records: Json[] };
  usageRetentionDays(): number;
  resetRequestId(identity: string, creditId: string): string;
  close(): void;
}
interface StoreModule {
  Store: new (directory: string, codec: SecretCodec) => TsStore;
}
interface VaultModule {
  fileKeyCodec(directory: string): SecretCodec;
}
interface Expected {
  profiles: Json[];
  secrets: Record<string, string>;
  apiAccounts: Record<string, Json[]>;
  accountMetadata: Record<string, Json[]>;
  quotas: Array<{ profileId: string; accountId: string; quota: Json }>;
  usage: Record<
    string,
    { count: number; records: Array<{ sequence: number; record: Json }> }
  >;
  retentionDays: number;
  resetRequest: { identity: string; creditId: string; requestId: string };
}

const directory = process.argv[2];
const checkout = process.env.NSV_TS_CHECKOUT;
if (!directory || !checkout)
  throw new Error(
    "Usage: NSV_TS_CHECKOUT=<checkout> bun tests/downgrade/read-with-ts.ts <dir>",
  );
const source = join(resolve(checkout), "src/server");
// SAFETY: store.ts exports this Store in every TypeScript release of the server.
const { Store } = (await import(join(source, "store.ts"))) as StoreModule;
// SAFETY: vault.ts exports this fileKeyCodec in every TypeScript release of the server.
const { fileKeyCodec } = (await import(
  join(source, "vault.ts")
)) as VaultModule;
// SAFETY: written by the fixture generator or the Rust migration test in this shape.
const expected = JSON.parse(
  await readFile(join(directory, "expected.json"), "utf8"),
) as Expected;

const data = join(directory, "data");
const store = new Store(data, fileKeyCodec(data));
try {
  assert.deepEqual(store.profiles(), expected.profiles);

  const secretIds = store.db
    .prepare("SELECT id FROM secrets ORDER BY id")
    .all()
    .map((row) => String(row.id));
  assert.deepEqual(secretIds, Object.keys(expected.secrets).sort());
  const readers = new Map([
    ["client", (owner: string) => store.clientKey(owner)],
    ["core", (owner: string) => store.coreKey(owner)],
    ["management", (owner: string) => store.managementKey(owner)],
    ["api", (owner: string) => store.apiKey(owner)],
  ]);
  for (const [id, plaintext] of Object.entries(expected.secrets)) {
    const separator = id.lastIndexOf(":");
    const read = readers.get(id.slice(separator + 1));
    assert.ok(read, `unknown secret kind in ${id}`);
    assert.equal(read(id.slice(0, separator)), plaintext, id);
  }

  for (const [profileId, accounts] of Object.entries(expected.apiAccounts))
    assert.deepEqual(store.apiAccounts(profileId), accounts, profileId);
  for (const [profileId, accounts] of Object.entries(expected.accountMetadata))
    assert.deepEqual(store.accountMetadata(profileId), accounts, profileId);
  for (const { profileId, accountId, quota } of expected.quotas)
    assert.deepEqual(store.quota(profileId, accountId), quota, accountId);
  for (const [profileId, { count, records }] of Object.entries(
    expected.usage,
  )) {
    assert.equal(store.usage(profileId, "", 1e9).length, count, profileId);
    for (const { sequence, record } of records)
      assert.deepEqual(
        store.usageAfter(profileId, sequence - 1, sequence).records,
        [record],
        `${profileId} #${sequence}`,
      );
  }
  assert.equal(store.usageRetentionDays(), expected.retentionDays);
  const { identity, creditId, requestId } = expected.resetRequest;
  assert.equal(store.resetRequestId(identity, creditId), requestId);

  // Every stored body is exactly what JSON.stringify writes for it.
  for (const sql of [
    "SELECT body AS text FROM profiles",
    "SELECT body AS text FROM api_accounts",
    "SELECT body AS text FROM quotas",
    "SELECT body AS text FROM usage",
    "SELECT value AS text FROM settings WHERE id LIKE 'accounts:%'",
  ])
    for (const { text } of store.db.prepare(sql).all())
      assert.equal(JSON.stringify(JSON.parse(String(text))), String(text), sql);
} finally {
  store.close();
}
process.stdout.write(`The TypeScript store read ${directory} as expected.\n`);
