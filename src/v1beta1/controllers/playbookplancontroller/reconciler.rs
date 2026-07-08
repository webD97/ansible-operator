use chrono::{DateTime, FixedOffset, Utc};
use futures_util::{Stream, StreamExt as _};
use k8s_openapi::api::{
    batch::v1::Job,
    coordination::v1::Lease,
    core::v1::{Pod, Secret},
};
use kube::{
    Api,
    api::{ListParams, Patch, PatchParams, PostParams},
    runtime::{
        Controller,
        controller::Action,
        reflector::{ObjectRef, Store, store::Writer},
        watcher,
    },
};
use std::{collections::BTreeMap, sync::Arc};
use tracing::{debug, error, info, warn};

use crate::v1beta1::{
    AnsibleInventory, ClusterInventory, ExecutionMode, NodeAccessPolicy, Phase, PlaybookPlanStatus,
    ResolvedInventoryGroup, StaticInventory, Toleration, ansible, flatten_hosts, labels,
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
            callback_output,
            execution_evaluator::{self, find_outdated_hosts},
            job_builder, mappers, node_access, status,
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
    /// Reflector-backed cache of the admin-authored `NodeAccessPolicy` resources in the operator
    /// namespace, read by `node_access::enforce` to clamp managed-ssh nodes without a per-reconcile
    /// list. Populated + kept fresh by the reflector spawned in `new`; policy edits also re-trigger
    /// affected plans via `mappers::node_access_policy_to_playbookplans`.
    node_access_policies: Arc<Store<NodeAccessPolicy>>,
}

