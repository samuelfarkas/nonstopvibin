import { z } from "zod";
import type { ResetCredits } from "../shared/types.ts";
import { resetResultCodes } from "../shared/vocabulary.ts";

// OpenAI's backend-client/src/types.rs; these are ChatGPT subscription APIs.
// Checked against the renderer's dependency-free shared type: every field it
// reads must be validated here with the same type.
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
