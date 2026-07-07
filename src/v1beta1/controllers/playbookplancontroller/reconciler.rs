use chrono::Utc;
use futures_util::{Stream, StreamExt as _};
use k8s_openapi::api::{
    batch::v1::Job,
    coordination::v1::Lease,
    core::v1::{Pod, Secret},
};
use kube::{
    Api,
    api::{ListParams, LogParams, Patch, PatchParams, PostParams},
    runtime::{
        Controller,
        controller::Action,
        reflector::{ObjectRef, store::Writer},
        watcher,
    },
};
use std::{collections::BTreeMap, sync::Arc};
use tracing::{debug, error, info, warn};

use crate::v1beta1::{
    AnsibleInventory, ClusterInventory, ExecutionMode, Phase, PlaybookPlanStatus,
    ResolvedInventoryGroup, StaticInventory, Toleration, ansible, flatten_hosts,
    playbookplancontroller::{
        execution_evaluator::{ExecutionHash, find_all_hosts},
        locking, managed_ssh,
        triggers::{Timing, evaluate_schedule, forecast_next_run},
        workspace::{self, render_secret},
    },
};
use crate::{
    utils::create_or_update,
    v1beta1::{
        self, PlaybookPlan,
        ca::CertificateAuthority,
        controllers::reconcile_error::ReconcileError,
        playbookplancontroller::{
            callback_output, execution_evaluator::{self, find_outdated_hosts}, job_builder, mappers,
            status,
        },
    },
};

struct ReconciliationContext {
    client: kube::Client,
    /// Namespace the operator itself runs in — where per-run Leases, managed-ssh proxy pods,
    /// and the operator's own CA Secret live (never the PlaybookPlan's namespace). Read from
    /// `POD_NAMESPACE` at operator startup (see `main.rs`).
    operator_namespace: String,
    /// The operator's self-managed SSH certificate authority — generated once at startup if
    /// missing, no auto-rotation in v1.
    ca: Arc<CertificateAuthority>,
}

pub fn new(
    client: kube::Client,
    operator_namespace: String,
    ca: Arc<CertificateAuthority>,
) -> impl Stream<
    Item = Result<
        (ObjectRef<v1beta1::PlaybookPlan>, Action),
        kube::runtime::controller::Error<ReconcileError, kube::runtime::watcher::Error>,
    >,