/// Per-tick identifiers shared by `try_start_run` and `advance_applying_run`: the resource's
/// namespace/name, which hosts this run targets, its execution hash, and the Lease holder identity
/// derived from them. Kube `Api<T>` handles are deliberately *not* here — those are plumbing built
/// on demand from `ReconciliationContext::client` plus `namespace`, not run identity.
struct RunContext<'a> {
    namespace: &'a str,
    name: &'a str,
    execution_hash: ExecutionHash,
    hosts_to_trigger: &'a [String],
    holder_identity: &'a str,
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
    let playbookplans_api: Api<v1beta1::PlaybookPlan> = Api::all(client.clone());
    let jobs_api: Api<Job> = Api::all(client.clone());
    let secrets_api: Api<Secret> = Api::all(client.clone());
    // Policies only ever govern managed-ssh in the operator namespace, so cache/watch just those.
    let node_access_policies_api: Api<NodeAccessPolicy> =
        Api::namespaced(client.clone(), &operator_namespace);

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

    let node_access_policy_reflector_reader = {
        let writer = Writer::<NodeAccessPolicy>::default();
        let reader = Arc::new(writer.as_reader());

        let reflector = kube::runtime::reflector(
            writer,
            watcher(node_access_policies_api.clone(), watcher::Config::default()),
        );

        tokio::spawn(async move {
            reflector
                .for_each(|event| async {
                    if let Err(e) = event {
                        error!("NodeAccessPolicy reflector error: {e:?}");
                    }
                })
                .await;
        });

        reader
    };

    let context = Arc::new(ReconciliationContext {
        client,
        operator_namespace,
        ca,
        node_access_policies: Arc::clone(&node_access_policy_reflector_reader),
    });

    Controller::new(playbookplans_api, watcher::Config::default())
        .owns(jobs_api, watcher::Config::default())
        .watches(
            secrets_api,
            watcher::Config::default(),
            mappers::secret_to_playbookplans(Arc::clone(&playbookplan_reflector_reader)),
        )
        .watches(
            node_access_policies_api,
            watcher::Config::default(),
            mappers::node_access_policy_to_playbookplans(Arc::clone(&playbookplan_reflector_reader)),
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
///   0. resolve inventory, 1. compute outdated hosts/evaluate schedule, 2-5. `try_start_run`
///   (locks, managed-ssh proxy infra, workspace secret, the one Job), 6-7. `advance_applying_run`
///   (once the Job is finished: parse+record results, cleanup). A single tick can walk through
///   both halves — e.g. Pending -> locks acquired -> proxy ready -> Job created -> immediately
///   checked for completion — since nothing here is gated on a persisted step, only on `Phase`.
async fn reconcile(
    object: Arc<v1beta1::PlaybookPlan>,
    context: Arc<ReconciliationContext>,
) -> Result<Action, ReconcileError> {
    if object.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }

    let (namespace, name, _) = extract_resource_info(&object)?;

    let api = Api::<v1beta1::PlaybookPlan>::namespaced(context.client.clone(), namespace);
    let secrets_api = Api::<Secret>::namespaced(context.client.clone(), namespace);

    let mut requeue_after = std::time::Duration::from_secs(3600);
    let mut resource_status = object.status.clone().unwrap_or_default();

    // Step 0: resolve inventory (kept separate per-resource, not flattened — connection
    // mechanism is implicit by which resource produced a group).
    let mut target_groups = resolve_inventory(&context, &object).await?;

    // Step 0b: NodeAccessPolicy enforcement — clamp managed-ssh (ClusterInventory) nodes to what
    // this namespace is permitted to target, before eligible_hosts and any proxy infra derive from
    // them. Fail-closed: an ungoverned namespace resolves to zero managed-ssh nodes.
    let excluded_nodes = node_access::enforce(
        &context.client,
        &context.node_access_policies,
        namespace,
        &mut target_groups,
    )
    .await?;
    if !excluded_nodes.is_empty() {
        warn!(
            "NodeAccessPolicy excluded nodes {excluded_nodes:?} from {namespace}/{name} \
             (not granted to this namespace)"
        );
    }

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
        // ...and may legitimately need to run in the same slot the old version already used, so
        // forget which slot was last triggered.
        resource_status.last_triggered_run = None;
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
    let run = RunContext {
        namespace,
        name,
        execution_hash,
        hosts_to_trigger: &hosts_to_trigger,
        holder_identity: &holder_identity,
    };

    // What makes a run eligible to *start* this tick differs by mode:
    //   - OneShot keeps applying until every host is on the current hash, then goes quiet — so it's
    //     gated on there being outdated hosts left (which is exactly `hosts_to_trigger`).
    //   - Recurring runs on every schedule tick regardless of host hashes (a successful run marks
    //     all hosts up-to-date, so an outdated-based gate would fire once and never again). It's
    //     gated only on having a schedule to tick on; slot dedup via `last_triggered_run` is what
    //     stops a single tick from starting more than one run, and without a schedule there'd be no
    //     slot to dedup against (it would busy-loop).
    let eligible_to_start = !hosts_to_trigger.is_empty()
        && match object.spec.mode {
            ExecutionMode::OneShot => true,
            ExecutionMode::Recurring => object.spec.schedule.is_some(),
        };

    if eligible_to_start && resource_status.phase != Phase::Applying {
        match timing {
            Timing::Delayed(until) => {
                requeue_after = (until - now()).to_std().unwrap();
                resource_status.phase = Phase::Scheduled;
                resource_status.next_run = Some(until.fixed_offset());
            }
            Timing::Now(start) => {
                let this_slot = start.map(|s| s.fixed_offset());

                if slot_already_triggered(this_slot, resource_status.last_triggered_run) {
                    // A run for this scheduled slot already started within its grace window;
                    // `evaluate_schedule` keeps returning `Now` for the rest of that window, so
                    // don't start another — sleep until the next slot instead. Without this a run
                    // that finishes inside its own grace window is immediately re-triggered.
                    if let Some(schedule) = object.spec.schedule.as_deref() {
                        let next =
                            forecast_next_run(schedule, now(), Some(chrono::Duration::seconds(-5)));
                        requeue_after = (next - now()).to_std().unwrap_or_default();
                        resource_status.next_run = Some(next.fixed_offset());
                    }
                } else if let Some(d) = try_start_run(
                    &context,
                    &run,
                    &target_groups,
                    &object,
                    &mut resource_status,
                )
                .await?
                {
                    requeue_after = d;
                } else {
                    // `try_start_run` ran to completion (the Job was created or an active one
                    // adopted, so `phase` is now `Applying`). Record this slot so it can't
                    // re-trigger inside its grace window. `None` for unscheduled plans, which have
                    // no slot and are never suppressed.
                    resource_status.last_triggered_run = this_slot;
                }
            }
        };
    }

    if resource_status.phase == Phase::Applying
        && let Some(d) = advance_applying_run(&context, &run, &object, &mut resource_status).await?
    {
        requeue_after = d;
    }

    patch_status(&api, &object, resource_status).await?;

    Ok(Action::requeue(requeue_after))
}

