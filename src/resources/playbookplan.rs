use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::{JsonSchema, SchemaGenerator, schema::Schema};
use serde::{Deserialize, Serialize};

use crate::resources::LabelMap;

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(transparent)]
pub struct GenericMap(pub serde_json::Value);

impl JsonSchema for GenericMap {
    fn schema_name() -> String {
        "GenericMap".to_string()
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        use schemars::schema::InstanceType;
        use schemars::schema::SchemaObject;
        use serde_json::json;

        let schema_obj = SchemaObject {
            instance_type: Some(InstanceType::Object.into()),
            ..Default::default()
        };

        // Inject the Kubernetes extension
        let mut raw = serde_json::to_value(&schema_obj).unwrap();
        let obj = raw.as_object_mut().unwrap();
        obj.insert(
            "x-kubernetes-preserve-unknown-fields".to_string(),
            json!(true),
        );

        serde_json::from_value(raw).unwrap()
    }
}

#[derive(CustomResource, Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[kube(
    group = "ansible.cloudbending.dev",
    version = "v1alpha1",
    kind = "PlaybookPlan",
    namespaced
)]
#[kube(printcolumn = r#"{"name":"Schedule","type":"string","jsonPath":".spec.triggers.schedule"}"#)]
#[kube(printcolumn = r#"{"name":"Hosts","type":"number","jsonPath":".status.eligibleHostsCount"}"#)]
#[kube(printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#)]
#[kube(printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#)]
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
    pub template: String,
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
    FromStaticList {
        #[serde(rename = "fromList")]
        from_list: Vec<String>,
    },
}

impl Default for Hosts {
    fn default() -> Self {
        Self::FromClusterNodes {
            from_nodes: NodeSelectorTerm::default(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(untagged)]
pub enum NodeSelectorTerm {
    MatchLabels {
        #[serde(rename = "matchLabels")]
        labels: LabelMap,
    },
}

impl Default for NodeSelectorTerm {
    fn default() -> Self {
        Self::MatchLabels {
            labels: BTreeMap::new(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(untagged)]
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
pub struct PlaybookPlanStatus {
    pub eligible_hosts: Option<BTreeMap<String, Vec<String>>>,
    pub eligible_hosts_count: Option<usize>,
    pub phase: Phase,
    pub last_rendered_generation: Option<i64>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "PascalCase")]
pub enum Phase {
    #[default]
    Waiting,
    Running,
    Succeeded,
    Failed,
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
                        from_nodes: NodeSelectorTerm::MatchLabels {
                            labels: {
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
                        from_nodes: NodeSelectorTerm::MatchLabels {
                            labels: {
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
            template: r#"
- tasks:
    - name: Ensure httpd installed
        ansible.builtin.dnf:
            name: httpd
            state: installed
            "#
            .into(),
        },
    );

    let serialized = serde_yaml::to_string(&playbookplan).unwrap();

    println!("{serialized}");
}
