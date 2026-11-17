import { z } from "zod";
import type { ApiAccount, Model } from "../shared/types.ts";
import { modelsFor } from "../shared/providers.ts";
import { AppError } from "./errors.ts";

const source = "https://models.dev/api.json";
const price = z.number().finite().nonnegative();
const rates = z.object({
  input: price,
  output: price,
  reasoning: price.optional(),
  cache_read: price.optional(),
  cache_write: price.optional(),
});
const definition = z.object({
  name: z.string().min(1).max(500),
  limit: z.object({
    context: z.number().int().positive(),
    output: z.number().int().positive(),
    input: z.number().int().positive().optional(),
  }),
  modalities: z.object({
    input: z.array(z.string()),
    output: z.array(z.string()),
  }),
  reasoning: z.boolean(),
  tool_call: z.boolean(),
  cost: rates
    .extend({
      tiers: z
        .array(
          rates.extend({
            tier: z.object({
              type: z.literal("context"),
              size: z.number().int().positive(),
            }),
          }),
        )
        .optional(),
    })
    .optional(),
});
const catalogSchema = z.record(
  z.string(),
  // Retain only fields needed by agents, not the catalog's full model payloads.
  z.object({
    models: z.record(z.string(), z.unknown()).transform((models) =>
      Object.fromEntries(
        Object.entries(models).flatMap(([id, value]) => {
          const parsed = definition.safeParse(value);
          return parsed.success ? [[id, parsed.data]] : [];
        }),
      ),
    ),
  }),
);
type Catalog = z.input<typeof catalogSchema>;
const providers = new Map([
  ["claude", "anthropic"],
  ["codex", "openai"],
  ["gemini", "google"],
  ["kimi", "kimi-for-coding"],
]);
const providerId = (id: string) => providers.get(id) ?? id;

// Match the routing provider and exact upstream ID. Never borrow a same-named
// model's pricing from another vendor or guess model versions from a prefix.
export function enrichModels(
  models: Model[],
  accounts: ApiAccount[],
  catalog: Catalog,
  fetchedAt: string,
): Model[] {
  return models.map((model) => {
    const routes = accounts
      .filter((account) => !account.disabled)
      .flatMap((account) =>
        account.models.flatMap(({ id }) =>
          model.id === (account.prefix ? `${account.prefix}/${id}` : id)
            ? [{ provider: providerId(account.provider), id }]
            : [],
        ),
      );
    if (!routes.length && model.owned_by)
      routes.push({ provider: providerId(model.owned_by), id: model.id });
    const matches = [
      ...new Set(routes.map((route) => `${route.provider}\n${route.id}`)),
    ];
    if (matches.length !== 1) return model;
    const [provider, id] = matches[0].split("\n");
    const parsed = definition.safeParse(catalog[provider]?.models[id]);
    if (!parsed.success) return model;
    const data = parsed.data;
    if (
      !data.modalities.input.includes("text") ||
      !data.modalities.output.includes("text")
    )
      return model;
    return {
      ...model,
      metadata: {
        name: data.name,
        context: data.limit.context,
        output: data.limit.output,
        inputLimit: data.limit.input,
        input: data.modalities.input.filter(
          (item): item is "text" | "image" =>
            item === "text" || item === "image",
        ),
        reasoning: data.reasoning,
        toolCall: data.tool_call,
        cost: data.cost,
        source: `${source}#${encodeURIComponent(provider)}/${encodeURIComponent(id)}`,
        fetchedAt,
      },
    };
  });
}

