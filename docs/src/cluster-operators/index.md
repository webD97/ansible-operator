# For cluster operators

This chapter is for the **cluster administrators** who install, secure, and run the operator — as
opposed to the tenants who author playbooks (that is [Running playbooks](../running-playbooks/index.md)).
Read it before installing: the operator is a **cluster-privileged, node-root primitive**, and a few
of its controls are fail-closed by default in ways you must deliberately open up (namespace
enrollment, node access).

## Your responsibilities in one place

- **Install** the Helm chart into a dedicated, `privileged`-labelled namespace, and pin the
  managed-SSH proxy image. → [Deployment](./deployment.md)
- **Enroll** the tenant namespaces that are allowed to run playbooks (fail-closed: a plan in an
  un-enrolled namespace is refused). → [Deployment → enrolled namespaces](./deployment.md#enrolled-namespaces)
- **Grant node access** with `NodeAccessPolicy` resources (fail-closed: no policy ⇒ a namespace can
  reach no Nodes). → [Node access policies](./node-access-policies.md)
- **Understand the trust boundaries** you are taking on, and the invariants that hold them. →
  [Security model](./security.md)

## The source of truth

This chapter is an operator-facing summary. The exhaustive, adversarial analysis — threats,
mitigations, and the numbered invariants (INV-1…INV-7) — lives in the repository's
`THREAT_MODEL.md`. Where this guide and `THREAT_MODEL.md` appear to disagree, the threat model wins;
treat any discrepancy as a docs bug to fix.

## In this chapter

- [Deployment](./deployment.md) — install, namespaces, Pod Security Admission, SELinux, the proxy image, config
- [Security model](./security.md) — node-root, the ephemeral CA, per-run isolation, the blast radius you accept
- [Node access policies](./node-access-policies.md) — capping which Nodes a namespace may reach
