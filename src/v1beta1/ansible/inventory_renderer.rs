use std::collections::BTreeMap;

use serde_yaml::{Mapping, Value};

use crate::v1beta1::ResolvedInventoryGroup;

/// Connect timeout (seconds) rendered for a host we already know is unreachable — its proxy pod never
/// became Ready, so `pod_ip` is the unroutable sentinel. Kept low because the dial is certain to
/// fail; it only bounds how long Ansible waits to confirm that (vs. the 10s default × retries).
const UNREACHABLE_CONNECT_TIMEOUT_SECONDS: i64 = 5;

/// Resolved managed-ssh connection details for the hosts in this run, keyed by hostname — proxy
/// pod IP/port are only known once the proxy pods are Ready, so this is threaded in by the caller.
#[derive(Default)]
pub struct ManagedSshHostInfo {
    pub pod_ip: String,
    pub port: i32,
    /// The proxy pod never became Ready in time: `pod_ip` is the unroutable sentinel, and the host is
    /// rendered with a short connect timeout so Ansible fails fast and records it `unreachable`.
    pub unreachable: bool,
}

pub struct RenderContext<'a> {
    pub managed_ssh_hosts: &'a BTreeMap<String, ManagedSshHostInfo>,
    pub managed_ssh_client_key_path: &'a str,
    pub managed_ssh_known_hosts_path: &'a str,
    /// `StaticInventory` resource name -> (private key mount path, known_hosts mount path).
    /// Resolved by the caller (which owns the mount-path conventions in
    /// `controllers::playbookplancontroller::paths`) rather than computed here, so this module
    /// stays decoupled from controller-internal path conventions.
    pub ssh_paths_by_static_inventory: &'a BTreeMap<String, (String, String)>,
}

pub fn render_inventory(
    groups: &[ResolvedInventoryGroup],
    ctx: &RenderContext,
) -> Result<String, super::RenderError> {
    let mut yaml_inventory = Mapping::new();

    for group in groups.iter() {
        let hosts = group.hosts();
        let mut host_entries = Mapping::new();

        for hostname in &hosts.hosts {
            let vars = match group {
                ResolvedInventoryGroup::ManagedSsh { .. } => {
                    render_managed_ssh_host_vars(hostname, ctx)
                }
                ResolvedInventoryGroup::Ssh {
                    static_inventory_name,
                    config,
                    ..
                } => render_ssh_host_vars(static_inventory_name, config, ctx),
            };

            host_entries.insert(Value::String(hostname.into()), Value::Mapping(vars));
        }

        let mut yaml_group = Mapping::new();
        yaml_group.insert(Value::String("hosts".into()), Value::Mapping(host_entries));

        yaml_inventory.insert(
            Value::String(hosts.name.to_owned()),
            Value::Mapping(yaml_group),
        );
    }

    Ok(serde_yaml::to_string(&yaml_inventory)?)
}

fn render_managed_ssh_host_vars(hostname: &str, ctx: &RenderContext) -> Mapping {
    let mut vars = Mapping::new();

    if let Some(info) = ctx.managed_ssh_hosts.get(hostname) {
        vars.insert(
            Value::String("ansible_host".into()),
            Value::String(info.pod_ip.clone()),
        );
        vars.insert(
            Value::String("ansible_port".into()),
            Value::Number(info.port.into()),
        );
        // Known-unreachable host: fail the (doomed) dial to the sentinel fast instead of burning the
        // default connect timeout — Ansible then records it `unreachable`.
        if info.unreachable {
            vars.insert(
                Value::String("ansible_timeout".into()),
                Value::Number(UNREACHABLE_CONNECT_TIMEOUT_SECONDS.into()),
            );
        }
    }

    vars.insert(
        Value::String("ansible_ssh_private_key_file".into()),
        Value::String(ctx.managed_ssh_client_key_path.to_string()),
    );
    // ansible_host is the proxy pod's IP, but the host cert's principal (and the wildcard
    // @cert-authority known_hosts line) is the node's name — without HostKeyAlias, the SSH
    // client checks the cert/known_hosts entry against the dialed IP, not the node name, and
    // rejects with "Certificate invalid: name is not a listed principal" even though everything
    // else is correctly signed.
    vars.insert(
        Value::String("ansible_ssh_common_args".into()),
        Value::String(format!(
            "-o UserKnownHostsFile={} -o HostKeyAlias={hostname}",
            ctx.managed_ssh_known_hosts_path
        )),
    );

    vars
}

