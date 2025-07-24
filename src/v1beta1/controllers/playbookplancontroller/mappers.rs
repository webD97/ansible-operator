use std::sync::Arc;

use k8s_openapi::api::core::v1::{Node, Secret};
use kube::runtime::reflector::ObjectRef;
use tracing::debug;

use crate::v1beta1;

/// Returns a closure that maps a Node to all PlaybookPlans that might reference it, i.e. all nodes
/// with and inventory that contains Hosts::FromClusterNodes.
///
/// # Panics
///
/// Panics if the node returned from the apiserver does not have a name.
pub fn node_to_playbookplans(
    node_reflector_reader: Arc<kube::runtime::reflector::Store<v1beta1::PlaybookPlan>>,
) -> impl Fn(Node) -> Vec<ObjectRef<v1beta1::PlaybookPlan>> {
    move |node| {
        node_reflector_reader
            .state()
            .iter()
            .filter(|resource| {
                resource
                    .spec
                    .inventory
                    .iter()
                    .any(|inventory| match &inventory.hosts {
                        v1beta1::Hosts::FromClusterNodes { .. } => true,
                        v1beta1::Hosts::FromStaticList { .. } => false,
                    })
            })
            .map(|resource| ObjectRef::from(&**resource))
            .inspect(|object_ref| {
                debug!(
                    "Reconcile of {} triggered by node {}",
                    object_ref,
                    node.metadata.name.as_ref().unwrap()
                )
            })
            .collect::<Vec<_>>()
    }
}

/// Returns a closure that maps a Secret to all PlaybookPlans that reference it.
///
/// # Panics
///
/// Panics if the secret returned from the apiserver does not have a name.
pub fn secret_to_playbookplans(
    secret_reflector_reader: Arc<kube::runtime::reflector::Store<v1beta1::PlaybookPlan>>,
) -> impl Fn(Secret) -> Vec<ObjectRef<v1beta1::PlaybookPlan>> {
    move |secret| {
        let secret_name = secret
            .metadata
            .name
            .as_deref()
            .expect("Secret must have a name");

        secret_reflector_reader
            .state()
            .iter()
            .filter(|resource| resource.metadata.namespace == secret.metadata.namespace)
            .filter(|plan| {
                if let Some(vars) = &plan.spec.template.variables {
                    if vars.iter().any(|var| {
                        matches!(
                            var,
                            v1beta1::PlaybookVariableSource::SecretRef { secret_ref }
                            if secret_ref.name == secret_name
                        )
                    }) {
                        return true;
                    }
                }

                if let Some(files) = &plan.spec.template.files {
                    return files.iter().any(|file| {
                        matches!(
                            file,
                            v1beta1::FilesSource::Secret { name: _, secret_ref }
                            if secret_ref.name == secret_name
                        )
                    });
                }

                false
            })
            .map(|plan| ObjectRef::from(&**plan))
            .inspect(|obj_ref| {
                debug!(
                    "Reconcile of {} triggered by secret {}",
                    obj_ref, secret_name
                )
            })
            .collect::<Vec<_>>()
    }
}
