# Targeting cluster nodes

A `ClusterInventory` builds host groups **dynamically from your cluster's own Nodes**, matched by
Node labels, and reaches them over *managed SSH* — the operator's agentless, node-root access path.
Use it to run playbooks against the machines your cluster runs on: OS patching, kernel/k3s
upgrades, node-level configuration, and the like.

> **This is a node-root primitive.** A managed-SSH session is `root` on the target Node — that is
> the whole point, and the reason access is gated. Which Nodes your namespace may actually reach is
> capped by a cluster-admin-authored `NodeAccessPolicy`; if a `ClusterInventory` resolves to
> **zero** hosts even though Nodes match your selector, no policy grants your namespace those Nodes.
> See [Node access policies](../cluster-operators/node-access-policies.md).

## Defining host groups

Each entry under `spec.hosts` is a group: a `name` plus a Node **label selector**. A Node that
matches lands in that group; the group name becomes an Ansible group your playbook can target.
Selectors come in two flavours (a group may use either, matching Kubernetes' own label-selector
semantics):

- **`matchLabels`** — an exact-match map; a Node must carry every listed label/value.
- **`matchExpressions`** — a list of `{ key, operator, values }` terms with operators `In`, `NotIn`,
  `Exists`, `DoesNotExist`.

```yaml
apiVersion: ansible.cloudbending.dev/v1beta1
kind: ClusterInventory
metadata:
  name: cluster-nodes
spec:
  hosts:
    - name: controlplanes
      matchLabels:
        kubernetes.io/os: linux
        node-role.kubernetes.io/control-plane: "true"
    - name: workers
      matchExpressions:
        - { key: kubernetes.io/os, operator: In, values: [linux] }
        - { key: node-role.kubernetes.io/control-plane, operator: DoesNotExist }
```

The controller watches Nodes and keeps `.status.resolvedHosts`/`.status.hostCount` up to date as
Nodes are labelled, added, or removed, so `kubectl get clusterinventory` shows how many Nodes
currently match.

## Tolerations

To reach a tainted Node (a control-plane node, say), the managed-SSH proxy pod for that Node must
tolerate its taints. Set `spec.tolerations` on the `ClusterInventory`; they are applied to the proxy
pods this inventory creates. `tolerations: [{ operator: Exists }]` tolerates everything — safe here,
because each proxy pod is already pinned to one exact Node, so tolerating all taints only lets it
schedule onto *that* Node, not wander elsewhere.

```yaml
spec:
  tolerations:
    - operator: Exists
```

## How managed SSH reaches a Node (what happens under the hood)

You do not configure any of this — it is background for understanding the security model and for
troubleshooting. For each targeted Node in a run, the operator:

1. Schedules a short-lived **proxy pod** onto that exact Node. The pod runs a real `sshd` and is
   granted just enough privilege (`hostPID`, a host `/proc` mount, `CAP_SYS_ADMIN` +
   `CAP_SYS_PTRACE`) that each SSH session can `nsenter` into the Node's host namespaces — making
   the session `root` on the Node. It deliberately does **not** use
   `privileged: true`/`hostNetwork`/`hostIPC`.
2. Mints a fresh SSH **host certificate** for that run from the operator's in-memory certificate
   authority, and a matching **client certificate** for the Job. Certificates are per-run and
   short-lived; a run can only authenticate to *its own* proxy pods.
3. Locks each proxy pod's ingress to that run's Job with a NetworkPolicy (defense in depth on top of
   the certificate isolation).
4. Renders the inventory so Ansible dials the proxy pod and verifies the Node's host certificate.
5. Tears the proxy pods, per-run Secrets, and NetworkPolicy down when the run finishes.

There is **no standing agent or DaemonSet** on your Nodes: proxy pods exist only for the duration of
a run. The security properties of this path (per-run certificate isolation, the in-memory CA, why
`NodeAccessPolicy` is mandatory) are covered in
[Security model](../cluster-operators/security.md).

## Requirements and gotchas

- The operator must be installed and your namespace **enrolled** (see
  [Deployment](../cluster-operators/deployment.md)).
- A `NodeAccessPolicy` must grant your namespace the Nodes you want to reach, or the inventory
  resolves to nothing for you.
- The proxy image must be a real OpenSSH `sshd` image the cluster can pull (an operator concern; the
  default and how to pin it are covered under [Deployment](../cluster-operators/deployment.md)).
- Managed SSH targets **Linux** Nodes.

## When to use a StaticInventory instead

`ClusterInventory` is only for machines that are **Kubernetes Nodes of this cluster**. To reach
anything else — external servers, appliances, network gear, or nodes of a *different* cluster — use
a `StaticInventory` with your own SSH key. See [Targeting external hosts](./external-hosts.md).
