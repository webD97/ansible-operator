use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures_util::{Stream, StreamExt as _};
use k8s_openapi::{
    api::{
        batch::v1::Job,
        core::v1::{Node, Secret},
    },
    apimachinery::pkg::apis::meta::v1::OwnerReference,
    chrono::Utc,
};
use kube::{
    Api, Resource,
    api::{ListParams, ObjectList, PostParams},
    runtime::{
        Controller,
        controller::Action,
        reflector::{ObjectRef, store::Writer},
        watcher,
    },
};
use tracing::info;

use crate::{
    utils::{create_or_update, upsert_condition},
    v1beta1::{
        self, ansible,
        controllers::{inventory_resolver, reconcile_error::ReconcileError},
    },
};

struct Context {
    client: kube::Client,
}

pub fn new(
    client: kube::Client,
) -> impl Stream<
    Item = Result<
        (
            kube::runtime::reflector::ObjectRef<v1beta1::PlaybookPlan>,
            Action,
        ),
        kube::runtime::controller::Error<ReconcileError, kube::runtime::watcher::Error>,
    >,
> {
    let context = Arc::new(Context {
        client: client.clone(),
    });

    let playbookplans_api: Api<v1beta1::PlaybookPlan> = Api::all(client.clone());
    let nodes_api: Api<Node> = Api::all(client.clone());
    let jobs_api: Api<Job> = Api::all(client);

    let playbookplan_reflector_writer = Writer::<v1beta1::PlaybookPlan>::default();
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
                            v1beta1::Hosts::FromClusterNodes { .. } => true,
                            v1beta1::Hosts::FromStaticList { .. } => false,
                        })
                })
                .map(|resource| ObjectRef::from(&**resource))
                .collect::<Vec<_>>()
        })
        .run(
            reconcile,
            |_, _, _| Action::requeue(Duration::from_secs(15)),
            Arc::clone(&context),
        )
}