/// Whether the current schedule slot (`start`, the grace window's start) already had a run started
/// for it, per the persisted `last_triggered_run`. Unscheduled ticks carry no slot (`None`) and are
/// never suppressed — there is nothing to dedupe against. `DateTime` equality compares instants, so
/// the offset the two timestamps carry is irrelevant.
fn slot_already_triggered(
    start: Option<DateTime<FixedOffset>>,
    last_triggered_run: Option<DateTime<FixedOffset>>,
) -> bool {
    start.is_some() && start == last_triggered_run
}

/// Steps 2-5: acquire this run's per-host locks (all-or-nothing, renewed every tick for as long
/// as the run is in progress), ensure managed-ssh proxy infra is Ready, ensure the workspace
/// secret reflects this run, then ensure the one Job exists. Each guard clause returns early with
/// a short requeue the moment a precondition isn't met yet; `None` means it ran to completion
/// (the Job either already existed or was just created — see `spawn_ansible_job`).
async fn try_start_run(
    context: &ReconciliationContext,
    run: &RunContext<'_>,
    target_groups: &[ResolvedInventoryGroup],
    object: &PlaybookPlan,
    resource_status: &mut PlaybookPlanStatus,
) -> Result<Option<std::time::Duration>, ReconcileError> {
    let secrets_api = Api::<Secret>::namespaced(context.client.clone(), run.namespace);
    let jobs_api = Api::<Job>::namespaced(context.client.clone(), run.namespace);
    let leases_api = Api::<Lease>::namespaced(context.client.clone(), &context.operator_namespace);

    let run_groups = filter_groups_to_hosts(target_groups, run.hosts_to_trigger);

    let blocked =
        locking::ensure_locks(&leases_api, run.hosts_to_trigger, run.holder_identity).await?;
    if !blocked.is_empty() {
        debug!("Waiting on per-host locks for: {blocked:?}");
        return Ok(Some(std::time::Duration::from_secs(15)));
    }

    let (managed_ssh_hosts, tolerations) = managed_ssh_hosts_and_tolerations(&run_groups);

    let proxy_readiness = managed_ssh::ensure_proxy_infra(
        &context.client,
        &context.operator_namespace,
        run.namespace,
        &run.execution_hash,
        &managed_ssh_hosts,
        tolerations.as_deref(),
        &context.ca,
    )
    .await?;

    let proxy_infos = match proxy_readiness {
        managed_ssh::ProxyReadiness::Pending => {
            debug!("Waiting for managed-ssh proxy pods to become Ready");
            return Ok(Some(std::time::Duration::from_secs(5)));
        }
        managed_ssh::ProxyReadiness::AllReady(infos) => infos,
    };

    let managed_ssh_hosts_map: BTreeMap<String, ansible::ManagedSshHostInfo> = proxy_infos
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

    // Proxy pod IPs are fresh every run even with an unchanged spec, so rendering is also
    // triggered on "a run is starting now", not generation alone.
    if workspace::is_missing(&secrets_api, run.name).await? || workspace::is_outdated(object, true) {
        debug!("Rendering playbook to secret");
        upsert_workspace_secret(
            &secrets_api,
            run.name,
            render_secret(object, &run_groups, &managed_ssh_hosts_map)?,
        )
        .await?;
        resource_status.last_rendered_generation = object.metadata.generation;
    }

    spawn_ansible_job(
        &jobs_api,
        run.execution_hash,
        &run_groups,
        object,
        resource_status,
    )
    .await?;

    Ok(None)
}

