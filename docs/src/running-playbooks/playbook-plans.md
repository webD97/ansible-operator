# Playbook plans

A `PlaybookPlan` is the central resource: it is the thing the operator reconciles. It ties a
**playbook** to a set of **inventories**, a **schedule**, and an **execution mode**, and it is
where per-host results are reported back. This page explains how the fields fit together; the
exhaustive field list and defaults live in the CRD schema (`ansible-operator crds`) and the
generated API reference.

## Spec fields

| Field | Required | Meaning |
|---|---|---|
| `image` | yes | An OCI image that has `ansible-playbook` and every collection your playbook uses. The Job runs this image. |
| `inventoryRefs` | yes | Which inventories to target — one entry per referenced `ClusterInventory` or `StaticInventory`. |
| `template.playbook` | yes | The playbook text itself (see below). |
| `mode` | no (`OneShot`) | `OneShot` or `Recurring` — see [Scheduling and execution modes](./scheduling-and-modes.md). |
| `schedule` | no | A 5-field cron expression gating when the plan may run. Omit for "as soon as possible". |
| `timeZone` | no (UTC) | IANA time zone the `schedule` is evaluated in, e.g. `Europe/Berlin`. |
| `suspend` | no (`false`) | Pause switch (like a CronJob's `suspend`): while `true` the operator starts no new runs. See [Suspending a plan](./scheduling-and-modes.md#suspending-a-plan). |
| `template.variables` | no | Variables made available to the playbook — see [Variables and files](./variables-and-files.md). |
| `template.files` | no | Files made available at runtime — see [Variables and files](./variables-and-files.md). |
| `template.requirements` | no | An Ansible `requirements.yml` (e.g. collections) installed before the run. |
| `ttlSecondsAfterFinished` | no | How long a finished run's Job/pod is kept before Kubernetes reaps it. Values below 60s are raised to 60. |

## Choosing the image

The operator does **not** ship Ansible; your `image` provides it. Pick (or build) an image that
already contains `ansible-playbook` plus every collection and Python dependency your tasks need.
Community images such as `docker.io/serversideup/ansible-core:<version>` work well as a base. If
your playbook needs collections that are not baked into the image, list them under
`template.requirements` and they will be installed into the run before the playbook executes:

```yaml
template:
  requirements: |
    collections:
      - name: community.general
        version: ">=6.0.0"
  playbook: |
    - hosts: all
      tasks: []
```

Baking collections into the image is faster and more reproducible than installing them on every
run; use `requirements` for things you cannot or do not want to pre-bake.

## The playbook

`template.playbook` is an ordinary Ansible playbook as a YAML string. Two conventions matter:

- **Target `hosts: all`** (or a group name from your inventories). The operator renders the
  inventory for you; your playbook selects hosts out of it. Every host from every referenced
  inventory group is present, grouped by the group `name` you gave it.
- The operator injects the inventory and connection variables automatically. You do **not** set
  `ansible_host`, `ansible_user`, `ansible_ssh_private_key_file`, connection ports, or host-key
  settings — those are rendered from the inventories and (for cluster nodes) the managed-SSH
  machinery. Setting them yourself will fight the operator.

The playbook text is round-tripped through a YAML parser when the plan is reconciled, so a
syntactically broken playbook surfaces as an error early rather than as a failed Job.

## Referencing inventories

`inventoryRefs` is a list; each entry names **exactly one** inventory by kind:

```yaml
inventoryRefs:
  - clusterInventory: cluster-nodes        # a ClusterInventory in this namespace
  - staticInventory: edge-appliances       # a StaticInventory in this namespace
```

Inventories are resolved from the **same namespace** as the plan. The groups they define become
Ansible groups in the rendered inventory, so a playbook can target `hosts: workers` or
`hosts: edge-appliances` as well as `hosts: all`.

## One Job per run

Each run is a single Kubernetes Job (named `apply-<plan>-<id>-<retry>`) that applies the playbook to
all of that run's hosts together — not one Job per host. This is what lets a playbook use Ansible
features that span hosts (`serial`, `run_once`, delegation) normally. The operator adds per-host
**Leases** so two runs never touch the same host at once, and it softly steers the Job's own pod
*away* from the Nodes the run targets, so a disruptive playbook is less likely to evict its own
runner mid-run.

## Lifecycle at a glance

A plan moves through phases — `Pending` → `Applying` → `Succeeded`/`Failed` (for `OneShot`) or
`… → Scheduled → …` (for `Recurring`). Drift detection decides *which* hosts actually run: an
execution hash over the playbook plus every referenced Secret marks hosts out of date, and a host
that already succeeded on the current hash is skipped. See
[Scheduling and execution modes](./scheduling-and-modes.md) for the mechanics and
[Reading results](./results-and-troubleshooting.md) for how to read the outcome.
