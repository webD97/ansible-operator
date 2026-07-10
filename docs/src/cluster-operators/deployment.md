# Deployment

The operator ships as a Helm chart under `chart/`. This page covers installing it, the namespace and
Pod-Security requirements it imposes, the managed-SSH proxy image you must choose, and the two
fail-closed knobs — namespace enrollment and node access — you have to open deliberately.

## Install

Install into its **own dedicated namespace**:

```sh
helm install --create-namespace -n ansible-system ansible-operator ./chart
```

Do **not** create `PlaybookPlan`s or inventories in the operator's own namespace — those belong in
tenant namespaces. The operator namespace is where its runtime machinery lives: per-run Leases, the
managed-SSH proxy pods/Secrets/NetworkPolicies, and the admin-authored `NodeAccessPolicy` objects.
Keeping it separate means only this one namespace needs the privileged-pod exception below.

## Pod Security Admission

Managed-SSH proxy pods (created dynamically by the operator at runtime, not by the chart) run with
`hostPID: true` and added `SYS_ADMIN`/`SYS_PTRACE` capabilities so each SSH session can `nsenter`
into the target Node's namespaces. That combination is only permitted under the **`privileged`** Pod
Security Standard, so the operator's namespace must carry the label:

```sh
kubectl label namespace ansible-system pod-security.kubernetes.io/enforce=privileged
```

Note the proxy pods deliberately do *not* use `privileged: true`, `hostNetwork`, or `hostIPC` — only
`hostPID` plus the two capabilities. `hostPID` specifically cannot be avoided: `setns()` into the
host PID namespace is impossible from a pod that is not already in it, so there is no
capability-scoped workaround for that one flag (which is why `baseline`/`restricted` are not enough).
Because this exception is scoped to the single operator namespace, tenant namespaces need no
Pod-Security relaxation.

## SELinux-enforcing nodes

On SELinux-enforcing Nodes the proxy pods additionally set
`securityContext.seLinuxOptions.type: spc_t` ("super-privileged container"). Joining the host's mount
namespace via `nsenter` does not relabel the process — it keeps whatever label the runtime assigned
(typically `container_t`), which is denied write to almost all host paths regardless of Unix
permissions or capabilities. `spc_t` is the same label `privileged: true` pods and node-debug tools
get, and is what actually lets the `nsenter`'d process touch the host filesystem. This is a no-op on
non-SELinux nodes and needs no action from you.

## The managed-SSH proxy image

Cluster-node access needs a **real OpenSSH `sshd`** image for the proxy pods; the operator's own
image is distroless and cannot serve this role. It is configured via the chart's
`managedSsh.proxyImage`.

**This is a node-root pod, so treat the image as node-root supply chain.** The chart default is a
third-party `:latest` tag and is **not** digest-pinned — fine for a kick-the-tyres install, not for
production. In production, override it with an image from a registry you trust and **pin it to a
digest**:

```yaml
# values.yaml
managedSsh:
  proxyImage:
    repository: my-registry.example.com/sshd@sha256:<digest>
    tag: ""
```

The value is rendered into the operator's config and consumed at pod-build time; changing it rolls
the operator (via a `checksum/config` annotation) rather than hot-reloading.

## Enrolled namespaces

The operator's cluster-wide RBAC intentionally does **not** include `secrets`, `jobs`, or `pods`.
Those verbs are granted per-namespace, only for **enrolled** namespaces, via a `Role`/`RoleBinding`
the chart renders. The enrolled set is the operator's own namespace ∪ the chart's `watchNamespaces`:

```yaml
# values.yaml
watchNamespaces:
  - team-a
  - team-b
```

A `PlaybookPlan` created in a namespace that is **not** enrolled is refused with
`status.phase = UnauthorizedNamespace` — before any Secret is read or Job created. There is
deliberately **no "all namespaces" escape hatch**: this allowlist is the boundary that bounds an
operator compromise to the enrolled namespaces instead of the whole cluster.

Two consequences to plan for:

- **Enrolling is an admin action that requires a restart.** The config is read once at startup;
  editing `watchNamespaces` and running `helm upgrade` rolls the operator so it re-reads the set. It
  is not hot-reloaded. (The same is true of `managedSsh.proxyImage`.)
- **The operator can read *and delete* Secrets in every enrolled namespace.** Enroll only namespaces
  **dedicated to Ansible ops**, not general-purpose application namespaces, so that this power covers
  as few unrelated Secrets as possible. See
  [Security model → the blast radius you accept](./security.md#the-blast-radius-you-accept).

Under the hood this is driven by a small TOML config (`watch_namespaces`, `proxy_image`) that the
chart renders into a mounted ConfigMap. For local development you can point the binary at a config
file directly with `run --config <path>` and set `POD_NAMESPACE` (the operator's own namespace,
always enrolled).

## Custom Resource Definitions

The chart bundles the four CRDs (`PlaybookPlan`, `ClusterInventory`, `StaticInventory`,
`NodeAccessPolicy`) under `crds/`. Following Helm's convention, `crds/` is install-only and is
**not** upgraded by `helm upgrade`; when the CRDs change between versions, apply them manually:

```sh
kubectl apply -f chart/crds/
```

The bundled manifests are a static snapshot generated from the binary itself
(`ansible-operator crds`); the regeneration procedure lives in `chart/README.md`.

## Grant node access

Installing the operator and enrolling a namespace is **not** enough for cluster-node playbooks: node
access is itself fail-closed. Until you author a `NodeAccessPolicy`, every namespace resolves to
**zero** Nodes and managed-SSH plans target nothing. Continue at
[Node access policies](./node-access-policies.md).
