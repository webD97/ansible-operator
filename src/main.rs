use futures_util::StreamExt as _;
use kube::CustomResourceExt as _;
use kube::config::KubeConfigOptions;
use tracing::{debug, warn};
use tracing_subscriber::util::SubscriberInitExt as _;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::{fmt, layer::SubscriberExt as _};

use crate::resources::v1beta1;

mod ansible;
mod controllers;
mod nodeselector;
mod resources;
mod utils;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.contains(&"--crd".into()) {
        let crd = v1beta1::PlaybookPlan::crd();
        println!("{}", serde_yaml::to_string(&crd).unwrap());
        std::process::exit(0);
    }

    setup_tracing();

    let kubeconfig = kube::config::Config::from_kubeconfig(&KubeConfigOptions::default())
        .await
        .unwrap();

    let kubernetes_client = kube::client::Client::try_from(kubeconfig).unwrap();
    let playbookplan_controller = controllers::playbookplan::new(kubernetes_client);

    playbookplan_controller
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
