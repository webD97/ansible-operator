use std::collections::BTreeMap;

use k8s_openapi::{
    api::{
        batch,
        core::{
            self as kcore,
            v1::{KeyToPath, SecretVolumeSource, Volume},
        },
    },
    apimachinery::pkg::apis::meta::v1::OwnerReference,
};
use kube::runtime::reflector::Lookup as _;

use crate::{
    utils,
    v1beta1::{
        self, FilesSource, PlaybookPlan, PlaybookVariableSource,
        controllers::reconcile_error::ReconcileError, labels,
    },
};

pub fn create_job_for_host(
    host: &str,
    hash: u64,
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

    let pb_uid = object
        .metadata
        .uid
        .as_ref()
        .expect(".metadata.uid must be set here");

    let mut partial_job = match &object.spec.connection_strategy {
        v1beta1::ConnectionStrategy::Ssh { ssh } => {
            super::job_builder::create_ssh_job(host, object, ssh)
        }
        v1beta1::ConnectionStrategy::Chroot {} => todo!(),
    }?;

    partial_job.metadata.namespace = Some(pb_namespace.into());

    partial_job.metadata.owner_references = Some(vec![OwnerReference {
        api_version: v1beta1::PlaybookPlan::api_version(&()).into(),
        kind: v1beta1::PlaybookPlan::kind(&()).into(),
        name: pb_name.to_string(),
        uid: pb_uid.into(),
        ..Default::default()
    }]);

    partial_job.metadata.name = Some(format!(
        "apply-{pb_name}-{}-on-{host}",
        utils::generate_id(hash)
    ));
    partial_job.metadata.labels = Some(BTreeMap::from([
        (labels::PLAYBOOKPLAN_NAME.into(), pb_name.to_string()),
        (labels::PLAYBOOKPLAN_HASH.into(), hash.to_string()),
        (labels::PLAYBOOKPLAN_HOST.into(), host.into()),
    ]));

    Ok(partial_job)
}

/// Creates a Kubernetes Job to execute and SSH-based Ansible playbook.
fn create_ssh_job(
    host: &str,
    plan: &v1beta1::PlaybookPlan,
    ssh_config: &v1beta1::SshConfig,
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

    let mut volumes = vec![
        kcore::v1::Volume {
            name: "playbook".into(),
            secret: Some(kcore::v1::SecretVolumeSource {
                secret_name: Some(pb_name.into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        kcore::v1::Volume {
            name: "ssh".into(),
            secret: Some(kcore::v1::SecretVolumeSource {
                secret_name: Some(ssh_config.secret_ref.name.clone()),
                default_mode: Some(0o0400),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];

    let mut volume_mounts = vec![
        kcore::v1::VolumeMount {
            name: "playbook".into(),
            mount_path: "/run/ansible-operator".into(),
            ..Default::default()
        },
        kcore::v1::VolumeMount {
            name: "ssh".into(),
            mount_path: "/ssh".into(),
            ..Default::default()
        },
    ];

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
            mount_path: format!("/run/ansible-operator/vars/{secret_name}"),
            ..Default::default()
        });
    }

    for files_volume in extract_file_volumes(plan) {
        volumes.push(files_volume?);
        let volume = volumes.last().unwrap();

        volume_mounts.push(kcore::v1::VolumeMount {
            name: volume.name.clone(),
            mount_path: format!("/run/ansible-operator/files/{}", volume.name.clone()),
            ..Default::default()
        });
    }

    let pod_template = kcore::v1::PodTemplateSpec {
        metadata: None,
        spec: Some(kcore::v1::PodSpec {
            restart_policy: Some("Never".into()), // todo: maybe configurable
            volumes: Some(volumes),
            containers: vec![kcore::v1::Container {
                name: "ansible-playbook".into(),
                image: Some(plan.spec.image.clone()),
                working_dir: Some("/run/ansible-operator".into()),
                volume_mounts: Some(volume_mounts),
                command: Some(v1beta1::ansible::command_renderer::render_ansible_command(
                    plan,
                    host,
                    variable_secrets,
                )),
                ..Default::default()
            }],
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
    pp.spec
        .template
        .files
        .as_ref()
        .into_iter()
        .flat_map(|files| {
            files.iter().filter_map(|v| match v {
                FilesSource::Secret { .. } => None,
                FilesSource::Other { name, extra } => Some((name, extra)),
            })
        })
        .map(|(name, volume)| {
            let mut volume = serde_json::to_value(volume)?;
            volume
                .as_object_mut()
                .unwrap()
                .entry("name")
                .or_insert(serde_json::to_value(name)?);

            serde_json::from_value::<Volume>(volume)
        })
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

        let results = super::extract_file_volumes(&pp);
        let (oks, errs): (Vec<_>, Vec<_>) = results.partition(Result::is_ok);

        assert!(errs.is_empty(), "Some results were Err: {errs:#?}");

        let volumes: Vec<_> = oks.into_iter().map(Result::unwrap).collect();
        let volume1 = volumes.first().unwrap();

        assert_eq!("binary-assets", volume1.name);
        assert!(volume1.image.is_some());
        assert_eq!(
            volume1.image.as_ref().unwrap().reference,
            Some("my.registry.tld/the-image:v2".into())
        );
        assert_eq!(
            volume1.image.as_ref().unwrap().pull_policy,
            Some("IfNotPresent".into())
        );
    }
}
