# Security model

This page is the operator-facing summary of what you are trusting when you install the operator, and
of the mechanisms that bound that trust. The exhaustive, adversarial version — every threat, its
mitigation, and the numbered invariants (INV-1…INV-7) enforced by unit tests — is `THREAT_MODEL.md`
in the repository. **If this page and `THREAT_MODEL.md` disagree, the threat model is correct.**

## The core fact: managed SSH is node-root

Reaching a cluster Node via a `ClusterInventory` means a managed-SSH session that is **`root` on
that Node** — it `nsenter`s into the host namespaces of a proxy pod running with `hostPID` and
`SYS_ADMIN`/`SYS_PTRACE`. This is the intended capability (you cannot patch a Node's OS without it),
and everything else in the security model exists to answer: *who* may obtain it, over *which* Nodes,
and for *how long*. External hosts reached via a `StaticInventory` are a different, lower-privilege
path — there the operator is just an SSH client using a key you provided.

## Two fail-closed gates you must open

Both default to "deny", by design. Neither is a suggestion you can skip:

1. **Namespace enrollment** bounds *where the operator itself has power*. Its cluster RBAC omits
   `secrets`/`jobs`/`pods`; those are granted per-namespace only for enrolled namespaces. A plan in
   an un-enrolled namespace is refused (`UnauthorizedNamespace`). No "all namespaces" option exists.
   → [Deployment → enrolled namespaces](./deployment.md#enrolled-namespaces)
2. **Node access policies** bound *which Nodes a tenant may reach*. A namespace with no matching
   `NodeAccessPolicy` resolves to **zero** Nodes; an empty selector matches nothing, not everything.
   → [Node access policies](./node-access-policies.md)

## How node access is enforced (why it is trustworthy)

A `NodeAccessPolicy` is authored only by principals who can write to the operator namespace (i.e.
cluster admins) — a **different principal** than the tenant who authors the `ClusterInventory`.
Enforcement is a set **intersection**: a plan's requested Nodes are intersected with the union of
Nodes the matching policies grant that namespace, so it can only ever *shrink* the request. A forged,
buggy, or over-broad `ClusterInventory` can never reach a Node no policy allowed. The clamp runs at
inventory-resolve time, on **every** reconcile, **before** any proxy pod/Secret/NetworkPolicy is
created, against a **live** Node read.

## How runs are isolated from each other

Cross-run isolation is enforced at the **certificate** layer, not merely by network rules:

- The operator runs its **own SSH certificate authority**, generated **in memory at startup**. The
  CA private key is never written to a Secret, never persisted to etcd, never logged. **Restarting
  the operator rotates the CA** and invalidates every outstanding certificate.
- Each run gets fresh, short-lived host and client certificates. Each proxy pod's
  authorized-principals list contains **only its own run's execution hash** — never `root`, never a
  wildcard — so a run can authenticate only to *its own* proxy pods.
- A per-run **NetworkPolicy** locks each proxy pod's ingress to that run's Job. This is defense in
  depth *on top of* the certificate isolation, not the primary control.
- Proxy pods, their per-run Secrets, and the NetworkPolicy are **torn down when the run ends** —
  there is no standing SSH surface on your Nodes between runs.

## The privilege the proxy pods actually hold

Proxy pods take the **minimum** that makes `nsenter`-to-host work: `hostPID: true`, a host `/proc`
bind-mount, `CAP_SYS_ADMIN` + `CAP_SYS_PTRACE`, and (on SELinux nodes) the `spc_t` label. They
deliberately do **not** set `privileged: true`, `hostNetwork`, or `hostIPC`. Each is pinned to
exactly one Node. The image they run is the node-root supply-chain surface you must own — pin it to a
trusted digest (see [Deployment → the proxy image](./deployment.md#the-managed-ssh-proxy-image)).

## The blast radius you accept

Be clear-eyed about what a compromise of the operator (or of a tenant allowed to author a
`ClusterInventory`) can and cannot reach:

- **Bounded to enrolled namespaces.** The operator can read *and delete* Secrets, and create Jobs, in
  **every enrolled namespace** — but nowhere else. This is precisely why you should **enroll only
  namespaces dedicated to Ansible ops**, never general application namespaces: a dedicated namespace
  holds only Secrets that are already part of the Ansible trust boundary.
- **Bounded to policy-granted Nodes.** Even a fully forged request cannot reach a Node outside the
  intersection of the admin-authored policies.
- **No persistent node foothold from the mechanism itself.** Proxy infra is per-run and ephemeral,
  and the CA is in-memory and rotates on restart. (What a *playbook* does to a Node is, of course, up
  to the playbook — that is the tenant's power, gated by the two fences above.)

## Invariants (the load-bearing guarantees)

The properties above are pinned by seven invariants (INV-1…INV-7 in `THREAT_MODEL.md` §7), enforced
by unit tests. In brief: fail-closed selectors (INV-1); enforcement is intersection-only and can only
remove hosts (INV-2); it runs before any proxy infra, every reconcile (INV-3); cross-run isolation is
per-run cert principals (INV-4); the Node allow-set is a live read (INV-5); the CA private key never
leaves the process (INV-6); proxy pods are labelled so cleanup can never sweep the ansible Job pod
(INV-7). If you are modifying the operator, do not regress these without an explicit, deliberate
decision — they are the point of the design.
