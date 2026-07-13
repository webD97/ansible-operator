use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v1beta1::HostOutcome;

/// A single Ansible execution — one attempt of a `PlaybookPlan` run, backed 1:1 by one Job in the
/// plan's namespace. Purely a durable *history record*: the operator writes it, nothing reconciles
/// it into further cluster state. It exists so a run's Ansible recap survives the backing Job/pod's
/// short `ttlSecondsAfterFinished`, and so `kubectl get plays` gives an at-a-glance run history with
/// the recap tallies as columns.
///
/// Owned (ownerReference) by its `PlaybookPlan`, so deleting the plan cascades to all its Plays;
/// retention beyond that is bounded per-plan by `successfulPlaysHistoryLimit`/
/// `failedPlaysHistoryLimit`. Correlated to its Job by the shared execution hash plus `attempt`.
#[derive(CustomResource, Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[kube(
    group = "ansible.cloudbending.dev",
    version = "v1beta1",
    kind = "Play",
    namespaced,
    status = "PlayStatus",
    printcolumn = r#"{"name":"Plan","type":"string","jsonPath":".spec.playbookPlan"}"#,
    printcolumn = r#"{"name":"Attempt","type":"integer","jsonPath":".spec.attempt","priority":1}"#,
    printcolumn = r#"{"name":"Hosts","type":"integer","jsonPath":".status.hostCount"}"#,
    printcolumn = r#"{"name":"Ok","type":"integer","jsonPath":".status.recap.ok"}"#,
    printcolumn = r#"{"name":"Changed","type":"integer","jsonPath":".status.recap.changed"}"#,
    printcolumn = r#"{"name":"Failed","type":"integer","jsonPath":".status.recap.failed"}"#,
    printcolumn = r#"{"name":"Unreachable","type":"integer","jsonPath":".status.recap.unreachable"}"#,
    printcolumn = r#"{"name":"Rescued","type":"integer","jsonPath":".status.recap.rescued","priority":1}"#,
    printcolumn = r#"{"name":"Skipped","type":"integer","jsonPath":".status.recap.skipped","priority":1}"#,
    printcolumn = r#"{"name":"Ignored","type":"integer","jsonPath":".status.recap.ignored","priority":1}"#,
    printcolumn = r#"{"name":"Status","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct PlaySpec {
    /// The `PlaybookPlan` this run belongs to (also this Play's ownerReference).
    pub playbook_plan: String,

    /// The execution hash the run applied — matches the backing Job's hash label.
    pub execution_hash: String,

    /// Retry number within this hash: 1 for the first attempt, incrementing per retry. Mirrors the
    /// backing Job's numbered name (`apply-{plan}-{shortid}-{attempt}`).
    pub attempt: u32,

    /// The hosts this run targeted.
    pub hosts: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlayStatus {
    pub phase: PlayPhase,

    /// Name of the backing Job in the plan's namespace. The Job/pod may already have been reaped by
    /// Kubernetes' TTL controller; this Play outlives them.
    pub job_name: Option<String>,

    /// When the backing Job reached a terminal state. The run's *start* is the Play's own
    /// `metadata.creationTimestamp` (the Play is created when the run's Job is), so it isn't
    /// duplicated here. See the `#[serde(default, ...)]` timestamp note on
    /// `PlaybookPlanStatus::next_run`: merge patches drop `null` keys, so this is genuinely absent
    /// when `None`.
    #[serde(default, with = "crate::v1beta1::resources::custom_rfc3339")]
    #[schemars(with = "Option<String>")]
    pub finished_at: Option<DateTime<FixedOffset>>,

    /// Number of hosts this run targeted (mirrors `spec.hosts.len()`, surfaced as a column).
    pub host_count: u32,

    /// How many hosts ended `Failed` or `Unreachable`.
    pub failed_host_count: u32,

    /// The Ansible recap, summed across every targeted host — the recap columns read from here.
    pub recap: PlayRecap,

    /// Per-host recap and outcome, for drilling into which host did what.
    pub hosts: BTreeMap<String, PlayHostResult>,
}

/// The seven Ansible recap counters (`PLAY RECAP` line). Field order is irrelevant here — unlike
/// the positional wire format in `callback_output::HostStats`, these are named/`camelCase` for
/// JSONPath columns and merge-patch friendliness.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlayRecap {
    pub ok: u32,
    pub changed: u32,
    pub unreachable: u32,
    pub failed: u32,
    pub skipped: u32,
    pub rescued: u32,
    pub ignored: u32,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlayHostResult {
    pub recap: PlayRecap,
    pub outcome: HostOutcome,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
pub enum PlayPhase {
    /// The backing Job has been created and hasn't reached a terminal state yet.
    #[default]
    Running,
    /// The Job finished and no targeted host was `Failed`/`Unreachable`.
    Succeeded,
    /// The Job finished with at least one `Failed`/`Unreachable` host.
    Failed,
    /// The Job finished but its recap couldn't be read — reaped before the operator saw it, or a
    /// hard crash (OOM/SIGKILL) before the stats hook wrote `/dev/termination-log`.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_status_serializes_recap_camel_case_for_columns() {
        let mut hosts = BTreeMap::new();
        hosts.insert(
            "node-1".to_string(),
            PlayHostResult {
                recap: PlayRecap {
                    ok: 5,
                    changed: 2,
                    ..Default::default()
                },
                outcome: HostOutcome::Succeeded,
            },
        );

        let status = PlayStatus {
            phase: PlayPhase::Succeeded,
            job_name: Some("apply-web-a1b2c3-1".into()),
            host_count: 1,
            failed_host_count: 0,
            recap: PlayRecap {
                ok: 5,
                changed: 2,
                ..Default::default()
            },
            hosts,
            ..Default::default()
        };

        let json = serde_json::to_value(&status).unwrap();

        // The printer columns read these JSONPaths — pin the camelCase surface.
        assert_eq!(json["recap"]["ok"], 5);
        assert_eq!(json["hostCount"], 1);
        assert_eq!(json["phase"], "Succeeded");

        let back: PlayStatus = serde_json::from_value(json).unwrap();
        assert_eq!(back.recap, status.recap);
        assert_eq!(back.hosts["node-1"].outcome, HostOutcome::Succeeded);
    }

    /// An absent optional timestamp must deserialize (merge patches store it as genuinely missing,
    /// not `null`) — same contract as the other status types.
    #[test]
    fn play_status_deserializes_when_timestamps_are_absent() {
        let json = serde_json::json!({
            "phase": "Running",
            "hostCount": 2,
            "failedHostCount": 0,
            "recap": { "ok": 0, "changed": 0, "unreachable": 0, "failed": 0, "skipped": 0, "rescued": 0, "ignored": 0 },
            "hosts": {}
            // finishedAt / jobName deliberately omitted
        });

        let status: PlayStatus = serde_json::from_value(json).unwrap();
        assert_eq!(status.phase, PlayPhase::Running);
        assert_eq!(status.finished_at, None);
        assert_eq!(status.job_name, None);
    }
}
