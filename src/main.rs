use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::future::try_join_all;
use k8s_openapi::api::core::v1::Node;
use kube::CustomResourceExt as _;
use kube::api::{ListParams, PostParams};
use kube::runtime::controller::Action;
use kube::runtime::reflector::Lookup;
use kube::{config::KubeConfigOptions, runtime::Controller};
use tracing::{debug, info, warn};
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::types::{
    Group, Hosts, Inventory, NodeSelectorOperator, NodeSelectorRequirement, ResolvedGroup,
};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{fmt, layer::SubscriberExt as _};

mod playbookplan;
mod types;

struct Context {
    nodes: kube::Api<Node>,
    inventories: kube::Api<Inventory>,
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.contains(&"--crd".into()) {
        let crd = Inventory::crd();
        println!("{}", serde_yaml::to_string(&crd).unwrap());
        std::process::exit(0);
    }

    setup_tracing();

    let kubeconfig = kube::config::Config::from_kubeconfig(&KubeConfigOptions::default())
        .await
        .unwrap();

    let client = kube::client::Client::try_from(kubeconfig).unwrap();

    let context = Arc::new(Context {
        nodes: kube::Api::all(client.clone()),
        inventories: kube::Api::all(client.clone()),
    });

    let root_api: kube::Api<Inventory> = kube::Api::all(client);

    Controller::new(root_api, kube::runtime::watcher::Config::default())
        .run(reconcile, error_policy, Arc::clone(&context))
        .for_each(|res| async move {
            match res {
                Ok(o) => debug!("reconciled {:?}", o),
                Err(e) => warn!("reconcile failed: {:?}", e),
            }
        })
        .await;
}

fn setup_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .try_init()
        .expect("tracing-subscriber setup failed");
}

fn error_policy(_object: Arc<Inventory>, _error: &kube::Error, _context: Arc<Context>) -> Action {
    // TODO: logic here
    Action::requeue(Duration::from_secs(15))
}

async fn reconcile(object: Arc<Inventory>, context: Arc<Context>) -> Result<Action, kube::Error> {
    // Check for deletion
    if object.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }

    // Resolve groups
    let client = &context.inventories.clone().into_client();
    let inventories_api: kube::Api<Inventory> =
        kube::Api::namespaced(client.clone(), &object.metadata.namespace.clone().unwrap());
    let groups_spec = &object.spec.groups;

    let resolved = try_join_all(
        groups_spec
            .iter()
            .map(|group| resolve_group(group, Arc::clone(&context))),
    )
    .await?;

    let mut new_inventory = inventories_api
        .get_status(&object.name().unwrap())
        .await
        .unwrap();

    for group in &resolved {
        info!(
            "Resolved group {} to {} host(s): {}",
            group.name,
            group.hosts.len(),
            group.hosts.join(", ")
        )
    }

    new_inventory.status = Some({
        let mut status = object.status.clone().unwrap_or_default();
        status.resolved_groups = resolved;
        status
    });

    inventories_api
        .replace_status(
            object.metadata.name.as_ref().unwrap(),
            &PostParams::default(),
            serde_json::to_vec(&new_inventory).unwrap(),
        )
        .await?;

    Ok(Action::requeue(Duration::from_secs(3600)))
}

async fn resolve_group(group: &Group, context: Arc<Context>) -> Result<ResolvedGroup, kube::Error> {
    let nodes_api = &context.nodes;
    let nodes = nodes_api.list(&ListParams::default()).await?;

    let matching_nodes = nodes
        .items
        .iter()
        .filter(|node| {
            // todo: check all hosts
            let e = group.hosts.first().unwrap();
            match e {
                Hosts::FromNodeSelector { from_nodes } => {
                    from_nodes.match_expressions.iter().any(|expr| {
                        matches_expression(&node.metadata.labels.clone().unwrap_or_default(), expr)
                    })
                }
            }
        })
        .map(|node| node.name().unwrap_or_default().into())
        .collect();

    Ok(ResolvedGroup {
        name: group.name.clone(),
        hosts: matching_nodes,
    })
}

fn matches_expression(labels: &BTreeMap<String, String>, expr: &NodeSelectorRequirement) -> bool {
    match expr.operator {
        NodeSelectorOperator::In => {
            if let Some(val) = labels.get(&expr.key) {
                expr.values.as_ref().is_some_and(|vals| vals.contains(val))
            } else {
                false
            }
        }
        _ => false,
    }
}
