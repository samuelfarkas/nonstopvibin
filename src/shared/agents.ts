interface AgentEntry {
  title: string;
  /** Offered in the app; the server keeps supporting the others for existing connections. */
  offered: boolean;
  /** Connects one project at a time instead of the user's whole agent configuration. */
  project: boolean;
  /**
   * The vendor whose models the agent gets by default; the user chooses whether to add
   * other providers. Without one, the agent gets every model with models.dev pricing.
   */
  native?: string;
  /**
   * How the agent asks the gateway for its model list: its own request carries a header,
   * or its generated code adds a query parameter. Without one, the list is written into
   * its configuration at connection, so a changed selection must be saved.
   */
  catalog?: "header" | "query";
}
// One entry per coding agent nonstopvibin connects. Key order is the app's tab order.
const entries = {
  claude: {
    title: "Claude Code",
    offered: true,
    project: true,
    native: "Claude",
  },
  codex: {
    title: "Codex",
    offered: true,
    project: false,
    native: "OpenAI",
    catalog: "header",
  },
  pi: { title: "pi", offered: true, project: false, catalog: "query" },
  opencode: {
    title: "OpenCode",
    offered: false,
    project: false,
    catalog: "query",
  },
} satisfies Record<string, AgentEntry>;
export type AgentEntries = typeof entries;
export type Agent = keyof AgentEntries;
export const agents: Record<Agent, AgentEntry> = entries;

export function isAgent(value: string): value is Agent {
  return Object.hasOwn(agents, value);
}
export const agentIds = Object.keys(agents).filter(isAgent);
