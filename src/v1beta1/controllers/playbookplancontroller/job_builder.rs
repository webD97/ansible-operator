use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::{
    api::{
        batch::{self, v1::Job},
        core::{
            self as kcore,
            v1::{EmptyDirVolumeSource, EnvVar, KeyToPath, SecretVolumeSource, Volume},
        },
    },
    apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference},
};
use kube::runtime::reflector::Lookup as _;

use crate::{
    utils,
    v1beta1::{
        self, FilesSource, PlaybookPlan, PlaybookVariableSource, ResolvedInventoryGroup, SshConfig,
        controllers::reconcile_error::ReconcileError, labels,
        playbookplancontroller::{execution_evaluator::ExecutionHash, managed_ssh, paths},
    },
};

pub fn create_job_for_run(
    hash: &ExecutionHash,
    retry_count: u32,
    target_groups: &[ResolvedInventoryGroup],
    object: &PlaybookPlan,
) -> Result<batch::v1::Job, ReconcileError> {
    let pb_name = object
        .metadata
        .name
        .as_ref()
        .expect(".metadata.name must be set here");

    let pb_namespace = object
        .metadata
        .namespace
        .as_ref()
        .expect(".metadata.namespace must be set here");

    let mut job = create_job_skeleton(object, object.spec.template.requirements.is_some())?;

    if has_managed_ssh_group(target_groups) {
        let secret_name = managed_ssh::client_cert_secret_name(hash);
        configure_job_for_managed_ssh_client_cert(&mut job, &secret_name);
    }

    let ssh_configs = distinct_static_inventory_ssh_configs(target_groups);
    if !ssh_configs.is_empty() {
        configure_job_for_ssh(&mut job, &ssh_configs);
    }

    configure_job_for_callback_plugin(&mut job);

    job.metadata.namespace = Some(pb_namespace.into());

    // retry_count must be in the name — the hash alone is unchanged between retries of an
    // identical spec, so without it a new run's Job name would collide with a completed prior
    // run's and get silently skipped by the idempotency check.
    job.metadata.name = Some(format!(
        "apply-{pb_name}-{}-{retry_count}",
        utils::generate_id(**hash),
    ));

    let job_labels: BTreeMap<String, String> = BTreeMap::from([
        (labels::PLAYBOOKPLAN_NAME.into(), pb_name.to_string()),
        (labels::PLAYBOOKPLAN_HASH.into(), hash.to_string()),
    ]);
    job.metadata.labels = Some(job_labels.clone());

    // The NetworkPolicy scoping managed-ssh proxy-pod ingress selects on the execution-hash
    // label of the actual running Pod, not just the Job object — Jobs don't carry their own
    // labels down to their Pods unless the pod template's own metadata sets them explicitly.
    if let Some(spec) = job.spec.as_mut() {
        spec.template.metadata = Some(ObjectMeta {
            labels: Some(job_labels),
            ..Default::default()
        });
    }

    Ok(job)
}

