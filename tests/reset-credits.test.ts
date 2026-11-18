import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Json } from "../src/shared/types.ts";
import { httpManagement } from "../src/server/core.ts";
import { record, parse } from "../src/server/json.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { resetCreditsSchema } from "../src/server/reset-credits.ts";
import { management, testApplication } from "./support/application.ts";

const credits = {
  available_count: 1,
  credits: [
    {
      id: "credit-one",
      reset_type: "full",
      status: "available",
      expires_at: "2026-12-01T00:00:00Z",
      title: "Full reset",
      description: null,
    },
  ],
};

test(
  "Codex reset routes preserve identity, reject agent keys, and retry uncertain redemption durably",
  { timeout: 15000 },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "nv-resets-"));
    const requests: string[] = [];
    let code = "reset";
    let fail = false;
    let quotaFails = false;
    let release: (() => void) | undefined;
    let gate: Promise<void> | undefined;
    // The core's account list for the fixture profile: one Codex subscription.
    let accountFile: Json = {
      name: "fixture.json",
      provider: "codex",
      auth_index: "fixture-index",
      id_token: { chatgpt_account_id: "fixture-identity" },
    };
    const app = await testApplication(directory, {
      cores: {
        management: async (request) => {
          if (request.path === "/auth-files") return { files: [accountFile] };
          if (request.path !== "/api-call") return httpManagement(request);
          assert.equal(request.profileId, profile.id);
          assert.equal(request.method, "POST");
          const call = record(request.body);
          assert.equal(call.auth_index, "fixture-index");
          assert.equal(
            record(call.header)["Chatgpt-Account-Id"],
            "fixture-identity",
          );
          assert.equal(record(call.header).Authorization, "Bearer $TOKEN$");
          if (call.url === "https://chatgpt.com/backend-api/wham/usage") {
            return {
              status_code: quotaFails ? 503 : 200,
              body: JSON.stringify({
                rate_limit: {
                  primary_window: {
                    used_percent: 0,
                    limit_window_seconds: 18000,
                  },
                },
              }),
            };
          }
          if (call.method === "GET") {
            assert.equal(
              call.url,
              "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits",
            );
            return { status_code: 200, body: JSON.stringify(credits) };
          }
          assert.equal(
            call.url,
            "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume",
          );
          assert.equal(call.method, "POST");
          const payload = record(parse(String(call.data)));
          assert.equal(payload.credit_id, "credit-one");
          requests.push(String(payload.redeem_request_id));
          if (gate) await gate;
          return fail
            ? { status_code: 503, body: "sensitive upstream error" }
            : { status_code: 200, body: JSON.stringify({ code }) };
        },
      },
    });
    const profile = app.store.createProfile("Reset fixture");
    const other = app.store.createProfile("Other profile");
    try {
      await app.cores.pool.start(profile.id);
      const path = `/profiles/${profile.id}/accounts/fixture.json/reset-credits`;
      const request = (
        method = "GET",
        body?: Json,
        token = app.token,
        target = path,
      ) => management(app, target, { method, body, token });
      assert.equal(
        (
          await request(
            "POST",
            { creditId: "credit-one" },
            app.store.clientKey(profile.id),
          )
        ).status,
        401,
      );
      assert.equal(
        (
          await request("POST", {
            creditId: "credit-one",
            url: "https://evil.invalid",
          })
        ).status,
        400,
      );
      assert.equal((await request("POST", { creditId: "" })).status, 400);
      assert.equal(
        (
          await request(
            "GET",
            undefined,
            app.token,
            path.replace(profile.id, other.id),
          )
        ).status,
        409,
      );
      assert.equal((await request("DELETE")).status, 405);
      assert.deepEqual(await (await request()).json(), credits);
      assert.equal(requests.length, 0);
      fail = true;
      const uncertain = await request("POST", { creditId: "credit-one" });
      assert.equal(uncertain.status, 502);
      assert.doesNotMatch(await uncertain.text(), /sensitive upstream/);
      const reopened = new Store(directory, fileKeyCodec(directory));
      assert.equal(
        reopened.resetRequestId("fixture-identity", "credit-one"),
        requests[0],
      );
      assert.notEqual(
        reopened.resetRequestId("other-identity", "credit-one"),
        requests[0],
      );
      reopened.close();
      fail = false;
      gate = new Promise<void>((resolve) => {
        release = resolve;
      });
      const pending = app.cores.resets.consume(
        profile.id,
        "fixture.json",
        "credit-one",
      );
      await assert.rejects(
        app.cores.resets.consume(profile.id, "fixture.json", "credit-one"),
        /already in progress/,
      );
      release!();
      assert.deepEqual(await pending, { code: "reset", quotaRefreshed: true });
      gate = undefined;
      assert.equal(requests[0], requests[1]);
      code = "already_redeemed";
      quotaFails = true;
      assert.deepEqual(
        await (await request("POST", { creditId: "credit-one" })).json(),
        { code, quotaRefreshed: false },
      );
      assert.equal(requests[2], requests[0]);
      code = "unexpected";
      assert.equal(
        (await request("POST", { creditId: "credit-one" })).status,
        502,
      );
      assert.equal(
        app.store.resetRequestId("fixture-identity", "credit-one"),
        requests[0],
      );
      code = "nothing_to_reset";
      assert.equal(
        (await request("POST", { creditId: "credit-one" })).status,
        200,
      );
      assert.notEqual(
        app.store.resetRequestId("fixture-identity", "credit-one"),
        requests[0],
      );
      accountFile = { ...record(accountFile), provider: "claude" };
      await app.cores.accounts.sync(profile.id);
      assert.equal((await request()).status, 400);
      accountFile = {
        name: "fixture.json",
        provider: "codex",
        auth_index: "fixture-index",
      };
      await app.cores.accounts.sync(profile.id);
      assert.equal((await request()).status, 409);
      assert.equal(
        resetCreditsSchema.safeParse({ ...credits, available_count: -1 })
          .success,
        false,
      );
      assert.equal(
        resetCreditsSchema.safeParse({
          ...credits,
          credits: [{ ...credits.credits[0], expires_at: "invalid" }],
        }).success,
        false,
      );
    } finally {
      release?.();
      await app.close();
      await rm(directory, { recursive: true, force: true });
    }
  },
);