/// Steps 6-7: once this run's Job (recorded as `current_job_name`) is `Complete`/`Failed`, parses
/// its logs for per-host outcomes, records them, tears down this run's locks/proxy infra, and
/// advances `phase` to whatever comes next for this `ExecutionMode`. Returns `None` if there's
/// nothing to do yet (no Job recorded, or it hasn't reached a terminal state) or if advancing
/// shouldn't change the requeue duration (e.g. a terminal `OneShot` outcome) — the caller only
/// overrides its requeue duration when this returns `Some`.
async fn advance_applying_run(
    context: &ReconciliationContext,
    run: &RunContext<'_>,
    object: &PlaybookPlan,
    resource_status: &mut PlaybookPlanStatus,
) -> Result<Option<std::time::Duration>, ReconcileError> {
    let jobs_api = Api::<Job>::namespaced(context.client.clone(), run.namespace);
    let leases_api = Api::<Lease>::namespaced(context.client.clone(), &context.operator_namespace);

    // Looked up by the exact recorded name, not the PLAYBOOKPLAN_HASH label — that label is
    // stable across every retry of an unchanged spec, so a label-only `list()` could return
    // an older, already-finished retry's Job instead of the one this run just created.
    let Some(job_name) = resource_status.current_job_name.clone() else {
        return Ok(None);
    };
    let job = jobs_api.get_opt(&job_name).await?;

    // Still running -> keep waiting.
    if let Some(job) = &job
        && !status::job_finished(job)
    {
        status::evaluate_playbookplan_conditions(
            run.hosts_to_trigger,
            false,
            None,
            resource_status,
        );
        return Ok(Some(std::time::Duration::from_secs(15)));
    }

    // The Job either finished, or is already gone — reaped by Kubernetes' TTL controller (its result
    // outlived a long operator outage) or deleted out from under us. Both mean the run is over: read
    // the recap from the pod's termination message if the Job is still there, otherwise the outcome
    // is lost and every host falls to `Unknown`. Not returning early on a missing Job is what keeps
    // a reaped run from wedging in `Applying` forever. The recap comes from the container's
    // termination message (what the callback wrote to /dev/termination-log), not logs — a dedicated
    // channel that isn't interleaved with playbook output and needs no `pods/log` access.
    let parsed = match &job {
        Some(_) => {
            let pods_api: Api<Pod> = Api::namespaced(context.client.clone(), run.namespace);
            pods_api
                .list(&ListParams {
                    label_selector: Some(format!("job-name={job_name}")),
                    ..Default::default()
                })
                .await?
                .items
                .iter()
                .find_map(termination_message)
                .as_deref()
                .and_then(callback_output::parse_callback_output)
        }
        None => None,
    };

    status::evaluate_host_outcomes(
        run.hosts_to_trigger,
        parsed.as_ref(),
        &run.execution_hash,
        resource_status,
    );
    status::evaluate_playbookplan_conditions(
        run.hosts_to_trigger,
        true,
        parsed.as_ref(),
        resource_status,
    );

    managed_ssh::cleanup_proxy_infra(
        &context.client,
        &context.operator_namespace,
        &run.execution_hash,
    )
    .await?;
    locking::release_locks(&leases_api, run.hosts_to_trigger, run.holder_identity).await?;

    let total_count: usize = resource_status
        .eligible_hosts
        .iter()
        .map(|g| g.hosts.len())
        .sum();
    let outdated_count = find_outdated_hosts(resource_status, &run.execution_hash)?.len();

    resource_status.summary = match outdated_count {
        0 => Some(format!("{total_count}/{total_count} up-to-date")),
        n => Some(format!("{n}/{total_count} outdated")),
    };

    Ok(match &object.spec.mode {
        ExecutionMode::OneShot => {
            resource_status.next_run = None;
            resource_status.phase = match outdated_count {
                0 => Phase::Succeeded,
                _ => Phase::Failed,
            };
            None
        }
        ExecutionMode::Recurring => match &object.spec.schedule {
            Some(schedule) => {
                let tz = object.timezone().unwrap();
                let now = || Utc::now().with_timezone(&tz);

                resource_status.phase = Phase::Scheduled;
                let next = forecast_next_run(schedule, now(), Some(chrono::Duration::seconds(-5)));
                resource_status.next_run = Some(next.fixed_offset());
                Some((next - now()).to_std().unwrap())
            }
            None => {
                warn!("Mode is Recurring but schedule is not set!");
                None
            }
        },
    })
}

