import { providerEntry, type Protocol } from "../shared/providers.ts";
import { userAgent } from "./config.ts";
import { AppError } from "./errors.ts";
import { record, responseJson, text } from "./json.ts";
import type { ApiAccount } from "./store.ts";

// OpenCode Go publishes different wire protocols per model. Prefer live endpoint metadata.
export function goModelProtocol(id: string, endpoint?: string): Protocol {
  if (endpoint?.includes("/messages")) return "anthropic";
  if (endpoint?.includes("/responses")) return "responses";
  if (endpoint?.includes("/chat/completions")) return "openai";
  if (/^(gpt-|grok-|muse-)/.test(id)) return "responses";
  if (/^(minimax-|qwen)/.test(id)) return "anthropic";
  return "openai";
}

export async function discoverModels(
  input: Pick<ApiAccount, "provider" | "baseUrl"> & {
    apiKey: string;
    protocol: Protocol;
  },
): Promise<ApiAccount["models"]> {
  const preset = providerEntry(input.provider).preset;
  const response = await fetch(`${input.baseUrl.replace(/\/$/, "")}/models`, {
    headers: {
      ...(input.provider === "anthropic" || input.protocol === "anthropic"
        ? {
            "x-api-key": input.apiKey,
            "anthropic-version": "2023-06-01",
          }
        : { Authorization: `Bearer ${input.apiKey}` }),
      "User-Agent": userAgent,
    },
    redirect: "error",
    signal: AbortSignal.timeout(20_000),
  });
  if (!response.ok)
    throw new AppError(
      `Model discovery returned HTTP ${response.status}. Verify the API key, or enter model IDs manually.`,
      502,
    );
  const data = record(await responseJson(response));
  return Array.isArray(data.data)
    ? data.data
        .flatMap((raw) => {
          const r = record(raw);
          const id = text(r.id);
          return id === undefined
            ? []
            : [
                {
                  id,
                  protocol: preset?.fixedEndpoint
                    ? goModelProtocol(id, text(r.endpoint))
                    : input.provider === "anthropic"
                      ? ("anthropic" as const)
                      : input.protocol,
                },
              ];
        })
        .slice(0, 500)
    : [];
}
