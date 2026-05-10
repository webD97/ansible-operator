use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub type LabelMap = BTreeMap<String, String>;

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeSelectorTerm {
    pub match_labels: Option<LabelMap>,
    pub match_expressions: Option<Vec<SelectorExpression>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SelectorExpression {
    pub operator: SelectorOperator,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, PartialEq)]
pub enum SelectorOperator {
    In,
    NotIn,
    Exists,
    DoesNotExist,
}
