use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1beta1::{AnsibleInventory, ResolvedHosts, SecretRef};

#[derive(CustomResource, Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[kube(
    group = "ansible.cloudbending.dev",
    version = "v1beta1",
    kind = "StaticInventory",
    status = "StaticInventoryStatus",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct StaticInventorySpec {
    pub hosts: Vec<ResolvedHosts>,

    /// How to reach these hosts over SSH. Mandatory: a StaticInventory with no reachability
    /// info isn't usable by any PlaybookPlan.
    pub ssh: SshConfig,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SshConfig {
    pub user: String,
    pub secret_ref: SecretRef,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StaticInventoryStatus {
    pub host_count: usize,
}

impl AnsibleInventory for StaticInventory {
    fn get_hosts(&self) -> Vec<ResolvedHosts> {
        self.spec.hosts.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_example() {
        let inventory_str = include_str!("../../../examples/v1beta1/static-inventory.yaml");
        let _: StaticInventory = serde_yaml::from_str(inventory_str).unwrap();
    }
}
