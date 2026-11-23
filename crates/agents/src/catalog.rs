//! The curated catalog an agent is offered at connection and when it lists models.

use std::future::Future;

use nsv_domain::registry::{Agent, Registry};
use nsv_domain::wire::{CatalogModel, Model, ModelMetadata};
use serde_json::{Value, json};

use crate::error::{AgentError, refused};
use crate::json;

/// The agent whose vendor's models these are: `owned_by` is the vendor for OAuth models and
/// `<provider>-<hash>` for API-key accounts registered as compatible providers.
#[must_use]
pub fn native_agent(registry: &Registry, owner: Option<&str>) -> Option<Agent> {
    let owner = owner?;
    registry
        .providers
        .iter()
        .find(|(provider, _)| {
            let id = provider.as_str();
            owner == id
                || owner
                    .strip_prefix(id)
                    .is_some_and(|rest| rest.starts_with('-'))
        })
        .and_then(|(_, entry)| entry.native_agent)
}

/// The models an agent is offered: its vendor's own unless the user adds other providers, or,
/// for an agent without a vendor, every model models.dev lists with tool support and prices.
/// `enrich` adds models.dev metadata; it is called only for agents without a vendor.
///
/// # Errors
/// A 400 when an agent without a vendor would get no model.
pub async fn offered_models<F, Enriched>(
    registry: &Registry,
    agent: Agent,
    models: Vec<Model>,
    other_providers: bool,
    enrich: F,
) -> Result<Vec<CatalogModel>, AgentError>
where
    F: FnOnce(Vec<Model>) -> Enriched,
    Enriched: Future<Output = Vec<CatalogModel>>,
{
    if registry.agents.get(agent).native.is_some() {
        return Ok(models
            .into_iter()
            .filter(|model| {
                other_providers || native_agent(registry, model.owned_by.as_deref()) == Some(agent)
            })
            .map(|Model { id, owned_by }| CatalogModel {
                id,
                owned_by,
                metadata: None,
            })
            .collect());
    }
    let enriched = enrich(models).await;
    // Image models, retired IDs and hidden helpers have no tool support or pricing on
    // models.dev; they are left out and the app lists them next to the connection, so nothing
    // is omitted silently.
    let (usable, unusable): (Vec<_>, Vec<_>) = enriched.into_iter().partition(|model| {
        model
            .metadata
            .as_ref()
            .is_some_and(|m| m.tool_call && m.cost.is_some())
    });
    if usable.is_empty() {
        return Err(refused(if unusable.is_empty() {
            "This profile has no available models.".to_owned()
        } else {
            let ids: Vec<&str> = unusable.iter().map(|model| model.id.as_str()).collect();
            format!(
                "No model in this profile has verified tool support and pricing on models.dev, so {} cannot show any ({}).",
                agent.as_str(),
                ids.join(", ")
            )
        }));
    }
    Ok(usable)
}

/// The catalog entries generated pi and `OpenCode` code is served (`{ models: entries }`);
/// `None` for agents whose own request asks and get the `OpenAI` model list.
///
/// # Errors
/// A 400 when a model lacks models.dev metadata.
pub fn catalog_entries(
    registry: &Registry,
    agent: Agent,
    models: &[CatalogModel],
) -> Result<Option<Vec<Value>>, AgentError> {
    let entry: fn(&Registry, &CatalogModel) -> Result<Value, AgentError> = match agent {
        Agent::Pi => pi_model,
        Agent::Opencode => |_, model| open_code_model(model),
        Agent::Claude | Agent::Codex => return Ok(None),
    };
    models
        .iter()
        .map(|model| entry(registry, model))
        .collect::<Result<_, _>>()
        .map(Some)
}

/// A model's metadata and the name pi and `OpenCode` list it under. `tiers_note` says a tiered
/// price shows only its base rate in that client.
fn listed_model(
    model: &CatalogModel,
    tiers_note: bool,
) -> Result<(&ModelMetadata, String), AgentError> {
    let metadata = model
        .metadata
        .as_ref()
        .filter(|m| m.cost.is_some())
        .ok_or_else(|| refused("Model metadata is unavailable."))?;
    let mut name = format!("{} · API list price", metadata.name);
    if let Some(cost) = &metadata.cost {
        if tiers_note && cost.tiers.as_ref().is_some_and(|tiers| !tiers.is_empty()) {
            name.push_str(" (base rate; tiered pricing unavailable in this client)");
        }
        if cost.cache_read.is_none() || cost.cache_write.is_none() {
            name.push_str(" (cache pricing unavailable)");
        }
    }
    Ok((metadata, name))
}

/// `OpenCode`'s model entry, with the limits and prices it needs per model.
fn open_code_model(model: &CatalogModel) -> Result<Value, AgentError> {
    let (m, name) = listed_model(model, true)?;
    let mut limit = json::object([("context", m.context.into())]);
    if let Some(input) = m.input_limit {
        limit.insert("input".to_owned(), input.into());
    }
    limit.insert("output".to_owned(), m.output.into());
    Ok(Value::Object(json::object([
        ("id", model.id.clone().into()),
        ("name", name.into()),
        ("reasoning", m.reasoning.into()),
        ("tool_call", m.tool_call.into()),
        (
            "modalities",
            json!({ "input": m.input, "output": ["text"] }),
        ),
        ("limit", Value::Object(limit)),
        ("cost", json!(m.cost)),
    ])))
}

/// pi's model entry. pi speaks each vendor's native protocol through the gateway so thinking
/// levels and tool calls are not translated twice.
fn pi_model(registry: &Registry, model: &CatalogModel) -> Result<Value, AgentError> {
    let (m, name) = listed_model(model, false)?;
    let api = match native_agent(registry, model.owned_by.as_deref()) {
        Some(Agent::Claude) => "anthropic-messages",
        Some(Agent::Codex) => "openai-responses",
        _ => "openai-completions",
    };
    let mut cost = serde_json::Map::new();
    if let Some(prices) = &m.cost {
        cost.insert("input".to_owned(), json!(prices.input));
        cost.insert("output".to_owned(), json!(prices.output));
        if let Some(read) = prices.cache_read {
            cost.insert("cacheRead".to_owned(), json!(read));
        }
        if let Some(write) = prices.cache_write {
            cost.insert("cacheWrite".to_owned(), json!(write));
        }
        if let Some(tiers) = &prices.tiers {
            let tiers = tiers.iter().map(|tier| {
                let mut entry = json::object([
                    ("inputTokensAbove", tier.tier.size.into()),
                    ("input", json!(tier.input)),
                    ("output", json!(tier.output)),
                ]);
                if let Some(read) = tier.cache_read {
                    entry.insert("cacheRead".to_owned(), json!(read));
                }
                if let Some(write) = tier.cache_write {
                    entry.insert("cacheWrite".to_owned(), json!(write));
                }
                Value::Object(entry)
            });
            cost.insert("tiers".to_owned(), Value::Array(tiers.collect()));
        }
    }
    Ok(Value::Object(json::object([
        ("id", model.id.clone().into()),
        ("name", name.into()),
        ("api", api.into()),
        ("reasoning", m.reasoning.into()),
        ("input", json!(m.input)),
        ("contextWindow", m.context.into()),
        ("maxTokens", m.output.into()),
        ("cost", Value::Object(cost)),
    ])))
}
