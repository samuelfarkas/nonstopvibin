import { join } from "node:path";
import { agents, type Agent } from "../shared/agents.ts";
import type { CatalogModel, Model } from "../shared/types.ts";
import { modelsFor } from "../shared/providers.ts";
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
export const agentDefinitions = {
  codex,
  opencode: openCode,
  pi,
  claude,
} satisfies { [A in Agent]: AgentDefinition<A> };
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
