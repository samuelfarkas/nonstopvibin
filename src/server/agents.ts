import { join } from "node:path";
import { agents, type Agent } from "../shared/agents.ts";
import type { CatalogModel, Model } from "../shared/types.ts";
import { modelsFor, nativeAgent } from "../shared/providers.ts";
import { AppError } from "./errors.ts";
import type { AgentDefinition } from "./agent-connection.ts";
import { claude } from "./agent-claude.ts";
import { codex } from "./agent-codex.ts";
import { openCode } from "./agent-opencode.ts";
import { pi } from "./agent-pi.ts";

/**
 * One definition per coding agent: where its configuration goes, what it contains, how
 * it merges with the user's, how the agent launches and asks for its catalog. AgentSetup
 * runs the same flow for each. Key order is the service unit's environment order.
 */
export const agentDefinitions: Record<Agent, AgentDefinition> = {
  codex,
  opencode: openCode,
  pi,
  claude,
};
/** Environment variables that move agent configuration; a service must forward them. */
export const agentEnvironment = Object.values(agentDefinitions).flatMap(
  ({ directory }) =>
    !directory
      ? []
      : directory.xdg
        ? [directory.variable, "XDG_CONFIG_HOME"]
        : [directory.variable],
);
export const defaultConfigDirectory = (agent: Agent): string[] =>
  agentDefinitions[agent].directory?.home ?? [];
export function configDirectory(agent: Agent): string | undefined {
  const directory = agentDefinitions[agent].directory;
  if (!directory) return undefined;
  return (
    process.env[directory.variable] ||
    (directory.xdg && process.env.XDG_CONFIG_HOME
      ? join(process.env.XDG_CONFIG_HOME, directory.xdg)
      : undefined)
  );
}

/**
 * The models an agent is offered, at install and when it lists models: its vendor's
 * own unless the user adds other providers, or every model models.dev prices.
 */
export async function offeredModels(
  agent: Agent,
  models: Model[],
  otherProviders: boolean,
  enrich: (models: Model[]) => Promise<CatalogModel[]>,
): Promise<CatalogModel[]> {
  if (agents[agent].native) return modelsFor(agent, models, otherProviders);
  const enriched = await enrich(models);
  // Image models, retired IDs and hidden helpers have no tool support or
  // pricing on models.dev; they are left out and the app lists them next to
  // the connection, so nothing is omitted silently.
  const usable = enriched.filter(
    (model) => model.metadata?.toolCall && model.metadata.cost,
  );
  if (!usable.length)
    throw new AppError(
      enriched.length
        ? `No model in this profile has verified tool support and pricing on models.dev, so ${agent} cannot show any (${enriched.map((model) => model.id).join(", ")}).`
        : "This profile has no available models.",
    );
  return usable;
}

/** pi's and OpenCode's model entries, with the limits and prices both need per model. */
export function agentModels(models: CatalogModel[], agent: Agent) {
  // pi speaks each vendor's native protocol through the gateway so thinking
  // levels and tool calls are not translated twice.
  const piApi = (model: CatalogModel) =>
    nativeAgent(model.owned_by) === "claude"
      ? "anthropic-messages"
      : nativeAgent(model.owned_by) === "codex"
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
