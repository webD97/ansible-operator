use std::sync::Arc;

use futures_util::StreamExt as _;
use kube::CustomResourceExt as _;
use kube::config::KubeConfigOptions;
use tokio::join;
use tracing::{debug, warn};
use tracing_subscriber::util::SubscriberInitExt as _;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::{fmt, layer::SubscriberExt as _};

use v1beta1::ca::CertificateAuthority;

mod utils;
mod v1beta1;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.contains(&"--crd".into()) {
        let playbookplan = v1beta1::PlaybookPlan::crd();
        let cluster_inventory = v1beta1::ClusterInventory::crd();
        let static_inventory = v1beta1::StaticInventory::crd();
        let node_access_policy = v1beta1::NodeAccessPolicy::crd();
        println!(
            "{}",
            [
                serde_yaml::to_string(&playbookplan).unwrap(),
                serde_yaml::to_string(&cluster_inventory).unwrap(),
                serde_yaml::to_string(&static_inventory).unwrap(),
                serde_yaml::to_string(&node_access_policy).unwrap()
            ]
            .join("---\n")
        );
        std::process::exit(0);
    }

    setup_tracing();

    let client = kube::client::Client::try_from(discover_kubernetes_config().await).unwrap();

    let operator_namespace = std::env::var("POD_NAMESPACE").expect("POD_NAMESPACE must be set");

    // Ephemeral, in-memory CA: a fresh keypair per operator process, never persisted to the
    // cluster. Restarting the operator rotates the CA and invalidates all outstanding certs.
    let ca = Arc::new(
        CertificateAuthority::generate()
            .expect("failed to generate the operator's ephemeral SSH certificate authority"),
    );

    let playbookplan_controller =
        v1beta1::playbookplancontroller::reconciler::new(client.clone(), operator_namespace, ca)
            .for_each(|res| async move {
                match res {
                    Ok(o) => debug!("reconciled {:?}", o),
                    Err(e) => warn!("reconcile failed: {:?}", e),
                }
            });

    let inventory_controller =
        v1beta1::clusterinventorycontroller::new(client.clone()).for_each(|res| async move {
            match res {
                Ok(o) => debug!("reconciled {:?}", o),
                Err(e) => warn!("reconcile failed: {:?}", e),
            }
        });

    let node_access_policy_controller =
        v1beta1::nodeaccesspolicycontroller::new(client).for_each(|res| async move {
            match res {
                Ok(o) => debug!("reconciled {:?}", o),
                Err(e) => warn!("reconcile failed: {:?}", e),
            }
        });

    join!(
        playbookplan_controller,
        inventory_controller,
        node_access_policy_controller
    );
}

fn setup_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .try_init()
        .expect("tracing-subscriber setup failed");
}

async fn discover_kubernetes_config() -> kube::Config {
    let from_default_kubeconfig =
        kube::Config::from_kubeconfig(&KubeConfigOptions::default()).await;

    if let Ok(config) = from_default_kubeconfig {
        return config;
    }

    let from_incluster_env = kube::Config::incluster_env();

    if let Ok(config) = from_incluster_env {
        return config;
    }

    panic!("Failed to find a suitable Kubernetes client config.");
}
