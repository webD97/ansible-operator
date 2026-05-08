use std::sync::Arc;

use k8s_openapi::api::core::v1::Node;
use kube::runtime::reflector::ObjectRef;
use tracing::debug;

use crate::v1beta1;

/// Returns a closure that maps a Node to all PlaybookPlans that might reference it, i.e. all nodes
/// with and inventory that contains Hosts::FromClusterNodes.
///
/// # Panics
///
/// Panics if the node returned from the apiserver does not have a name.
pub fn node_to_inventories(
    node_reflector_reader: Arc<kube::runtime::reflector::Store<v1beta1::AnsibleInventory>>,
) -> impl Fn(Node) -> Vec<ObjectRef<v1beta1::AnsibleInventory>> {
    move |node| {
        node_reflector_reader
            .state()
            .iter()
            .filter(|resource| {
                resource
                    .spec
                    .hosts
                    .iter()
                    .any(|inventory| match &inventory.source() {
                        Err(_) => false,
                        Ok(source) => match source {
                            v1beta1::HostSource::FromClusterNodes { .. } => true,
                            v1beta1::HostSource::FromHostnames { .. } => false,
                        },
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
