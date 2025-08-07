use std::collections::BTreeMap;

use k8s_openapi::{api::core::v1::Secret, apimachinery::pkg::apis::meta::v1::OwnerReference};
use kube::runtime::reflector::Lookup;

use crate::v1beta1::{
    PlaybookPlan, PlaybookVariableSource, ansible, controllers::reconcile_error::ReconcileError,
};

pub fn is_outdated(object: &PlaybookPlan) -> bool {
    let generation = object
        .metadata
        .generation
        .expect(".metdata.generation must be set at this point");

    object
        .status
        .as_ref()
        .and_then(|s| s.last_rendered_generation)
        .map(|g| g < generation)
        .unwrap_or(true)
}

pub async fn is_missing(secrets_api: &kube::Api<Secret>, name: &str) -> Result<bool, kube::Error> {
    Ok(secrets_api.get_opt(name).await?.is_none())
}

/// Creates a Kubernetes secret that contains an inventory.yml, a playbook.yml and any static-variables*.yaml
/// for a given PlaybookPlan so that the playbook can be executed afterwards. The workspace is host-agnostic.
///
/// # Panics
///
/// Panics if the playbookplan does not have a namespace, name or uid
///
pub fn render_secret(
    object: &PlaybookPlan,
    inventories: &BTreeMap<String, Vec<String>>,
) -> Result<Secret, ReconcileError> {
    let pb_namespace = object
        .metadata
        .namespace
        .as_ref()
        .expect(".metdata.namespace must be set at this point");

    let pb_name = object
        .metadata
        .name
        .as_ref()
        .expect(".metdata.name must be set at this point");

    let pb_uid = object
        .metadata
        .uid
        .as_ref()
        .expect(".metdata.uid must be set at this point");

    let mut secret = Secret::default();

    secret.metadata.namespace = Some(pb_namespace.into());
    secret.metadata.name = Some(pb_name.into());

    secret.metadata.owner_references = Some(vec![OwnerReference {
        api_version: PlaybookPlan::api_version(&()).into(),
        kind: PlaybookPlan::kind(&()).into(),
        name: pb_name.into(),
        uid: pb_uid.into(),
        ..Default::default()
    }]);

    let rendered_playbook = ansible::render_playbook(&object.spec)?;
    let rendered_inventory = ansible::render_inventory(inventories)?;

    let inlined_variables = match &object.spec.template.variables {
        Some(variable_sources) => variable_sources
            .iter()
            .filter_map(|source| match source {
                PlaybookVariableSource::SecretRef { secret_ref: _ } => None,
                PlaybookVariableSource::Inline { inline } => Some(inline),
            })
            .map(serde_yaml::to_string)
            .collect(),
        None => Vec::new(),
    };

    let mut string_data = BTreeMap::new();
    string_data.insert("playbook.yml".into(), rendered_playbook);
    string_data.insert("inventory.yml".into(), rendered_inventory);

    if let Some(requirements) = &object.spec.template.requirements {
        string_data.insert("requirements.yml".into(), requirements.to_owned());
    }

    for (index, variable_set) in inlined_variables.into_iter().enumerate() {
        string_data.insert(format!("static-variables-{index}.yml"), variable_set?);
    }

    secret.string_data = Some(string_data);

    Ok(secret)
}