/// Creates a Kubernetes Job with everything needed for basic Ansible execution, without any
/// connection-specifics. Unlike the old chroot-based model, this Job pod needs no node-level
/// privilege at all — hostPID/hostIPC/hostNetwork/privileged/nodeSelector all now live on the
/// ephemeral managed-ssh proxy pods instead (see `managed_ssh.rs`).
fn create_job_skeleton(
    plan: &v1beta1::PlaybookPlan,
    with_requirements: bool,
) -> Result<batch::v1::Job, ReconcileError> {
    let pb_name = plan.name().ok_or(ReconcileError::PreconditionFailed(
        "expected .metadata.name in PlaybookPlan",
    ))?;

    let pb_uid = plan.uid().ok_or(ReconcileError::PreconditionFailed(
        "expected .metadata.uid in PlaybookPlan",
    ))?;

    let mut job = batch::v1::Job::default();

    job.metadata.owner_references = Some(vec![OwnerReference {
        api_version: v1beta1::PlaybookPlan::api_version(&()).into(),
        kind: v1beta1::PlaybookPlan::kind(&()).into(),
        name: pb_name.to_string(),
        uid: pb_uid.into(),
        ..Default::default()
    }]);

    let variable_secrets: Vec<&String> = extract_secret_names_for_variables(plan).collect();

    let mut volumes = vec![kcore::v1::Volume {
        name: "playbook".into(),
        secret: Some(kcore::v1::SecretVolumeSource {
            secret_name: Some(pb_name.into()),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let mut volume_mounts = vec![kcore::v1::VolumeMount {
        name: "playbook".into(),
        mount_path: paths::WORKSPACE_MOUNT_PATH.into(),
        ..Default::default()
    }];

    for secret_name in &variable_secrets {
        volumes.push(kcore::v1::Volume {
            name: secret_name.to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret_name.to_string()),
                default_mode: Some(0o0400),
                items: Some(vec![KeyToPath {
                    key: "variables.yaml".into(),
                    path: "variables.yaml".into(),
                    mode: None,
                }]),
                ..Default::default()
            }),
            ..Default::default()
        });

        volume_mounts.push(kcore::v1::VolumeMount {
            name: secret_name.to_string(),
            mount_path: format!("{}/vars/{secret_name}", paths::WORKSPACE_MOUNT_PATH),
            ..Default::default()
        });
    }

    for files_volume in extract_file_volumes(plan) {
        volumes.push(files_volume?);
        let volume = volumes.last().unwrap();

        volume_mounts.push(kcore::v1::VolumeMount {
            name: volume.name.clone(),
            mount_path: format!("{}/files/{}", paths::WORKSPACE_MOUNT_PATH, volume.name.clone()),
            ..Default::default()
        });
    }

    let mut init_containers = Vec::new();

    // Add an initcontainer to install collections (workaround until we can use image volumes)
    if with_requirements {
        volumes.push(kcore::v1::Volume {
            name: "collections".into(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        });

        volume_mounts.push(kcore::v1::VolumeMount {
            name: "collections".into(),
            mount_path: "/etc/ansible/collections".into(),
            ..Default::default()
        });

        let collections_installer = kcore::v1::Container {
            name: "download-collections".into(),
            image: Some(plan.spec.image.clone()),
            working_dir: Some(paths::WORKSPACE_MOUNT_PATH.into()),
            volume_mounts: Some(volume_mounts.clone()),
            command: Some(vec![
                "ansible-galaxy".into(),
                "install".into(),
                "-r".into(),
                "requirements.yml".into(),
            ]),
            ..Default::default()
        };

        init_containers.push(collections_installer);
    }

    let main_container = kcore::v1::Container {
        name: "ansible-playbook".into(),
        image: Some(plan.spec.image.clone()),
        working_dir: Some(paths::WORKSPACE_MOUNT_PATH.into()),
        volume_mounts: Some(volume_mounts),
        command: Some(render_ansible_command(plan, variable_secrets)),
        ..Default::default()
    };

    let pod_template = kcore::v1::PodTemplateSpec {
        metadata: None,
        spec: Some(kcore::v1::PodSpec {
            restart_policy: Some("Never".into()), // todo: maybe configurable
            volumes: Some(volumes),
            containers: vec![main_container],
            init_containers: Some(init_containers),
            ..Default::default()
        }),
    };

    let job_spec = batch::v1::JobSpec {
        backoff_limit: Some(0), // todo: maybe configurable
        template: pod_template,
        ..Default::default()
    };

    job.spec = Some(job_spec);

    Ok(job)
}

fn has_managed_ssh_group(groups: &[ResolvedInventoryGroup]) -> bool {
    groups
        .iter()
        .any(|g| matches!(g, ResolvedInventoryGroup::ManagedSsh { .. }))
}

/// Distinct `(StaticInventory name, SshConfig)` pairs referenced by this run's groups, deduped
/// by resource name — a run's Job pod needs one mounted SSH secret per distinct StaticInventory
/// it targets, not one per host-group (multiple groups can come from the same resource).
fn distinct_static_inventory_ssh_configs(
    groups: &[ResolvedInventoryGroup],
) -> Vec<(String, SshConfig)> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();

    for group in groups {
        if let ResolvedInventoryGroup::Ssh {
            static_inventory_name,
            config,
            ..
        } = group
            && seen.insert(static_inventory_name.clone())
        {
            result.push((static_inventory_name.clone(), config.clone()));
        }
    }

    result
}

/// Mounts one SSH secret per distinct `StaticInventory` referenced this run, each at its own
/// resource-name-keyed path (`paths::static_inventory_ssh_dir`) so multiple StaticInventories
/// with different credentials can coexist in the same Job pod without colliding.
fn configure_job_for_ssh(job: &mut Job, ssh_configs: &[(String, SshConfig)]) {
    job.spec.as_mut().and_then(|spec| {
        spec.template.spec.as_mut().map(|pod_spec| {
            let main_container = pod_spec
                .containers
                .first_mut()
                .expect("job should have a container");

            for (static_inventory_name, config) in ssh_configs {
                let volume_name = format!("ssh-{static_inventory_name}");

                pod_spec.volumes.get_or_insert_default().push(Volume {
                    name: volume_name.clone(),
                    secret: Some(SecretVolumeSource {
                        secret_name: Some(config.secret_ref.name.clone()),
                        default_mode: Some(0o0400),
                        ..Default::default()
                    }),
                    ..Default::default()
                });

                main_container
                    .volume_mounts
                    .get_or_insert_default()
                    .push(kcore::v1::VolumeMount {
                        name: volume_name,
                        mount_path: paths::static_inventory_ssh_dir(static_inventory_name),
                        ..Default::default()
                    });
            }
        })
    });
}

/// Mounts this run's managed-ssh client identity. The Secret is expected to already exist by the
/// time the Job is created (`managed_ssh::ensure_proxy_infra`'s `ensure_client_cert` step).
fn configure_job_for_managed_ssh_client_cert(job: &mut Job, secret_name: &str) {
    job.spec.as_mut().and_then(|spec| {
        spec.template.spec.as_mut().map(|pod_spec| {
            let main_container = pod_spec
                .containers
                .first_mut()
                .expect("job should have a container");

            pod_spec.volumes.get_or_insert_default().push(Volume {
                name: "managed-ssh-client".into(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(secret_name.to_string()),
                    default_mode: Some(0o0400),
                    ..Default::default()
                }),
                ..Default::default()
            });

            main_container
                .volume_mounts
                .get_or_insert_default()
                .push(kcore::v1::VolumeMount {
                    name: "managed-ssh-client".into(),
                    mount_path: paths::MANAGED_SSH_CLIENT_DIR.into(),
                    ..Default::default()
                });
        })
    });
}

