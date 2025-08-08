# Ansible Operator
An opinionated runner for Ansible on Kubernetes, inspired by Rancher's [system-upgrade-controller](https://github.com/rancher/system-upgrade-controller).


## Features
- [x] **Dynamic node-based inventories**: Build inventories based on cluster-nodes' labels
- [x] **Static hostname-based inventories**: Build inventories with arbitrary hostnames or IPs
- [x] **Chroot-based node mutation**: When targeting a cluster node, a chroot can be used as an alternative to SSH (using a highly-privileged pod)
- [x] **Secrets as variables**: Kubernetes secrets can be used as Ansible variables
- [x] **Volumes as files**: Use [image volumes](https://kubernetes.io/docs/tasks/configure-pod-container/image-volumes/) to access blobs (e.g. binaries or archives) at runtime without extending the runtime image*
- [ ] **Time windows**: Ensure that playbooks only run at a certain time
- [ ] **Scheduling**: Embrace idempotency and repeat playbook executions based on a schedule

\* As of August 2025, image volumes are a beta feature of Kubernetes and not yet supported by all container runtimes.

## Example use cases
- Upgrade k3s on all cluster nodes
- Manage node-level configuration files
- Schedule operating system upgrades
- Export certificates created by cert-manager to external devices
