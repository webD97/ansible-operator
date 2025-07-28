use std::collections::BTreeMap;

use k8s_openapi::{api::batch, chrono::Utc};
use kube::{api::ObjectList, runtime::reflector::Lookup as _};
use tracing::info;

use crate::{
    utils::upsert_condition,
    v1beta1::{HostStatus, PlaybookPlanCondition, PlaybookPlanStatus, labels},
};

fn count_successful(jobs: &ObjectList<batch::v1::Job>) -> usize {
    jobs.iter()
        .filter(|job| {
            job.status
                .as_ref()
                .and_then(|status| status.conditions.as_ref())
                .map(|conditions| {
                    conditions.iter().any(|condition| {
                        condition.type_ == "SuccessCriteriaMet" && condition.status == "True"
                    })
                })
                .unwrap_or(false)
        })
        .count()
}

fn count_failed(jobs: &ObjectList<batch::v1::Job>) -> usize {
    jobs.iter()
        .filter(|job| {
            job.status
                .as_ref()
                .and_then(|status| status.conditions.as_ref())
                .map(|conditions| {
                    conditions
                        .iter()
                        .any(|condition| condition.type_ == "Failed" && condition.status == "True")
                })
                .unwrap_or(false)
        })
        .count()
}

/// Updates the conditions in the passed status so that they reflect the state of the jobs argument
pub fn evaluate_playbookplan_conditions(
    jobs: &ObjectList<batch::v1::Job>,
    status: &mut PlaybookPlanStatus,
) {
    let num_total = jobs.iter().count();
    let num_successful = count_successful(jobs);
    let num_failed = count_failed(jobs);
    let num_finished = num_failed + num_successful;
    let num_running = num_total - num_finished;

    let running_condition = {
        if num_finished < num_total {
            PlaybookPlanCondition {
                type_: "Running".into(),
                status: "True".into(),
                reason: Some("JobsRunning".into()),
                message: Some(format!("{num_running} jobs are currently running")),
                last_transition_time: Some(Utc::now().to_rfc3339()),
            }
        } else {
            PlaybookPlanCondition {
                type_: "Running".into(),
                status: "False".into(),
                reason: None,
                message: None,
                last_transition_time: Some(Utc::now().to_rfc3339()),
            }
        }
    };

    let ready_condition = {
        if num_successful == num_total {
            PlaybookPlanCondition {
                type_: "Ready".into(),
                status: "True".into(),
                reason: Some("AllJobsSucceeded".into()),
                message: Some(format!(
                    "{num_successful}/{num_total} jobs completed successfully"
                )),
                last_transition_time: Some(Utc::now().to_rfc3339()),
            }
        } else if num_failed > 0 {
            PlaybookPlanCondition {
                type_: "Ready".into(),
                status: "False".into(),
                reason: Some("SomeOrAllJobsFailed".into()),
                message: Some(format!("{num_failed}/{num_total} jobs have failed")),
                last_transition_time: Some(Utc::now().to_rfc3339()),
            }
        } else {
            PlaybookPlanCondition {
                type_: "Ready".into(),
                status: "False".into(),
                reason: Some("AwaitingJobResults".into()),
                message: Some(format!("{num_running} jobs are running")),
                last_transition_time: Some(Utc::now().to_rfc3339()),
            }
        }
    };

    upsert_condition(&mut status.conditions, running_condition);
    upsert_condition(&mut status.conditions, ready_condition);
}

/// Updates the per-host status based on the passed jobs
pub fn evaluate_per_host_status(
    jobs: &ObjectList<batch::v1::Job>,
    hash: u64,
    status: &mut PlaybookPlanStatus,
) {
    jobs.iter()
        .filter(|job| {
            job.status
                .as_ref()
                .and_then(|status| status.conditions.as_ref())
                .map(|conditions| {
                    conditions.iter().any(|condition| {
                        condition.type_ == "SuccessCriteriaMet" && condition.status == "True"
                    })
                })
                .unwrap_or(false)
        })
        .for_each(|job| {
            if status.hosts_status.is_none() {
                status.hosts_status = Some(BTreeMap::new());
            }

            let binding = job.metadata.labels.clone().unwrap_or_default();
            let target_host = binding.get(labels::PLAYBOOKPLAN_HOST);

            if target_host.is_none() {
                return;
            }

            let target_host = target_host.unwrap();

            info!(
                "Job {} was observed with SuccessCriteriaMet condition.",
                job.name().unwrap()
            );

            status
                .hosts_status
                .as_mut()
                .unwrap()
                .entry(target_host.to_owned())
                .or_insert(HostStatus {
                    last_applied_hash: hash.to_string(),
                });
        });
}
