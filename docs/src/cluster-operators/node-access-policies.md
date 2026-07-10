# Node access policies

A `NodeAccessPolicy` is the admin-authored ceiling on which cluster **Nodes** a namespace's
`ClusterInventory` resources may reach. Because a `ClusterInventory` confers
[node-root](./security.md#the-core-fact-managed-ssh-is-node-root), any namespace allowed to create
one could otherwise target *any* Node — the policy is what stops that.

> **Fail-closed, always on.** A namespace with **no** matching policy resolves to **zero** allowed
> Nodes. There is no default-allow and no way to disable the check — until you author a policy,
> managed-SSH plans target nothing. This is the single most common "why does my `ClusterInventory`
> resolve to no hosts?" cause.

## Where to author them

Author policies **in the operator's own namespace** (e.g. `ansible-system`). The CRD is namespaced
and the API server will accept a policy created anywhere — but **enforcement reads only the operator
namespace**. A policy created in a tenant namespace still gets a populated `.status` (so it looks
like it is "working"), yet is completely ignored by enforcement. Restricting who can write to the
operator namespace via RBAC is therefore what makes this an *admin* control rather than a tenant one.

## What a policy says

Each policy maps a set of **namespaces** to a ceiling set of **Nodes** with two label selectors (each
a `matchLabels`/`matchExpressions` selector, like Kubernetes' own):

- `namespaceSelector` — which namespaces this policy grants access to. Kubernetes stamps every
  namespace with `kubernetes.io/metadata.name: <name>`, so you target a single namespace by that
  label; there is deliberately no separate name field.
- `nodeSelector` — the ceiling: the Nodes those namespaces may resolve. A `ClusterInventory`'s
  resolved Nodes are **intersected** with the Nodes matching this selector.

```yaml
apiVersion: ansible.cloudbending.dev/v1beta1
kind: NodeAccessPolicy
metadata:
  name: business-team
  namespace: ansible-system      # the operator namespace — admin-only via RBAC
spec:
  namespaceSelector:
    matchLabels:
      kubernetes.io/metadata.name: business-app
  nodeSelector:
    matchExpressions:
      - { key: node-pool, operator: In, values: [business] }
```

To cover several namespaces in one policy, use `matchExpressions` on the `namespaceSelector`, e.g.
`{ key: team, operator: In, values: [business, payments] }`.

## There is no "match everything" shortcut

An **empty** selector (`{}`) matches **nothing**, not everything — the opposite of Kubernetes' usual
convention, and a deliberate fail-closed choice. To grant *all* Nodes, match a label every Node
carries, explicitly:

```yaml
apiVersion: ansible.cloudbending.dev/v1beta1
kind: NodeAccessPolicy
metadata:
  name: cluster-admins
  namespace: ansible-system
spec:
  namespaceSelector:
    matchLabels:
      kubernetes.io/metadata.name: ansible-system
  nodeSelector:
    matchExpressions:
      - { key: kubernetes.io/hostname, operator: Exists }   # every Node has a hostname
```

## How multiple policies combine

A namespace's effective allow-set is the **union** of the `nodeSelector`s of **every** policy whose
`namespaceSelector` matches it. That union is then intersected with each `ClusterInventory`'s
resolved Nodes at run time. So you can layer policies — a broad baseline plus narrower grants — and a
namespace gets the sum of what any matching policy allows, never more than the Nodes that actually
exist.

## Observing a policy

Each policy's controller keeps its `.status` current for observability:

- `matchedNamespaces` — the namespaces currently selected.
- `allowedNodeCount` / `allowedNodes` — the size and the concrete, sorted list of Nodes the ceiling
  currently resolves to. `allowedNodeCount` is surfaced as the `Allowed nodes` printer column
  (`kubectl get nodeaccesspolicy`).

When a tenant reports a `ClusterInventory` resolving to nothing, compare that inventory's
`.status.hostCount` (Nodes matched *before* policy clamping) with the `allowedNodes` of the policy
that should cover their namespace — the gap is exactly what the ceiling removed.
