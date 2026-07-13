use std::borrow::Cow;
use std::collections::BTreeMap;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};

pub type LabelMap = BTreeMap<String, String>;

/// Marker type for `#[schemars(with = "UnsignedInt")]` on `u32` fields. A bare `u32` renders as
/// `format: uint32`, which Kubernetes does not recognise and warns about on every `kubectl apply`
/// of the CRD (it only knows `int32`/`int64`). This emits a plain non-negative integer
/// (`type: integer, minimum: 0`) instead — the same value constraint, without the unrecognised
/// format. Use `Option<UnsignedInt>` for optional `u32` fields.
pub struct UnsignedInt;

impl JsonSchema for UnsignedInt {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("UnsignedInt")
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        serde_json::from_value(serde_json::json!({
            "type": "integer",
            "minimum": 0
        }))
        .unwrap()
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeSelectorTerm {
    pub match_labels: Option<LabelMap>,
    pub match_expressions: Option<Vec<SelectorExpression>>,
}

/// A `matchLabels` + `matchExpressions` label selector — structurally identical to
/// `NodeSelectorTerm`, aliased for readability where the target is something other than a Node
/// (e.g. a namespace selector). Kubernetes' own `metav1.LabelSelector` has the same two fields.
pub type LabelSelector = NodeSelectorTerm;

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
