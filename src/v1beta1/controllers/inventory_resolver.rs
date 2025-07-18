use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Node;
use kube::{Api, api::ListParams};

use crate::v1beta1::{self, Inventory, controllers::nodeselector};

pub async fn resolve(
    nodes_api: &Api<Node>,
    inventories_spec: &[Inventory],
) -> Result<BTreeMap<String, Vec<String>>, kube::Error> {
    let mut resolved: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for inventory in inventories_spec {
        let resolved_hosts = resolve_hosts(nodes_api, &inventory.hosts).await?;
        resolved.insert(inventory.name.clone(), resolved_hosts);
    }

    Ok(resolved)
}

async fn resolve_hosts(
    nodes_api: &Api<Node>,
    hosts_source: &v1beta1::Hosts,
) -> Result<Vec<String>, kube::Error> {
    use kube::runtime::reflector::Lookup as _;

    let nodes = nodes_api.list(&ListParams::default()).await?;
    let hosts: Vec<String> = match hosts_source {
        v1beta1::Hosts::FromStaticList { from_list } => from_list.to_owned(),
        v1beta1::Hosts::FromClusterNodes { from_nodes } => nodes
            .items
            .iter()
            .filter(|node| nodeselector::node_matches(node, from_nodes))
            .map(|node| node.name().unwrap_or_default().into())
            .collect(),
    };

    Ok(hosts)
}