> {
    let context = Arc::new(ReconciliationContext {
        client: client.clone(),
        operator_namespace,
        ca,
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

/// Reconciles one PlaybookPlan. Level-triggered/idempotent "ensure" style — every step re-derives
/// what's needed from observed cluster state and short-circuits with a short `Action::requeue`
/// rather than a persisted "current step" state machine. Pipeline (each step re-run every tick):
///   0. resolve inventory, 1. compute outdated hosts/evaluate schedule, 2. ensure locks held,
///   3. ensure managed-ssh proxy infra ready, 4. ensure workspace secret reflects this run,
///   5. ensure the one Job exists, 6. ensure Job finished then parse+record results,
///   7. ensure cleanup (locks released, proxy infra torn down).
async fn reconcile(
    object: Arc<v1beta1::PlaybookPlan>,
    context: Arc<ReconciliationContext>,
) -> Result<Action, ReconcileError> {
    if object.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }

    let (namespace, name, generation) = extract_resource_info(&object)?;

    let api = Api::<v1beta1::PlaybookPlan>::namespaced(context.client.clone(), namespace);
    let secrets_api = Api::<Secret>::namespaced(context.client.clone(), namespace);
    let jobs_api = Api::<Job>::namespaced(context.client.clone(), namespace);
    let leases_api = Api::<Lease>::namespaced(context.client.clone(), &context.operator_namespace);

    let mut requeue_after = std::time::Duration::from_secs(3600);
    let mut resource_status = object.status.clone().unwrap_or_default();

    // Step 0: resolve inventory (kept separate per-resource, not flattened — connection
    // mechanism is implicit by which resource produced a group).
    let target_groups = resolve_inventory(&context, &object).await?;
    resource_status.eligible_hosts = flatten_hosts(&target_groups);

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
        // A new spec version starts retry counting over from scratch.
        resource_status.retry_count = 0;
    }

    // Step 1: compute outdated hosts / evaluate schedule — unchanged from before.
    let tz = object.timezone().unwrap();
    let now = || Utc::now().with_timezone(&tz);
    let time_window = chrono::Duration::seconds(15);
    let timing = evaluate_schedule(object.spec.schedule.as_deref(), now(), time_window);
    let outdated_hosts = find_outdated_hosts(&resource_status, &execution_hash)?;
    let all_hosts = find_all_hosts(&resource_status);

    let hosts_to_trigger = match object.spec.mode {
        ExecutionMode::OneShot => outdated_hosts.clone(),
        ExecutionMode::Recurring => all_hosts.clone(),
    };

    let holder_identity = format!("{namespace}/{name}/{execution_hash}");

    if !outdated_hosts.is_empty() && resource_status.phase != Phase::Applying {
        match timing {
            Timing::Delayed(until) => {
                requeue_after = (until - now()).to_std().unwrap();
                resource_status.phase = Phase::Scheduled;
                resource_status.next_run = Some(until.fixed_offset());
            }
            Timing::Now(_start) => {
                let run_groups = filter_groups_to_hosts(&target_groups, &hosts_to_trigger);

                // Step 2: ensure locks held — all-or-nothing across every host this run
                // targets; renewed every tick for as long as the run is in progress.
                let blocked =
                    locking::ensure_locks(&leases_api, &hosts_to_trigger, &holder_identity).await?;

                if !blocked.is_empty() {
                    debug!("Waiting on per-host locks for: {blocked:?}");
                    requeue_after = std::time::Duration::from_secs(15);
                } else {
                    let (managed_ssh_hosts, tolerations) =
                        managed_ssh_hosts_and_tolerations(&run_groups);

                    // Step 3: ensure managed-ssh proxy infra ready (only relevant if this run
                    // touches any ClusterInventory-sourced hosts).
                    let proxy_readiness = managed_ssh::ensure_proxy_infra(
                        &context.client,
                        &context.operator_namespace,
                        namespace,
                        &execution_hash,
                        &managed_ssh_hosts,
                        tolerations.as_deref(),
                        &context.ca,
                    )
                    .await?;

                    match proxy_readiness {
                        managed_ssh::ProxyReadiness::Pending => {
                            debug!("Waiting for managed-ssh proxy pods to become Ready");
                            requeue_after = std::time::Duration::from_secs(5);
                        }
                        managed_ssh::ProxyReadiness::AllReady(proxy_infos) => {
                            let managed_ssh_hosts_map: BTreeMap<String, ansible::ManagedSshHostInfo> =
                                proxy_infos
                                    .into_iter()
                                    .map(|p| {
                                        (
                                            p.host,
                                            ansible::ManagedSshHostInfo {
                                                pod_ip: p.pod_ip,
                                                port: p.port,
                                            },
                                        )
                                    })
                                    .collect();

                            // Step 4: ensure workspace secret reflects this run. Proxy pod IPs
                            // are fresh every run even with an unchanged spec, so rendering is
                            // also triggered on "a run is starting now", not generation alone.
                            if workspace::is_missing(&secrets_api, name).await?
                                || workspace::is_outdated(&object, true)
                            {
                                debug!("Rendering playbook to secret");
                                upsert_workspace_secret(
                                    &secrets_api,
                                    name,
                                    render_secret(&object, &run_groups, &managed_ssh_hosts_map)?,
                                )
                                .await?;
                                resource_status.last_rendered_generation = Some(generation);
                            }

                            // Step 5: ensure the one Job exists.
                            spawn_ansible_job(
                                &jobs_api,
                                execution_hash,
                                &run_groups,
                                &object,
                                &mut resource_status,
                            )
                            .await?;
                        }
                    }
                }
            }
        };
    }

    // Step 6: ensure the run's Job (if any) is finished, then parse + record results.
    if resource_status.phase == Phase::Applying {
        // Looked up by the exact recorded name, not the PLAYBOOKPLAN_HASH label — that label is
        // stable across every retry of an unchanged spec, so a label-only `list()` could return
        // an older, already-finished retry's Job instead of the one this run just created.
        let job = match &resource_status.current_job_name {
            Some(job_name) => jobs_api.get_opt(job_name).await?,
            None => None,
        };

        if let Some(job) = &job {
            if status::job_finished(job) {
                let pods_api: Api<Pod> = Api::namespaced(context.client.clone(), namespace);
                let job_name = job.metadata.name.as_ref().unwrap();

                let pod_name = pods_api
                    .list(&ListParams {
                        label_selector: Some(format!("job-name={job_name}")),
                        ..Default::default()
                    })
                    .await?
                    .items
                    .into_iter()
                    .next()
                    .and_then(|pod| pod.metadata.name);

                let logs = match &pod_name {
                    Some(pod_name) => pods_api.logs(pod_name, &LogParams::default()).await.ok(),
                    None => None,
                };

                let parsed = logs.as_deref().and_then(callback_output::parse_callback_output);

                status::evaluate_host_outcomes(
                    &hosts_to_trigger,
                    parsed.as_ref(),
                    &execution_hash,
                    &mut resource_status,
                );
                status::evaluate_playbookplan_conditions(
                    &hosts_to_trigger,
                    true,
                    parsed.as_ref(),
                    &mut resource_status,
                );

                // Step 7: ensure cleanup, regardless of success/failure.
                let (managed_ssh_hosts, _) = managed_ssh_hosts_and_tolerations(
                    &filter_groups_to_hosts(&target_groups, &hosts_to_trigger),
                );
                managed_ssh::cleanup_proxy_infra(
                    &context.client,
                    &context.operator_namespace,
                    &execution_hash,
                    &managed_ssh_hosts,
                )
                .await?;
                locking::release_locks(&leases_api, &hosts_to_trigger, &holder_identity).await?;

                let total_count: usize = resource_status
                    .eligible_hosts
                    .iter()
                    .map(|g| g.hosts.len())
                    .sum();
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
                            let next = forecast_next_run(
                                schedule,
                                now(),
                                Some(chrono::Duration::seconds(-5)),
                            );

                            requeue_after = (next - now()).to_std().unwrap();
                            resource_status.next_run = Some(next.fixed_offset());
                        } else {
                            warn!("Mode is Recurring but schedule is not set!");
                        }
                    }
                }
            } else {
                status::evaluate_playbookplan_conditions(
                    &hosts_to_trigger,
                    false,
                    None,
                    &mut resource_status,
                );
                requeue_after = std::time::Duration::from_secs(15);
            }
        }
    }

    patch_status(&api, &object, resource_status).await?;

    Ok(Action::requeue(requeue_after))
}

