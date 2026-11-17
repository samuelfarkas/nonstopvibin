import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { Json } from "../src/shared/types.ts";
import { Application } from "../src/server/server.ts";
import { record, parse } from "../src/server/json.ts";
import { Store } from "../src/server/store.ts";
import { fileKeyCodec } from "../src/server/vault.ts";
import { resetCreditsSchema } from "../src/server/reset-credits.ts";

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
    const app = await Application.create({
      directory,
      binary: resolve(".vendor/core/cli-proxy-api"),
      clientDirectory: resolve("dist/client"),
      port: 0,
    });
    const profile = app.store.createProfile("Reset fixture");
    const other = app.store.createProfile("Other profile");
    const requests: string[] = [];
    let code = "reset";
    let fail = false;
    let quotaFails = false;
    let release: (() => void) | undefined;
    let gate: Promise<void> | undefined;
    try {
      await app.core.start(profile.id);
      const runtime = app.core.runtimes.get(profile.id)!;
      runtime.accounts = [
        {
          id: "fixture.json",
          name: "Synthetic Codex",
          provider: "codex",
          kind: "oauth",
          disabled: false,
          status: "Ready",
          priority: 0,
          authIndex: "fixture-index",
        },
      ];
      runtime.chatgptAccountIds.set("fixture.json", "fixture-identity");
      const management = app.core.management.bind(app.core);
      app.core.management = async (id, path, method, body) => {
        if (path !== "/api-call") return management(id, path, method, body);
        assert.equal(id, profile.id);
        assert.equal(method, "POST");
        const call = record(body);
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
      };
      const path = `/api/profiles/${profile.id}/accounts/fixture.json/reset-credits`;
      async function request(
        method = "GET",
        body?: Json,
        token = app.token,
        target = path,
      ) {
        return fetch(`${app.origin}${target}`, {
          method,
          headers: {
            Authorization: `Bearer ${token}`,
            "Content-Type": "application/json",
          },
          body: body === undefined ? undefined : JSON.stringify(body),
        });
      }
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
      const pending = app.core.consumeResetCredit(
        profile.id,
        "fixture.json",
        "credit-one",
      );
      await assert.rejects(
        app.core.consumeResetCredit(profile.id, "fixture.json", "credit-one"),
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
      runtime.accounts[0].provider = "claude";
      assert.equal((await request()).status, 400);
      runtime.accounts[0].provider = "codex";
      runtime.chatgptAccountIds.clear();
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