/// The `ansible-playbook` container's termination message — the recap the callback wrote to
/// `/dev/termination-log`, surfaced by the kubelet as `state.terminated.message`. `None` if the
/// pod has no such terminated container yet or it wrote nothing (hard crash before the stats hook).
fn termination_message(pod: &Pod) -> Option<String> {
    pod.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .find(|cs| cs.name == job_builder::ANSIBLE_CONTAINER_NAME)
        .and_then(|cs| cs.state.as_ref())
        .and_then(|state| state.terminated.as_ref())
        .and_then(|terminated| terminated.message.clone())
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

/// Picks the most recently created Job that hasn't reached a terminal state — the "still active"
/// attempt for a run, if there is one. Pure so it's unit-testable without a kube client.
fn newest_active_job(jobs: &[Job]) -> Option<&Job> {
    jobs.iter()
        .filter(|job| !status::job_finished(job))
        .max_by_key(|job| job.metadata.creation_timestamp.as_ref().map(|t| t.0))
}

/// Ensures exactly one active Job exists for this run, adopting an already-active one instead of
/// creating a duplicate.
///
/// The `reconcile` spawn gate keys off `phase` read from the *reflector cache*, which lags this
/// controller's own `patch_status` writes — so several reconciles fired in quick succession
/// (proxy pods turning Ready, Job status events) can all reach this point before any observes
/// `phase = Applying`. Guarding on the cached status therefore can't prevent duplicates; only a
/// fresh (quorum) `list` by the run's hash label reliably sees a Job a previous tick just created.
/// If one is still active, adopt it; otherwise this is a genuinely new attempt (first run, or a
/// retry after the previous one reached a terminal state) and we create the next numbered Job.
async fn spawn_ansible_job(
    api: &Api<Job>,
    hash: ExecutionHash,
    run_groups: &[ResolvedInventoryGroup],
    playbookplan: &PlaybookPlan,
    resource_status: &mut PlaybookPlanStatus,
) -> Result<(), ReconcileError> {
    use kube::runtime::reflector::Lookup as _;

    let existing = api
        .list(&ListParams::default().labels(&format!("{}={hash}", labels::PLAYBOOKPLAN_HASH)))
        .await?;

    if let Some(active) = newest_active_job(&existing.items) {
        let job_name = active.name().expect("a listed Job always has a name");
        debug!("Adopting already-active job {job_name} for this run");
        resource_status.current_job_name = Some(job_name.to_string());
        resource_status.phase = Phase::Applying;
        resource_status.next_run = None;
        return Ok(());
    }

    // No active Job for this run — a genuinely new attempt. `retry_count` climbs monotonically so
    // the new name is expected not to collide with an already-finished attempt's; it's reset to 0
    // in `reconcile` whenever `current_hash` changes.
    resource_status.retry_count += 1;

    let job = job_builder::create_job_for_run(
        &hash,
        resource_status.retry_count,
        run_groups,
        playbookplan,
    )?;
    let job_name = job
        .name()
        .expect(".metadata.name must be set at this point")
        .to_string();

    resource_status.current_job_name = Some(job_name.clone());
    resource_status.phase = Phase::Applying;
    resource_status.next_run = None;

    info!("Creating job {job_name}");
    match api
        .create(
            &PostParams {
                field_manager: Some("ansible-operator".into()),
                ..Default::default()
            },
            &job,
        )
        .await
    {
        Ok(_) => {}
        // A Job by this exact name already exists. In principle `retry_count` should always be
        // ahead of every name already in the cluster, but if a previous tick created a Job and
        // then errored *before* `patch_status` ran, the bump above never got persisted — so this
        // tick recomputes the same name a real Job already holds. Treating that as fatal (instead
        // of adopting it here) would be the actual bug: erroring via `?` skips `patch_status` too,
        // so nothing this tick would get persisted either, and the next tick would recompute the
        // exact same name and hit the exact same 409 — a permanent stall on one name, observed
        // live. Adopting instead means current_job_name/phase are persisted this tick regardless,
        // so the run can proceed against whatever Job actually holds that name, and the next
        // genuinely-new attempt computes its retry_count from state that now matches reality.
        Err(err) if is_conflict(&err) => {
            info!("Job {job_name} already exists, adopting it");
        }
        Err(err) => return Err(err.into()),
    }

    Ok(())
}

fn is_conflict(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(status) if status.code == 409)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1beta1::{PlaybookPlanSpec, ResolvedHosts, SecretRef, SshConfig};

    fn managed_ssh_group(
        name: &str,
        hosts: &[&str],
        tolerations: Option<Vec<Toleration>>,
    ) -> ResolvedInventoryGroup {
        ResolvedInventoryGroup::ManagedSsh {
            hosts: ResolvedHosts {
                name: name.into(),
                hosts: hosts.iter().map(|h| h.to_string()).collect(),
            },
            tolerations,
        }
    }

    fn ssh_group(
        name: &str,
        hosts: &[&str],
        static_inventory_name: &str,
    ) -> ResolvedInventoryGroup {
        ResolvedInventoryGroup::Ssh {
            hosts: ResolvedHosts {
                name: name.into(),
                hosts: hosts.iter().map(|h| h.to_string()).collect(),
            },
            static_inventory_name: static_inventory_name.into(),
            config: SshConfig {
                user: "root".into(),
                secret_ref: SecretRef {
                    name: "ssh-key".into(),
                },
            },
        }
    }

    #[test]
    fn filter_groups_to_hosts_keeps_only_triggered_hosts_and_drops_empty_groups() {
        let groups = vec![
            managed_ssh_group("controlplanes", &["worker-1", "worker-2"], None),
            ssh_group("external", &["ccu.fritz.box"], "ccu"),
        ];

        let filtered = filter_groups_to_hosts(&groups, &["worker-1".to_string()]);

        assert_eq!(
            filtered.len(),
            1,
            "the ssh group has no triggered hosts and should be dropped entirely"
        );
        let ResolvedInventoryGroup::ManagedSsh { hosts, .. } = &filtered[0] else {
            panic!("expected the managed-ssh group to survive");
        };
        assert_eq!(hosts.hosts, vec!["worker-1".to_string()]);
    }

    #[test]
    fn filter_groups_to_hosts_preserves_group_specific_config() {
        let tolerations = Some(vec![Toleration {
            key: Some("dedicated".into()),
            ..Default::default()
        }]);
        let groups = vec![managed_ssh_group(
            "controlplanes",
            &["worker-1"],
            tolerations.clone(),
        )];

        let filtered = filter_groups_to_hosts(&groups, &["worker-1".to_string()]);

        let ResolvedInventoryGroup::ManagedSsh { tolerations: t, .. } = &filtered[0] else {
            panic!("expected a ManagedSsh group");
        };
        assert_eq!(t, &tolerations);
    }

    #[test]
    fn managed_ssh_hosts_and_tolerations_flattens_only_managed_ssh_groups() {
        let groups = vec![
            managed_ssh_group("controlplanes", &["worker-1"], None),
            ssh_group("external", &["ccu.fritz.box"], "ccu"),
            managed_ssh_group("workers", &["worker-2"], None),
        ];

        let (hosts, _) = managed_ssh_hosts_and_tolerations(&groups);

        assert_eq!(hosts, vec!["worker-1".to_string(), "worker-2".to_string()]);
    }

    #[test]
    fn managed_ssh_hosts_and_tolerations_uses_first_non_none_toleration() {
        let first = vec![Toleration {
            key: Some("first".into()),
            ..Default::default()
        }];
        let second = vec![Toleration {
            key: Some("second".into()),
            ..Default::default()
        }];
        let groups = vec![
            managed_ssh_group("a", &["worker-1"], None),
            managed_ssh_group("b", &["worker-2"], Some(first.clone())),
            managed_ssh_group("c", &["worker-3"], Some(second)),
        ];

        let (_, tolerations) = managed_ssh_hosts_and_tolerations(&groups);

        assert_eq!(tolerations, Some(first));
    }

    #[test]
    fn is_conflict_matches_only_409() {
        let conflict = kube::Error::Api(Box::new(kube::core::Status {
            code: 409,
            ..Default::default()
        }));
        let not_found = kube::Error::Api(Box::new(kube::core::Status {
            code: 404,
            ..Default::default()
        }));

        assert!(is_conflict(&conflict));
        assert!(!is_conflict(&not_found));
    }

    #[test]
    fn newest_active_job_skips_finished_and_picks_the_latest() {
        use k8s_openapi::api::batch::v1::{Job, JobCondition, JobStatus};
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
        use k8s_openapi::jiff::Timestamp;

        fn job(name: &str, created_secs: i64, finished: bool) -> Job {
            let conditions = finished.then(|| {
                vec![JobCondition {
                    type_: "Failed".into(),
                    status: "True".into(),
                    ..Default::default()
                }]
            });
            Job {
                metadata: ObjectMeta {
                    name: Some(name.into()),
                    creation_timestamp: Some(Time(Timestamp::from_second(created_secs).unwrap())),
                    ..Default::default()
                },
                status: Some(JobStatus {
                    conditions,
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        // A finished attempt plus two still-running ones — the newest active wins, not the newest
        // overall and not a finished one.
        let jobs = vec![
            job("apply-x-4", 100, true),
            job("apply-x-5", 200, false),
            job("apply-x-6", 300, false),
        ];
        assert_eq!(
            newest_active_job(&jobs).and_then(|j| j.metadata.name.as_deref()),
            Some("apply-x-6")
        );

        // Everything terminal -> no active job, so the caller creates a fresh retry.
        let all_finished = vec![job("apply-x-4", 100, true), job("apply-x-5", 200, true)];
        assert!(newest_active_job(&all_finished).is_none());

        assert!(newest_active_job(&[]).is_none());
    }

    #[test]
    fn slot_already_triggered_suppresses_only_a_repeat_of_the_same_slot() {
        let slot = |s: &str| Some(s.parse::<DateTime<FixedOffset>>().unwrap());

        // Unscheduled ticks (no slot) are never suppressed.
        assert!(!slot_already_triggered(None, None));
        assert!(!slot_already_triggered(None, slot("2025-08-12T20:00:00Z")));

        // The first time a slot is seen it hasn't been triggered yet.
        assert!(!slot_already_triggered(slot("2025-08-12T20:00:00Z"), None));

        // The same slot already recorded -> suppress the re-trigger inside its grace window.
        assert!(slot_already_triggered(
            slot("2025-08-12T20:00:00Z"),
            slot("2025-08-12T20:00:00Z"),
        ));

        // Equality is by instant, so an equivalent moment in another offset still matches.
        assert!(slot_already_triggered(
            slot("2025-08-12T22:00:00+02:00"),
            slot("2025-08-12T20:00:00Z"),
        ));

        // A later slot than the recorded one -> a genuinely new run.
        assert!(!slot_already_triggered(
            slot("2025-08-13T20:00:00Z"),
            slot("2025-08-12T20:00:00Z"),
        ));
    }

    #[test]
    fn extract_resource_info_requires_namespace_name_and_generation() {
        let mut pp = PlaybookPlan::new("placeholder", PlaybookPlanSpec::default());
        pp.metadata.name = None;

        assert!(matches!(
            extract_resource_info(&pp),
            Err(ReconcileError::PreconditionFailed("namespace not set"))
        ));

        pp.metadata.namespace = Some("default".into());
        assert!(matches!(
            extract_resource_info(&pp),
            Err(ReconcileError::PreconditionFailed("name not set"))
        ));

        pp.metadata.name = Some("an-example".into());
        assert!(matches!(
            extract_resource_info(&pp),
            Err(ReconcileError::PreconditionFailed("generation not set"))
        ));

        pp.metadata.generation = Some(3);
        assert_eq!(
            extract_resource_info(&pp).unwrap(),
            ("default", "an-example", 3)
        );
    }

    #[test]
    fn get_related_secrets_collects_variable_and_file_secrets_but_not_inline_or_image_sources() {
        let yaml = r#"
apiVersion: ansible.cloudbending.dev/v1beta1
kind: PlaybookPlan
metadata:
  name: an-example
spec:
  image: docker.io/serversideup/ansible-core:2.18
  mode: OneShot
  inventoryRefs: []
  template:
    variables:
      - inline:
          key: value
      - secretRef:
          name: secret-with-variables
    files:
      - name: binary-assets
        image:
          reference: my.registry.tld/the-image:v2
          pullPolicy: IfNotPresent
      - name: some-configs
        secretRef:
          name: secret-with-config-files
    playbook: |
      - hosts: all
        tasks: []
        "#;
        let pp = serde_yaml::from_str::<PlaybookPlan>(yaml).unwrap();

        let secrets: Vec<&str> = get_related_secrets(&pp)
            .into_iter()
            .map(String::as_str)
            .collect();

        assert_eq!(
            secrets,
            vec!["secret-with-variables", "secret-with-config-files"]
        );
    }
}
