use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

type GenericMap = serde_json::Value;

#[derive(CustomResource, Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[kube(
    group = "ansible.cloudbending.dev",
    version = "v1alpha1",
    kind = "PlaybookPlan",
    namespaced
)]
#[kube(status = "PlaybookPlanStatus")]
#[serde(rename_all = "camelCase")]
pub struct PlaybookPlanSpec {
    /// An OCI image with Ansible and all required collections
    pub image: String,

    /// Controls when a playbook is executed
    pub triggers: Triggers,

    /// These host groups will be available in our playbook
    pub inventory: Vec<Inventory>,

    /// Used to decide on a connection plugin. We will always create one Ansible (cron)job per host.
    pub execution_strategy: ExecutionStrategy,

    // Variables that will be available in Ansible
    pub variables: Option<Variables>,

    /// The playbook will be built from this, some fields will be set automatically (vars, hosts)
    pub templates: Vec<Template>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Triggers {
    pub immediate: Option<bool>,
    pub schedule: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub name: String,
    pub hosts: Hosts,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(untagged)]
pub enum Hosts {
    FromClusterNodes {
        #[serde(rename = "fromNodes")]
        from_nodes: NodeSelectorTerm,
    },
}

impl Default for Hosts {
    fn default() -> Self {
        Self::FromClusterNodes {
            from_nodes: NodeSelectorTerm {
                match_labels: BTreeMap::new(),
            },
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeSelectorTerm {
    pub match_labels: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
pub enum ExecutionStrategy {
    Ssh { ssh: SshConfig },
    Chroot {},
}

impl Default for ExecutionStrategy {
    fn default() -> Self {
        Self::Chroot {}
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SshConfig {
    pub user: String,
    pub secret_ref: SecretRef,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    pub name: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Variables {
    pub inline: GenericMap,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Template {
    pub hosts: String,
    pub tasks: Vec<GenericMap>,
    pub pre_tasks: Option<Vec<GenericMap>>,
    pub post_tasks: Option<Vec<GenericMap>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaybookPlanStatus {
    pub eligible_hosts: Option<BTreeMap<String, Vec<String>>>,
}

#[test]
fn test_schema() {
    let playbookplan = PlaybookPlan::new(
        "blubb",
        PlaybookPlanSpec {
            image: "registry.tld/ansible:1.0.0".to_string(),
            triggers: Triggers {
                immediate: Some(false),
                schedule: Some("0 1 * * *".into()),
            },
            inventory: vec![
                Inventory {
                    name: "controlplane".into(),
                    hosts: Hosts::FromClusterNodes {
                        from_nodes: NodeSelectorTerm {
                            match_labels: {
                                let mut labels = BTreeMap::new();
                                labels.insert(
                                    "node.kubernetes.io/role".into(),
                                    "controlplane".into(),
                                );
                                labels
                            },
                        },
                    },
                },
                Inventory {
                    name: "workers".into(),
                    hosts: Hosts::FromClusterNodes {
                        from_nodes: NodeSelectorTerm {
                            match_labels: {
                                let mut labels = BTreeMap::new();
                                labels.insert("node.kubernetes.io/role".into(), "worker".into());
                                labels
                            },
                        },
                    },
                },
            ],
            execution_strategy: ExecutionStrategy::Ssh {
                ssh: SshConfig {
                    user: "root".into(),
                    secret_ref: SecretRef {
                        name: "ssh-key".into(),
                    },
                },
            },
            variables: None,
            templates: vec![Template {
                hosts: "all".into(),
                pre_tasks: None,
                post_tasks: None,
                tasks: vec![
                    serde_yaml::from_str(
                        r#"
name: Ensure httpd installed
ansible.builtin.dnf:
  name: httpd
  state: installed
"#,
                    )
                    .unwrap(),
                ],
            }],
        },
    );

    let serialized = serde_yaml::to_string(&playbookplan).unwrap();

    println!("{}", serialized);
}
