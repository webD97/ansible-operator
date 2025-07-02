use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures_util::Stream;
use k8s_openapi::{
    api::core::v1::{Node, Secret},
    apimachinery::pkg::apis::meta::v1::OwnerReference,
};
use kube::{
    api::{ListParams, PostParams},
    runtime::{Controller, controller::Action, reflector::Lookup as _},
};
use tracing::{info, warn};

use crate::{
    ansible,
    nodeselector::node_matches,
    resources::playbookplan::{Hosts, PlaybookPlan, PlaybookPlanStatus},
    utils::create_or_update,
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

    let namespace = object.namespace().expect("expected a namespace");
    let name = object.name().expect("expected a name");
    let uid = object.uid().expect("expected a uid");
    let generation = object.metadata.generation.expect("expected generation");

    let mut resource_status = object.status.clone().unwrap_or_default();

    // Resolve groups
    info!("Resolving groups");
    let resolved_inventories = resolve_inventories(&context, &object).await?;

    resource_status.eligible_hosts_count = Some(
        resolved_inventories
            .values()
            .flatten()
            .cloned()
            .collect::<std::collections::HashSet<String>>()
            .len(),
    );
    resource_status.eligible_hosts = Some(resolved_inventories);

    // Render playbook if necessary
    if let Some(status) = &object.status
        && status.last_rendered_generation.unwrap_or_default() < generation
    {
        info!("Rendering playbook to secret");
        match ansible::render_playbook(&object.spec) {
            Ok(rendered_playbook) => {
                let secret = create_secret_for_playbook(&namespace, &name, &uid, rendered_playbook);

                let secrets_api =
                    kube::Api::<Secret>::namespaced(context.client.clone(), &namespace);

                create_or_update(
                    secrets_api,
                    "ansible-operator",
                    &name,
                    secret,
                    |desired, actual| {
                        actual.data = desired.data;
                    },
                )
                .await?;

                resource_status.last_rendered_generation = Some(generation);
            }
            Err(e) => warn!("Failed to render playbook: {e}"),
        }
    }

    persist_status(&playbookplan_api, &object, resource_status).await?;

    Ok(Action::requeue(Duration::from_secs(3600)))
}

async fn resolve_inventories(
    context: &Context,
    object: &PlaybookPlan,
) -> Result<BTreeMap<String, Vec<String>>, kube::Error> {
    let inventories_spec = &object.spec.inventory;

    let mut resolved: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for inventory in inventories_spec {
        let resolved_hosts = resolve_hosts(&inventory.hosts, context).await?;
        resolved.insert(inventory.name.clone(), resolved_hosts);
    }

    Ok(resolved)
}

async fn resolve_hosts(
    hosts_source: &Hosts,
    context: &Context,
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

async fn persist_status(
    api: &kube::Api<PlaybookPlan>,
    object: &PlaybookPlan,
    status: PlaybookPlanStatus,
) -> Result<(), kube::Error> {
    let mut patch_object = object.clone();
    patch_object.status = Some(status);

    api.replace_status(
        &object.name().expect("expected a name"),
        &PostParams::default(),
        serde_json::to_vec(&patch_object).unwrap(),
    )
    .await?;

    Ok(())
}

fn create_secret_for_playbook(
    pb_namespace: &str,
    pb_name: &str,
    pb_uid: &str,
    playbook: String,
) -> Secret {
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

    let mut string_data = BTreeMap::new();
    string_data.insert("playbook.yml".into(), playbook);

    secret.string_data = Some(string_data);

    secret
}
