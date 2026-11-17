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
}
// One entry per coding agent nonstopvibin connects. Key order is the app's tab order.
const entries = {
  claude: {
    title: "Claude Code",
    offered: true,
    project: true,
    native: "Claude",
  },
  codex: { title: "Codex", offered: true, project: false, native: "OpenAI" },
  pi: { title: "pi", offered: true, project: false },
  opencode: { title: "OpenCode", offered: false, project: false },
} satisfies Record<string, AgentEntry>;
export type Agent = keyof typeof entries;
export const agents: Record<Agent, AgentEntry> = entries;

export function isAgent(value: string): value is Agent {
  return Object.hasOwn(agents, value);
}
export const agentIds = Object.keys(agents).filter(isAgent);
