use std::collections::BTreeMap;

use k8s_openapi::{
    api::{
        batch,
        core::{self as kcore, v1::SecretVolumeSource},
    },
    apimachinery::pkg::apis::meta::v1::OwnerReference,
};
use kube::runtime::reflector::Lookup as _;

use crate::v1beta1::{
    self, PlaybookPlan, PlaybookVariableSource, controllers::reconcile_error::ReconcileError,
};

/// Creates a Kubernetes Job to execute and SSH-based Ansible playbook.
pub fn create_job_for_ssh_playbook(
    host: &str,
    plan: &v1beta1::PlaybookPlan,
    ssh_config: &v1beta1::SshConfig,
    job_prefix: &str,
) -> Result<batch::v1::Job, ReconcileError> {
    let pb_namespace = plan.namespace().ok_or(ReconcileError::PreconditionFailed(
        "expected .metadata.namespace in PlaybookPlan",
    ))?;

    let pb_name = plan.name().ok_or(ReconcileError::PreconditionFailed(
        "expected .metadata.name in PlaybookPlan",
    ))?;

    let pb_uid = plan.uid().ok_or(ReconcileError::PreconditionFailed(
        "expected .metadata.uid in PlaybookPlan",
    ))?;

    let mut job = batch::v1::Job::default();

    job.metadata.namespace = Some(pb_namespace.into());
    job.metadata.name = Some(format!("{job_prefix}-on-{host}"));

    job.metadata.owner_references = Some(vec![OwnerReference {
        api_version: v1beta1::PlaybookPlan::api_version(&()).into(),
        kind: v1beta1::PlaybookPlan::kind(&()).into(),
        name: pb_name.to_string(),
        uid: pb_uid.into(),
        ..Default::default()
    }]);

    job.metadata.labels = Some(BTreeMap::from([
        (
            "ansible.cloudbending.dev/playbookplan".into(),
            job_prefix.into(),
        ),
        ("ansible.cloudbending.dev/target-host".into(), host.into()),
    ]));

    let variable_secrets = extract_secret_names_for_variables(plan);

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

pub fn extract_secret_names_for_variables(pp: &PlaybookPlan) -> Vec<&String> {
    let Some(ref variables) = pp.spec.template.variables else {
        return Vec::new();
    };

    variables
        .iter()
        .filter_map(|v| match v {
            PlaybookVariableSource::Inline { inline: _ } => None,
            PlaybookVariableSource::SecretRef { secret_ref } => Some(&secret_ref.name),
        })
        .collect()
}
