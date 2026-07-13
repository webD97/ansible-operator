# Targeting external hosts

A `StaticInventory` targets hosts you name **literally** — hostnames or IPs — and reaches them over
ordinary SSH with a key **you supply**. Use it for anything that is not a Node of this cluster:
external servers, IoT and edge appliances, network gear, or the nodes of a different cluster.

Unlike a `ClusterInventory`, there is no managed-SSH proxy, no node-root elevation, and no
`NodeAccessPolicy` gating. The operator connects out as whatever user your key authorizes.

## Defining hosts

`spec.hosts` is a list of groups; each group has a `name` and a literal `hosts` list. The group name
becomes an Ansible group your playbook can target.

```yaml
apiVersion: ansible.cloudbending.dev/v1beta1
kind: StaticInventory
metadata:
  name: edge-appliances
spec:
  hosts:
    - name: webservers
      hosts:
        - server1.example.com
        - server2.example.com
        - 192.168.73.42
  ssh:
    user: root
    secretRef:
      name: ssh-key
```

## SSH credentials

`spec.ssh` is mandatory — a `StaticInventory` with no way to reach its hosts is not usable:

- `ssh.user` — the SSH login user (`ansible_user`).
- `ssh.secretRef.name` — a Kubernetes Secret **in the same namespace** holding the private key.

The referenced Secret is mounted read-only into the run and its keys are used as files:

- **`id_rsa`** (required) — the SSH **private key** to authenticate with. Despite the name it may be
  any key type OpenSSH accepts, e.g. Ed25519.
- **`known_hosts`** (optional) — an OpenSSH `known_hosts` file used to verify the hosts. Provide it
  to pin host keys; without it, host-key verification follows your image's SSH defaults.

Create the key Secret before the run, for example:

```sh
kubectl create secret generic ssh-key \
  --namespace my-team \
  --from-file=id_rsa=./id_ed25519 \
  --from-file=known_hosts=./known_hosts
```

Because the key lives in a Secret in the plan's namespace, changing it re-triggers affected plans
(the operator watches referenced Secrets), and rotating a key is just updating the Secret.

## Multiple inventories, multiple credentials

A single `PlaybookPlan` can reference several `StaticInventory`s, each with its **own** `ssh` block
and key Secret; they are mounted at distinct paths and do not collide. You can also mix
`StaticInventory` and `ClusterInventory` references in one plan; external hosts and cluster Nodes then
appear in the same rendered inventory and are applied by the same Job.

## What you do not set

As with cluster nodes, the operator renders `ansible_user`, `ansible_ssh_private_key_file`, and the
host-key options into the inventory for you from the `ssh` block. Do not set these in your playbook —
target `hosts: <group>` (or `all`) and let the operator wire up the connection.
