use serde::{Deserialize, Serialize};

use super::JsNumber;

/// A model in a profile's list, as the core's `/v1/models` names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct Model {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
}

/// A model with the models.dev limits and prices the agents and Activity need.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct CatalogModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ModelMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum Modality {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct ModelMetadata {
    pub name: String,
    /// Context window in tokens.
    pub context: u64,
    /// Output limit in tokens.
    pub output: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limit: Option<u64>,
    pub input: Vec<Modality>,
    pub reasoning: bool,
    pub tool_call: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    pub source: String,
    pub fetched_at: String,
}

/// USD per million tokens, in models.dev's snake case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct Cost {
    pub input: JsNumber,
    pub output: JsNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<JsNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<JsNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<JsNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<CostTier>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(optional_fields))]
pub struct CostTier {
    pub input: JsNumber,
    pub output: JsNumber,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<JsNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<JsNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<JsNumber>,
    pub tier: ContextTier,
}

/// The prices apply above `size` tokens of context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ContextTier {
    #[serde(rename = "type")]
    pub kind: TierType,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub enum TierType {
    Context,
}
