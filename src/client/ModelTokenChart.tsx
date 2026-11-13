import { useId, useState } from "react";
import type { ActivityModel } from "./activity-models.ts";

const exactCount = new Intl.NumberFormat([], { maximumFractionDigits: 0 });

export function ModelTokenChart({
  models,
  requests,
}: {
  models: ActivityModel[];
  requests: number;
}) {
  const id = useId();
  const [active, setActive] = useState<string>();
  const sorted = [...models].sort(
    (a, b) => b[1].tokens - a[1].tokens || a[0].localeCompare(b[0]),
  );
  const maximum = Math.max(1, ...sorted.map(([, group]) => group.tokens));
  return (
    <section
      className="activity-section model-token-chart"
      aria-labelledby={`${id}-title`}
      aria-describedby={`${id}-note`}
    >
      <div className="activity-section-heading">
        <h2 className="label" id={`${id}-title`}>
          Token usage by model
        </h2>
        <code>{exactCount.format(requests)} sampled requests</code>
      </div>
      <p className="model-token-note" id={`${id}-note`}>
        Selected period · latest 500 requests at most · token counts, not
        monetary costs.
      </p>
      {!sorted.length ? (
        <p className="model-token-empty">No token usage in this period.</p>
      ) : (
        <table className="model-token-table">
          <caption className="sr-only">
            Token usage by model, selected-period request sample
          </caption>
          <thead>
            <tr>
              <th scope="col">Model</th>
              <th scope="col">Tokens</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map(([model, group], index) => {
              const label = `${model}: ${exactCount.format(group.tokens)} tokens, ${exactCount.format(group.requests)} requests`;
              return (
                <tr key={model}>
                  <th scope="row">{model}</th>
                  <td>
                    <div className="model-token-value">
                      <div
                        className="model-token-target"
                        role="img"
                        tabIndex={0}
                        aria-label={label}
                        onMouseEnter={() => setActive(model)}
                        onMouseLeave={() => setActive(undefined)}
                        onFocus={() => setActive(model)}
                        onBlur={() => setActive(undefined)}
                        onKeyDown={(event) => {
                          if (event.key === "Escape") setActive(undefined);
                        }}
                      >
                        <span
                          className="model-token-bar"
                          aria-hidden="true"
                          style={{
                            width: `${(group.tokens / maximum) * 100}%`,
                          }}
                        />
                        {active === model && (
                          <span
                            className="model-token-tooltip"
                            role="tooltip"
                            id={`${id}-tooltip-${index}`}
                          >
                            {label}
                          </span>
                        )}
                      </div>
                      <code>{exactCount.format(group.tokens)}</code>
                    </div>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
    </section>
  );
}
