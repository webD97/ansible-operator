use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1beta1::{AnsibleInventory, GenericMap, ResolvedHosts, SecretRef};

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
    pub hosts: Vec<StaticInventoryGroup>,

    /// How to reach these hosts over SSH. Mandatory: a StaticInventory with no reachability
    /// info isn't usable by any PlaybookPlan.
    pub ssh: SshConfig,
}

/// One named group of external hosts, optionally carrying group variables applied to every host
/// in the group.
//
// Same `name` + `hosts` shape as `ResolvedHosts`, plus author-supplied `variables`; kept a distinct
// type so `ResolvedHosts` (which also backs status and the execution hash) stays a plain
// name/host-list pair.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StaticInventoryGroup {
    pub name: String,
    pub hosts: Vec<String>,

    /// Group variables applied to every host in this group, rendered as Ansible group `vars:`,
    /// e.g. `ansible_python_interpreter`. Operator-managed connection variables (`ansible_user`,
    /// `ansible_ssh_*`, `ansible_host`, `ansible_port`) are rejected — the operator owns those.
    pub variables: Option<GenericMap>,
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
        self.spec
            .hosts
            .iter()
            .map(|group| ResolvedHosts {
                name: group.name.clone(),
                hosts: group.hosts.clone(),
            })
            .collect()
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