async fn reconcile(
    object: Arc<v1beta1::PlaybookPlan>,
    context: Arc<Context>,
) -> Result<Action, ReconcileError> {
    use kube::runtime::reflector::Lookup as _;

    // Check for deletion
    if object.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }

    let namespace = object
        .namespace()
        .ok_or(ReconcileError::PreconditionFailed("namespace not set"))?;
    let name = object
        .name()
        .ok_or(ReconcileError::PreconditionFailed("name not set"))?;
    let uid = object
        .uid()
        .ok_or(ReconcileError::PreconditionFailed("uid not set"))?;
    let generation = object
        .metadata
        .generation
        .ok_or(ReconcileError::PreconditionFailed("generation not set"))?;

    let playbookplan_api =
        Api::<v1beta1::PlaybookPlan>::namespaced(context.client.clone(), &namespace);
    let secrets_api = Api::<Secret>::namespaced(context.client.clone(), &namespace);
    let jobs_api = Api::<Job>::namespaced(context.client.clone(), &namespace);
    let nodes_api = Api::<Node>::all(context.client.clone());

    let mut resource_status = object.status.clone().unwrap_or_default();

    // Resolve groups
    info!("Resolving groups");
    let resolved_inventories =
        inventory_resolver::resolve(&nodes_api, &object.spec.inventory).await?;

    resource_status.eligible_hosts_count = Some(
        resolved_inventories
            .values()
            .flatten()
            .cloned()
            .collect::<std::collections::HashSet<String>>()
            .len(),
    );
    resource_status.eligible_hosts = Some(resolved_inventories.clone());

    let rendered_playbook_outdated = object
        .status
        .as_ref()
        .and_then(|s| s.last_rendered_generation)
        .map(|g| g < generation)
        .unwrap_or(true);

    // Render playbook if necessary
    if secrets_api.get_opt(&name).await?.is_none() || rendered_playbook_outdated {
        info!("Rendering playbook to secret");
        let rendered_playbook = ansible::render_playbook(&object.spec)?;
        let rendered_inventory = ansible::render_inventory(&resolved_inventories)?;

        let inlined_variables = match &object.spec.template.variables {
            Some(variable_sources) => variable_sources
                .iter()
                .filter_map(|source| match source {
                    v1beta1::PlaybookVariableSource::SecretRef { secret_ref: _ } => None,
                    v1beta1::PlaybookVariableSource::Inline { inline } => Some(inline),
                })
                .map(serde_yaml::to_string)
                .collect(),
            None => Vec::new(),
        };

        let mut secret = create_secret_for_playbook(&namespace, &name, &uid);

        let mut string_data = BTreeMap::new();
        string_data.insert("playbook.yml".into(), rendered_playbook);
        string_data.insert("inventory.yml".into(), rendered_inventory);

        for (index, variable_set) in inlined_variables.into_iter().enumerate() {
            string_data.insert(format!("static-variables-{index}.yml"), variable_set?);
        }

        secret.string_data = Some(string_data);

        create_or_update(
            &secrets_api,
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

    let triggers = object.spec.execution_triggers.as_ref();

    // Create jobs
    if triggers.is_none() || triggers.is_some_and(|triggers| triggers.delayed_until.is_none()) {
        for (_, hosts) in resolved_inventories.iter() {
            for host in hosts {
                let job = match &object.spec.connection_strategy {
                    v1beta1::ConnectionStrategy::Ssh { ssh } => {
                        super::job_builder::create_job_for_ssh_playbook(
                            host,
                            &object,
                            ssh,
                            &format_job_prefix(&name, &generation.to_string()),
                        )
                    }
                    v1beta1::ConnectionStrategy::Chroot {} => todo!(),
                }?;

                let job_name = job.name().ok_or(ReconcileError::PreconditionFailed(
                    "name not set in rendered job",
                ))?;

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
    let jobs = get_jobs_for_playbookplan(&jobs_api, &name, &generation.to_string()).await?;
    let num_total = jobs.iter().count();
    let num_successful = count_successful(&jobs);
    let num_failed = count_failed(&jobs);
    let num_finished = num_failed + num_successful;
    let num_running = num_total - num_finished;

    // Handle "Running" condition
    if num_finished < num_total {
        upsert_condition(
            &mut resource_status.conditions,
            v1beta1::PlaybookPlanCondition {
                type_: "Running".into(),
                status: "True".into(),
                reason: Some("JobsRunning".into()),
                message: Some(format!("{num_running} jobs are currently running")),
                last_transition_time: Some(Utc::now().to_rfc3339()),
            },
        );
    } else {
        upsert_condition(
            &mut resource_status.conditions,
            v1beta1::PlaybookPlanCondition {
                type_: "Running".into(),
                status: "False".into(),
                reason: None,
                message: None,
                last_transition_time: Some(Utc::now().to_rfc3339()),
            },
        );
    }

    // Handle "Ready" condition
    if num_successful == num_total {
        upsert_condition(
            &mut resource_status.conditions,
            v1beta1::PlaybookPlanCondition {
                type_: "Ready".into(),
                status: "True".into(),
                reason: Some("AllJobsSucceeded".into()),
                message: Some(format!(
                    "{num_successful}/{num_total} jobs completed successfully"
                )),
                last_transition_time: Some(Utc::now().to_rfc3339()),
            },
        );
    } else if num_failed > 0 {
        upsert_condition(
            &mut resource_status.conditions,
            v1beta1::PlaybookPlanCondition {
                type_: "Ready".into(),
                status: "False".into(),
                reason: Some("SomeOrAllJobsFailed".into()),
                message: Some(format!("{num_failed}/{num_total} jobs have failed")),
                last_transition_time: Some(Utc::now().to_rfc3339()),
            },
        );
    } else {
        upsert_condition(
            &mut resource_status.conditions,
            v1beta1::PlaybookPlanCondition {
                type_: "Ready".into(),
                status: "False".into(),
                reason: Some("AwaitingJobResults".into()),
                message: Some(format!("{num_running} jobs are running")),
                last_transition_time: Some(Utc::now().to_rfc3339()),
            },
        );
    }

    persist_status(&playbookplan_api, &object, resource_status).await?;

    Ok(Action::requeue(Duration::from_secs(3600)))
}

async fn get_jobs_for_playbookplan(
    jobs_api: &Api<Job>,
    playbookplan_name: &str,
    generation: &str,
) -> Result<ObjectList<Job>, kube::Error> {
    jobs_api
        .list(
            &ListParams::default().labels(
                format!(
                    "ansible.cloudbending.dev/playbookplan={}",
                    format_job_prefix(playbookplan_name, generation)
                )
                .as_str(),
            ),
        )
        .await
}

fn count_successful(jobs: &ObjectList<Job>) -> usize {
    jobs.iter()
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
        .count()
}

fn count_failed(jobs: &ObjectList<Job>) -> usize {
    jobs.iter()
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
        .count()
}

async fn persist_status(
    api: &Api<v1beta1::PlaybookPlan>,
    object: &v1beta1::PlaybookPlan,
    status: v1beta1::PlaybookPlanStatus,
) -> Result<(), ReconcileError> {
    use kube::runtime::reflector::Lookup as _;

    let mut patch_object = object.clone();
    patch_object.status = Some(status);

    let name = &object
        .name()
        .ok_or(ReconcileError::PreconditionFailed("expected a name"))?;

    let data = serde_json::to_vec(&patch_object)?;

    api.replace_status(name, &PostParams::default(), data)
        .await?;

    Ok(())
}

fn create_secret_for_playbook(pb_namespace: &str, pb_name: &str, pb_uid: &str) -> Secret {
    let mut secret = Secret::default();

    secret.metadata.namespace = Some(pb_namespace.into());
    secret.metadata.name = Some(pb_name.into());

    secret.metadata.owner_references = Some(vec![OwnerReference {
        api_version: v1beta1::PlaybookPlan::api_version(&()).into(),
        kind: v1beta1::PlaybookPlan::kind(&()).into(),
        name: pb_name.into(),
        uid: pb_uid.into(),
        ..Default::default()
    }]);

    secret
}

fn format_job_prefix(playbookplan_name: &str, generation: &str) -> String {
    format!("apply-{playbookplan_name}-{generation}")
}
