import { z } from "zod";
import type { CatalogModel, Model } from "../shared/types.ts";
import { providerEntry } from "../shared/providers.ts";
import { AppError } from "./errors.ts";
import type { ApiAccount, Store } from "./store.ts";

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
const providerId = (id: string) => providerEntry(id)?.catalog ?? id;

// Match the routing provider and exact upstream ID. Never borrow a same-named
// model's pricing from another vendor or guess model versions from a prefix.
export function enrichModels(
  models: Model[],
  accounts: ApiAccount[],
  catalog: Catalog,
  fetchedAt: string,
): CatalogModel[] {
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
  async models(
    models: Model[],
    accounts: ApiAccount[],
  ): Promise<CatalogModel[]> {
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
    const enrich = (entries: Model[]): CatalogModel[] =>
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

/** The application's models.dev catalog, applied to profiles' model lists. */
export class ProfileModels {
  readonly catalog: ModelCatalog;
  private readonly store: Store;
  constructor(store: Store, catalog = new ModelCatalog()) {
    this.store = store;
    this.catalog = catalog;
  }
  /** A profile's models as its core lists them, with models.dev metadata matched through its API accounts. */
  enriched(profileId: string, models: Model[]): Promise<CatalogModel[]> {
    return this.catalog.models(models, this.store.apiAccounts(profileId));
  }
}
