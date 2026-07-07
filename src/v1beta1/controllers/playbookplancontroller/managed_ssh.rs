use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use k8s_openapi::{
    api::{
        core::v1::{
            Capabilities, Container, HostPathVolumeSource, Pod, PodSpec, Probe, Secret,
            SecurityContext, TCPSocketAction, Volume, VolumeMount,
        },
        networking::v1::{
            NetworkPolicy, NetworkPolicyIngressRule, NetworkPolicyPeer, NetworkPolicyPort,
            NetworkPolicySpec,
        },
    },
    apimachinery::pkg::{
        apis::meta::v1::{LabelSelector, ObjectMeta},
        util::intstr::IntOrString,
    },
};
use kube::{Api, api::PostParams};

use super::paths;
use crate::{
    utils,
    v1beta1::{
        ca::CertificateAuthority,
        controllers::{
            playbookplancontroller::execution_evaluator::ExecutionHash,
            reconcile_error::ReconcileError,
        },
        labels,
        resources::Toleration,
    },
};

/// Default sshd image for managed-ssh proxy pods, overridable via the Helm chart's `values.yaml`.
pub const DEFAULT_PROXY_IMAGE: &str = "testcontainers/sshd:latest";
pub const PROXY_SSH_PORT: i32 = 22;

const SSHD_CONFIG_MOUNT_PATH: &str = "/etc/ansible-operator-sshd";
const HOST_KEY_FILENAME: &str = "ssh_host_ed25519_key";
const HOST_CERT_FILENAME: &str = "ssh_host_ed25519_key-cert.pub";
const CA_PUB_FILENAME: &str = "ca.pub";
const ENTER_HOST_SCRIPT_FILENAME: &str = "enter-host.sh";

/// Placeholder value for the `Subsystem sftp` directive, never executed as a binary. Without a
/// `Subsystem sftp` line, sshd rejects sftp requests before `ForceCommand` ever runs; declaring
/// one (even a nonsense one) is what makes sshd hand the request to `ForceCommand` instead, which
/// checks `$SSH_ORIGINAL_COMMAND` against this marker in `render_enter_host_script`.
const SFTP_SUBSYSTEM_MARKER: &str = "ansible-operator-sftp";

/// Where the host's real `/proc` is bind-mounted inside the proxy pod. The pod runs with ordinary
/// pod networking (no `hostNetwork`/`hostIPC`/`privileged`), so sshd binds port 22 in its own
/// namespace rather than colliding with the node's real sshd; each *session* instead nsenters into
/// the host's mount/net/ipc/uts namespaces via `/host/proc/1/ns/*` — see
/// `render_enter_host_script`. This also keeps the NetworkPolicy in `build_network_policy`
/// enforceable, since most CNIs don't apply NetworkPolicy to `hostNetwork` pods.
///
/// `hostPID` is still required though: `setns(CLONE_NEWPID)` can only move to a *descendant* PID
/// namespace, never the host's (an ancestor), so a session can't join it via nsenter — the pod's
/// PID namespace has to start out as the host's.
const HOST_PROC_MOUNT_PATH: &str = "/host/proc";

pub struct ProxyPodInfo {
    pub host: String,
    pub pod_ip: String,
    pub port: i32,
}

pub enum ProxyReadiness {
    AllReady(Vec<ProxyPodInfo>),
    Pending,
}

/// Deterministic, human-readable resource name for a (host, run) pair. The host is used verbatim
/// (not hashed) since managed-ssh only targets `ClusterInventory` hosts, i.e. real Node names,
/// which are already valid Kubernetes object name components. The run uses `utils::generate_id`'s
/// short-id, matching `job_builder::create_job_for_run`'s Job naming.
fn resource_name(host: &str, execution_hash: &ExecutionHash) -> String {
    format!("ansible-sshd-{host}-{}", utils::generate_id(**execution_hash))
}

