use std::collections::BTreeMap;

use serde::Deserialize;

/// Must match the marker strings printed by `ansible/callback_plugin.py`.
const MARKER_START: &str = "===ANSIBLE-OPERATOR-RECAP-START===";
const MARKER_END: &str = "===ANSIBLE-OPERATOR-RECAP-END===";

// Only `failed`/`unreachable` are consulted today (via `is_failure`); the rest mirror the
// callback's full JSON contract and are groundwork for future per-task progression info.
#[allow(dead_code)]
#[derive(Deserialize, Debug, Clone, Default)]
pub struct HostStats {
    #[serde(default)]
    pub ok: u32,
    #[serde(default)]
    pub changed: u32,
    #[serde(default)]
    pub unreachable: u32,
    #[serde(default)]
    pub failed: u32,
    #[serde(default)]
    pub skipped: u32,
    #[serde(default)]
    pub rescued: u32,
    #[serde(default)]
    pub ignored: u32,
}

impl HostStats {
    pub fn is_failure(&self) -> bool {
        self.failed > 0 || self.unreachable > 0
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct CallbackOutput {
    pub processed: BTreeMap<String, HostStats>,
}

/// Scans the full pod log output for the callback plugin's delimited JSON block. Returns `None`
/// if the marker is missing or the JSON inside is malformed/truncated — callers must surface that
/// as `HostOutcome::Unknown`, not `NotReached` (which means Ansible legitimately never got there).
pub fn parse_callback_output(logs: &str) -> Option<CallbackOutput> {
    let after_start = logs.find(MARKER_START)? + MARKER_START.len();
    let rest = &logs[after_start..];
    let end = rest.find(MARKER_END)?;
    let json_str = rest[..end].trim();

    serde_json::from_str(json_str).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(json: &str) -> String {
        format!("some task output\n{MARKER_START}\n{json}\n{MARKER_END}\nmore output\n")
    }

    #[test]
    fn parses_well_formed_marker_block() {
        let logs = wrap(r#"{"processed":{"host-1":{"ok":2,"changed":1,"unreachable":0,"failed":0,"skipped":0,"rescued":0,"ignored":0}}}"#);

        let parsed = parse_callback_output(&logs).unwrap();
        assert_eq!(parsed.processed.len(), 1);
        assert!(!parsed.processed["host-1"].is_failure());
    }

    #[test]
    fn missing_marker_returns_none() {
        let logs = "just some regular ansible task output, no recap here\n";
        assert!(parse_callback_output(logs).is_none());
    }

    #[test]
    fn malformed_json_inside_marker_returns_none_not_panic() {
        let logs = wrap("{not valid json");
        assert!(parse_callback_output(&logs).is_none());
    }

    #[test]
    fn truncated_log_missing_end_marker_returns_none() {
        let logs = format!("some output\n{MARKER_START}\n{{\"processed\": {{}}");
        assert!(parse_callback_output(&logs).is_none());
    }

    #[test]
    fn failed_and_unreachable_both_count_as_failure() {
        let failed = HostStats {
            failed: 1,
            ..Default::default()
        };
        let unreachable = HostStats {
            unreachable: 1,
            ..Default::default()
        };
        let ok = HostStats {
            ok: 3,
            rescued: 1,
            ..Default::default()
        };

        assert!(failed.is_failure());
        assert!(unreachable.is_failure());
        assert!(!ok.is_failure(), "a rescued host with no failed/unreachable counts is a success");
    }
}
