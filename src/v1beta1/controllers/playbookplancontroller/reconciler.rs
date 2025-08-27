use crate::v1beta1::{
    ExecutionMode, Phase, labels,
    playbookplancontroller::{
        execution_evaluator::{ExecutionHash, find_all_hosts},
        status::all_jobs_finished,
        triggers::{Timing, evaluate_schedule, forecast_next_run},
        workspace::{self, render_secret},
    },
};
use chrono::Utc;
use chrono_tz::Tz;
use futures_util::{Stream, StreamExt as _};
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{Node, Secret},
};
use kube::{
    Api,
    api::{ListParams, PostParams},
    runtime::{
        Controller,
        controller::Action,
        reflector::{ObjectRef, store::Writer},
        watcher,
    },
};
use std::{collections::BTreeMap, sync::Arc};
use tracing::{debug, info, warn};

use crate::{
    utils::create_or_update,
    v1beta1::{
        self, PlaybookPlan,
        controllers::{inventory_resolver, reconcile_error::ReconcileError},
        playbookplancontroller::{
            execution_evaluator::{self, find_outdated_hosts},
            job_builder, mappers,
            status::{evaluate_per_host_status, evaluate_playbookplan_conditions},
        },
    },
};

struct ReconciliationContext {
    client: kube::Client,
}

pub fn new(
    client: kube::Client,
) -> impl Stream<
    Item = Result<
        (ObjectRef<v1beta1::PlaybookPlan>, Action),
        kube::runtime::controller::Error<ReconcileError, kube::runtime::watcher::Error>,
    >,
> {
    let context = Arc::new(ReconciliationContext {
        client: client.clone(),
    });

    let playbookplans_api: Api<v1beta1::PlaybookPlan> = Api::all(client.clone());
    let nodes_api: Api<Node> = Api::all(client.clone());
    let jobs_api: Api<Job> = Api::all(client.clone());
    let secrets_api: Api<Secret> = Api::all(client);

    let playbookplan_reflector_reader = {
        let playbookplan_reflector_writer = Writer::<v1beta1::PlaybookPlan>::default();
        let playbookplan_reflector_reader = Arc::new(playbookplan_reflector_writer.as_reader());

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

        playbookplan_reflector_reader
    };

    Controller::new(playbookplans_api, watcher::Config::default())
        .owns(jobs_api, watcher::Config::default())
        .watches(
            nodes_api,
            watcher::Config::default(),
            mappers::node_to_playbookplans(Arc::clone(&playbookplan_reflector_reader)),
        )
        .watches(
            secrets_api,
            watcher::Config::default(),
            mappers::secret_to_playbookplans(Arc::clone(&playbookplan_reflector_reader)),
        )
        .run(
            reconcile,
            |_, _, _| Action::requeue(std::time::Duration::from_secs(15)),
            Arc::clone(&context),
        )
}

