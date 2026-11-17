import { useEffect, useState } from "react";
import { LoaderCircle, Plug } from "lucide-react";
import type { Model, ProfileState } from "../shared/types.ts";
import { agentIds, agents, type Agent } from "../shared/agents.ts";
import { api } from "./api.ts";
import { messageOf } from "./action.ts";
import { Empty } from "./components.tsx";
import { CopyButton } from "./controls.tsx";
import { QuickSetup } from "./QuickSetup.tsx";
import "./connect.css";

const agentOptions = agentIds.filter((id) => agents[id].offered);

export function Connect({ profile }: { profile: ProfileState }) {
  const [models, setModels] = useState<Model[]>([]);
  const [agent, setAgent] = useState<Agent>("claude");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [search, setSearch] = useState("");
  const [revealed, setRevealed] = useState(false);
  const [key, setKey] = useState("");
  const accountCatalog = JSON.stringify(
    profile.accounts.map((account) => [
      account.id,
      account.disabled,
      account.modelCount,
    ]),
  );
  useEffect(() => {
    let cancelled = false;
    setModels([]);
    setError("");
    setLoading(false);
    if (profile.runtime !== "running") return;
    setLoading(true);
    api("GET /profiles/:profileId/models", {
      params: { profileId: profile.id },
    })
      .then((result) => {
        if (!cancelled) {
          setModels(result);
        }
      })
      .catch((e) => {
        if (!cancelled) setError(e.message);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [profile.id, profile.runtime, accountCatalog, refresh]);
  async function getProfileKey() {
    if (key) return key;
    const result = await api("GET /profiles/:profileId/key", {
      params: { profileId: profile.id },
    });
    setKey(result.key);
    return result.key;
  }

  const subscriptionCount = profile.accounts.length;
  if (profile.runtime !== "running")
    return (
      <Empty
        icon={<Plug size={27} />}
        title="Start this profile to connect an agent"
      >
        <p>Its model catalog and connection settings will appear here.</p>
      </Empty>
    );
  return (
    <div className="connect-page">
      <div className="connect-intro">
        <h2>Connect your agent</h2>
        <p>Use your {profile.name} subscriptions.</p>
      </div>
      {error && (
        <div className="inline-error" role="alert">
          {error}
        </div>
      )}
      <div className="agent-tabs" role="tablist" aria-label="Agent">
        {agentOptions.map((id, index) => (
          <button
            key={id}
            id={`agent-tab-${id}`}
            role="tab"
            aria-selected={agent === id}
            aria-controls="agent-connection"
            tabIndex={agent === id ? 0 : -1}
            className={agent === id ? "active" : ""}
            onClick={() => {
              setAgent(id);
            }}
            onKeyDown={(event) => {
              const next =
                event.key === "ArrowRight"
                  ? (index + 1) % agentOptions.length
                  : event.key === "ArrowLeft"
                    ? (index + agentOptions.length - 1) % agentOptions.length
                    : event.key === "Home"
                      ? 0
                      : event.key === "End"
                        ? agentOptions.length - 1
                        : undefined;
              if (next === undefined) return;
              event.preventDefault();
              const nextAgent = agentOptions[next];
              setAgent(nextAgent);
              document.getElementById(`agent-tab-${nextAgent}`)?.focus();
            }}
          >
            {agents[id].title}
          </button>
        ))}
      </div>
      <div
        id="agent-connection"
        role="tabpanel"
        aria-labelledby={`agent-tab-${agent}`}
      >
        {loading ? (
          <div className="loading">
            <LoaderCircle className="spin" size={18} /> Loading models…
          </div>
        ) : !models.length ? (
          <Empty title="Connect a subscription first">
            <p>
              The proxy has no models available in this profile yet. Add an
              account from Subscriptions.
            </p>
          </Empty>
        ) : (
          <QuickSetup
            key={`${profile.id}/${agent}`}
            profile={profile}
            agent={agent}
            models={models}
          />
        )}
      </div>
      {models.length > 0 && (
        <details className="manual-setup">
          <summary>Models & manual setup</summary>
          <div className="model-catalog">
            <p>
              {models.length} model{models.length === 1 ? "" : "s"} available in{" "}
              {profile.name}. Any agent that speaks the OpenAI or Anthropic API
              can use them with the endpoint and key below.
            </p>
            <div className="catalog-search">
              <label className="field" htmlFor="catalog-search">
                Find a model
                <input
                  id="catalog-search"
                  type="search"
                  value={search}
                  onChange={(event) => setSearch(event.target.value)}
                  placeholder="Search model IDs"
                />
              </label>
              <button
                className="button small"
                onClick={() => setRefresh((value) => value + 1)}
              >
                Refresh availability
              </button>
            </div>
            <ul className="catalog-models">
              {models
                .filter((model) =>
                  model.id.toLowerCase().includes(search.toLowerCase()),
                )
                .map((model) => (
                  <li key={model.id}>
                    <div>
                      <code>{model.id}</code>
                    </div>
                    <CopyButton
                      text={model.id}
                      label={`Copy ${model.id} model ID`}
                      onError={setError}
                    />
                  </li>
                ))}
            </ul>
            {!models.some((model) =>
              model.id.toLowerCase().includes(search.toLowerCase()),
            ) && <p role="status">No matching models.</p>}
          </div>
          <div className="panel endpoint-panel">
            <div className="endpoint-row">
              <div>
                <span className="label">Base URL</span>
                <code>{profile.endpoint}</code>
              </div>
              <CopyButton text={profile.endpoint} onError={setError} />
            </div>
            <div className="endpoint-row">
              <div>
                <span className="label">Profile key</span>
                <code>
                  {revealed ? key : "nv_••••••••••••••••••••••••••••"}
                </code>
              </div>
              <div className="endpoint-actions">
                <button
                  className="button small"
                  onClick={async () => {
                    try {
                      if (!revealed) await getProfileKey();
                      setRevealed(!revealed);
                    } catch (e) {
                      setError(messageOf(e, "Could not reveal key."));
                    }
                  }}
                >
                  {revealed ? "Hide" : "Reveal"}
                </button>
                <CopyButton text={getProfileKey} onError={setError} />
              </div>
            </div>
          </div>
          <p className="scope-note">
            Scoped to the {subscriptionCount} subscription
            {subscriptionCount === 1 ? "" : "s"} in {profile.name}. Never falls
            back to another profile.
          </p>
        </details>
      )}
    </div>
  );
}
