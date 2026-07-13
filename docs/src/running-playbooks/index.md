# Running playbooks

This chapter is for **users** — anyone who writes resources in a tenant namespace to run playbooks.
It assumes the operator is already installed and your namespace has been *enrolled* by a cluster
administrator. If a plan you create is stuck in `UnauthorizedNamespace`, enrollment is what is
missing; see [Deployment → enrolled namespaces](../cluster-operators/deployment.md#enrolled-namespaces).

## How a run works

1. You write a `PlaybookPlan`: the playbook text, which inventories to target, a schedule, and any
   variables and files.
2. You write one or more inventories the plan references — a `ClusterInventory` for cluster Nodes
   and a `StaticInventory` for external hosts.
3. When the schedule fires (or immediately, if there is no schedule), the operator renders a private
   workspace, makes the target hosts reachable, and runs one Kubernetes Job that applies the
   playbook to every targeted host at once.
4. When the Job finishes, the operator records a per-host outcome and a summary on the plan's
   `.status`, then stops (`OneShot`) or reschedules (`Recurring`).

A single plan can mix both kinds of target in one run: cluster Nodes and external hosts land in one
rendered inventory and one Job.

## Example

Patch every worker node nightly:

```yaml
apiVersion: ansible.cloudbending.dev/v1beta1
kind: ClusterInventory
metadata:
  name: cluster-nodes
  namespace: my-team
spec:
  hosts:
    - name: workers
      matchExpressions:
        - { key: node-role.kubernetes.io/control-plane, operator: DoesNotExist }
  tolerations:
    - operator: Exists
---
apiVersion: ansible.cloudbending.dev/v1beta1
kind: PlaybookPlan
metadata:
  name: patch-workers
  namespace: my-team
spec:
  image: docker.io/serversideup/ansible-core:2.18
  mode: Recurring
  schedule: "0 3 * * *"
  timeZone: Europe/Berlin
  inventoryRefs:
    - clusterInventory: cluster-nodes
  template:
    playbook: |
      - hosts: all
        tasks:
          - name: Upgrade all packages
            ansible.builtin.apt:
              upgrade: dist
              update_cache: true
```

## In this chapter

- [Playbook plans](./playbook-plans.md) — the `PlaybookPlan` resource in full
- [Targeting cluster nodes](./cluster-nodes.md) — `ClusterInventory` and managed SSH
- [Targeting external hosts](./external-hosts.md) — `StaticInventory` and bring-your-own SSH
- [Scheduling and execution modes](./scheduling-and-modes.md) — when and how often a plan runs
- [Variables and files](./variables-and-files.md) — passing data into the playbook
- [Reading results and troubleshooting](./results-and-troubleshooting.md) — interpreting `.status`
