use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures_util::{Stream, StreamExt as _};
use k8s_openapi::{
    api::{
        batch::v1::{Job, JobSpec},
        core::v1::{
            Container, Node, PodSpec, PodTemplateSpec, Secret, SecretVolumeSource, Volume,
            VolumeMount,
        },
    },
    apimachinery::pkg::apis::meta::v1::OwnerReference,
};
use kube::{
    Resource,
    api::{ListParams, PostParams},
    runtime::{
        Controller,
        controller::Action,
        reflector::{ObjectRef, store::Writer},
        watcher,
    },
};
use tracing::{info, warn};

use crate::{
    ansible,
    nodeselector::node_matches,
    resources::playbookplan::{ExecutionStrategy, Hosts, Phase, PlaybookPlan, PlaybookPlanStatus},
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

    let playbookplans_api: kube::Api<PlaybookPlan> = kube::Api::all(client.clone());
    let nodes_api: kube::Api<Node> = kube::Api::all(client.clone());
    let jobs_api: kube::Api<Job> = kube::Api::all(client);

    let playbookplan_reflector_writer = Writer::<PlaybookPlan>::default();
    let playbookplan_reflector_reader = playbookplan_reflector_writer.as_reader();

    let playbookplan_reflector = kube::runtime::reflector(
        playbookplan_reflector_writer,
        watcher(playbookplans_api.clone(), watcher::Config::default()),
    );

    tokio::spawn(async move {
        playbookplan_reflector
            .for_each(|event| async {
                match event {
                    Ok(_) => {}
                    Err(e) => eprintln!("Reflector error: {e:?}"),
                }
            })
            .await;
    });

    Controller::new(playbookplans_api, kube::runtime::watcher::Config::default())
        // If a managed job updates, trigger PlaybookPlan reconciliation
        .owns(jobs_api, watcher::Config::default())
        // If a node updates, trigger reconciliation of all PlaybookPlans with a `fromNodes` inventory
        .watches(nodes_api, watcher::Config::default(), move |_| {
            playbookplan_reflector_reader
                .state()
                .iter()
                .filter(|resource| {
                    resource
                        .spec
                        .inventory
                        .iter()
                        .any(|inventory| match &inventory.hosts {
                            Hosts::FromClusterNodes { .. } => true,
                            Hosts::FromStaticList { .. } => false,
                        })
                })
                .map(|resource| ObjectRef::from(&**resource))
                .collect::<Vec<_>>()
        })
        .run(reconcile, error_policy, Arc::clone(&context))
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
    use kube::runtime::reflector::Lookup as _;

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
    resource_status.eligible_hosts = Some(resolved_inventories.clone());

    let secrets_api = kube::Api::<Secret>::namespaced(context.client.clone(), &namespace);

    let rendered_playbook_outdated = if let Some(status) = &object.status {
        status.last_rendered_generation.unwrap_or_default() < generation
    } else {
        true
    };

    // Render playbook if necessary
    if secrets_api.get_opt(&name).await?.is_none() || rendered_playbook_outdated {
        info!("Rendering playbook to secret");
        let rendered_playbook = match ansible::render_playbook(&object.spec) {
            Ok(rendered_playbook) => rendered_playbook,
            Err(e) => {
                warn!("Failed to render playbook: {e}");
                "".into()
            }
        };

        let rendered_inventory = match ansible::render_inventory(&resolved_inventories) {
            Ok(rendered_inventory) => rendered_inventory,
            Err(e) => {
                warn!("Failed to render inventory: {e}");
                "".into()
            }
        };

        let rendered_variables = match &object.spec.variables {
            Some(variables) => serde_yaml::to_string(&variables.inline).unwrap(),
            None => "".into(),
        };

        let secret = create_secret_for_playbook(
            &namespace,
            &name,
            &uid,
            rendered_playbook,
            rendered_inventory,
            rendered_variables,
        );

        create_or_update(
            secrets_api,
            "ansible-operator",
            &name,
            secret,
            |existing, desired_state| {
                desired_state.metadata.managed_fields = None;

                // `string_data` contains our new or updated keys. If they exist in `data`, remove them from there so that `string_data` can take precedence.
                desired_state.data = {
                    let desired_data = desired_state.string_data.clone().unwrap_or_default();

                    existing.data.map(|d| {
                        BTreeMap::from_iter(
                            d.iter()
                                .filter(|(key, _)| !desired_data.contains_key(*key))
                                .map(|(key, value)| (key.clone(), value.clone())),
                        )
                    })
                };
            },
        )
        .await?;

        resource_status.last_rendered_generation = Some(generation);
    }

    let jobs_api = kube::Api::<Job>::namespaced(context.client.clone(), &namespace);

    // Create jobs
    if let Some(immediate) = object.spec.triggers.immediate
        && immediate
    {
        for (_, hosts) in resolved_inventories.iter() {
            for host in hosts {
                let job = create_job_for_ssh_playbook(&namespace, &name, host, &uid, &object);
                let job_name = job.name().expect("expected rendered job to contain a name");

                if jobs_api.get_opt(&job_name).await?.is_some() {
                    continue;
                }

                info!("Creating job {job_name}");
                jobs_api
                    .create(
                        &PostParams {
                            field_manager: Some("ansible-operator".into()),
                            ..Default::default()
                        },
                        &job,
                    )
                    .await?;
            }
        }
    }

    // Read managed jobs and populate status
    let jobs = jobs_api
        .list(
            &ListParams::default().labels(
                format!(
                    "ansible.cloudbending.dev/playbookplan={}",
                    format_job_prefix(&name, generation)
                )
                .as_str(),
            ),
        )
        .await?;

    let num_successful = jobs
        .iter()
        .filter(|job| {
            job.status
                .as_ref()
                .and_then(|status| status.conditions.as_ref())
                .map(|conditions| {
                    conditions.iter().any(|condition| {
                        condition.type_ == "SuccessCriteriaMet" && condition.status == "True"
                    })
                })
                .unwrap_or(false)
        })
        .count();

    let num_failed = jobs
        .iter()
        .filter(|job| {
            job.status
                .as_ref()
                .and_then(|status| status.conditions.as_ref())
                .map(|conditions| {
                    conditions
                        .iter()
                        .any(|condition| condition.type_ == "Failed" && condition.status == "True")
                })
                .unwrap_or(false)
        })
        .count();

    if num_successful == jobs.iter().count() {
        resource_status.phase = Phase::Succeeded;
    } else if num_failed > 0 {
        resource_status.phase = Phase::Failed;
    } else {
        resource_status.phase = Phase::Running;
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
    use kube::runtime::reflector::Lookup as _;

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
    use kube::runtime::reflector::Lookup as _;

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
    inventory: String,
    variables: String,
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
    string_data.insert("inventory.yml".into(), inventory);
    string_data.insert("variables.yml".into(), variables);

    secret.string_data = Some(string_data);

    secret
}

fn format_job_prefix(playbookplan_name: &str, generation: i64) -> String {
    format!("apply-{playbookplan_name}-{generation}")
}

fn create_job_for_ssh_playbook(
    pb_namespace: &str,
    pb_name: &str,
    host: &str,
    pb_uid: &str,
    plan: &PlaybookPlan,
) -> Job {
    let generation = plan
        .metadata
        .generation
        .expect("expected PlaybookPlan to have a generation");

    let job_prefix = format_job_prefix(pb_name, generation);
    let mut job = Job::default();
    job.metadata.namespace = Some(pb_namespace.into());
    job.metadata.name = Some(format!("{job_prefix}-on-{host}"));

    job.metadata.owner_references = Some(vec![OwnerReference {
        api_version: PlaybookPlan::api_version(&()).into(),
        kind: PlaybookPlan::kind(&()).into(),
        name: pb_name.into(),
        uid: pb_uid.into(),
        ..Default::default()
    }]);

    job.metadata.labels = Some(BTreeMap::from([(
        "ansible.cloudbending.dev/playbookplan".into(),
        job_prefix,
    )]));

    let pod_template = PodTemplateSpec {
        metadata: None,
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            volumes: Some(vec![
                Volume {
                    name: "playbook".into(),
                    secret: Some(SecretVolumeSource {
                        secret_name: Some(pb_name.into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Volume {
                    name: "ssh".into(),
                    secret: Some(SecretVolumeSource {
                        secret_name: Some("ssh".into()),
                        default_mode: Some(0o0400),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ]),
            containers: vec![Container {
                name: "ansible-playbook".into(),
                image: Some(plan.spec.image.clone()),
                working_dir: Some("/run/ansible-operator".into()),
                volume_mounts: Some(vec![
                    VolumeMount {
                        name: "playbook".into(),
                        mount_path: "/run/ansible-operator".into(),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: "ssh".into(),
                        mount_path: "/ssh".into(),
                        ..Default::default()
                    },
                ]),
                command: Some(render_ansible_command(plan, host)),
                ..Default::default()
            }],
            ..Default::default()
        }),
    };

    let job_spec = JobSpec {
        backoff_limit: Some(0),
        template: pod_template,
        ..Default::default()
    };

    job.spec = Some(job_spec);

    job
}

fn render_ansible_command(plan: &PlaybookPlan, hostname: &str) -> Vec<String> {
    let mut ansible_command = vec![
        "ansible-playbook".into(),
        "--extra-vars".into(),
        "@/run/ansible-operator/variables.yml".into(),
    ];

    let connection_args = match &plan.spec.execution_strategy {
        ExecutionStrategy::Chroot {} => vec!["-i".into(), "/mnt/host,".into()],
        ExecutionStrategy::Ssh { ssh } => vec![
            "--ssh-common-args='-o UserKnownHostsFile=/ssh/known_hosts'".into(),
            "--private-key".into(),
            "/ssh/id_rsa".into(),
            "--user".into(),
            ssh.user.clone(),
            "-i".into(),
            "inventory.yml".into(),
            "-l".into(),
            format!("{hostname},"),
        ],
    };

    ansible_command.extend(connection_args);
    ansible_command.push("playbook.yml".into());

    ansible_command
}
