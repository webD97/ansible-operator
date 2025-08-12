use std::collections::BTreeMap;

use crate::{utils::Condition, v1beta1::LabelMap};
use chrono::{DateTime, FixedOffset};
use kube::CustomResource;
use schemars::{JsonSchema, SchemaGenerator, schema::Schema};
use serde::{Deserialize, Serialize};

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
    version = "v1beta1",
    kind = "PlaybookPlan",
    namespaced,
    status = "PlaybookPlanStatus",
    printcolumn = r#"{"name":"Hosts","type":"number","jsonPath":".status.eligibleHostsCount"}"#,
    printcolumn = r#"{"name":"Mode","type":"string","jsonPath":".spec.mode"}"#,
    printcolumn = r#"{"name":"Schedule","type":"string","jsonPath":".spec.schedule"}"#,
    printcolumn = r#"{"name":"Next run","type":"string","jsonPath":".status.nextRun"}"#,
    printcolumn = r#"{"name":"Current hash","type":"string","jsonPath":".status.currentHash"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Running","type":"string","jsonPath":".status.conditions[?(@.type==\"Running\")].status"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct PlaybookPlanSpec {
    /// An OCI image with Ansible and all required collections
    pub image: String,

    /// Controls if a playbook is executed once or repeatedly
    #[schemars(default)]
    pub mode: ExecutionMode,

    /// 5-part cron expression that tells at which time the playbook may execute
    pub schedule: Option<String>,

    /// Time zone for the _schedule_ field, if unset UTC is assumed
    pub time_zone: Option<String>,

    /// These host groups will be available in our playbook
    pub inventory: Vec<Inventory>,

    /// Used to decide on a connection plugin. We will always create one Ansible (cron)job per host.
    pub connection_strategy: ConnectionStrategy,

    /// The playbook will be built from this, some fields will be set automatically (vars, hosts)
    pub template: PlaybookTemplate,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
pub enum ExecutionMode {
    #[default]
    OneShot,
    Recurring,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
pub struct PlaybookTemplate {
    /// The actual playbook contents
    pub playbook: String,

    /// Variables for the playbook
    pub variables: Option<Vec<PlaybookVariableSource>>,

    /// Files for the playbook
    #[schemars(with = "Option<Vec<GenericMap>>")]
    pub files: Option<Vec<FilesSource>>,

    /// Runtime requirements (e.g. Ansible collections)
    pub requirements: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum FilesSource {
    #[serde(rename_all = "camelCase")]
    Secret { name: String, secret_ref: SecretRef },
    Other {
        name: String,
        #[serde(flatten)]
        extra: BTreeMap<String, serde_json::Value>,
    },
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionTriggers {
    /// Set this to a cron expression to delay playbook execution after the PlaybookPlan or a related secret have changed.
    /// If omitted, the playbook will be applied immediately.
    pub delayed_until: Option<String>,
    /// Set this to a cron expression to execute the playbook on a recurring basis.
    pub schedule: Option<String>,

    /// Time zone to use for cron evaluation, defaults to UTC if unset
    pub time_zone: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, JsonSchema)]
#[serde(rename_all = "camelCase", untagged)]
pub enum PlaybookVariableSource {
    /// Extra variables to read from a secret. These must be within `.data."variables.yaml"`.
    #[serde(rename_all = "camelCase")]
    SecretRef {
        secret_ref: SecretRef,
    },
    Inline {
        inline: GenericMap,
    },
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
pub enum ConnectionStrategy {
    Ssh { ssh: SshConfig },
    Chroot {},
}

impl Default for ConnectionStrategy {
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
// #[serde(rename_all = "PascalCase")]
pub enum Phase {
    /// Triggers have not yet been evaluated
    #[default]
    Pending,

    /// Playbook execution has been delayed.
    Delayed,

    /// Playbook has not yet been applied to all hosts.
    Applying,

    /// Playbook is scheduled for reexecution.
    Scheduled,

