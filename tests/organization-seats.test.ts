import { test } from "node:test";
import assert from "node:assert/strict";
import {
  mkdtemp,
  readFile,
  readdir,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { CorePool } from "../src/server/core.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { claudeIdentity, sameSeat } from "../src/server/identity.ts";
import { ImportCatalog } from "../src/server/imports.ts";
import { accountLabel } from "../src/client/format.ts";
import type { OAuthSession, Profile } from "../src/shared/types.ts";

const accountUuid = "abcdef01-2345-4678-9abc-def012345678";
const organizationA = "00000000-0000-4000-8000-000000000002";
const organizationB = "00000000-0000-4000-8000-000000000003";
const credential = (organization: string, token = organization) => ({
  type: "claude",
  email: "same@example.invalid",
  account_uuid: accountUuid,
  organization_uuid: organization,
  organization_name: organization === organizationA ? "Example A" : "Example B",
  access_token: `synthetic-${token}`,
  expired: "2099-01-01T00:00:00Z",
});

test("seat matching requires both UUIDs, independent of email, display names, and tokens", () => {
  const ids = [accountUuid, organizationA, organizationB];
  for (const subject of ids)
    for (const organization of ids) {
      const identity = claudeIdentity({
        account_uuid: subject,
        organization_uuid: organization,
      });
      for (const otherSubject of ids)
        for (const otherOrganization of ids) {
          assert.equal(
            sameSeat(
              identity,
              claudeIdentity({
                account_uuid: otherSubject,
                organization_uuid: otherOrganization,
              }),
            ),
            subject === otherSubject && organization === otherOrganization,
          );
        }
      assert.equal(sameSeat(identity, { accountUuid: subject }), false);
      assert.equal(
        sameSeat(identity, { organizationUuid: organization }),
        false,
      );
    }
  assert.equal(sameSeat({}, {}), false);
  for (const invalid of [
    null,
    0,
    "",
    "same@example.invalid",
    "../credential",
    {},
    [],
  ]) {
    assert.equal(
      claudeIdentity({ account_uuid: invalid, organization_uuid: invalid })
        .organizationUuid,
      undefined,
    );
  }
  assert.equal(
    claudeIdentity({ organization_name: "Injected\nname" }).organizationName,
    undefined,
  );
  assert.equal(
    claudeIdentity({ account_uuid: accountUuid.toUpperCase() }).accountUuid,
    accountUuid,
  );
});

test(
  "same-email seats stay separate through staged sign-in, reconnect, imports, restart, and cleanup",
  { timeout: 60_000 },
  async (t) => {
    const directory = await mkdtemp(join(tmpdir(), "nonstopvibin-seats-"));
    const store = new Store(directory, fileKeyCodec(directory));
    const core = new CorePool(store, resolve(".vendor/core/cli-proxy-api"));
    const first = store.createProfile("First profile");
    const second = store.createProfile("Second profile");
    const originalManagement = CorePool.prototype.management;
    const originalFetch = globalThis.fetch;
    const completed = new WeakSet<CorePool>();
    let nextCredential = credential(organizationA);
    let profileUnavailable = false;
    // Only the external OAuth exchange/profile service is simulated. File storage,
    // staging processes, management uploads, watchers, and account synchronization use the verified core.
    CorePool.prototype.management = async function (
      profileId,
      path,
      method,
      body,
    ) {
      if (path === "/oauth-callback") {
        await originalManagement.call(
          this,
          profileId,
          "/auth-files?name=claude-same%40example.invalid.json",
          "POST",
          nextCredential,
        );
        completed.add(this);
        return { status: "ok" };
      }
      if (path.startsWith("/get-auth-status") && completed.has(this))
        return { status: "ok" };
      if (path === "/api-call")
        return {
          status_code: 200,
          body: { five_hour: { utilization: 25 } },
        };
      return originalManagement.call(this, profileId, path, method, body);
    };
    globalThis.fetch = Object.assign(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const url =
          input instanceof URL
            ? input.href
            : input instanceof Request
              ? input.url
              : input;
        if (url === "https://api.anthropic.com/api/oauth/profile") {
          assert.equal(
            new Headers(init?.headers).get("Authorization"),
            `Bearer ${nextCredential.access_token}`,
          );
          if (profileUnavailable)
            return new Response("synthetic private provider body", {
              status: 503,
            });
          return Response.json({
            account: { uuid: accountUuid, email: "same@example.invalid" },
            organization: {
              uuid: nextCredential.organization_uuid,
              name: nextCredential.organization_name,
            },
          });
        }
        return originalFetch(input, init);
      },
      originalFetch,
    );
    async function authorize(
      profile: Profile,
      raw = credential(organizationA),
      reconnectAccountId?: string,
    ) {
      nextCredential = raw;
      const session = await core.beginOAuth(
        profile.id,
        "claude",
        reconnectAccountId,
      );
      const callback = new URL(
        new URL(session.url).searchParams.get("redirect_uri")!,
      );
      callback.searchParams.set("state", session.state);
      callback.searchParams.set("code", "synthetic-code");
      await core.oauthCallback(profile.id, callback.href);
      return session;
    }
    async function confirm(profile: Profile, session: OAuthSession) {
      const review = await core.oauthStatus(profile.id, session.state);
      assert.equal(review.status, "review");
      const result = await core.confirmOAuth(profile.id, session.state);
      assert.equal(result.status, "ok");
      assert.ok(result.status === "ok" && result.account);
      if (!result.account.disabled)
        assert.equal(result.account.quota?.windows[0]?.remainingPercent, 75);
      return result.account;
    }
    try {
      await core.verifyBinary();
      let firstId = "";
      let secondId = "";
      let firstIndex = "";
      await t.test(
        "a pending organization is invisible to active profiles until confirmation",
        async () => {
          const session = await authorize(first);
          assert.equal(core.accounts(first.id).length, 0);
          const review = await core.oauthStatus(first.id, session.state);
          assert.ok(review.status === "review");
          assert.equal(review.review.account.organizationName, "Example A");
          assert.equal(review.review.action, "add");
          assert.equal(core.accounts(first.id).length, 0);
          assert.ok(
            !JSON.stringify(review).includes(nextCredential.access_token),
          );
          await assert.rejects(
            core.confirmOAuth(second.id, session.state),
            /not found for this profile/,
          );
          const account = await confirm(first, session);
          firstId = account.id;
          firstIndex = account.authIndex!;
          assert.ok(!firstId.includes("@"));
          assert.equal(accountLabel(account), "Example A");
          assert.equal(
            (await stat(join(core.directory(first.id), "auth", firstId))).mode &
              0o777,
            0o600,
          );
          assert.deepEqual(await readdir(join(directory, "oauth-pending")), []);
        },
      );
      await t.test(
        "a second organization with the same email adds a separate connection",
        async () => {
          const session = await authorize(first, credential(organizationB));
          const account = await confirm(first, session);
          secondId = account.id;
          assert.notEqual(secondId, firstId);
          assert.notEqual(account.authIndex, firstIndex);
          assert.equal(core.accounts(first.id).length, 2);
          assert.equal(
            new Set(core.accounts(first.id).map((a) => a.email)).size,
            1,
          );
        },
      );
      await t.test(
        "reconnecting preserves the ID, paused state, custom label, priority, quota, and the other seat",
        async () => {
          await core.setAccount(first.id, firstId, {
            disabled: true,
            name: "Work seat",
            priority: 7,
          });
          store.saveQuota(first.id, firstId, {
            status: "available",
            windows: [],
            checkedAt: "2026-09-06T00:00:00Z",
          });
          const otherBefore = await readFile(
            join(core.directory(first.id), "auth", secondId),
            "utf8",
          );
          const session = await authorize(
            first,
            credential(organizationA, "reauthorized"),
            firstId,
          );
          const review = await core.oauthStatus(first.id, session.state);
          assert.ok(review.status === "review");
          assert.equal(review.review.action, "reconnect");
          const oldProcess = core.runtimes.get(first.id)!.child;
          const account = await confirm(first, session);
          assert.ok(
            oldProcess.exitCode !== null || oldProcess.signalCode !== null,
          );
          assert.notEqual(
            core.runtimes.get(first.id)!.child.pid,
            oldProcess.pid,
          );
          assert.equal(account.id, firstId);
          assert.equal(account.authIndex, firstIndex);
          assert.equal(account.disabled, true);
          assert.equal(account.name, "Work seat");
          assert.equal(account.priority, 7);
          assert.equal(account.quota?.checkedAt, "2026-09-06T00:00:00Z");
          assert.equal(
            await readFile(
              join(core.directory(first.id), "auth", secondId),
              "utf8",
            ),
            otherBefore,
          );
          assert.equal(core.accounts(first.id).length, 2);
        },
      );
      await t.test(
        "reconnecting a stopped profile preserves its stopped state",
        async () => {
          await core.stop(first.id);
          const session = await authorize(
            first,
            credential(organizationA, "stopped-reconnect"),
            firstId,
          );
          const account = await confirm(first, session);
          assert.equal(account.id, firstId);
          assert.equal(account.authIndex, firstIndex);
          assert.equal(core.runtimes.get(first.id)!.state, "stopped");
          assert.equal(store.profile(first.id).enabled, false);
          assert.equal(account.disabled, true);
          await core.start(first.id);
        },
      );
      await t.test(
        "wrong organization on reconnect is blocked before any credential is replaced",
        async () => {
          const before = await readFile(
            join(core.directory(first.id), "auth", firstId),
            "utf8",
          );
          const session = await authorize(
            first,
            credential(organizationB, "wrong-seat"),
            firstId,
          );
          const result = await core.oauthStatus(first.id, session.state);
          assert.ok(result.status === "review");
          assert.equal(result.review.action, "blocked");
          await assert.rejects(
            core.confirmOAuth(first.id, session.state),
            /not the organization/,
          );
          assert.equal(
            await readFile(
              join(core.directory(first.id), "auth", firstId),
              "utf8",
            ),
            before,
          );
          await core.cancelOAuth(first.id, session.state);
        },
      );
      await t.test(
        "the same seat can be added and reconnected independently in another profile",
        async () => {
          const before = await readFile(
            join(core.directory(first.id), "auth", firstId),
            "utf8",
          );
          const session = await authorize(
            second,
            credential(organizationA, "another-token"),
          );
          const result = await core.oauthStatus(second.id, session.state);
          assert.ok(result.status === "review");
          assert.equal(result.review.action, "add");
          const account = await confirm(second, session);
          assert.notEqual(account.id, firstId);
          assert.equal(account.disabled, false);
          const reconnect = await authorize(
            second,
            credential(organizationA, "reconnect-second"),
            account.id,
          );
          assert.equal((await confirm(second, reconnect)).id, account.id);
          await assert.rejects(
            core.importAuth(
              second.id,
              credential(organizationA, "independent-token"),
            ),
            /already connected/,
          );
          const imported = await core.importAuth(
            second.id,
            credential(organizationB),
          );
          assert.notEqual(imported.id, secondId);
          assert.equal(imported.quota?.windows[0]?.remainingPercent, 75);
          assert.equal(
            await readFile(
              join(core.directory(first.id), "auth", firstId),
              "utf8",
            ),
            before,
          );
          assert.equal(core.accounts(first.id).length, 2);
          assert.equal(core.accounts(second.id).length, 2);
        },
      );
      await t.test(
        "identical Codex credentials can be imported into multiple profiles but only once per profile",
        async () => {
          const raw = {
            type: "codex",
            access_token: "synthetic-codex-shared",
            refresh_token: "synthetic-refresh-shared",
            expired: "2099-01-01T00:00:00Z",
          };
          const profiles = [
            store.createProfile("Codex A"),
            store.createProfile("Codex B"),
          ];
          const accounts = [];
          for (const profile of profiles) {
            accounts.push(await core.importAuth(profile.id, raw));
            await assert.rejects(
              core.importAuth(profile.id, raw),
              /already connected/,
            );
            assert.equal(core.accounts(profile.id).length, 1);
          }
          assert.notEqual(accounts[0]!.id, accounts[1]!.id);
        },
      );
      await t.test(
        "provider verification can be retried without starting another authorization",
        async () => {
          const session = await authorize(
            first,
            credential(organizationB, "retry-token"),
          );
          profileUnavailable = true;
          await assert.rejects(
            core.oauthStatus(first.id, session.state),
            (error: Error) =>
              /HTTP 503/.test(error.message) &&
              !error.message.includes("private provider body"),
          );
          assert.ok(core.oauth);
          profileUnavailable = false;
          const result = await core.oauthStatus(first.id, session.state);
          assert.equal(result.status, "review");
          await core.cancelOAuth(first.id, session.state);
        },
      );
      await t.test(
        "cancellation and expiry reject late callbacks and leave no staging credentials",
        async () => {
          const session = await authorize(
            first,
            credential(organizationB, "cancelled"),
          );
          await core.cancelOAuth(first.id, session.state);
          await assert.rejects(
            core.oauthCallback(
              first.id,
              `http://localhost/?state=${session.state}`,
            ),
            /not found/,
          );
          assert.deepEqual(await readdir(join(directory, "oauth-pending")), []);
          const expired = await core.beginOAuth(first.id, "claude");
          core.oauth!.expiresAt = Date.now() - 1;
          await assert.rejects(
            core.confirmOAuth(first.id, expired.state),
            /expired/,
          );
          const next = await core.beginOAuth(first.id, "claude");
          await core.cancelOAuth(first.id, next.state);
          assert.deepEqual(await readdir(join(directory, "oauth-pending")), []);
        },
      );
      await t.test(
        "unrelated malformed JSON does not break account display",
        async () => {
          const path = join(core.directory(first.id), "auth", "unrelated.json");
          await writeFile(path, "malformed", { mode: 0o600 });
          try {
            await core.syncAccounts(first.id);
            assert.equal(core.accounts(first.id).length, 2);
            assert.equal(
              core.accounts(first.id).find((a) => a.id === firstId)
                ?.organizationUuid,
              organizationA,
            );
          } finally {
            await rm(path);
          }
        },
      );
      await t.test(
        "organization metadata is available in imports and persists across restart without renaming",
        async () => {
          const catalog = new ImportCatalog([
            join(core.directory(first.id), "auth"),
          ]);
          const sources = await catalog.scan();
          assert.equal(sources.length, 2);
          assert.deepEqual(
            new Set(sources.map((s) => s.organizationName)),
            new Set(["Example A", "Example B"]),
          );
          assert.ok(!JSON.stringify(sources).includes("synthetic-"));
          await core.stop(first.id);
          await core.start(first.id);
          assert.deepEqual(
            new Set(core.accounts(first.id).map((a) => a.id)),
            new Set([firstId, secondId]),
          );
          assert.equal(
            core.accounts(first.id).find((a) => a.id === firstId)?.authIndex,
            firstIndex,
          );
          assert.equal(
            store.accountMetadata(first.id).find((a) => a.id === secondId)
              ?.organizationUuid,
            organizationB,
          );
        },
      );
      await t.test(
        "shutdown cleans up pending sign-in even after its core has stopped",
        async () => {
          await core.start(second.id);
          await core.beginOAuth(first.id, "codex");
          await core.stop(first.id);
          await core.shutdown();
          assert.equal(core.oauth, undefined);
          assert.equal(core.runtimes.get(second.id)!.state, "stopped");
        },
      );
    } finally {
      await core.shutdown();
      CorePool.prototype.management = originalManagement;
      globalThis.fetch = originalFetch;
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);