fn render_ssh_host_vars(
    static_inventory_name: &str,
    config: &crate::v1beta1::SshConfig,
    ctx: &RenderContext,
) -> Mapping {
    let mut vars = Mapping::new();
    vars.insert(
        Value::String("ansible_user".into()),
        Value::String(config.user.clone()),
    );

    if let Some((key_path, known_hosts_path)) =
        ctx.ssh_paths_by_static_inventory.get(static_inventory_name)
    {
        vars.insert(
            Value::String("ansible_ssh_private_key_file".into()),
            Value::String(key_path.clone()),
        );
        vars.insert(
            Value::String("ansible_ssh_common_args".into()),
            Value::String(format!("-o UserKnownHostsFile={known_hosts_path}")),
        );
    }

    vars
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1beta1::{ResolvedHosts, SecretRef, SshConfig};

    #[test]
    fn renders_managed_ssh_group_with_proxy_ip_and_cert_paths() {
        let group = ResolvedInventoryGroup::ManagedSsh {
            hosts: ResolvedHosts {
                name: "controlplanes".into(),
                hosts: vec!["worker-1".into()],
            },
            tolerations: None,
        };

        let mut managed_ssh_hosts = BTreeMap::new();
        managed_ssh_hosts.insert(
            "worker-1".to_string(),
            ManagedSshHostInfo {
                pod_ip: "10.0.0.5".into(),
                port: 22,
                unreachable: false,
            },
        );

        let ssh_paths = BTreeMap::new();
        let ctx = RenderContext {
            managed_ssh_hosts: &managed_ssh_hosts,
            managed_ssh_client_key_path: "/run/ansible-operator/managed-ssh/client_key",
            managed_ssh_known_hosts_path: "/run/ansible-operator/managed-ssh/known_hosts",
            ssh_paths_by_static_inventory: &ssh_paths,
        };

        let rendered = render_inventory(&[group], &ctx).unwrap();

        assert!(rendered.contains("ansible_host: 10.0.0.5"));
        assert!(rendered.contains("ansible_port: 22"));
        // A reachable host gets no connect-timeout override.
        assert!(!rendered.contains("ansible_timeout"));
        assert!(rendered.contains("client_key"));
        // The host cert's principal is the node name, not the proxy pod IP dialed via
        // ansible_host, so the SSH client needs HostKeyAlias to check the cert/known_hosts
        // entry against the right name.
        assert!(rendered.contains("-o HostKeyAlias=worker-1"));
    }

    #[test]
    fn renders_unreachable_host_with_sentinel_and_short_timeout() {
        let group = ResolvedInventoryGroup::ManagedSsh {
            hosts: ResolvedHosts {
                name: "controlplanes".into(),
                hosts: vec!["worker-9".into()],
            },
            tolerations: None,
        };

        let mut managed_ssh_hosts = BTreeMap::new();
        managed_ssh_hosts.insert(
            "worker-9".to_string(),
            ManagedSshHostInfo {
                pod_ip: "192.0.2.1".into(),
                port: 22,
                unreachable: true,
            },
        );

        let ssh_paths = BTreeMap::new();
        let ctx = RenderContext {
            managed_ssh_hosts: &managed_ssh_hosts,
            managed_ssh_client_key_path: "/run/ansible-operator/managed-ssh/client_key",
            managed_ssh_known_hosts_path: "/run/ansible-operator/managed-ssh/known_hosts",
            ssh_paths_by_static_inventory: &ssh_paths,
        };

        let rendered = render_inventory(&[group], &ctx).unwrap();

        // Dialed at the unroutable sentinel, with a short connect timeout so Ansible fails fast and
        // records it unreachable.
        assert!(rendered.contains("ansible_host: 192.0.2.1"));
        assert!(rendered.contains("ansible_timeout: 5"));
    }

    #[test]
    fn renders_ssh_group_from_static_inventorys_own_config() {
        let group = ResolvedInventoryGroup::Ssh {
            hosts: ResolvedHosts {
                name: "external-devices".into(),
                hosts: vec!["ccu.fritz.box".into()],
            },
            static_inventory_name: "ccu".into(),
            config: SshConfig {
                user: "root".into(),
                secret_ref: SecretRef {
                    name: "ssh-key".into(),
                },
            },
        };

        let managed_ssh_hosts = BTreeMap::new();
        let mut ssh_paths = BTreeMap::new();
        ssh_paths.insert(
            "ccu".to_string(),
            (
                "/run/ansible-operator/ssh/ccu/id_rsa".to_string(),
                "/run/ansible-operator/ssh/ccu/known_hosts".to_string(),
            ),
        );
        let ctx = RenderContext {
            managed_ssh_hosts: &managed_ssh_hosts,
            managed_ssh_client_key_path: "unused",
            managed_ssh_known_hosts_path: "unused",
            ssh_paths_by_static_inventory: &ssh_paths,
        };

        let rendered = render_inventory(&[group], &ctx).unwrap();

        assert!(rendered.contains("ansible_user: root"));
        assert!(rendered.contains("/run/ansible-operator/ssh/ccu/id_rsa"));
    }

    #[test]
    fn mixed_run_renders_both_groups_without_cross_contamination() {
        let managed = ResolvedInventoryGroup::ManagedSsh {
            hosts: ResolvedHosts {
                name: "controlplanes".into(),
                hosts: vec!["worker-1".into()],
            },
            tolerations: None,
        };
        let ssh = ResolvedInventoryGroup::Ssh {
            hosts: ResolvedHosts {
                name: "external-devices".into(),
                hosts: vec!["ccu.fritz.box".into()],
            },
            static_inventory_name: "ccu".into(),
            config: SshConfig {
                user: "root".into(),
                secret_ref: SecretRef {
                    name: "ssh-key".into(),
                },
            },
        };

        let managed_ssh_hosts = BTreeMap::new();
        let mut ssh_paths = BTreeMap::new();
        ssh_paths.insert(
            "ccu".to_string(),
            (
                "/run/ansible-operator/ssh/ccu/id_rsa".to_string(),
                "/run/ansible-operator/ssh/ccu/known_hosts".to_string(),
            ),
        );
        let ctx = RenderContext {
            managed_ssh_hosts: &managed_ssh_hosts,
            managed_ssh_client_key_path: "/run/ansible-operator/managed-ssh/client_key",
            managed_ssh_known_hosts_path: "/run/ansible-operator/managed-ssh/known_hosts",
            ssh_paths_by_static_inventory: &ssh_paths,
        };

        let rendered = render_inventory(&[managed, ssh], &ctx).unwrap();

        assert!(rendered.contains("controlplanes"));
        assert!(rendered.contains("external-devices"));
        assert!(rendered.contains("ansible_user: root"));
        assert!(rendered.contains("/run/ansible-operator/ssh/ccu/id_rsa"));
    }
}
