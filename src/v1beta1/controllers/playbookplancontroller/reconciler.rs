use crate::v1beta1::{
    AnsibleInventory, ClusterInventory, ExecutionMode, Phase, PlaybookPlanSpec, PlaybookPlanStatus,
    ResolvedHosts, StaticInventory, labels,
    playbookplancontroller::{
        execution_evaluator::{ExecutionHash, find_all_hosts},
        status::all_jobs_finished,
        triggers::{Timing, evaluate_schedule, forecast_next_run},
        workspace::{self, render_secret},
    },
};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use futures_util::{Stream, StreamExt as _};
use k8s_openapi::api::{batch::v1::Job, core::v1::Secret};
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
use tracing::{debug, error, info, warn};

use crate::{
    utils::create_or_update,
    v1beta1::{
        self, PlaybookPlan,
        controllers::reconcile_error::ReconcileError,
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
                        Err(e) => error!("Reflector error: {e:?}"),
                    }
                })
                .await;
        });

        playbookplan_reflector_reader
    };

    Controller::new(playbookplans_api, watcher::Config::default())
        .owns(jobs_api, watcher::Config::default())
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
    // If object is being deleted, stop reconciliation
    if object.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }

    let (namespace, name, generation) = extract_resource_info(&object)?;

    let api = Api::<v1beta1::PlaybookPlan>::namespaced(context.client.clone(), namespace);
    let secrets_api = Api::<Secret>::namespaced(context.client.clone(), namespace);
    let jobs_api = Api::<Job>::namespaced(context.client.clone(), namespace);

    // These may be updated as needed during reconciliation
    let mut requeue_after = std::time::Duration::from_secs(3600);
    let mut resource_status = object.status.clone().unwrap_or_default();

    let target_hosts = resolve_inventory(&context, &object).await?;

    resource_status.eligible_hosts = target_hosts.clone();

    if workspace::is_missing(&secrets_api, name).await? || workspace::is_outdated(&object) {
        debug!("Rendering playbook to secret");
        upsert_workspace_secret(&secrets_api, name, render_secret(&object, &target_hosts)?).await?;
        resource_status.last_rendered_generation = Some(generation);
    }

    let related_secrets = get_related_secrets(&object);
    let execution_hash = hash_playbook_inputs(
        &object.spec.template.playbook,
        &related_secrets,
        &secrets_api,
    )
    .await;

    if resource_status.current_hash != execution_hash.to_string() {
        resource_status.phase = Phase::Pending;
        resource_status.current_hash = execution_hash.to_string();
    }

    let tz = object.timezone().unwrap();
    let now = || Utc::now().with_timezone(&tz);
    let time_window = chrono::Duration::seconds(15);
    let timing = evaluate_schedule(object.spec.schedule.as_deref(), now(), time_window);
    let outdated_hosts = find_outdated_hosts(&resource_status, &execution_hash)?;

    if !outdated_hosts.is_empty() && resource_status.phase != Phase::Applying {
        match timing {
            Timing::Delayed(until) => {
                requeue_after = (until - now()).to_std().unwrap();
                resource_status.phase = Phase::Scheduled;
                resource_status.next_run = Some(until.fixed_offset());
            }
            Timing::Now(start) => {
                let all_hosts = find_all_hosts(&resource_status);
                spawn_ansible_jobs(
                    &jobs_api,
                    start,
                    execution_hash,
                    outdated_hosts,
                    all_hosts,
                    &object,
                    &mut resource_status,
                )
                .await?;
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

    if all_jobs_finished(&jobs) && !jobs.items.is_empty() {
        let total_count: usize = target_hosts.iter().map(|g| g.hosts.len()).sum();
        let outdated_count = find_outdated_hosts(&resource_status, &execution_hash)?.len();

        resource_status.summary = match outdated_count {
            0 => Some(format!("{total_count}/{total_count} up-to-date")),
            n => Some(format!("{n}/{total_count} outdated")),
        };

        match &object.spec.mode {
            ExecutionMode::OneShot => {
                resource_status.next_run = None;
                resource_status.phase = match outdated_count {
                    0 => Phase::Succeeded,
                    _ => Phase::Failed,
                };
            }
            ExecutionMode::Recurring => {
                if let Some(schedule) = &object.spec.schedule {
                    resource_status.phase = Phase::Scheduled;
                    let next =
                        forecast_next_run(schedule, now(), Some(chrono::Duration::seconds(-5)));

                    requeue_after = (next - now()).to_std().unwrap();
                    resource_status.next_run = Some(next.fixed_offset());
                } else {
                    warn!("Mode is Recurring but schedule is not set!");
                }
            }
        }
    }

    replace_status(&api, &object, resource_status).await?;

    Ok(Action::requeue(requeue_after))
}

async fn upsert_workspace_secret(
    api: &Api<Secret>,
    secret_name: &str,
    secret: Secret,
) -> Result<(), ReconcileError> {
    Ok(create_or_update(
        api,
        "ansible-operator",
        secret_name,
        secret,
        |existing, desired_state| {
            desired_state.metadata.managed_fields = None;

            // `string_data` contains our new or updated keys. If they exist in `data`, remove them from there so that `string_data` can take precedence.
            desired_state.data = {
                const EMPTY: &BTreeMap<String, String> = &BTreeMap::new();
                let desired_data = desired_state.string_data.as_ref().unwrap_or(EMPTY);

                existing.data.map(|d| {
                    BTreeMap::from_iter(
                        d.into_iter()
                            .filter(|(key, _)| !desired_data.contains_key(key)),
                    )
                })
            };
        },
    )
    .await?)
}

/// Returns a list of all secret names that the given PlaybookPlan references. This includes for
/// example secrets used as Ansible variables.
fn get_related_secrets(playbookplan: &PlaybookPlan) -> Vec<&String> {
    job_builder::extract_secret_names_for_variables(playbookplan)
        .chain(job_builder::extract_secret_names_for_files(playbookplan))
        .chain(std::iter::once(
            playbookplan
                .metadata
                .name
                .as_ref()
                .expect(".metadata.name must be set here"),
        ))
        .collect()
}

async fn replace_status(
    api: &Api<PlaybookPlan>,
    target: &PlaybookPlan,
    status: PlaybookPlanStatus,
) -> Result<(), ReconcileError> {
    use kube::runtime::reflector::Lookup as _;

    let name = target
        .name()
        .ok_or(ReconcileError::PreconditionFailed("name not set"))?;

    let patch_object = PlaybookPlan {
        metadata: target.metadata.clone(),
        spec: PlaybookPlanSpec::default(),
        status: Some(status),
    };

    api.replace_status(&name, &PostParams::default(), &patch_object)
        .await?;

    Ok(())
}

async fn hash_playbook_inputs(
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

async fn resolve_inventory(
    context: &ReconciliationContext,
    object: &PlaybookPlan,
) -> Result<Vec<ResolvedHosts>, ReconcileError> {
    use kube::ResourceExt;

    let namespace = object
        .namespace()
        .ok_or(ReconcileError::PreconditionFailed("namespace not set"))?;

    let cluster_inventory_api: Api<ClusterInventory> =
        Api::namespaced(context.client.clone(), &namespace);
    let static_inventory_api: Api<StaticInventory> =
        Api::namespaced(context.client.clone(), &namespace);

    let inventory_refs = &object.spec.inventory_refs;

    let cluster_inventories = inventory_refs
        .iter()
        .filter_map(|inventory_ref| inventory_ref.cluster_inventory.as_ref())
        .map(|name| cluster_inventory_api.get(name));

    let (cluster_inventories, errors): (Vec<_>, Vec<_>) =
        futures::future::join_all(cluster_inventories)
            .await
            .into_iter()
            .partition(Result::is_ok);

    let cluster_inventory_errors: Vec<_> = errors.into_iter().map(Result::unwrap_err).collect();

    let static_inventories = inventory_refs
        .iter()
        .filter_map(|inventory_ref| inventory_ref.static_inventory.as_ref())
        .map(|name| static_inventory_api.get(name));

    let (static_inventories, errors): (Vec<_>, Vec<_>) =
        futures::future::join_all(static_inventories)
            .await
            .into_iter()
            .partition(Result::is_ok);

    let static_inventory_errors: Vec<_> = errors.into_iter().map(Result::unwrap_err).collect();

    let resolved_hosts: Vec<_> = cluster_inventories
        .into_iter()
        .map(|r| Box::new(r.unwrap()) as Box<dyn AnsibleInventory>)
        .chain(
            static_inventories
                .into_iter()
                .map(|r| Box::new(r.unwrap()) as Box<dyn AnsibleInventory>),
        )
        .flat_map(|i| i.get_hosts())
        .collect();

    let mut all_errors = cluster_inventory_errors
        .into_iter()
        .chain(static_inventory_errors);

    if let Some(first) = all_errors.next() {
        return Err(ReconcileError::KubeError(first));
    }

    Ok(resolved_hosts)
}

fn extract_resource_info(object: &PlaybookPlan) -> Result<(&str, &str, i64), ReconcileError> {
    let namespace = object
        .metadata
        .namespace
        .as_deref()
        .ok_or(ReconcileError::PreconditionFailed("namespace not set"))?;

    let name = object
        .metadata
        .name
        .as_deref()
        .ok_or(ReconcileError::PreconditionFailed("name not set"))?;

    let generation = object
        .metadata
        .generation
        .ok_or(ReconcileError::PreconditionFailed("generation not set"))?;

    Ok((namespace, name, generation))
}

async fn spawn_ansible_jobs(
    api: &Api<Job>,
    start: Option<DateTime<Tz>>,
    hash: ExecutionHash,
    outdated_hosts: Vec<String>,
    all_hosts: Vec<String>,
    playbookplan: &PlaybookPlan,
    resource_status: &mut PlaybookPlanStatus,
) -> Result<(), ReconcileError> {
    use kube::runtime::reflector::Lookup as _;

    let hosts_to_trigger = match &playbookplan.spec.mode {
        ExecutionMode::OneShot => outdated_hosts,
        ExecutionMode::Recurring => all_hosts,
    };

    if hosts_to_trigger.is_empty() {
        resource_status.next_run = None;
    }

    for host in hosts_to_trigger {
        let job = job_builder::create_job_for_host(
            &host,
            &hash,
            start.map(|t| t.to_utc()).as_ref(),
            playbookplan,
        )?;
        let job_name = job
            .name()
            .expect(".metadata.name must be set at this point");

        // Job already exists, skip creating another one
        // TODO: Check for jobs with another hash and decide if we need to replace them
        if api.get_opt(&job_name).await?.is_some() {
            info!("Job for {host} already exists");
            continue;
        }

        // Now that we finally know that there are hosts where we need to apply something,
        // set the status accordingly.
        resource_status.phase = Phase::Applying;
        resource_status.next_run = None;

        info!("Creating job {job_name}");
        api.create(
            &PostParams {
                field_manager: Some("ansible-operator".into()),
                ..Default::default()
            },
            &job,
        )
        .await?;
    }

    Ok(())
}
