use std::{sync::Arc, time::Duration};

use futures::{Stream, StreamExt as _};
use k8s_openapi::api::core::v1::Node;
use kube::{
    Api,
    api::{ListParams, PostParams},
    runtime::{
        Controller,
        controller::{self, Action},
        reflector::{Lookup, ObjectRef, store::Writer},
        watcher,
    },
};
use tracing::error;

use crate::v1beta1::{
    self, AnsibleInventory, AnsibleInventorySpec, AnsibleInventoryStatus,
    controllers::{nodeselector::node_matches, reconcile_error::ReconcileError},
    inventorycontroller::mappers,
};

struct ReconciliationContext {
    client: kube::Client,
}
pub fn new(
    client: kube::Client,
) -> impl Stream<
    Item = Result<
        (ObjectRef<v1beta1::AnsibleInventory>, Action),
        controller::Error<ReconcileError, kube::runtime::watcher::Error>,
    >,
> {
    let context = Arc::new(ReconciliationContext {
        client: client.clone(),
    });

    let inventories_api: Api<v1beta1::AnsibleInventory> = Api::all(client.clone());
    let nodes_api: Api<Node> = Api::all(client.clone());

    let inventory_reflector_reader = {
        let inventory_reflector_writer = Writer::<v1beta1::AnsibleInventory>::default();
        let inventory_reflector_reader = Arc::new(inventory_reflector_writer.as_reader());

        let inventory_reflector = kube::runtime::reflector(
            inventory_reflector_writer,
            watcher(inventories_api.clone(), watcher::Config::default()),
        );

        tokio::spawn(async move {
            inventory_reflector
                .for_each(|event| async {
                    match event {
                        Ok(_) => {}
                        Err(e) => error!("Reflector error: {e:?}"),
                    }
                })
                .await;
        });

        inventory_reflector_reader
    };

    Controller::new(inventories_api, watcher::Config::default())
        .watches(
            nodes_api,
            watcher::Config::default(),
            mappers::node_to_inventories(Arc::clone(&inventory_reflector_reader)),
        )
        .run(
            reconcile,
            |_, _, _| Action::requeue(std::time::Duration::from_secs(15)),
            Arc::clone(&context),
        )
}

async fn reconcile(
    object: Arc<v1beta1::AnsibleInventory>,
    context: Arc<ReconciliationContext>,
) -> Result<Action, ReconcileError> {
    let namespace = object
        .namespace()
        .ok_or(ReconcileError::PreconditionFailed("namespace not set"))?;

    let nodes_api: Api<Node> = Api::all(context.client.clone());
    let all_nodes = nodes_api.list_metadata(&ListParams::default()).await?;

    let to_resolve = &object.spec.hosts;
    let resolved_hosts: Vec<v1beta1::ResolvedHosts> = to_resolve
        .iter()
        .map(|group| {
            let name = group.name.to_owned();
            let hosts = match group.source() {
                Err(_) => Vec::new(),
                Ok(source) => match source {
                    v1beta1::HostSource::FromClusterNodes { from_cluster_nodes } => all_nodes
                        .iter()
                        .filter(|node| node_matches(node, from_cluster_nodes))
                        .map(|node| node.name().expect("name is set").to_string())
                        .collect(),
                    v1beta1::HostSource::FromHostnames { from_hostnames } => {
                        from_hostnames.to_vec()
                    }
                },
            };

            v1beta1::ResolvedHosts { name, hosts }
        })
        .collect();

    let host_count: usize = resolved_hosts.iter().map(|group| group.hosts.len()).sum();

    let next_status = AnsibleInventoryStatus {
        host_count,
        resolved_hosts,
    };

    let api: Api<AnsibleInventory> = Api::namespaced(context.client.clone(), &namespace);
    replace_status(api, &object, next_status).await?;

    Ok(Action::requeue(Duration::from_hours(1)))
}

async fn replace_status(
    api: Api<AnsibleInventory>,
    target: &AnsibleInventory,
    status: AnsibleInventoryStatus,
) -> Result<(), ReconcileError> {
    let name = target
        .name()
        .ok_or(ReconcileError::PreconditionFailed("name not set"))?;

    let patch_object = AnsibleInventory {
        metadata: target.metadata.clone(),
        spec: AnsibleInventorySpec::default(),
        status: Some(status),
    };

    api.replace_status(&name, &PostParams::default(), &patch_object)
        .await?;

    Ok(())
}