/// Name of this run's client-cert Secret, shared by `job_builder`'s mount and `ensure_client_cert`.
pub fn client_cert_secret_name(execution_hash: &ExecutionHash) -> String {
    format!("managed-ssh-client-{execution_hash}")
}

fn run_labels(execution_hash: &ExecutionHash, host: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (labels::PLAYBOOKPLAN_HASH.to_string(), execution_hash.to_string()),
        (labels::PLAYBOOKPLAN_HOST.to_string(), host.to_string()),
    ])
}

/// `ForceCommand` routes every session through `enter-host.sh` rather than `ChrootDirectory` —
/// nsenter-ing the host's mount namespace already makes `/` the host's real root, so no chroot
/// step is needed. `UsePAM` is omitted: some minimal sshd builds reject it outright (no PAM
/// support), and auth here is pubkey/cert-only anyway.
fn render_sshd_config() -> String {
    format!(
        "Port {PROXY_SSH_PORT}\n\
         HostKey {SSHD_CONFIG_MOUNT_PATH}/{HOST_KEY_FILENAME}\n\
         HostCertificate {SSHD_CONFIG_MOUNT_PATH}/{HOST_CERT_FILENAME}\n\
         TrustedUserCAKeys {SSHD_CONFIG_MOUNT_PATH}/{CA_PUB_FILENAME}\n\
         ForceCommand {SSHD_CONFIG_MOUNT_PATH}/{ENTER_HOST_SCRIPT_FILENAME}\n\
         PermitRootLogin yes\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         Subsystem sftp {SFTP_SUBSYSTEM_MARKER}\n"
    )
}

/// Wraps every SSH session in an `nsenter` into the host's mount/net/ipc/uts namespaces via the
/// bind-mounted `/host/proc/1/ns/*`. Requires `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE` on the container
/// (see `build_pod`'s `SecurityContext`), not `privileged: true`.
///
/// No `-p`/pid join: `setns(CLONE_NEWPID)` can only move to a descendant PID namespace, never the
/// host's (an ancestor); `build_pod` sets `hostPID: true` instead.
///
/// Flags use the glued short-option form (`-m"$NS/mnt"`, no `=`) rather than `--mount=` — BusyBox's
/// `nsenter` (the default proxy image, `testcontainers/sshd`) doesn't parse the long form at all
/// and fails silently. The glued short form also works against genuine util-linux `nsenter`.
///
/// Special-cases sftp: `ForceCommand` overrides `Subsystem sftp` requests the same way it does
/// shell/exec, setting `$SSH_ORIGINAL_COMMAND` to `SFTP_SUBSYSTEM_MARKER`. Since there's no
/// portable path for the `sftp-server` binary across distros, this tries the common ones on the
/// target host's filesystem and execs whichever exists.
fn render_enter_host_script() -> String {
    format!(
        "#!/bin/sh\n\
         set -e\n\
         NS={HOST_PROC_MOUNT_PATH}/1/ns\n\
         if [ \"$SSH_ORIGINAL_COMMAND\" = \"{SFTP_SUBSYSTEM_MARKER}\" ]; then\n\
         \texec nsenter -m\"$NS/mnt\" -n\"$NS/net\" -i\"$NS/ipc\" -u\"$NS/uts\" -- sh -c '\n\
         \t\tfor c in /usr/lib/openssh/sftp-server /usr/libexec/openssh/sftp-server /usr/lib/ssh/sftp-server /usr/lib/misc/sftp-server /usr/lib64/misc/sftp-server /usr/lib64/openssh/sftp-server; do\n\
         \t\t\t[ -x \"$c\" ] && exec \"$c\"\n\
         \t\tdone\n\
         \t\techo \"no sftp-server binary found on target host\" >&2\n\
         \t\texit 1\n\
         \t'\n\
         elif [ -n \"$SSH_ORIGINAL_COMMAND\" ]; then\n\
         \texec nsenter -m\"$NS/mnt\" -n\"$NS/net\" -i\"$NS/ipc\" -u\"$NS/uts\" -- sh -c \"$SSH_ORIGINAL_COMMAND\"\n\
         else\n\
         \texec nsenter -m\"$NS/mnt\" -n\"$NS/net\" -i\"$NS/ipc\" -u\"$NS/uts\" -- sh\n\
         fi\n"
    )
}

