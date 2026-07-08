use std::sync::Arc;

use futures_util::StreamExt as _;
use k8s_openapi::api::core::v1::Secret;
use kube::CustomResourceExt as _;
use kube::api::{Api, PostParams};
use kube::config::KubeConfigOptions;
use tokio::join;
use tracing::{debug, warn};
use tracing_subscriber::util::SubscriberInitExt as _;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::{fmt, layer::SubscriberExt as _};

use v1beta1::ca::CertificateAuthority;

mod utils;
mod v1beta1;

/// Name of the Secret (in the operator's own namespace) holding the operator's self-managed SSH
/// CA private key. v1 scope: generated once if missing, no auto-rotation.
const CA_SECRET_NAME: &str = "ansible-operator-ca";
const CA_SECRET_KEY: &str = "ca_key";

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

    let ca = Arc::new(
        ensure_ca(&client, &operator_namespace)
            .await
            .expect("failed to bootstrap the operator's SSH certificate authority"),
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

/// Loads the operator's CA from its Secret in `operator_namespace` if one already exists,
/// otherwise generates a brand-new CA and persists it there for next time. v1 scope: no
/// auto-rotation — this is a one-time bootstrap, not something re-run per reconcile.
async fn ensure_ca(
    client: &kube::Client,
    operator_namespace: &str,
) -> Result<CertificateAuthority, Box<dyn std::error::Error>> {
    let secrets_api: Api<Secret> = Api::namespaced(client.clone(), operator_namespace);

    if let Some(secret) = secrets_api.get_opt(CA_SECRET_NAME).await? {
        let key_bytes = secret
            .data
            .as_ref()
            .and_then(|d| d.get(CA_SECRET_KEY))
            .ok_or("CA secret exists but is missing its key data")?;
        let pem = String::from_utf8(key_bytes.0.clone())?;
        return Ok(CertificateAuthority::from_private_key_openssh(&pem)?);
    }

    let ca = CertificateAuthority::generate()?;
    let mut string_data = std::collections::BTreeMap::new();
    string_data.insert(CA_SECRET_KEY.to_string(), ca.private_key_openssh()?);

    let secret = Secret {
        metadata: kube::api::ObjectMeta {
            name: Some(CA_SECRET_NAME.to_string()),
            ..Default::default()
        },
        string_data: Some(string_data),
        ..Default::default()
    };

    secrets_api.create(&PostParams::default(), &secret).await?;

    Ok(ca)
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