async fn reconcile(
    object: Arc<v1beta1::PlaybookPlan>,
    context: Arc<ReconciliationContext>,
) -> Result<Action, ReconcileError> {
    use kube::runtime::reflector::Lookup as _;

    // If object is being deleted, stop reonciliation
    if object.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }

    let mut requeue_after = std::time::Duration::from_secs(3600);

    let namespace = object
        .namespace()
        .ok_or(ReconcileError::PreconditionFailed("namespace not set"))?;
    let name = object
        .name()
        .ok_or(ReconcileError::PreconditionFailed("name not set"))?;
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
    debug!("Resolving groups");
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

    // Render playbook if necessary
    if workspace::is_missing(&secrets_api, &name).await? || workspace::is_outdated(&object) {
        info!("Rendering playbook to secret");
        let secret = render_secret(&object, &resolved_inventories)?;

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

    let related_secrets = get_related_secrets(&object);
    let execution_hash = hash_playbook_and_secrets(
        &object.spec.template.playbook,
        &related_secrets,
        &secrets_api,
    )
    .await;

    resource_status.current_hash = Some(execution_hash.to_string());

    let tz = object
        .spec
        .time_zone
        .as_ref()
        .map(|tz| tz.parse::<Tz>().unwrap())
        .unwrap_or(Tz::UTC);

    let now = || Utc::now().with_timezone(&tz);
    let time_window = chrono::Duration::seconds(15);
    let timing = evaluate_schedule(object.spec.schedule.as_deref(), now(), time_window);
    let mode = &object.spec.mode;
    let outdated_hosts = find_outdated_hosts(&resource_status, &execution_hash)?;

    if !outdated_hosts.is_empty() && !matches!(resource_status.phase, Some(Phase::Finished)) {
        match timing {
            Timing::Delayed(until) => {
                requeue_after = (until - now()).to_std().unwrap();
                resource_status.phase = Some(Phase::Scheduled);
                resource_status.next_run = Some(until.fixed_offset());
            }
            Timing::Now(start) => {
                let hosts_to_trigger = match mode {
                    ExecutionMode::OneShot => outdated_hosts,
                    ExecutionMode::Recurring => find_all_hosts(&resource_status),
                };

                if hosts_to_trigger.is_empty() {
                    resource_status.phase = Some(Phase::Finished);
                    resource_status.next_run = None;
                }

                for host in hosts_to_trigger {
                    let job = job_builder::create_job_for_host(
                        &host,
                        &execution_hash,
                        start.map(|t| t.to_utc()).as_ref(),
                        &object,
                    )?;
                    let job_name = job
                        .name()
                        .expect(".metadata.name must be set at this point");

                    // Job already exists, skip creating another one
                    // TODO: Check for jobs with another hash and decide if we need to replace them
                    if jobs_api.get_opt(&job_name).await?.is_some() {
                        info!("Job for {host} already exists");
                        continue;
                    }

                    // Now that we finally know that there are hosts where we need to apply something,
                    // set the status accordingly.
                    resource_status.phase = Some(Phase::Applying);
                    resource_status.next_run = None;

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
        };
    }

    // Read managed jobs and populate status
    let jobs = jobs_api
        .list(
            &ListParams::default().labels(
                format!(
                    "{}={name},{}={execution_hash}",
                    labels::PLAYBOOKPLAN_NAME,
                    labels::PLAYBOOKPLAN_HASH
                )
                .as_str(),
            ),
        )
        .await?;

    evaluate_playbookplan_conditions(&jobs, &mut resource_status);
    evaluate_per_host_status(&jobs, &execution_hash, &mut resource_status);

    // For recurring playbooks, update .status.nextRun and ensure requeue
    if matches!(mode, ExecutionMode::Recurring) && all_jobs_finished(&jobs) {
        if let Some(schedule) = &object.spec.schedule {
            resource_status.phase = Some(Phase::Scheduled);
            let next = forecast_next_run(schedule, now(), Some(chrono::Duration::seconds(-5)));

            requeue_after = (next - now()).to_std().unwrap();
            resource_status.next_run = Some(next.fixed_offset());
        } else {
            warn!("Mode is Recurring but schedule is not set!");
        }
    }

    // For oneshot playbooks, check if finished
    if matches!(mode, ExecutionMode::OneShot)
        && find_outdated_hosts(&resource_status, &execution_hash)?.is_empty()
    {
        resource_status.next_run = None;
        resource_status.phase = Some(Phase::Finished);
    }

    persist_status(&playbookplan_api, &object, resource_status).await?;

    Ok(Action::requeue(requeue_after))
}

/// Returns a list of all secret names that the given PlaybookPlan references. This includes for
/// example secrets used as Ansible variables.
fn get_related_secrets(playbookplan: &PlaybookPlan) -> Vec<&String> {
    job_builder::extract_secret_names_for_variables(playbookplan)
        .chain(job_builder::extract_secret_names_for_files(playbookplan))
        .collect()
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

async fn hash_playbook_and_secrets(
    playbook: &str,
    secret_names: &[&String],
    secrets_api: &Api<Secret>,
) -> ExecutionHash {
    let secrets = futures::future::join_all(
        secret_names
            .iter()
            .map(|secret_name| secrets_api.get(secret_name)),
    )
    .await;

    let variables_secrets: Vec<BTreeMap<_, _>> = secrets
        .iter()
        .filter_map(|result| result.as_ref().ok())
        .filter_map(|secret| secret.data.clone())
        .collect();

    execution_evaluator::calculate_execution_hash(playbook, variables_secrets.iter())
}