/// Builds the per-host Secret carrying the proxy pod's sshd host key/cert (generated by the
/// operator, not the pod, so there's no need to wait for a key to be reported back), the CA
/// public key, the rendered sshd_config, and the nsenter entry script.
fn build_secret(
    name: &str,
    execution_hash: &ExecutionHash,
    host: &str,
    ca: &CertificateAuthority,
) -> Result<Secret, ReconcileError> {
    let host_key = crate::v1beta1::ca::generate_ephemeral_keypair()?;
    let host_cert = ca.sign_host_cert(host_key.public_key(), host)?;
    let ca_pub = ca.public_key_openssh()?;

    let host_key_openssh = host_key
        .to_openssh(ssh_key::LineEnding::LF)
        .map_err(crate::v1beta1::ca::CaError::from)?
        .to_string();

    let mut string_data = BTreeMap::new();
    string_data.insert(HOST_KEY_FILENAME.to_string(), host_key_openssh);
    string_data.insert(HOST_CERT_FILENAME.to_string(), host_cert);
    string_data.insert(CA_PUB_FILENAME.to_string(), ca_pub);
    string_data.insert("sshd_config".to_string(), render_sshd_config());
    string_data.insert(
        ENTER_HOST_SCRIPT_FILENAME.to_string(),
        render_enter_host_script(),
    );

    Ok(Secret {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(run_labels(execution_hash, host)),
            ..Default::default()
        },
        string_data: Some(string_data),
        ..Default::default()
    })
}

