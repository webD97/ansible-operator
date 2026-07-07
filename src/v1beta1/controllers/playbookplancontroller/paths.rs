//! Mount-path conventions shared between `inventory_renderer.rs` (which needs to render
//! `ansible_ssh_private_key_file`/etc. inventory vars pointing at these paths) and `job_builder.rs`
//! (which actually mounts the Secrets at these paths). Centralized here so the two can't drift.

/// Base directory the workspace secret (playbook.yml/inventory.yml/callback plugin/etc.) is
/// already mounted at.
pub const WORKSPACE_MOUNT_PATH: &str = "/run/ansible-operator";

/// Directory holding this run's managed-ssh client identity (one client cert/key per run,
/// trusted by every proxy pod that run via the CA — not per-host).
pub const MANAGED_SSH_CLIENT_DIR: &str = "/run/ansible-operator/managed-ssh";
pub const MANAGED_SSH_CLIENT_KEY_FILENAME: &str = "client_key";
pub const MANAGED_SSH_CLIENT_CERT_FILENAME: &str = "client_key-cert.pub";
pub const MANAGED_SSH_KNOWN_HOSTS_FILENAME: &str = "known_hosts";

pub fn managed_ssh_client_key_path() -> String {
    format!("{MANAGED_SSH_CLIENT_DIR}/{MANAGED_SSH_CLIENT_KEY_FILENAME}")
}

pub fn managed_ssh_known_hosts_path() -> String {
    format!("{MANAGED_SSH_CLIENT_DIR}/{MANAGED_SSH_KNOWN_HOSTS_FILENAME}")
}

/// Directory holding a given `StaticInventory`'s SSH key/known_hosts — keyed by the
/// `StaticInventory` resource name since one PlaybookPlan run can reference multiple
/// StaticInventories with different credentials simultaneously.
pub fn static_inventory_ssh_dir(static_inventory_name: &str) -> String {
    format!("/run/ansible-operator/ssh/{static_inventory_name}")
}

pub fn static_inventory_ssh_key_path(static_inventory_name: &str) -> String {
    format!("{}/id_rsa", static_inventory_ssh_dir(static_inventory_name))
}

pub fn static_inventory_known_hosts_path(static_inventory_name: &str) -> String {
    format!(
        "{}/known_hosts",
        static_inventory_ssh_dir(static_inventory_name)
    )
}