export class ModelCatalog {
  private cached?: { data: Catalog; at: number };
  private pending?: Promise<{ data: Catalog; at: number }>;
  private fetcher: typeof fetch;
  private nextActivityRefresh = 0;
  constructor(fetcher: typeof fetch = fetch) {
    this.fetcher = fetcher;
  }
  private async load() {
    if (this.cached && Date.now() - this.cached.at < 3_600_000)
      return this.cached;
    if (this.pending) return this.pending;
    this.pending = (async () => {
      const response = await this.fetcher(source, {
        redirect: "error",
        signal: AbortSignal.timeout(10_000),
      });
      if (!response.ok || !response.body)
        throw new Error("Catalog unavailable");
      const reader = response.body.getReader();
      const chunks: Uint8Array[] = [];
      let length = 0;
      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          length += value.length;
          if (length > 16_000_000) throw new Error("Catalog too large");
          chunks.push(value);
        }
      } finally {
        await reader.cancel();
      }
      const data = catalogSchema.parse(
        JSON.parse(Buffer.concat(chunks).toString("utf8")),
      );
      this.cached = { data, at: Date.now() };
      return this.cached;
    })()
      .catch(() => {
        throw new AppError(
          "Could not refresh model metadata from models.dev. Try again before connecting or restarting the agent.",
          502,
        );
      })
      .finally(() => {
        this.pending = undefined;
      });
    return this.pending;
  }
  async models(models: Model[], accounts: ApiAccount[]): Promise<Model[]> {
    const { data, at } = await this.load();
    return enrichModels(models, accounts, data, new Date(at).toISOString());
  }
  /** Activity never waits for the network. Failed refreshes back off for a minute. */
  snapshot(models: Model[]) {
    if (
      (!this.cached || Date.now() - this.cached.at >= 3_600_000) &&
      Date.now() >= this.nextActivityRefresh &&
      !this.pending
    ) {
      this.nextActivityRefresh = Date.now() + 60_000;
      void this.load().catch(() => {});
    }
    const cached = this.cached;
    const enrich = (entries: Model[]) =>
      cached
        ? enrichModels(
            entries,
            [],
            cached.data,
            new Date(cached.at).toISOString(),
          )
        : entries;
    return {
      models: enrich(models),
      enrich,
      fetchedAt: cached ? new Date(cached.at).toISOString() : null,
      refreshing: !!this.pending,
    };
  }
}

// Both agents need limits and prices per model. Image models, retired IDs
// and hidden helpers have none on models.dev; they are left out and the app
// lists them next to the connection, so nothing is omitted silently.
export function agentModels(models: Model[], agent: "pi" | "opencode") {
  const usable = models.filter(
    (model) => model.metadata?.toolCall && model.metadata.cost,
  );
  if (!usable.length)
    throw new AppError(
      models.length
        ? `No model in this profile has verified tool support and pricing on models.dev, so ${agent} cannot show any (${models.map((model) => model.id).join(", ")}).`
        : "This profile has no available models.",
    );
  models = usable;
  // pi speaks each vendor's native protocol through the gateway so thinking
  // levels and tool calls are not translated twice.
  const piApi = (model: Model) =>
    modelsFor("claude", [model], false).length
      ? "anthropic-messages"
      : modelsFor("codex", [model], false).length
        ? "openai-responses"
        : "openai-completions";
  return models.map((model) => {
    const { id, metadata: m } = model;
    if (!m?.cost) throw new AppError("Model metadata is unavailable.");
    const name = `${m.name} · API list price${agent === "opencode" && m.cost.tiers?.length ? " (base rate; tiered pricing unavailable in this client)" : ""}${m.cost.cache_read === undefined || m.cost.cache_write === undefined ? " (cache pricing unavailable)" : ""}`;
    return agent === "pi"
      ? {
          id,
          name,
          api: piApi(model),
          reasoning: m.reasoning,
          input: m.input,
          contextWindow: m.context,
          maxTokens: m.output,
          cost: {
            input: m.cost.input,
            output: m.cost.output,
            ...(m.cost.cache_read !== undefined && {
              cacheRead: m.cost.cache_read,
            }),
            ...(m.cost.cache_write !== undefined && {
              cacheWrite: m.cost.cache_write,
            }),
            ...(m.cost.tiers && {
              tiers: m.cost.tiers.map((t) => ({
                inputTokensAbove: t.tier.size,
                input: t.input,
                output: t.output,
                ...(t.cache_read !== undefined && { cacheRead: t.cache_read }),
                ...(t.cache_write !== undefined && {
                  cacheWrite: t.cache_write,
                }),
              })),
            }),
          },
        }
      : {
          id,
          name,
          reasoning: m.reasoning,
          tool_call: m.toolCall,
          modalities: { input: m.input, output: ["text"] },
          limit: { context: m.context, input: m.inputLimit, output: m.output },
          cost: m.cost,
        };
  });
}
