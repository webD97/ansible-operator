use std::sync::Arc;

use k8s_openapi::api::core::v1::Node;
use kube::runtime::reflector::ObjectRef;
use tracing::debug;

use crate::v1beta1;

/// Returns a closure that returns a list of ObjectRefs for all ClusterInventory resources
///
/// # Panics
///
/// Panics if the node returned from the apiserver does not have a name.
pub fn node_to_inventories(
    cluster_inventory_reader: Arc<kube::runtime::reflector::Store<v1beta1::ClusterInventory>>,
) -> impl Fn(Node) -> Vec<ObjectRef<v1beta1::ClusterInventory>> {
    move |node| {
        cluster_inventory_reader
            .state()
            .iter()
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
