use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub type LabelMap = BTreeMap<String, String>;

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeSelectorTerm {
    pub match_labels: LabelMap,
}
