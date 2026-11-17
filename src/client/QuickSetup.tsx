import { useEffect, useState } from "react";
import { Check, FolderOpen, LoaderCircle } from "lucide-react";
import type { AgentSetup, Model, ProfileState } from "../shared/types.ts";
import { agents, type Agent } from "../shared/agents.ts";
import { api, desktop } from "./api.ts";
import { messageOf, useAction } from "./action.ts";
import { modelsFor } from "../shared/providers.ts";
import { CopyButton, Select, useKeepDraft } from "./controls.tsx";

export function QuickSetup({
  profile,
  agent,
  models,
}: {
  profile: ProfileState;
  agent: Agent;
  models: Model[];
}) {
  const [setup, setSetup] = useState<AgentSetup | null>(null);
  const [loading, setLoading] = useState(true);
  const { busy, error, setError, run } = useAction();
  const [checked, setChecked] = useState(false);
  const [projectDirectory, setProjectDirectory] = useState("");
  const [projects, setProjects] = useState<string[]>([]);
  const [selectedProject, setSelectedProject] = useState<{ path: string }>();
  const [disconnected, setDisconnected] = useState(false);
  const [otherProviders, setOtherProviders] = useState(false);
  const { title, project, native } = agents[agent];
  const chooses = native !== undefined;
  useEffect(() => {
    if (selectedProject?.path === "") return;
    let cancelled = false;
    api("GET /profiles/:profileId/agent-setup", {
      params: { profileId: profile.id },
      query: { agent, projectDirectory: selectedProject?.path || undefined },
    })
      .then((result) => {
        if (cancelled) return;
        setSetup(result);
        if (result) {
          setProjects(result.projects ?? []);
          setProjectDirectory(result.projectDirectory ?? "");
          // Connections made before the choice existed offered everything.
          setOtherProviders(result.otherProviders ?? true);
        }
      })
      .catch((error: Error) => {
        if (!cancelled) setError(error.message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [profile.id, agent, selectedProject, setError]);
  const offered = modelsFor(agent, models, otherProviders);
  const otherCount = models.length - modelsFor(agent, models, false).length;
  // Codex, pi and OpenCode discover models live; only Claude's picker is a
  // stored list that can drift from the catalog.
  const modelsChanged = Boolean(
    setup &&
    ((chooses && (setup.otherProviders ?? true) !== otherProviders) ||
      (agent === "claude" &&
        (setup.models.length !== offered.length ||
          offered.some((model) => !setup.models.includes(model.id))))),
  );
  const current = Boolean(setup && !setup.needsReconnect && !modelsChanged);
  useKeepDraft(
    busy ||
      Boolean(error) ||
      disconnected ||
      (chooses &&
        otherProviders !== (setup ? (setup.otherProviders ?? true) : false)) ||
      projectDirectory !== (setup?.projectDirectory ?? ""),
  );
  // pi and OpenCode need models.dev limits and prices; the rest are left out.
  const leftOut =
    setup && !chooses
      ? models.filter((model) => !setup.models.includes(model.id))
      : [];
  async function changeSetup(remove = false) {
    await run(async () => {
      setDisconnected(false);
      if (remove) {
        await api("DELETE /profiles/:profileId/agent-setup", {
          params: { profileId: profile.id },
          body: {
            agent,
            ...(project && {
              projectDirectory: setup?.projectDirectory,
            }),
          },
        });
        if (project) {
          setProjects(
            projects.filter((project) => project !== setup?.projectDirectory),
          );
          setSelectedProject({ path: "" });
          setProjectDirectory("");
        }
        setSetup(null);
        setDisconnected(true);
      } else {
        const result = await api("POST /profiles/:profileId/agent-setup", {
          params: { profileId: profile.id },
          body: {
            agent,
            ...(project && { projectDirectory }),
            ...(chooses && { otherProviders }),
          },
        });
        setSetup(result);
        setProjects(result.projects ?? []);
        setProjectDirectory(result.projectDirectory ?? "");
      }
    }, "Could not change agent setup.");
  }
  const restart = `Restart ${title} to pick up the change.`;
  return (
    <div className="quick-setup">
      <p>
        {agent === "claude"
          ? "Choose a project to connect."
          : agent === "codex"
            ? "Connect once, use in any project."
            : `Add ${profile.name} to ${title}.`}
      </p>
      {project && (
        <div className="setup-project">
          {projects.length > 0 && (
            <div className="field">
              <label htmlFor="claude-connected-project">
                Connected projects
              </label>
              <div className="setup-project-input">
                <Select
                  id="claude-connected-project"
                  value={setup?.projectDirectory ?? ""}
                  title={setup?.projectDirectory}
                  placeholder="Choose a connected project"
                  disabled={loading || busy}
                  onValueChange={(path) => {
                    setSelectedProject({ path });
                    setProjectDirectory(path);
                    setSetup(null);
                    setLoading(true);
                    setError("");
                    setDisconnected(false);
                  }}
                  options={projects.map((project) => ({
                    value: project,
                    label: project.split("/").at(-1) || "/",
                    hint: project,
                  }))}
                />
                {setup && (
                  <button
                    className="text-button"
                    disabled={loading || busy}
                    onClick={() => {
                      setSelectedProject({ path: "" });
                      setSetup(null);
                      setProjectDirectory("");
                      setError("");
                      setDisconnected(false);
                    }}
                  >
                    Connect another project
                  </button>
                )}
              </div>
            </div>
          )}
          {!setup && (
            <div className="field">
              <label htmlFor="claude-project-directory">Project folder</label>
              <div className="setup-project-input">
                <input
                  id="claude-project-directory"
                  type="text"
                  placeholder="/absolute/path/to/your/project"
                  value={projectDirectory}
                  disabled={loading || busy}
                  onChange={(event) => setProjectDirectory(event.target.value)}
                />
                {desktop && (
                  <button
                    className="button"
                    type="button"
                    disabled={loading || busy}
                    onClick={async () => {
                      setError("");
                      try {
                        const folder = await desktop?.chooseProject();
                        if (folder) setProjectDirectory(folder);
                      } catch (error) {
                        setError(
                          messageOf(error, "Could not choose a project."),
                        );
                      }
                    }}
                  >
                    <FolderOpen size={15} /> Choose folder
                  </button>
                )}
              </div>
            </div>
          )}
        </div>
      )}
      {chooses && (
        <div className="setup-option">
          <div>
            <label htmlFor="other-providers">Include other providers</label>
            <p>
              {otherCount
                ? `${otherCount} extra model${otherCount === 1 ? "" : "s"} · Experimental`
                : `Only ${native} models available.`}
            </p>
          </div>
          <input
            id="other-providers"
            type="checkbox"
            role="switch"
            checked={otherProviders}
            disabled={loading || busy}
            onChange={(event) => setOtherProviders(event.target.checked)}
          />
        </div>
      )}
      {error && (
        <div className="inline-error" role="alert">
          {error}
        </div>
      )}
      {setup?.needsReconnect && (
        <p className="setup-note" role="alert">
          The connection is out of date. Reconnect, then restart {title}.
        </p>
      )}
      {modelsChanged && !setup?.needsReconnect && (
        <p className="setup-note" role="status">
          Save to apply the new model selection. {restart}
        </p>
      )}
      <div className="setup-actions">
        <button
          className="button primary"
          disabled={
            loading ||
            busy ||
            !offered.length ||
            current ||
            (project && !projectDirectory.trim())
          }
          onClick={() => void changeSetup()}
        >
          {busy || loading ? (
            <LoaderCircle size={15} className="spin" />
          ) : current ? (
            <Check size={15} />
          ) : null}
          {current
            ? "Connected"
            : setup?.needsReconnect
              ? `Reconnect ${title}`
              : setup
                ? "Save changes"
                : `Connect ${title}`}
        </button>
        {setup && (
          <button
            className="text-button"
            disabled={busy || loading}
            onClick={() => void changeSetup(true)}
          >
            Disconnect
          </button>
        )}
        {!offered.length && !loading && (
          <span className="setup-note">
            No {native} models in this profile. Turn on other providers or add a{" "}
            {native} subscription.
          </span>
        )}
      </div>
      <p className="setup-note">
        Keep NonstopVibin and this profile running while you work.
      </p>
      {disconnected && (
        <p className="setup-note" role="status">
          Connection removed. {restart}
        </p>
      )}
      {current && setup && (
        <div className="setup-launch" role="status">
          <h3>{agent === "claude" ? "Launch Claude Code" : `Open ${title}`}</h3>
          <div className="command-line">
            <code>{setup.command}</code>
            <CopyButton
              text={setup.command}
              label="Copy command"
              onError={setError}
            />
          </div>
          <p>{setup.instructions}</p>
        </div>
      )}
      <details className="setup-details">
        <summary>Connection details</summary>
        <ul>
          <li>
            {agent === "claude"
              ? "Saves project settings and a model picker. Includes worktrees inside the project."
              : agent === "codex"
                ? `Adds the nonstopvibin-${profile.slug} Codex profile. Your usual login stays available.`
                : `Adds a ${title} provider alongside your existing ones.`}
          </li>
          {agent === "claude" && (
            <li>
              Managed settings or a Claude gateway login may override this
              connection.
            </li>
          )}
          {chooses && otherProviders && (
            <li>
              Other providers use experimental request and tool-call
              translation.
            </li>
          )}
          <li>Disconnect removes only this connection’s settings.</li>
        </ul>
        {current && setup && (
          <>
            <p className="setup-caveat">
              {setup.models.length} model{setup.models.length === 1 ? "" : "s"}{" "}
              in the picker.{" "}
              {agent === "claude"
                ? "Needs Claude Code 2.1.243 or later. Save again after adding subscriptions."
                : agent === "codex"
                  ? "Codex refreshes the list when it starts. Your normal Codex login stays available."
                  : "Limits and prices shown in the agent are models.dev estimates, not subscription charges."}
            </p>
            {leftOut.length > 0 && (
              <p className="setup-caveat">
                Left out, no verified pricing on models.dev:{" "}
                <code>{leftOut.map((model) => model.id).join(", ")}</code>
              </p>
            )}
          </>
        )}
        {setup && (
          <ul>
            {setup.files.map((file) => (
              <li key={file}>
                <code>{file}</code>
              </li>
            ))}
          </ul>
        )}
        <div className="setup-check">
          <button
            className="button small"
            disabled={busy || !models.length}
            onClick={() =>
              run(async () => {
                setChecked(false);
                await api("POST /profiles/:profileId/agent-check", {
                  params: { profileId: profile.id },
                  body: {},
                });
                setChecked(true);
              }, "Connection check failed.")
            }
          >
            Check connection
          </button>
          {checked && (
            <span role="status">
              <Check size={15} /> Profile key and model catalog verified.
            </span>
          )}
        </div>
      </details>
    </div>
  );
}