fn build_pod(
    name: &str,
    secret_name: &str,
    execution_hash: &ExecutionHash,
    host: &str,
    tolerations: Option<&[Toleration]>,
) -> Pod {
    let secret_volume = Volume {
        name: "sshd-config".into(),
        secret: Some(k8s_openapi::api::core::v1::SecretVolumeSource {
            secret_name: Some(secret_name.to_string()),
            // 0500 not 0400 — the entry script needs to be executable; sshd's host-key
            // permission check only cares about group/world access, which stays closed.
            default_mode: Some(0o0500),
            ..Default::default()
        }),
        ..Default::default()
    };

    let host_proc_volume = Volume {
        name: "host-proc".into(),
        host_path: Some(HostPathVolumeSource {
            type_: Some("Directory".into()),
            path: "/proc".into(),
        }),
        ..Default::default()
    };

    let container = Container {
        name: "sshd".into(),
        image: Some(DEFAULT_PROXY_IMAGE.into()),
        command: Some(vec![
            "/usr/sbin/sshd".into(),
            "-D".into(),
            "-e".into(),
            "-f".into(),
            format!("{SSHD_CONFIG_MOUNT_PATH}/sshd_config"),
        ]),
        volume_mounts: Some(vec![
            VolumeMount {
                name: "sshd-config".into(),
                mount_path: SSHD_CONFIG_MOUNT_PATH.into(),
                read_only: Some(true),
                ..Default::default()
            },
            VolumeMount {
                name: "host-proc".into(),
                mount_path: HOST_PROC_MOUNT_PATH.into(),
                read_only: Some(true),
                ..Default::default()
            },
        ]),
        security_context: Some(SecurityContext {
            // Not `privileged: true` — only the two capabilities nsenter needs.
            capabilities: Some(Capabilities {
                add: Some(vec!["SYS_ADMIN".into(), "SYS_PTRACE".into()]),
                ..Default::default()
            }),
            // nsenter-ing the host's mount namespace doesn't change the process's SELinux label
            // (stays `container_t`, denied host filesystem access). `spc_t` is the same label
            // `privileged: true` pods and `oc debug node/...` get, and is what actually allows
            // host filesystem access. No-op on non-SELinux nodes.
            se_linux_options: Some(k8s_openapi::api::core::v1::SELinuxOptions {
                type_: Some("spc_t".into()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        readiness_probe: Some(Probe {
            tcp_socket: Some(TCPSocketAction {
                port: IntOrString::Int(PROXY_SSH_PORT),
                ..Default::default()
            }),
            period_seconds: Some(2),
            ..Default::default()
        }),
        ..Default::default()
    };

    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(run_labels(execution_hash, host)),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            volumes: Some(vec![secret_volume, host_proc_volume]),
            restart_policy: Some("Never".into()),
            // Required: unlike the other host namespaces, PID can't be joined per-session via
            // nsenter (see HOST_PROC_MOUNT_PATH doc), so it must be shared from pod creation.
            host_pid: Some(true),
            node_selector: Some(BTreeMap::from([(
                "kubernetes.io/hostname".into(),
                host.into(),
            )])),
            tolerations: tolerations
                .map(|ts| ts.iter().map(|t| t.clone().into()).collect()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// NetworkPolicy restricting ingress on this run's proxy pods to only the ansible Job pod for
/// this run. Needs both a podSelector and a namespaceSelector (via `kubernetes.io/metadata.name`)
/// since the policy lives in the operator's namespace but the Job pod lives in the plan's —
/// a bare podSelector alone would match nothing. Requires a NetworkPolicy-enforcing CNI.
fn build_network_policy(
    name: &str,
    execution_hash: &ExecutionHash,
    job_namespace: &str,
) -> NetworkPolicy {
    NetworkPolicy {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(BTreeMap::from([(
                labels::PLAYBOOKPLAN_HASH.to_string(),
                execution_hash.to_string(),
            )])),
            ..Default::default()
        },
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    labels::PLAYBOOKPLAN_HASH.to_string(),
                    execution_hash.to_string(),
                )])),
                ..Default::default()
            }),
            policy_types: Some(vec!["Ingress".into()]),
            ingress: Some(vec![NetworkPolicyIngressRule {
                from: Some(vec![NetworkPolicyPeer {
                    namespace_selector: Some(LabelSelector {
                        match_labels: Some(BTreeMap::from([(
                            "kubernetes.io/metadata.name".to_string(),
                            job_namespace.to_string(),
                        )])),
                        ..Default::default()
                    }),
                    pod_selector: Some(LabelSelector {
                        match_labels: Some(BTreeMap::from([(
                            labels::PLAYBOOKPLAN_HASH.to_string(),
                            execution_hash.to_string(),
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ports: Some(vec![NetworkPolicyPort {
                    port: Some(IntOrString::Int(PROXY_SSH_PORT)),
                    protocol: Some("TCP".into()),
                    ..Default::default()
                }]),
            }]),
            ..Default::default()
        }),
    }
}

/// Ensures this run's client-cert Secret exists — one client identity trusted by every proxy pod
/// via the CA, not per-host `authorized_keys`. Idempotent.
async fn ensure_client_cert(
    secrets_api: &Api<Secret>,
    execution_hash: &ExecutionHash,
    ca: &CertificateAuthority,
) -> Result<(), ReconcileError> {
    let name = client_cert_secret_name(execution_hash);

    if secrets_api.get_opt(&name).await?.is_some() {
        return Ok(());
    }

    let client_key = crate::v1beta1::ca::generate_ephemeral_keypair()?;
    let principal = execution_hash.to_string();
    // "root" must be a principal to match `PermitRootLogin yes` (sshd requires the connecting
    // username to be in the cert's principal list); the execution-hash is just an identity marker.
    let client_cert = ca.sign_client_cert(client_key.public_key(), &["root", &principal])?;
    let ca_pub = ca.public_key_openssh()?;

    let client_key_openssh = client_key
        .to_openssh(ssh_key::LineEnding::LF)
        .map_err(crate::v1beta1::ca::CaError::from)?
        .to_string();

    let mut string_data = BTreeMap::new();
    string_data.insert(
        paths::MANAGED_SSH_CLIENT_KEY_FILENAME.to_string(),
        client_key_openssh,
    );
    string_data.insert(
        paths::MANAGED_SSH_CLIENT_CERT_FILENAME.to_string(),
        client_cert,
    );
    string_data.insert(
        paths::MANAGED_SSH_KNOWN_HOSTS_FILENAME.to_string(),
        format!("@cert-authority * {ca_pub}"),
    );

    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(name),
            labels: Some(BTreeMap::from([(
                labels::PLAYBOOKPLAN_HASH.to_string(),
                execution_hash.to_string(),
            )])),
            ..Default::default()
        },
        string_data: Some(string_data),
        ..Default::default()
    };

    secrets_api.create(&PostParams::default(), &secret).await?;

    Ok(())
}

/// Ensures a proxy pod (+ its Secret + the run's NetworkPolicy) exists and is Ready for every
/// host in `hosts`. Safe to call every reconcile tick — only missing pieces are created.
pub async fn ensure_proxy_infra(
    client: &kube::Client,
    operator_namespace: &str,
    job_namespace: &str,
    execution_hash: &ExecutionHash,
    hosts: &[String],
    tolerations: Option<&[Toleration]>,
    ca: &CertificateAuthority,
) -> Result<ProxyReadiness, ReconcileError> {
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), operator_namespace);
    let secrets_api: Api<Secret> = Api::namespaced(client.clone(), operator_namespace);
    let netpol_api: Api<NetworkPolicy> = Api::namespaced(client.clone(), operator_namespace);

    if !hosts.is_empty() {
        let netpol_name = format!("managed-ssh-{:x}", {
            let mut hasher = twox_hash::XxHash3_64::new();
            execution_hash.to_string().hash(&mut hasher);
            hasher.finish()
        });
        if netpol_api.get_opt(&netpol_name).await?.is_none() {
            let netpol = build_network_policy(&netpol_name, execution_hash, job_namespace);
            netpol_api.create(&PostParams::default(), &netpol).await?;
        }

        ensure_client_cert(&secrets_api, execution_hash, ca).await?;
    }

    let mut ready = Vec::new();
    let mut all_ready = true;

    for host in hosts {
        let name = resource_name(host, execution_hash);

        if secrets_api.get_opt(&name).await?.is_none() {
            let secret = build_secret(&name, execution_hash, host, ca)?;
            secrets_api.create(&PostParams::default(), &secret).await?;
        }

        let pod = match pods_api.get_opt(&name).await? {
            Some(pod) => pod,
            None => {
                let pod = build_pod(&name, &name, execution_hash, host, tolerations);
                pods_api.create(&PostParams::default(), &pod).await?
            }
        };

        let pod_ready = pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .map(|conditions| {
                conditions
                    .iter()
                    .any(|c| c.type_ == "Ready" && c.status == "True")
            })
            .unwrap_or(false);

        let pod_ip = pod.status.as_ref().and_then(|s| s.pod_ip.clone());

        match (pod_ready, pod_ip) {
            (true, Some(ip)) => ready.push(ProxyPodInfo {
                host: host.clone(),
                pod_ip: ip,
                port: PROXY_SSH_PORT,
            }),
            _ => all_ready = false,
        }
    }

    Ok(if all_ready {
        ProxyReadiness::AllReady(ready)
    } else {
        ProxyReadiness::Pending
    })
}

/// Deletes every proxy pod/Secret/NetworkPolicy belonging to this run. Not reliant on
/// ownerReferences, since Kubernetes GC doesn't act on references that cross namespaces (these
/// live in the operator's namespace, the Job/PlaybookPlan live in the target namespace).
pub async fn cleanup_proxy_infra(
    client: &kube::Client,
    operator_namespace: &str,
    execution_hash: &ExecutionHash,
    hosts: &[String],
) -> Result<(), ReconcileError> {
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), operator_namespace);
    let secrets_api: Api<Secret> = Api::namespaced(client.clone(), operator_namespace);
    let netpol_api: Api<NetworkPolicy> = Api::namespaced(client.clone(), operator_namespace);

    for host in hosts {
        let name = resource_name(host, execution_hash);
        let _ = pods_api.delete(&name, &Default::default()).await;
        let _ = secrets_api.delete(&name, &Default::default()).await;
    }

    let netpol_name = format!("managed-ssh-{:x}", {
        let mut hasher = twox_hash::XxHash3_64::new();
        execution_hash.to_string().hash(&mut hasher);
        hasher.finish()
    });
    let _ = netpol_api.delete(&netpol_name, &Default::default()).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_name_is_deterministic_per_host_and_run() {
        use crate::v1beta1::controllers::playbookplancontroller::execution_evaluator::calculate_execution_hash;

        let hash_a = calculate_execution_hash("playbook-a", std::iter::empty());
        let hash_b = calculate_execution_hash("playbook-b", std::iter::empty());

        let a1 = resource_name("worker-1", &hash_a);
        let a2 = resource_name("worker-1", &hash_a);
        let b = resource_name("worker-1", &hash_b);
        let other_host = resource_name("worker-2", &hash_a);

        assert_eq!(a1, a2);
        assert_ne!(a1, b, "same host, different run must differ");
        assert_ne!(a1, other_host, "different host, same run must differ");
        assert_eq!(
            a1,
            format!("ansible-sshd-worker-1-{}", utils::generate_id(*hash_a))
        );
    }

    #[test]
    fn sshd_config_forces_the_enter_host_script_and_has_no_pam_directive() {
        let config = render_sshd_config();
        assert!(config.contains(&format!("ForceCommand {SSHD_CONFIG_MOUNT_PATH}/{ENTER_HOST_SCRIPT_FILENAME}")));
        assert!(config.contains("TrustedUserCAKeys"));
        // HostCertificate isn't auto-discovered from the HostKey filename — omitting it makes
        // sshd present a bare key, failing host-key verification for `@cert-authority` clients.
        assert!(config.contains(&format!("HostCertificate {SSHD_CONFIG_MOUNT_PATH}/{HOST_CERT_FILENAME}")));
        assert!(!config.contains("ChrootDirectory"));
        assert!(!config.contains("UsePAM"));
        // Without this line sshd rejects the sftp subsystem before ForceCommand ever runs.
        assert!(config.contains(&format!("Subsystem sftp {SFTP_SUBSYSTEM_MARKER}")));
    }

    #[test]
    fn enter_host_script_nsenters_via_host_proc_and_handles_both_command_forms() {
        let script = render_enter_host_script();
        assert!(script.contains(&format!("{HOST_PROC_MOUNT_PATH}/1/ns")));
        // Glued short-option form, not `--mount=`/etc — BusyBox's nsenter doesn't parse the long form.
        assert!(script.contains("-m\"$NS/mnt\""));
        assert!(script.contains("-n\"$NS/net\""));
        assert!(script.contains("-i\"$NS/ipc\""));
        assert!(script.contains("-u\"$NS/uts\""));
        // No `-p`/pid join — hostPID: true on the PodSpec covers this instead.
        assert!(!script.contains("-p\""));
        assert!(script.contains("SSH_ORIGINAL_COMMAND"));
    }

    #[test]
    fn enter_host_script_recognizes_sftp_marker_and_searches_common_server_paths() {
        let script = render_enter_host_script();
        assert!(script.contains(&format!("\"$SSH_ORIGINAL_COMMAND\" = \"{SFTP_SUBSYSTEM_MARKER}\"")));
        for candidate in [
            "/usr/lib/openssh/sftp-server",
            "/usr/libexec/openssh/sftp-server",
            "/usr/lib/ssh/sftp-server",
            "/usr/lib/misc/sftp-server",
            "/usr/lib64/misc/sftp-server",
            "/usr/lib64/openssh/sftp-server",
        ] {
            assert!(script.contains(candidate), "missing candidate path {candidate}");
        }
    }
}