/// Sets the env vars that make Ansible load and use the operator's per-host-outcome recap
/// callback (rendered into the workspace secret alongside playbook.yml/inventory.yml — see
/// `workspace.rs`), without disabling the default human-readable stdout callback.
fn configure_job_for_callback_plugin(job: &mut Job) {
    job.spec.as_mut().and_then(|spec| {
        spec.template.spec.as_mut().map(|pod_spec| {
            let main_container = pod_spec
                .containers
                .first_mut()
                .expect("job should have a container");

            main_container.env.get_or_insert_default().extend([
                EnvVar {
                    name: "ANSIBLE_CALLBACKS_ENABLED".into(),
                    value: Some("ansible_operator_recap".into()),
                    ..Default::default()
                },
                EnvVar {
                    name: "ANSIBLE_CALLBACK_PLUGINS".into(),
                    value: Some(paths::WORKSPACE_MOUNT_PATH.into()),
                    ..Default::default()
                },
            ]);
        })
    });
}

pub fn extract_secret_names_for_variables(pp: &PlaybookPlan) -> impl Iterator<Item = &String> {
    pp.spec
        .template
        .variables
        .as_ref()
        .into_iter()
        .flat_map(|variables| {
            variables.iter().filter_map(|v| match v {
                PlaybookVariableSource::Inline { inline: _ } => None,
                PlaybookVariableSource::SecretRef { secret_ref } => Some(&secret_ref.name),
            })
        })
}

