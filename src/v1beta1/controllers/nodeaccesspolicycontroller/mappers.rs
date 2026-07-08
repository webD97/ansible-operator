use std::sync::Arc;

use kube::runtime::reflector::{ObjectRef, Store};

use crate::v1beta1::NodeAccessPolicy;

/// Maps any watched object (a Namespace or Node change) to *every* `NodeAccessPolicy`, so each
/// policy's status is recomputed when the namespaces/nodes it selects over change. The triggering
/// object is irrelevant — a policy's status depends on the whole namespace/node set — so this
/// mirrors `clusterinventorycontroller::mappers::node_to_inventories` but ignores the input.
pub fn to_all_policies<T>(
    policy_reader: Arc<Store<NodeAccessPolicy>>,
) -> impl Fn(T) -> Vec<ObjectRef<NodeAccessPolicy>> {
    move |_| {
        policy_reader
            .state()
            .iter()
            .map(|policy| ObjectRef::from(&**policy))
            .collect::<Vec<_>>()
    }
}
