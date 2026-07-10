use std::{sync::Arc, time::Duration};

use futures::{Stream, StreamExt as _};
use k8s_openapi::api::core::v1::{Namespace, Node};
use kube::{
    Api, ResourceExt,
    api::{ListParams, Patch, PatchParams},
    runtime::{
        Controller,
        controller::{self, Action},
        reflector::{Lookup, ObjectRef, store::Writer},
        watcher,
    },
};
use tracing::error;

use crate::v1beta1::{
    self, NodeAccessPolicy, NodeAccessPolicyStatus,
    controllers::{nodeselector::selector_matches_fail_closed, reconcile_error::ReconcileError},
};

use super::mappers;

struct ReconciliationContext {
    client: kube::Client,
}

pub fn new(
    client: kube::Client,
) -> impl Stream<
    Item = Result<
        (ObjectRef<v1beta1::NodeAccessPolicy>, Action),
        controller::Error<ReconcileError, kube::runtime::watcher::Error>,
    >,
> {
    let context = Arc::new(ReconciliationContext {
        client: client.clone(),
    });

    let policies_api: Api<NodeAccessPolicy> = Api::all(client.clone());
    let namespaces_api: Api<Namespace> = Api::all(client.clone());
    let nodes_api: Api<Node> = Api::all(client.clone());

    let policy_reflector_reader = {
        let policy_reflector_writer = Writer::<NodeAccessPolicy>::default();
        let policy_reflector_reader = Arc::new(policy_reflector_writer.as_reader());

        let policy_reflector = kube::runtime::reflector(
            policy_reflector_writer,
            watcher(policies_api.clone(), watcher::Config::default()),
        );

        tokio::spawn(async move {
            policy_reflector
                .for_each(|event| async {
                    if let Err(e) = event {
                        error!("Reflector error: {e:?}");
                    }
                })
                .await;
        });

        policy_reflector_reader
    };

    // Recompute every policy's status when any namespace or node changes — a policy's
    // matchedNamespaces/allowedNodeCount depend on the whole set, not one object.
    Controller::new(policies_api, watcher::Config::default())
        .watches(
            namespaces_api,
            watcher::Config::default(),
            mappers::to_all_policies(Arc::clone(&policy_reflector_reader)),
        )
        .watches(
            nodes_api,
            watcher::Config::default(),
            mappers::to_all_policies(Arc::clone(&policy_reflector_reader)),
        )
        .run(
            reconcile,
            |_, _, _| Action::requeue(Duration::from_secs(15)),
            context,
        )
}

async fn reconcile(
    object: Arc<NodeAccessPolicy>,
    context: Arc<ReconciliationContext>,
) -> Result<Action, ReconcileError> {
    // Fail-closed matching, identical to the enforcement path: an empty selector matches nothing.
    let namespaces_api: Api<Namespace> = Api::all(context.client.clone());
    let all_namespaces = namespaces_api.list_metadata(&ListParams::default()).await?;
    let matched_namespaces: Vec<String> = all_namespaces
        .iter()
        .filter(|ns| selector_matches_fail_closed(ns.labels(), &object.spec.namespace_selector))
        .filter_map(|ns| ns.metadata.name.clone())
        .collect();

    let nodes_api: Api<Node> = Api::all(context.client.clone());
    let all_nodes = nodes_api.list_metadata(&ListParams::default()).await?;
    let mut allowed_nodes: Vec<String> = all_nodes
        .iter()
        .filter(|node| selector_matches_fail_closed(node.labels(), &object.spec.node_selector))
        .filter_map(|node| node.metadata.name.clone())
        .collect();
    allowed_nodes.sort();

    let allowed_node_count = allowed_nodes.len() as i64;

    let next_status = NodeAccessPolicyStatus {
        matched_namespaces,
        allowed_node_count,
        allowed_nodes,
    };

    let api: Api<NodeAccessPolicy> = Api::all(context.client.clone());
    patch_status(&api, &object, next_status).await?;

    Ok(Action::requeue(Duration::from_hours(1)))
}

/// Persists `status` via a JSON merge patch — see the identical reasoning in
/// `playbookplancontroller::reconciler::patch_status`.
async fn patch_status(
    api: &Api<NodeAccessPolicy>,
    target: &NodeAccessPolicy,
    status: NodeAccessPolicyStatus,
) -> Result<(), ReconcileError> {
    let name = target
        .name()
        .ok_or(ReconcileError::PreconditionFailed("name not set"))?;

    api.patch_status(
        &name,
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "status": status })),
    )
    .await?;

    Ok(())
}