/// Filters a run's resolved groups down to only the hosts actually targeted this run
/// (`hosts_to_trigger`), preserving group membership so `serial:`/native grouping in the user's
/// playbook still means something — a single run's Job/inventory only ever targets this subset,
/// not the plan's full `eligible_hosts`.
fn filter_groups_to_hosts(
    groups: &[ResolvedInventoryGroup],
    hosts_to_trigger: &[String],
) -> Vec<ResolvedInventoryGroup> {
    let allowed: std::collections::HashSet<&str> =
        hosts_to_trigger.iter().map(String::as_str).collect();

    groups
        .iter()
        .filter_map(|group| {
            let hosts = group.hosts();
            let filtered_hostnames: Vec<String> = hosts
                .hosts
                .iter()
                .filter(|h| allowed.contains(h.as_str()))
                .cloned()
                .collect();

            if filtered_hostnames.is_empty() {
                return None;
            }

            let mut filtered_hosts = hosts.clone();
            filtered_hosts.hosts = filtered_hostnames;

            Some(match group {
                ResolvedInventoryGroup::ManagedSsh { tolerations, .. } => {
                    ResolvedInventoryGroup::ManagedSsh {
                        hosts: filtered_hosts,
                        tolerations: tolerations.clone(),
                    }
                }
                ResolvedInventoryGroup::Ssh {
                    static_inventory_name,
                    config,
                    ..
                } => ResolvedInventoryGroup::Ssh {
                    hosts: filtered_hosts,
                    static_inventory_name: static_inventory_name.clone(),
                    config: config.clone(),
                },
            })
        })
        .collect()
}

