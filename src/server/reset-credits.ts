import { z } from "zod";
import type { ResetCredits, ResetResult } from "../shared/types.ts";
import { providerEntry } from "../shared/providers.ts";
import { resetResultCodes } from "../shared/vocabulary.ts";
import type { Accounts } from "./accounts.ts";
import type { CorePool } from "./core.ts";
import { AppError } from "./errors.ts";
import { unwrap } from "./json.ts";
import { callUpstream, codexApi, codexHeader, type Quotas } from "./quota.ts";
import type { Store } from "./store.ts";

// OpenAI's backend-client/src/types.rs; these are ChatGPT subscription APIs.
export const resetCreditsSchema = z.object({
  available_count: z.number().int().nonnegative(),
  credits: z.array(
    z.object({
      id: z.string().min(1).max(500),
      reset_type: z.string(),
      status: z.string(),
      expires_at: z.string().datetime({ offset: true }).nullable(),
      title: z.string().nullable().optional(),
      description: z.string().nullable().optional(),
    }),
  ),
}) satisfies z.ZodType<ResetCredits>;
export const resetResultSchema = z.object({
  code: z.enum(resetResultCodes),
});

export class CodexResets {
  private readonly pool: CorePool;
  private readonly store: Store;
  private readonly accounts: Accounts;
  private readonly quotas: Quotas;
  private readonly redeeming = new Set<string>();
  constructor(pool: CorePool, accounts: Accounts, quotas: Quotas) {
    this.pool = pool;
    this.store = accounts.store;
    this.accounts = accounts;
    this.quotas = quotas;
  }
  private account(profileId: string, accountId: string) {
    if (!this.pool.usable(profileId))
      throw new AppError("Start this profile first.", 409);
    const account = this.accounts
      .oauth(profileId)
      .find((a) => a.id === accountId);
    if (!account) throw new AppError("Account not found in this profile.", 404);
    if (
      !providerEntry(account.provider)?.bankedResets ||
      account.kind !== "oauth"
    )
      throw new AppError("Banked resets require a Codex subscription.");
    const identity = this.accounts.chatgptAccountId(profileId, accountId);
    if (!identity || !account.authIndex)
      throw new AppError(
        "Reconnect this subscription to verify its account identity.",
        409,
      );
    return { account, identity };
  }
  private async call(profileId: string, accountId: string, creditId?: string) {
    const { account, identity } = this.account(profileId, accountId);
    const { status, body } = await callUpstream(
      this.pool,
      profileId,
      account.authIndex,
      {
        method: creditId ? "POST" : "GET",
        url: `${codexApi}/rate-limit-reset-credits${creditId ? "/consume" : ""}`,
        header: codexHeader(identity),
        data: creditId
          ? JSON.stringify({
              credit_id: creditId,
              redeem_request_id: this.store.resetRequestId(identity, creditId),
            })
          : undefined,
      },
    );
    if (status !== 200)
      throw new AppError(
        `Codex returned HTTP ${status ?? "unknown"}. Retry the same reset; reconnect if authorization has expired.`,
        502,
      );
    try {
      return unwrap(body);
    } catch {
      throw new AppError(
        "Codex returned an unreadable reset response. Retry the same reset.",
        502,
      );
    }
  }
  async credits(profileId: string, accountId: string) {
    const result = resetCreditsSchema.safeParse(
      await this.call(profileId, accountId),
    );
    if (!result.success)
      throw new AppError("Codex reset information is unavailable.", 502);
    return result.data;
  }
  async consume(
    profileId: string,
    accountId: string,
    creditId: string,
  ): Promise<ResetResult> {
    const { identity } = this.account(profileId, accountId);
    if (this.redeeming.has(identity))
      throw new AppError(
        "A reset is already in progress for this subscription.",
        409,
      );
    this.redeeming.add(identity);
    try {
      const result = resetResultSchema.safeParse(
        await this.call(profileId, accountId, creditId),
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
          (await this.quotas.refresh(profileId, accountId)).status ===
          "available";
      } catch {
        // Redemption is settled even if a concurrent refresh or account removal wins.
      }
      return { ...result.data, quotaRefreshed };
    } finally {
      this.redeeming.delete(identity);
    }
  }
}
