//! Startup configuration for the operator, read once from a TOML file that the Helm chart renders
//! into a ConfigMap and mounts at [`DEFAULT_CONFIG_PATH`]. The config is deliberately *not*
//! hot-reloaded: a change to the ConfigMap rolls the Deployment (via a `checksum/config` pod
//! annotation), so the new config is picked up by the restarted process. See `R1_PLAN.md`.

use std::collections::BTreeSet;

use serde::Deserialize;

/// Where the chart mounts the rendered config. Overridable via the `--config <path>` CLI flag for
/// local runs / tests.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/ansible-operator/config.toml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorConfig {
    /// Tenant namespaces the operator is enrolled to serve — the admin-authored allowlist that
    /// bounds where the operator may read/write Secrets and create Jobs (see R1 / T-INFO-1). The
    /// operator's own namespace is always enrolled implicitly (see [`Self::enrolled_namespaces`]),
    /// so this lists only the *additional* tenant namespaces.
    #[serde(default)]
    pub watch_namespaces: Vec<String>,

    /// Image for the managed-ssh proxy pods the operator schedules onto target nodes (the node-root
    /// primitive — see THREAT_MODEL T-ESC-5). `None` (field absent) falls back to the built-in
    /// `DEFAULT_PROXY_IMAGE`. Rendered by the Helm chart from `managedSsh.proxyImage` into the mounted
    /// ConfigMap; a change rolls the operator pod (via `checksum/config`) rather than hot-reloading,
    /// exactly like `watch_namespaces`. Accepts a digest-pinned reference (`repo@sha256:…`).
    #[serde(default)]
    pub proxy_image: Option<String>,

    /// How long the operator waits for a `NotReady` node's managed-ssh proxy pod to become Ready
    /// before treating the node as unreachable for the run (see `ProxyGracePolicy`). Rendered by the
    /// Helm chart from `managedSsh.readiness` into the `[managed_ssh]` table; absent ⇒ all defaults.
    #[serde(default)]
    pub managed_ssh: ManagedSshConfig,
}

/// The `[managed_ssh]` config table: tunables for the adaptive readiness gate. The base wait is
/// divided by `aggressiveness` at each successive heartbeat-age tier (`threshold_days`), so a node
/// that has been silent longer is given up on faster. Defaults reproduce a 600 → 300 → 150 → 0
/// (seconds) schedule at 3 / 7 / 30 day boundaries.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ManagedSshConfig {
    /// Full (tier-0) wait, in seconds, for a recently-alive node. Default 600.
    pub grace_seconds: i64,
    /// Divisor applied to the wait at each successive tier. Default 2; clamped to `>= 1` downstream.
    pub aggressiveness: u32,
    /// Three ascending heartbeat-age boundaries, in days. Past the last one the wait is 0.
    /// Default `[3, 7, 30]`.
    pub threshold_days: [i64; 3],
}

impl Default for ManagedSshConfig {
    fn default() -> Self {
        Self {
            grace_seconds: 600,
            aggressiveness: 2,
            threshold_days: [3, 7, 30],
        }
    }
}

impl OperatorConfig {
    /// Loads config from `path`. Fail-closed and loud:
    /// - a **missing** file is not an error — it yields an empty config, so the operator serves only
    ///   its own namespace (the safe default);
    /// - a **present but malformed** file is a hard error — a broken config must not silently widen
    ///   or narrow the enrollment set.
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents).map_err(|source| ConfigError::Parse {
                path: path.to_string(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_string(),
                source,
            }),
        }
    }

    /// The effective enrolled namespace set = the operator's own namespace ∪ the configured tenant
    /// namespaces. The operator namespace is always included so its managed-ssh cert Secrets, Leases
    /// and proxy pods remain reachable even when `watch_namespaces` is empty.
    pub fn enrolled_namespaces(&self, operator_namespace: &str) -> BTreeSet<String> {
        let mut set: BTreeSet<String> = self.watch_namespaces.iter().cloned().collect();
        set.insert(operator_namespace.to_string());
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_empty_config_so_only_the_operator_namespace_is_enrolled() {
        let config = OperatorConfig::load("/nonexistent/ansible-operator/config.toml").unwrap();
        assert!(config.watch_namespaces.is_empty());
        let enrolled = config.enrolled_namespaces("ansible-system");
        assert_eq!(enrolled, BTreeSet::from(["ansible-system".to_string()]));
    }

    #[test]
    fn enrolled_set_is_the_operator_namespace_unioned_with_watch_namespaces() {
        let config = OperatorConfig {
            watch_namespaces: vec!["team-a".to_string(), "team-b".to_string()],
            ..Default::default()
        };
        let enrolled = config.enrolled_namespaces("ansible-system");
        assert_eq!(
            enrolled,
            BTreeSet::from([
                "ansible-system".to_string(),
                "team-a".to_string(),
                "team-b".to_string(),
            ])
        );
    }

    #[test]
    fn proxy_image_is_optional_and_overridable() {
        // Absent -> None, so the caller falls back to DEFAULT_PROXY_IMAGE.
        let default: OperatorConfig = toml::from_str("watch_namespaces = []").unwrap();
        assert!(default.proxy_image.is_none());

        // A digest-pinned override round-trips verbatim (see THREAT_MODEL T-ESC-5 / R5).
        let overridden: OperatorConfig =
            toml::from_str("proxy_image = \"registry.example.com/sshd@sha256:abc\"").unwrap();
        assert_eq!(
            overridden.proxy_image.as_deref(),
            Some("registry.example.com/sshd@sha256:abc")
        );
    }

    #[test]
    fn managed_ssh_defaults_when_table_absent() {
        let config: OperatorConfig = toml::from_str("watch_namespaces = []").unwrap();
        assert_eq!(config.managed_ssh.grace_seconds, 600);
        assert_eq!(config.managed_ssh.aggressiveness, 2);
        assert_eq!(config.managed_ssh.threshold_days, [3, 7, 30]);
    }

    #[test]
    fn managed_ssh_table_round_trips_and_rejects_unknown_keys() {
        let overridden: OperatorConfig = toml::from_str(
            "[managed_ssh]\ngrace_seconds = 300\naggressiveness = 4\nthreshold_days = [1, 2, 5]\n",
        )
        .unwrap();
        assert_eq!(overridden.managed_ssh.grace_seconds, 300);
        assert_eq!(overridden.managed_ssh.aggressiveness, 4);
        assert_eq!(overridden.managed_ssh.threshold_days, [1, 2, 5]);

        // Unknown key under the table is rejected (deny_unknown_fields).
        assert!(
            toml::from_str::<OperatorConfig>("[managed_ssh]\nnope = 1\n").is_err(),
            "unknown [managed_ssh] key must be rejected"
        );
        // The three boundaries are enforced by the fixed-size array.
        assert!(
            toml::from_str::<OperatorConfig>("[managed_ssh]\nthreshold_days = [1, 2]\n").is_err(),
            "threshold_days of the wrong length must be rejected"
        );
    }

    #[test]
    fn malformed_toml_is_a_hard_error() {
        let dir = std::env::temp_dir().join("ansible-operator-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.toml");
        std::fs::write(&path, "watch_namespaces = \"not-a-list\"").unwrap();
        assert!(OperatorConfig::load(path.to_str().unwrap()).is_err());
        std::fs::remove_file(&path).ok();
    }
}
