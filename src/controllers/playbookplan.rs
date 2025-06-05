use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures_util::Stream;
use k8s_openapi::api::core::v1::Node;
use kube::{
    api::{ListParams, PostParams},
    runtime::{Controller, controller::Action, reflector::Lookup as _},
};
use tracing::info;

use crate::{
    nodeselector::node_matches,
    resources::playbookplan::{Hosts, Phase, PlaybookPlan, PlaybookPlanStatus},
};

struct Context {
    client: kube::Client,
}

pub fn new(
    kubeconfig: kube::Config,
) -> impl Stream<
    Item = Result<
        (kube::runtime::reflector::ObjectRef<PlaybookPlan>, Action),
        kube::runtime::controller::Error<kube::Error, kube::runtime::watcher::Error>,
    >,
> {
    let client = kube::client::Client::try_from(kubeconfig).unwrap();

    let context = Arc::new(Context {
        client: client.clone(),
    });

    let root_api: kube::Api<PlaybookPlan> = kube::Api::all(client);

    Controller::new(root_api, kube::runtime::watcher::Config::default()).run(
        reconcile,
        error_policy,
        Arc::clone(&context),
    )
}

fn error_policy(
    _object: Arc<PlaybookPlan>,
    _error: &kube::Error,
    _context: Arc<Context>,
) -> Action {
    Action::requeue(Duration::from_secs(15))
}

async fn reconcile(
    object: Arc<PlaybookPlan>,
    context: Arc<Context>,
) -> Result<Action, kube::Error> {
    // Check for deletion
    if object.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }

    let playbookplan_api: kube::Api<PlaybookPlan> = kube::Api::namespaced(
        context.client.clone(),
        &object.metadata.namespace.clone().unwrap(),
    );

    // Setup initial status and requeue
    if object.status.is_none() {
        info!("Populating initial status");
        setup_initial_status(&object, &playbookplan_api).await?;
        return Ok(Action::requeue(Duration::ZERO));
    }

    // Resolve groups
    info!("Resolving groups");
    let resolved_inventories = resolve_inventories(context, &object).await?;
    let mut patch_object = playbookplan_api.get_status(&object.name().unwrap()).await?;

    if let Some(ref mut status) = patch_object.status {
        status.eligible_hosts_count = Some(
            resolved_inventories
                .values()
                .flatten()
                .cloned()
                .collect::<std::collections::HashSet<String>>()
                .len(),
        );
        status.eligible_hosts = Some(resolved_inventories);
    }

    playbookplan_api
        .replace_status(
            object.metadata.name.as_ref().unwrap(),
            &PostParams::default(),
            serde_json::to_vec(&patch_object).unwrap(),
        )
        .await?;

    Ok(Action::requeue(Duration::from_secs(3600)))
}

async fn resolve_inventories(
    context: Arc<Context>,
    object: &PlaybookPlan,
) -> Result<BTreeMap<String, Vec<String>>, kube::Error> {
    let inventories_spec = &object.spec.inventory;

    let mut resolved: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for inventory in inventories_spec {
        let resolved_hosts = resolve_hosts(&inventory.hosts, Arc::clone(&context)).await?;
        resolved.insert(inventory.name.clone(), resolved_hosts);
    }

    Ok(resolved)
}

async fn resolve_hosts(
    hosts_source: &Hosts,
    context: Arc<Context>,
) -> Result<Vec<String>, kube::Error> {
    let nodes_api: kube::Api<Node> = kube::Api::all(context.client.clone());
    let nodes = nodes_api.list(&ListParams::default()).await?;

    let hosts: Vec<String> = match hosts_source {
        Hosts::FromStaticList { from_list } => from_list.to_owned(),
        Hosts::FromClusterNodes { from_nodes } => nodes
            .items
            .iter()
            .filter(|node| node_matches(node, from_nodes))
            .map(|node| node.name().unwrap_or_default().into())
            .collect(),
    };

    Ok(hosts)
}

async fn setup_initial_status(
    object: &PlaybookPlan,
    api: &kube::Api<PlaybookPlan>,
) -> Result<(), kube::Error> {
    let mut patch_object = api.get_status(&object.name().unwrap()).await?;

    let initial_status = PlaybookPlanStatus {
        phase: Some(Phase::Waiting),
        ..Default::default()
    };

    patch_object.status = Some(initial_status);

    api.replace_status(
        object.metadata.name.as_ref().unwrap(),
        &PostParams::default(),
        serde_json::to_vec(&patch_object).unwrap(),
    )
    .await?;

    Ok(())
}
