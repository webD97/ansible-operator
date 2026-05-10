use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub trait AnsibleInventory {
    fn get_hosts(&self) -> Vec<ResolvedHosts>;
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedHosts {
    pub name: String,
    pub hosts: Vec<String>,
}
