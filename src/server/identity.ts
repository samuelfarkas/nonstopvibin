import { z } from "zod";
import type { Json, SeatIdentity } from "../shared/types.ts";
import type { OrganizationProvider } from "../shared/providers.ts";
import { AppError } from "./errors.ts";
import { record, responseJson, text, type JsonObject } from "./json.ts";

const uuid = z
  .string()
  .trim()
  .uuid()
  .transform((value) => value.toLowerCase());
const label = z
  .string()
  .trim()
  .min(1)
  .max(200)
  .regex(/^[^\p{Cc}]+$/u);

/** The beta flag Anthropic's OAuth account endpoints require. */
export const claudeOAuthBeta = "oauth-2025-04-20";

// Credential files may predate organization metadata. Missing identity never matches another seat.
export function claudeIdentity(value: Json | undefined): SeatIdentity {
  const raw = record(value);
  return {
    accountUuid: uuid.safeParse(raw.account_uuid).data,
    organizationUuid: uuid.safeParse(raw.organization_uuid).data,
    organizationName: label.safeParse(raw.organization_name).data,
  };
}

export function sameSeat(a: SeatIdentity, b: SeatIdentity): boolean {
  return !!(
    a.accountUuid &&
    a.organizationUuid &&
    a.accountUuid === b.accountUuid &&
    a.organizationUuid === b.organizationUuid
  );
}

/** Asks Claude whose token this is: a copy of the credential with that seat and email. */
export async function verifyClaudeIdentity(
  raw: JsonObject,
): Promise<JsonObject> {
  const token = text(raw.access_token);
  if (!token)
    throw new AppError(
      "Claude did not return a usable credential. Sign in again.",
      502,
    );
  let response: Response;
  try {
    response = await fetch("https://api.anthropic.com/api/oauth/profile", {
      headers: {
        Authorization: `Bearer ${token}`,
        "anthropic-beta": claudeOAuthBeta,
      },
      redirect: "error",
      signal: AbortSignal.timeout(15_000),
    });
  } catch {
    throw new AppError(
      "Could not verify your Claude organization. Check your connection and try again.",
      502,
    );
  }
  if (!response.ok)
    throw new AppError(
      `Claude could not verify this organization (HTTP ${response.status}). Sign in again.`,
      502,
    );
  let profile: JsonObject;
  try {
    profile = record(await responseJson(response));
  } catch {
    throw new AppError(
      "Claude returned an unreadable account profile. Try again.",
      502,
    );
  }
  const account = record(profile.account);
  const organization = record(profile.organization);
  const identity = claudeIdentity({
    account_uuid: account.uuid,
    organization_uuid: organization.uuid,
    organization_name: organization.name,
  });
  if (!identity.accountUuid || !identity.organizationUuid)
    throw new AppError(
      "Claude did not identify the account and organization. Sign in again and select your subscription.",
      502,
    );
  // Use the identity returned for this token, not imported labels or the browser's active organization.
  const email = label.safeParse(account.email).data;
  return {
    ...raw,
    account_uuid: identity.accountUuid,
    organization_uuid: identity.organizationUuid,
    organization_name: identity.organizationName,
    ...(email && { email }),
  };
}

/** Who a signed-in token belongs to, asked of its own provider: one per provider with organizations. */
export const verifySeat = {
  claude: verifyClaudeIdentity,
} satisfies Record<
  OrganizationProvider,
  (raw: JsonObject) => Promise<JsonObject>
>;
