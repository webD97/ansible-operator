use std::collections::HashMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[kube(
    group = "ansible.cloudbending.dev",
    version = "v1alpha1",
    kind = "Inventory",
    namespaced
)]
#[kube(status = "InventoryStatus")]
pub struct InventorySpec {
    pub groups: Vec<Group>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
pub struct Group {
    pub name: String,
    pub variables: HashMap<String, serde_json::Value>,
    pub hosts: Vec<Hosts>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeSelectorTerm {
    pub match_expressions: Vec<NodeSelectorRequirement>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct NodeSelectorRequirement {
    pub key: String,
    pub operator: NodeSelectorOperator,
    pub values: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum NodeSelectorOperator {
    In,
    NotIn,
    Exists,
    DoesNotExist,
    Gt,
    Lt,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(untagged)]
pub enum Hosts {
    FromNodeSelector {
        #[serde(rename = "fromNodes")]
        from_nodes: NodeSelectorTerm,
    },
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InventoryStatus {
    pub resolved_groups: Vec<ResolvedGroup>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
pub struct ResolvedGroup {
    pub name: String,
    pub hosts: Vec<String>,
}

#[test]
fn test_schema() {
    use serde_json::json;
    use serde_yaml::Value;

    let expected_yaml = r#"
apiVersion: ansible.cloudbending.dev/v1alpha1
kind: Inventory
metadata:
  name: digital-signage
spec:
  groups:
    - name: digital-signage
      hosts:
        - fromNodes:
            matchExpressions:
              - key: node.kubernetes.io/role
                operator: In
                values: [digital-signage]
      variables:
        key1: string value
        key2: 42
        key3: false
"#;

    let object = {
        let expr0 = NodeSelectorRequirement {
            key: "node.kubernetes.io/role".into(),
            operator: NodeSelectorOperator::In,
            values: Some(vec!["digital-signage".into()]),
        };

        let mut vars = HashMap::new();

        vars.insert("key1".into(), json!("string value"));
        vars.insert("key2".into(), json!(42));
        vars.insert("key3".into(), json!(false));

        Inventory::new(
            "digital-signage",
            InventorySpec {
                groups: vec![Group {
                    name: "digital-signage".into(),
                    variables: vars,
                    hosts: vec![Hosts::FromNodeSelector {
                        from_nodes: NodeSelectorTerm {
                            match_expressions: vec![expr0],
                        },
                    }],
                }],
            },
        )
    };

    let actual: Value = serde_yaml::to_value(&object).expect("Serialization failed");
    let expected: Value = serde_yaml::from_str(expected_yaml).expect("Deserialization failed");

    assert_eq!(expected, actual);
}