/// Flat list of managed-ssh-sourced hostnames in these groups, plus the tolerations to use for
/// their proxy pods. If a run spans multiple ClusterInventory resources with different
/// tolerations, only the first non-`None` one found is used for all of them.
fn managed_ssh_hosts_and_tolerations(
    groups: &[ResolvedInventoryGroup],
) -> (Vec<String>, Option<Vec<Toleration>>) {
    let mut hosts = Vec::new();
    let mut tolerations = None;

    for group in groups {
        if let ResolvedInventoryGroup::ManagedSsh {
            hosts: h,
            tolerations: t,
        } = group
        {
            hosts.extend(h.hosts.clone());
            if tolerations.is_none() {
                tolerations = t.clone();
            }
        }
    }

    (hosts, tolerations)
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

/// Returns a list of all secret names that the given PlaybookPlan references (e.g. secrets used
/// as Ansible variables).
///
/// Deliberately excludes the workspace secret itself — its content legitimately differs on every
/// run even with an unchanged spec (managed-ssh proxy pod IPs are baked into inventory.yml), so
/// including it here would make `execution_hash` unstable across otherwise-identical runs and
/// break naming consistency for proxy infra/Job labels/lock identity mid-run. Workspace-secret
/// staleness is handled independently via `workspace::is_outdated`/`is_missing`.
fn get_related_secrets(playbookplan: &PlaybookPlan) -> Vec<&String> {
    job_builder::extract_secret_names_for_variables(playbookplan)
        .chain(job_builder::extract_secret_names_for_files(playbookplan))
        .collect()
}

/// Persists `status` via a JSON merge patch, not `Api::replace_status` (a PUT requiring
/// `resourceVersion` to exactly match the server's current one). This reconcile function spans
/// many async steps between reading `target` and this final write, long enough that a concurrent
/// write to the same object routinely lands first and would reject a version-checked PUT with a
/// 409. A merge patch carries no such precondition.
async fn patch_status(
    api: &Api<PlaybookPlan>,
    target: &PlaybookPlan,
    status: PlaybookPlanStatus,
) -> Result<(), ReconcileError> {
    use kube::runtime::reflector::Lookup as _;

    let name = target
        .name()
        .ok_or(ReconcileError::PreconditionFailed("name not set"))?;

    api.patch_status(
        &name,
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({ "status": status })),
    )
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

/// Resolves every inventory this PlaybookPlan references into `ResolvedInventoryGroup`s,
/// preserving which resource (and therefore which connection mechanism + config) each group of
/// hosts came from — `ClusterInventory` always implies managed-ssh, `StaticInventory` always
/// implies its own embedded SSH config. Not flattened into a single list, since downstream steps
/// (locking, proxy pods, inventory rendering, job building) need to know which mechanism applies
/// to which group.
async fn resolve_inventory(
    context: &ReconciliationContext,
    object: &PlaybookPlan,
) -> Result<Vec<ResolvedInventoryGroup>, ReconcileError> {
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

    let mut all_errors = cluster_inventory_errors
        .into_iter()
        .chain(static_inventory_errors);

    if let Some(first) = all_errors.next() {
        return Err(ReconcileError::KubeError(first));
    }

    let mut groups = Vec::new();

    for ci in cluster_inventories.into_iter().map(Result::unwrap) {
        let tolerations = ci.spec.tolerations.clone();
        for hosts in ci.get_hosts() {
            groups.push(ResolvedInventoryGroup::ManagedSsh {
                hosts,
                tolerations: tolerations.clone(),
            });
        }
    }

    for si in static_inventories.into_iter().map(Result::unwrap) {
        let static_inventory_name = si.name_any();
        let config = si.spec.ssh.clone();
        for hosts in si.get_hosts() {
            groups.push(ResolvedInventoryGroup::Ssh {
                hosts,
                static_inventory_name: static_inventory_name.clone(),
                config: config.clone(),
            });
        }
    }

    Ok(groups)
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

/// Ensures this run's single Job exists (idempotency: skip creation if a Job with this run's
/// deterministic name already exists). There's exactly one Job per run now, not one per host.
async fn spawn_ansible_job(
    api: &Api<Job>,
    hash: ExecutionHash,
    run_groups: &[ResolvedInventoryGroup],
    playbookplan: &PlaybookPlan,
    resource_status: &mut PlaybookPlanStatus,
) -> Result<(), ReconcileError> {
    use kube::runtime::reflector::Lookup as _;

    // Safe to bump unconditionally: this only runs on the tick that transitions `phase` into
    // `Applying` for a genuinely new attempt, so it can't double-increment. Reset to 0 elsewhere
    // whenever `current_hash` changes.
    resource_status.retry_count += 1;

    let job = job_builder::create_job_for_run(
        &hash,
        resource_status.retry_count,
        run_groups,
        playbookplan,
    )?;
    let job_name = job
        .name()
        .expect(".metadata.name must be set at this point");

    // Recorded regardless of which branch below runs — step 6 looks the Job up by this exact
    // name, since the PLAYBOOKPLAN_HASH label alone could match an older, already-finished
    // retry's Job instead of this run's.
    resource_status.current_job_name = Some(job_name.to_string());

    // Job already exists, skip creating another one
    // TODO: Check for jobs with another hash and decide if we need to replace them
    if api.get_opt(&job_name).await?.is_some() {
        info!("Job for this run already exists");
        return Ok(());
    }

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

    Ok(())
}
