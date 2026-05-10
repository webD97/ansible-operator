use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1beta1::{AnsibleInventory, NodeSelectorTerm, ResolvedHosts};

#[derive(CustomResource, Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[kube(
    group = "ansible.cloudbending.dev",
    version = "v1beta1",
    kind = "ClusterInventory",
    status = "ClusterInventoryStatus",
    namespaced,
    printcolumn = r#"{"name":"Hosts","type":"string","jsonPath":".status.hostCount"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ClusterInventorySpec {
    pub hosts: Vec<InventoryHosts>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClusterInventoryStatus {
    pub host_count: usize,
    pub resolved_hosts: Vec<ResolvedHosts>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InventoryHosts {
    pub name: String,
    #[serde(flatten)]
    pub match_labels: Option<NodeSelectorTerm>,
    #[serde(flatten)]
    pub match_expressions: Option<BTreeMap<String, serde_json::Value>>, // todo: placeholder
}

impl AnsibleInventory for ClusterInventory {
    fn get_hosts(&self) -> Vec<ResolvedHosts> {
        self.status
            .as_ref()
            .map(|s| s.resolved_hosts.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_example() {
        let inventory_str = include_str!("../../../examples/v1beta1/cluster-inventory.yaml");
        let _: ClusterInventory = serde_yaml::from_str(inventory_str).unwrap();
    }
}
