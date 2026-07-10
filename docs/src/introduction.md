# Ansible Operator

A Kubernetes operator that runs Ansible playbooks against your cluster's own Nodes and against
arbitrary external hosts — on a schedule, idempotently, and **without a standing privileged agent
on your nodes**.

You describe *what* to apply and *where* as Kubernetes resources. The operator renders an Ansible
inventory and playbook, makes your target hosts reachable, runs `ansible-playbook` in a Kubernetes
Job, and records a per-host outcome back onto the resource's status.

## What it can target

A single run can reach two kinds of host, together:

- **Your cluster's own Nodes**, reached as node-root through short-lived *managed-SSH proxy pods*
  the operator schedules onto each target Node and tears down afterwards. See
  [Targeting cluster nodes](./running-playbooks/cluster-nodes.md).
- **Arbitrary external hosts** (servers, appliances, network gear), reached over plain SSH with a
  key you supply. See [Targeting external hosts](./running-playbooks/external-hosts.md).

## The resources you author

| Resource | What it is |
|---|---|
| `PlaybookPlan` | The thing that runs: a playbook + which inventories to run it against + a schedule + variables/files. This is what the operator reconciles. |
| `ClusterInventory` | Cluster targets: host groups resolved dynamically from cluster **Node** labels (reached via managed SSH). |
| `StaticInventory` | External targets: host groups given as literal hostnames/IPs, with the SSH credentials to reach them. |

A fourth resource, `NodeAccessPolicy`, is authored by cluster **admins**, not tenants — it caps
which Nodes a namespace may reach. See
[Node access policies](./cluster-operators/node-access-policies.md).

## Who this guide is for

It is written for two audiences, in two chapters:

- **[Running playbooks](./running-playbooks/index.md)** — for **users** who author `PlaybookPlan`s
  and inventories in a tenant namespace.
- **[For cluster operators](./cluster-operators/index.md)** — for **admins** who install, secure,
  and run the operator.

> **Field-level reference.** This guide is narrative. For the exhaustive list of every field,
> default, and enum value on the custom resources, see the generated API reference (`cargo doc`) or
> the CRD schemas the operator emits with `ansible-operator crds`.

## A taste

Patch every worker node nightly:

```yaml
apiVersion: ansible.cloudbending.dev/v1beta1
kind: ClusterInventory
metadata:
  name: cluster-nodes
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
