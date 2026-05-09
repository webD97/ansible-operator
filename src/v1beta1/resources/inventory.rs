use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1beta1::NodeSelectorTerm;

#[derive(CustomResource, Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[kube(
    group = "ansible.cloudbending.dev",
    version = "v1beta1",
    kind = "Inventory",
    root = "AnsibleInventory",
    status = "AnsibleInventoryStatus",
    namespaced,
    printcolumn = r#"{"name":"Hosts","type":"string","jsonPath":".status.hostCount"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AnsibleInventorySpec {
    pub hosts: Vec<InventoryHosts>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AnsibleInventoryStatus {
    pub host_count: usize,
    pub resolved_hosts: Vec<ResolvedHosts>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedHosts {
    pub name: String,
    pub hosts: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InventoryHosts {
    pub name: String,
    pub from_cluster_nodes: Option<NodeSelectorTerm>,
    pub from_hostnames: Option<Vec<String>>,
}

impl InventoryHosts {
    pub fn source(&self) -> Result<HostSource<'_>, &'static str> {
        match (&self.from_cluster_nodes, &self.from_hostnames) {
            (Some(nodes), None) => Ok(HostSource::FromClusterNodes {
                from_cluster_nodes: nodes,
            }),
            (None, Some(hostnames)) => Ok(HostSource::FromHostnames {
                from_hostnames: hostnames,
            }),
            (Some(_), Some(_)) => {
                Err("hosts entry specifies both fromClusterNodes and fromHostnames")
            }
            (None, None) => Err("hosts entry specifies neither fromClusterNodes nor fromHostnames"),
        }
    }
}

pub enum HostSource<'a> {
    FromClusterNodes {
        from_cluster_nodes: &'a NodeSelectorTerm,
    },
    FromHostnames {
        from_hostnames: &'a [String],
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_example() {
        let inventory_str = include_str!("../../../examples/inventory.yaml");
        let _: AnsibleInventory = serde_yaml::from_str(inventory_str).unwrap();
    }
}
