//! The curated catalog an agent is offered at connection and when it lists models.

use std::future::Future;

use nsv_domain::error::AppError;
use nsv_domain::registry::{Agent, registry};
use nsv_domain::wire::{AgentSetupInput, CatalogModel, Model, ModelMetadata};
use serde_json::{Value, json};

use crate::error::{AgentError, refused};
use crate::json;

const NO_MODELS: &str = "This profile has no available models.";

/// The curated-catalog flow: the models an agent is offered from its profile's `models`. An
/// agent with a curated catalog gets its vendor's own, or every model with `other_providers`;
/// the others get every model `enrich` finds tool support and prices for on models.dev.
///
/// # Errors
/// A 400 when an agent without a curated catalog would get no model; `enrich`'s error.
pub async fn curated<F, Enriched>(
    agent: Agent,
    models: Vec<Model>,
    other_providers: bool,
    enrich: F,
) -> Result<Vec<CatalogModel>, AgentError>
where
    F: FnOnce(Vec<Model>) -> Enriched,
    Enriched: Future<Output = Result<Vec<CatalogModel>, AppError>>,
{
    if registry().curated(agent) {
        return Ok(models
            .into_iter()
            .filter(|model| {
                other_providers || registry().native_agent(model.owned_by.as_deref()) == Some(agent)
            })
            .map(|Model { id, owned_by }| CatalogModel {
                id,
                owned_by,
                metadata: None,
            })
            .collect());
    }
    let enriched = enrich(models).await?;
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
            NO_MODELS.to_owned()
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

/// The model IDs a new connection offers: [`curated`], with other providers unless the input
/// leaves them out.
///
/// # Errors
/// A 400 when the profile has no model or none the agent is offered; [`curated`]'s errors.
pub async fn connection_models<F, Enriched>(
    input: &AgentSetupInput,
    models: Vec<Model>,
    enrich: F,
) -> Result<Vec<String>, AgentError>
where
    F: FnOnce(Vec<Model>) -> Enriched,
    Enriched: Future<Output = Result<Vec<CatalogModel>, AppError>>,
{
    if models.is_empty() {
        return Err(refused(NO_MODELS));
    }
    let other_providers = input.other_providers.unwrap_or(true);
    let offered = curated(input.agent, models, other_providers, enrich).await?;
    // Only an agent with a native vendor can be left without models.
    if offered.is_empty() {
        return Err(refused(format!(
            "This profile has no {} models. Add one, or offer models from other providers.",
            registry()
                .agents
                .get(input.agent)
                .native
                .as_deref()
                .unwrap_or("")
        )));
    }
    Ok(offered.into_iter().map(|model| model.id).collect())
}

/// What the gateway answers an agent's own model-list request with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogAnswer {
    /// The `OpenAI` model list the agent reads itself (`{ "object": "list", "data" }`).
    List(String),
    /// Entries for generated pi and `OpenCode` code (`{ "models" }`).
    Entries(String),
}

/// The gateway's answer for `agent`, from what [`curated`] offers it without other providers.
///
/// # Errors
/// A 400 when generated code needs a model's models.dev metadata and it has none.
pub fn gateway_catalog(
    agent: Agent,
    offered: &[CatalogModel],
) -> Result<CatalogAnswer, AgentError> {
    if let Some(entries) = catalog_entries(agent, offered)? {
        return Ok(CatalogAnswer::Entries(
            json!({ "models": entries }).to_string(),
        ));
    }
    let data: Vec<Value> = offered
        .iter()
        .map(|model| {
            let mut entry = json!({ "object": "model" });
            if let (Some(entry), Ok(Value::Object(fields))) =
                (entry.as_object_mut(), serde_json::to_value(model))
            {
                entry.extend(fields);
            }
            entry
        })
        .collect();
    Ok(CatalogAnswer::List(
        json!({ "object": "list", "data": data }).to_string(),
    ))
}

/// The catalog entries generated pi and `OpenCode` code is served (`{ models: entries }`);
/// `None` for agents whose own request asks and get the `OpenAI` model list.
///
/// # Errors
/// A 400 when a model lacks models.dev metadata.
fn catalog_entries(
    agent: Agent,
    models: &[CatalogModel],
) -> Result<Option<Vec<Value>>, AgentError> {
    let entry: fn(&CatalogModel) -> Result<Value, AgentError> = match agent {
        Agent::Pi => pi_model,
        Agent::Opencode => open_code_model,
        Agent::Claude | Agent::Codex => return Ok(None),
    };
    models.iter().map(entry).collect::<Result<_, _>>().map(Some)
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
fn pi_model(model: &CatalogModel) -> Result<Value, AgentError> {
    let (m, name) = listed_model(model, false)?;
    let api = match registry().native_agent(model.owned_by.as_deref()) {
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
