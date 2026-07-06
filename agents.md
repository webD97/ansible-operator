# ansible-operator — agent notes

Opinionated Kubernetes operator that runs Ansible playbooks against cluster
nodes or arbitrary hosts (inspired by Rancher's system-upgrade-controller).
Rust, built on `kube-rs` 3.x. Single binary, two controllers running
concurrently in `main.rs` via `tokio::join!`.

## Layout

```
src/main.rs                          entrypoint, tracing setup, --crd flag for CRD YAML dump
src/utils.rs                         create_or_update (server-side apply helper), Condition trait, generate_id (k8s-like short ID)
src/v1beta1/
  resources/                         CRD types (kube::CustomResource derives)
    playbookplan.rs                  PlaybookPlan: spec, status, Phase enum
    cluster_inventory.rs             ClusterInventory: hosts resolved from Node labels
    static_inventory.rs              StaticInventory: hosts given as literal names/IPs
    generic.rs                       NodeSelectorTerm/SelectorExpression (k8s-style match_labels/match_expressions), GenericMap in playbookplan.rs
    custom_rfc3339.rs                serde helpers for Option<DateTime<FixedOffset>>
  controllers/
    playbookplancontroller/          the big one — see below
    clusterinventorycontroller/      resolves Node → hosts, watches Nodes, writes ClusterInventoryStatus
    ansible_inventory.rs             AnsibleInventory trait (get_hosts) + ResolvedHosts, implemented by both inventory kinds
    nodeselector.rs                  node_matches: evaluates NodeSelectorTerm against a node's labels
    reconcile_error.rs               shared ReconcileError enum (thiserror) for both controllers
  ansible/                           renders CR spec fields into files that go in the workspace Secret
    playbook_renderer.rs             re-serializes spec.template.playbook YAML (round-trip, mostly a validation step)
    inventory_renderer.rs            ResolvedHosts → Ansible YAML inventory
    render_error.rs                  RenderError
  labels.rs                          k8s label keys used to tag Jobs (playbookplan name/hash/target-host)
```

## Core reconcile flow (playbookplancontroller/reconciler.rs)

One `PlaybookPlan` fans out into one Kubernetes `Job` per target host. Roughly:

1. Resolve inventory refs (`ClusterInventory`/`StaticInventory`) → `Vec<ResolvedHosts>`, store as `status.eligible_hosts`.
2. Render/refresh a per-PlaybookPlan "workspace" Secret (`workspace.rs`) containing `playbook.yml`, `inventory.yml`, `requirements.yml`, `static-variables-N.yml` — only when missing or `status.last_rendered_generation < metadata.generation`.
3. Compute an `ExecutionHash` (`execution_evaluator.rs`) over the playbook text + the contents of every referenced Secret (variables + files), order-insensitive (XxHash3_64, wrapping-add per secret). This hash is the cache key for "does this host need to re-run."
4. Evaluate the cron `schedule` (`triggers.rs`, `evaluate_schedule`/`forecast_next_run`) against `now()` in the plan's configured timezone, inside a 15s window. Result is `Timing::Now(_)` or `Timing::Delayed(until)`.
5. If hosts are outdated (`find_outdated_hosts` compares `status.hosts_status[host].last_applied_hash` to the current `ExecutionHash`) and timing says now, spawn one `Job` per host (`job_builder.rs`).
6. List Jobs by label selector (`playbookplan` name + hash), fold their status into `PlaybookPlanCondition`s (`Ready`, `Running`) and per-host `hosts_status` (`status.rs`).
7. Once all Jobs for the current hash are finished, compute `status.summary` and set the terminal `Phase` (`Succeeded`/`Failed` for `OneShot`, or reschedule for `Recurring`).
8. `replace_status` — always rebuilds a fresh `PlaybookPlan{ metadata, spec: default, status }` object and calls `.replace_status()`, it does not patch in place.

Requeue interval is dynamic: 3600s default, tightened to "time until next scheduled run" or 15s (Job-polling interval, from the controller's error_policy) as appropriate.

### Execution modes (`ExecutionMode`)

- `OneShot` (default): only outdated hosts get Jobs; once finished, phase becomes `Succeeded`/`Failed` and `next_run` clears. Re-triggers only if the execution hash changes again.
- `Recurring`: *all* hosts (not just outdated ones) get Jobs every time the schedule fires; after completion it reschedules via `forecast_next_run` and goes back to `Phase::Scheduled`.

### Connection strategies (`ConnectionStrategy`)

- `Ssh { user, secretRef }` — mounts an SSH key volume, runs `ansible-playbook` with `-c ssh`-style args, `--user`, `--private-key /ssh/id_rsa`, limits to the one target host with `-l <host>,`.
- `Chroot { tolerations }` (default) — mounts the node's `/` via `hostPath`, runs privileged with `hostIPC/hostNetwork/hostPID/hostUsers`, uses `community.general.chroot` connection plugin, and pins the Job to the target node via `nodeSelector: kubernetes.io/hostname`.

Both strategies produce exactly one Job per host; the "one Job per host" invariant is load-bearing for how `hosts_status`/labels/hashing work — don't change it lightly.

### Job naming and idempotency

Job name is `apply-{plan}-{shortid}-on-{host}` where `shortid = generate_id(execution_hash ^ start_time_hash)` (`utils::generate_id`, a base-27 k8s-style encoding). Before creating a Job, the code checks `api.get_opt(&job_name)` and skips if it already exists — this is the dedup/idempotency mechanism, not a Kubernetes owner-based lookup. `// TODO: Check for jobs with another hash and decide if we need to replace them` — old Jobs for stale hashes are never cleaned up automatically.

### Secret change triggers

The controller `.watches(secrets_api, ..., mappers::secret_to_playbookplans(...))` — any Secret change re-triggers reconciliation for every `PlaybookPlan` in the same namespace that references it (as a variable `secretRef` or a file `secretRef`), using an in-memory reflector store rather than a live API list. Same pattern exists in `clusterinventorycontroller` for Node → ClusterInventory via `mappers::node_to_inventories`.

## Current WIP / in-flight work

- **`PLAY_RECAP_RE` in `reconciler.rs`** (added in `84fd542 wip: parse final logs`, uncommitted-feeling — still on `52c71bb checkpoint: pre-start`): after all Jobs for a PlaybookPlan finish, the code tails each Job's pod logs and regex-matches Ansible's `PLAY RECAP` line (`host : ok=N changed=N unreachable=N failed=N skipped=N rescued=N ignored=N`), but currently just `println!`s the parsed fields — nothing is persisted to `PlaybookPlanStatus` yet. **This is exploratory, not a finished design** — if picking this up, decide where the parsed counts should live (e.g. extend `HostStatus` with recap counters, or fold into `status.summary`) rather than assuming an existing plan.
- This log-fetching code path (`pods_api.list(...).unwrap()`, `.logs(...).unwrap()`) uses raw `.unwrap()` instead of `ReconcileError` propagation, unlike the rest of `reconcile()` — worth hardening if this becomes permanent.

## Known rough edges / things to know before touching related code

- **Stale examples**: `examples/chroot.yaml`, `examples/k3s-worker.yaml`, `examples/oneshot-*.yaml`, `examples/static-inventory.yaml` (top-level) use a pre-refactor schema (`spec.inventory: [{name, hosts: {fromNodes|fromList}}]`) that predates the `ClusterInventory`/`StaticInventory` split (`8f93a9d`). They no longer deserialize against the current `PlaybookPlanSpec` (which has `inventory_refs: Vec<InventoryRef>` instead). **Only `examples/v1beta1/*.yaml` reflects the current CRD shape.** Treat the top-level ones as due for an update/removal, not as a reference when writing new examples or docs.
- README's feature checklist (`Time windows`, `Scheduling` marked unchecked) is stale relative to the code — cron `schedule` + timezone + a delay window are already fully implemented in `triggers.rs`.
- `PlaybookPlan::replace_status` and `ClusterInventory`'s equivalent always send a full spec-defaulted object with only `status` populated — this works because `replace_status` on the k8s API ignores everything but `.status`, but it means you must not add spec fields that don't have safe/empty `Default` impls.
- Many places use `.unwrap()`/`.expect()` on the assumption that Kubernetes guarantees hold (e.g. `.metadata.name` being set on any object read back from a `PlaybookPlan`-owned Job). This is a deliberate style choice in this codebase, not an oversight — preconditions genuinely enforced by the apiserver are unwrapped; only things this operator itself must guarantee (namespace/name/generation on the primary object) go through `ReconcileError::PreconditionFailed`.
- `FilesSource::Other` accepts arbitrary JSON/YAML and round-trips it through a real `k8s_openapi::api::core::v1::Volume` via `serde_json` — this is intentional so any Kubernetes volume type is supported without hand-modeling it (see `job_builder::extract_file_volumes` doc comment). Errors surface per-item as `Result`, not a panic.
- `ansible/playbook_renderer.rs` re-parses+re-serializes the playbook YAML mostly as a validation step (round-trip through `serde_yaml::Sequence`), not because the structure is transformed.

## Testing & workflow

- `cargo test` — 37 unit tests as of this writing, all colocated in `#[cfg(test)] mod tests` at the bottom of the file they test (no separate `tests/` dir). Follow this convention for new tests.
- `cargo clippy` is clean; no `clippy.toml`/lint overrides exist — keep it that way rather than special-casing lints.
- No integration/e2e tests against a real cluster; correctness for reconcile logic is tested via pure functions extracted for exactly that purpose (`execution_evaluator`, `triggers`, `status`, `nodeselector`, `job_builder::extract_file_volumes`) — when adding reconciler logic, prefer pulling it into a small pure function like these rather than testing through the full `reconcile()`.
- CI (`.github/workflows/container.yml`): `cargo test` and `cargo build --release` on `rust:1-bookworm`, then builds/pushes a Containerfile-based distroless image (build output binary is just copied in, no cargo build inside the container image itself).
- `./ansible-operator --crd` dumps all three CRDs' YAML (for `kubectl apply` or Helm chart generation) and exits — check this path still works after changing any `#[derive(CustomResource)]` type.
- `.agents/skills/` is a vendored skill pack (rust-skills) unrelated to this project's own domain logic — not something to modify as part of feature work here.