    /// Jobs for all hosts have run either successfully or not.
    Finished,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaybookPlanStatus {
    pub eligible_hosts: Option<BTreeMap<String, Vec<String>>>,
    pub eligible_hosts_count: Option<usize>,
    pub last_rendered_generation: Option<i64>,
    pub conditions: Vec<PlaybookPlanCondition>,
    pub hosts_status: Option<BTreeMap<String, HostStatus>>,
    #[serde(with = "crate::v1beta1::resources::custom_rfc3339")]
    #[schemars(with = "Option<String>")]
    pub next_run: Option<DateTime<FixedOffset>>,
    pub phase: Option<Phase>,
    pub current_hash: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub last_applied_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaybookPlanCondition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub reason: Option<String>,
    pub message: Option<String>,
    #[serde(with = "crate::v1beta1::resources::custom_rfc3339")]
    #[schemars(with = "Option<String>")]
    pub last_transition_time: Option<DateTime<FixedOffset>>,
}

impl Condition for PlaybookPlanCondition {
    fn type_(&self) -> &str {
        &self.type_
    }

    fn status(&self) -> &str {
        &self.status
    }

    fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialization() {
        let playbookplan = PlaybookPlan::new(
            "blubb",
            PlaybookPlanSpec {
                image: "registry.tld/ansible:1.0.0".to_string(),
                mode: ExecutionMode::Recurring,
                schedule: Some("0 1 * * *".into()),
                time_zone: None,
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
                                    labels
                                        .insert("node.kubernetes.io/role".into(), "worker".into());
                                    labels
                                },
                            },
                        },
                    },
                ],
                connection_strategy: ConnectionStrategy::Ssh {
                    ssh: SshConfig {
                        user: "root".into(),
                        secret_ref: SecretRef {
                            name: "ssh-key".into(),
                        },
                    },
                },
                template: PlaybookTemplate {
                    variables: Some(vec![PlaybookVariableSource::SecretRef {
                        secret_ref: SecretRef {
                            name: "some-secret".into(),
                        },
                    }]),
                    files: Some(vec![FilesSource::Secret {
                        name: "some-name".into(),
                        secret_ref: SecretRef {
                            name: "secret-with-files".into(),
                        },
                    }]),
                    playbook: r#"
- tasks:
    - name: Ensure httpd installed
        ansible.builtin.dnf:
            name: httpd
            state: installed
            "#
                    .into(),
                    ..Default::default()
                },
            },
        );

        let serialized = serde_yaml::to_string(&playbookplan).unwrap();

        println!("{serialized}");
    }

    #[test]
    fn test_deserialization() {
        let yaml = r#"
apiVersion: ansible.cloudbending.dev/v1beta1
kind: PlaybookPlan
metadata:
  name: an-example
spec:
  image: docker.io/serversideup/ansible-core:2.18
  inventory:
    - name: ccu
      hosts:
        fromList:
          - ccu.fritz.box
    - name: k3s
      hosts:
        fromNodes:
          matchLabels:
            node.kubernetes.io/instance-type: k3s
  mode: OneShot
  connectionStrategy:
    ssh:
      user: root
      secretRef:
        name: ssh
  template:
    variables:
      - inline:
          key: value
          nested:
            otherkey: othervalue
      - secretRef:
          name: secret-with-variables
    files:
      - name: some-configs
        secretRef:
          name: secret-with-config-files
      - name: binary-assets
        image:
          reference: my.registry.tld/the-image:v2
          pullPolicy: IfNotPresent
    playbook: |
      - hosts: all
        tasks:
          - name: Echo someting
            ansible.builtin.command:
              command: echo Hello
        "#;

        let pp = serde_yaml::from_str::<PlaybookPlan>(yaml).unwrap();

        assert!(pp.spec.template.files.is_some());

        let files = pp.spec.template.files.as_ref().unwrap();

        assert!(matches!(
            files.first().unwrap(),
            FilesSource::Secret {
                name,
                secret_ref: _
            } if name == "some-configs"
        ));

        assert!(matches!(
            files.get(1).unwrap(),
            FilesSource::Other {name, extra: _} if name == "binary-assets"
        ));

        println!("{pp:?}");
    }
}
