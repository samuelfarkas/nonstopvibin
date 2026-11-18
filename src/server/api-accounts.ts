import { randomUUID } from "node:crypto";
import type { ApiAccountInput } from "../shared/api.ts";
import {
  providerEntry,
  type Protocol,
  type Upstream,
} from "../shared/providers.ts";
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
/** How each model of a preset with `upstream` picks its protocol. */
const modelProtocols = {
  "opencode-go": goModelProtocol,
} satisfies Record<Upstream, (id: string, endpoint?: string) => Protocol>;

function providerURL(raw: string): URL {
  const url = new URL(raw);
  if (
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    (url.protocol !== "https:" &&
      !(
        url.protocol === "http:" &&
        ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)
      ))
  )
    throw new AppError(
      "Use an HTTPS base URL, or HTTP for a local provider. Do not include credentials or query parameters in the URL.",
    );
  return url;
}

/** The account `input` describes, with its models entered or discovered; nothing is saved. */
export async function apiAccount(input: ApiAccountInput): Promise<ApiAccount> {
  providerURL(input.baseUrl);
  const preset = providerEntry(input.provider).preset;
  if (
    preset?.fixedEndpoint &&
    input.baseUrl.replace(/\/$/, "") !== preset.baseUrl
  )
    throw new AppError(`${preset.name} must use its official endpoint.`);
  const upstream = preset?.upstream;
  let models = upstream
    ? input.models.map((m) => ({
        ...m,
        protocol: modelProtocols[upstream](m.id),
      }))
    : input.models;
  if (!models.length) models = await discoverModels(input);
  if (!models.length)
    throw new AppError(
      "No models were found. Enter the model IDs your provider supports.",
    );
  return {
    id: randomUUID(),
    name: input.name,
    provider: input.provider,
    baseUrl: input.baseUrl.replace(/\/$/, ""),
    prefix: input.prefix,
    models,
    disabled: false,
  };
}

async function discoverModels(
  input: Pick<ApiAccount, "provider" | "baseUrl"> & {
    apiKey: string;
    protocol: Protocol;
  },
): Promise<ApiAccount["models"]> {
  const upstream = providerEntry(input.provider).preset?.upstream;
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
                  protocol: upstream
                    ? modelProtocols[upstream](id, text(r.endpoint))
                    : input.provider === "anthropic"
                      ? ("anthropic" as const)
                      : input.protocol,
                },
              ];
        })
        .slice(0, 500)
    : [];
}