pub fn extract_secret_names_for_files(pp: &PlaybookPlan) -> impl Iterator<Item = &String> {
    pp.spec
        .template
        .files
        .as_ref()
        .into_iter()
        .flat_map(|files| {
            files.iter().filter_map(|v| match v {
                FilesSource::Other { .. } => None,
                FilesSource::Secret { secret_ref, .. } => Some(&secret_ref.name),
            })
        })
}

/// Takes the mostly schemarless volumes defined the PlaybookPlan and turns them into
/// proper Kubernetes Volumes that can be used in a PodSpec. This is necessary because
/// we don't want to handle every possible kind of volume in our code.
///
/// Instead we use serialiation magic to turn whatever the user gave us into whatever
/// the currently targeted Kubernetes version supports. This can fail if the user tries
/// to use a volume kind that does not exist, hence each item in the Iterator has its
/// own Result.
fn extract_file_volumes(
    pp: &PlaybookPlan,
) -> impl Iterator<Item = Result<Volume, serde_json::Error>> {
    let files = pp.spec.template.files.as_ref();

    files.into_iter().flatten().map(|source| {
        let value = match source {
            FilesSource::Secret { name, secret_ref } => serde_json::to_value(kcore::v1::Volume {
                name: name.to_owned(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(secret_ref.name.to_owned()),
                    ..Default::default()
                }),
                ..Default::default()
            })?,
            FilesSource::Other { name, extra } => {
                let mut volume = serde_json::to_value(extra)?;
                volume
                    .as_object_mut()
                    .unwrap()
                    .entry("name")
                    .or_insert(serde_json::to_value(name)?);

                volume
            }
        };
        serde_json::from_value::<Volume>(value)
    })
}

/// Builds the `ansible-playbook` invocation. Connection details no longer appear here at all —
/// each host's connection mechanism is expressed as inventory vars in the rendered
/// `inventory.yml` instead, so there's no more per-strategy `-c`/`-l`/`--private-key` branching.
fn render_ansible_command(plan: &v1beta1::PlaybookPlan, extra_vars_filepaths: Vec<&String>) -> Vec<String> {
    let static_vars_filenames: Vec<String> = plan
        .spec
        .template
        .variables
        .as_ref()
        .map(|variables| {
            variables
                .iter()
                .filter_map(|source| match source {
                    PlaybookVariableSource::SecretRef { secret_ref: _ } => None,
                    PlaybookVariableSource::Inline { inline: _ } => Some(()),
                })
                .enumerate()
                .map(|(index, _)| format!("static-variables-{index}.yml"))
                .collect()
        })
        .unwrap_or_default();

    let mut ansible_command = vec!["ansible-playbook".into()];

    ansible_command.extend(
        static_vars_filenames
            .iter()
            .flat_map(|path| ["--extra-vars".into(), format!("@{path}")]),
    );

    ansible_command.extend(extra_vars_filepaths.iter().flat_map(|path| {
        [
            "--extra-vars".into(),
            format!("@{}/vars/{path}/variables.yaml", paths::WORKSPACE_MOUNT_PATH),
        ]
    }));

    ansible_command.extend(["-i".into(), "inventory.yml".into()]);
    ansible_command.push("playbook.yml".into());

    ansible_command
}

#[cfg(test)]
mod tests {
    use crate::v1beta1::PlaybookPlan;

    #[test]
    fn test_extract_file_volumes_generates_correct_volumes() {
        let yaml = r#"
apiVersion: ansible.cloudbending.dev/v1beta1
kind: PlaybookPlan
metadata:
  name: an-example
spec:
  image: docker.io/serversideup/ansible-core:2.18
  mode: OneShot
  inventoryRefs:
    - name: something
      staticInventory: blubb
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

        let results = super::extract_file_volumes(&pp);
        let (oks, errs): (Vec<_>, Vec<_>) = results.partition(Result::is_ok);

        assert!(errs.is_empty(), "Some results were Err: {errs:#?}");

        let volumes: Vec<_> = oks.into_iter().map(Result::unwrap).collect();
        let volume1 = volumes.first().unwrap();
        let volume2 = volumes.get(1).unwrap();

        assert_eq!("some-configs", volume1.name);
        assert!(volume1.secret.is_some());
        assert_eq!(
            volume1.secret.as_ref().unwrap().secret_name,
            Some("secret-with-config-files".into())
        );

        assert_eq!("binary-assets", volume2.name);
        assert!(volume2.image.is_some());
        assert_eq!(
            volume2.image.as_ref().unwrap().reference,
            Some("my.registry.tld/the-image:v2".into())
        );
        assert_eq!(
            volume2.image.as_ref().unwrap().pull_policy,
            Some("IfNotPresent".into())
        );
    }

    #[test]
    fn render_ansible_command_has_no_connection_flags_and_uses_full_inventory() {
        use crate::v1beta1::controllers::playbookplancontroller::job_builder::render_ansible_command;

        let yaml = r#"
apiVersion: ansible.cloudbending.dev/v1beta1
kind: PlaybookPlan
metadata:
  name: an-example
spec:
  image: docker.io/serversideup/ansible-core:2.18
  mode: OneShot
  inventoryRefs: []
  template:
    playbook: |
      - hosts: all
        tasks: []
        "#;
        let pp = serde_yaml::from_str::<PlaybookPlan>(yaml).unwrap();

        let command = render_ansible_command(&pp, Vec::new());

        assert!(!command.iter().any(|arg| arg == "-c"));
        assert!(!command.iter().any(|arg| arg == "-l"));
        assert!(!command.iter().any(|arg| arg == "--private-key"));
        assert!(command.iter().any(|arg| arg == "inventory.yml"));
        assert!(command.iter().any(|arg| arg == "playbook.yml"));
    }

    #[test]
    fn create_job_for_run_names_by_retry_count_not_a_time_nonce() {
        use crate::v1beta1::controllers::playbookplancontroller::execution_evaluator::calculate_execution_hash;
        use kube::runtime::reflector::Lookup as _;

        let yaml = r#"
apiVersion: ansible.cloudbending.dev/v1beta1
kind: PlaybookPlan
metadata:
  name: an-example
  namespace: default
  uid: 11111111-1111-1111-1111-111111111111
spec:
  image: docker.io/serversideup/ansible-core:2.18
  mode: OneShot
  inventoryRefs: []
  template:
    playbook: |
      - hosts: all
        tasks: []
        "#;
        let pp = serde_yaml::from_str::<PlaybookPlan>(yaml).unwrap();
        let hash = calculate_execution_hash("- hosts: all", std::iter::empty());

        let attempt_1 = super::create_job_for_run(&hash, 1, &[], &pp).unwrap();
        let attempt_2 = super::create_job_for_run(&hash, 2, &[], &pp).unwrap();
        let attempt_1_again = super::create_job_for_run(&hash, 1, &[], &pp).unwrap();

        let name_1 = attempt_1.name().unwrap().to_string();
        let name_2 = attempt_2.name().unwrap().to_string();
        let name_1_again = attempt_1_again.name().unwrap().to_string();

        assert_eq!(
            name_1, name_1_again,
            "same hash + same retry_count must be deterministic"
        );
        assert_ne!(
            name_1, name_2,
            "different retry_count for the same spec must produce a different name"
        );
        assert!(name_1.ends_with("-1"));
        assert!(name_2.ends_with("-2"));

        // The shortid portion stays the same across retries — it's the spec-version identifier.
        let shortid_1 = name_1.rsplit_once('-').unwrap().0;
        let shortid_2 = name_2.rsplit_once('-').unwrap().0;
        assert_eq!(shortid_1, shortid_2);
    }
}
