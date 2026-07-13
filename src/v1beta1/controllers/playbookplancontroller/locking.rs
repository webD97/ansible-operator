use std::hash::{Hash, Hasher};

use chrono::{DateTime, Duration, Utc};
use k8s_openapi::{
    api::coordination::v1::{Lease, LeaseSpec},
    apimachinery::pkg::apis::meta::v1::ObjectMeta,
    jiff,
};
use kube::{Api, api::PostParams};
use tracing::debug;

use crate::v1beta1::controllers::reconcile_error::ReconcileError;

/// How long a Lease is considered valid without being renewed. Deliberately short and renewed
/// every reconcile tick (not sized to a run's total length) so a crashed operator only leaves a
/// stale lock around for a short window before it's eligible for reclaim.
pub const LEASE_DURATION_SECONDS: i32 = 90;

#[derive(Debug, PartialEq, Eq)]
pub enum LeaseDecision {
    /// No Lease exists yet for this host.
    Create,
    /// A Lease exists, is held by someone else, and has expired — safe to take over.
    Replace { resource_version: String },
    /// A Lease exists and is already held by us — needs its renewTime bumped, nothing else.
    Renew { resource_version: String },
    /// A Lease exists, is held by someone else, and has not expired.
    HeldByOther,
}

/// Pure decision logic for whether/how to acquire or renew a per-host Lease. Contains no I/O so
/// it can be unit-tested without a k8s client.
pub fn decide(existing: Option<&Lease>, desired_holder: &str, now: DateTime<Utc>) -> LeaseDecision {
    let Some(lease) = existing else {
        return LeaseDecision::Create;
    };

    let resource_version = || {
        lease
            .metadata
            .resource_version
            .clone()
            .expect("a Lease read back from the API always has a resourceVersion")
    };

    let spec = lease.spec.as_ref();
    let holder = spec.and_then(|s| s.holder_identity.as_deref());

    if holder == Some(desired_holder) {
        return LeaseDecision::Renew {
            resource_version: resource_version(),
        };
    }

    let expired = spec
        .and_then(|s| {
            let renew_time = jiff_to_chrono(&s.renew_time.as_ref()?.0)?;
            let duration = Duration::seconds(
                s.lease_duration_seconds.unwrap_or(LEASE_DURATION_SECONDS) as i64,
            );
            Some(renew_time + duration < now)
        })
        // No renewTime recorded at all (shouldn't normally happen since we always set one) —
        // treat as stale/reclaimable rather than getting stuck forever.
        .unwrap_or(true);

    if expired {
        LeaseDecision::Replace {
            resource_version: resource_version(),
        }
    } else {
        LeaseDecision::HeldByOther
    }
}

fn jiff_to_chrono(ts: &jiff::Timestamp) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(ts.as_second(), 0)
}

fn chrono_to_jiff(dt: DateTime<Utc>) -> jiff::Timestamp {
    jiff::Timestamp::from_second(dt.timestamp())
        .expect("chrono::DateTime<Utc> is always within jiff's representable range")
}

/// Deterministic Lease name for a resolved host identity (Node name or arbitrary
/// StaticInventory hostname/IP) — hashed rather than used verbatim since the latter can contain
/// characters invalid in a Kubernetes resource name (e.g. IPv6 addresses, uppercase DNS names).
pub fn lease_name(host: &str) -> String {
    let mut hasher = twox_hash::XxHash3_64::new();
    host.hash(&mut hasher);
    format!("ansible-lock-{:x}", hasher.finish())
}

fn build_lease(name: &str, holder_identity: &str, now: DateTime<Utc>) -> Lease {
    Lease {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(holder_identity.to_string()),
            lease_duration_seconds: Some(LEASE_DURATION_SECONDS),
            renew_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(
                chrono_to_jiff(now),
            )),
            acquire_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(
                chrono_to_jiff(now),
            )),
            ..Default::default()
        }),
    }
}

fn is_conflict(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(status) if status.code == 409)
}

/// Attempts to acquire or renew the per-host Lease for every host in `target_hosts`, all under
/// the same `holder_identity`. Returns the subset of hosts that are still held by someone else —
/// an empty result means every lock is held by us. Safe to call every reconcile tick: hosts we
/// already hold just get their renewTime bumped.
pub async fn ensure_locks(
    api: &Api<Lease>,
    target_hosts: &[String],
    holder_identity: &str,
) -> Result<Vec<String>, ReconcileError> {
    let now = Utc::now();
    let mut blocked = Vec::new();

    for host in target_hosts {
        let name = lease_name(host);
        let existing = api.get_opt(&name).await?;
        let decision = decide(existing.as_ref(), holder_identity, now);

        let result = match decision {
            LeaseDecision::Create => {
                let lease = build_lease(&name, holder_identity, now);
                api.create(&PostParams::default(), &lease).await.map(drop)
            }
            LeaseDecision::Replace { resource_version } | LeaseDecision::Renew { resource_version } => {
                let mut lease = build_lease(&name, holder_identity, now);
                lease.metadata.resource_version = Some(resource_version);
                api.replace(&name, &PostParams::default(), &lease)
                    .await
                    .map(drop)
            }
            LeaseDecision::HeldByOther => {
                blocked.push(host.clone());
                continue;
            }
        };

        match result {
            Ok(()) => {}
            // Someone else raced us between our read and write — treat as blocked for this
            // tick, we'll re-evaluate next time rather than treating it as a hard failure.
            Err(err) if is_conflict(&err) => {
                debug!("Lease conflict for host {host}, will retry next tick");
                blocked.push(host.clone());
            }
            Err(err) => return Err(err.into()),
        }
    }

    Ok(blocked)
}

/// Releases every Lease this run holds for `target_hosts`. Called explicitly when a run
/// finishes (success or terminal failure) — TTL expiry is the crash safety net only, not the
/// everyday release path.
pub async fn release_locks(
    api: &Api<Lease>,
    target_hosts: &[String],
    holder_identity: &str,
) -> Result<(), ReconcileError> {
    for host in target_hosts {
        let name = lease_name(host);

        let Some(existing) = api.get_opt(&name).await? else {
            continue;
        };

        let held_by_us = existing
            .spec
            .as_ref()
            .and_then(|s| s.holder_identity.as_deref())
            == Some(holder_identity);

        if !held_by_us {
            continue;
        }

        match api.delete(&name, &Default::default()).await {
            Ok(_) => {}
            Err(err) if is_conflict(&err) => {
                // Someone else already reclaimed/deleted it — nothing left for us to release.
            }
            Err(err) => return Err(err.into()),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease_with(holder: &str, renew_time: DateTime<Utc>, duration_seconds: i32) -> Lease {
        Lease {
            metadata: ObjectMeta {
                name: Some("some-lease".into()),
                resource_version: Some("42".into()),
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(holder.to_string()),
                lease_duration_seconds: Some(duration_seconds),
                renew_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(
                    chrono_to_jiff(renew_time),
                )),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn decide_creates_when_absent() {
        let now = Utc::now();
        assert_eq!(decide(None, "ns/plan/hash", now), LeaseDecision::Create);
    }

    #[test]
    fn decide_renews_when_held_by_us() {
        let now = Utc::now();
        let lease = lease_with("ns/plan/hash", now - Duration::seconds(10), 90);
        assert_eq!(
            decide(Some(&lease), "ns/plan/hash", now),
            LeaseDecision::Renew {
                resource_version: "42".into()
            }
        );
    }

    #[test]
    fn decide_blocks_when_held_by_other_and_not_expired() {
        let now = Utc::now();
        let lease = lease_with("ns/other-plan/hash", now - Duration::seconds(10), 90);
        assert_eq!(
            decide(Some(&lease), "ns/plan/hash", now),
            LeaseDecision::HeldByOther
        );
    }

    #[test]
    fn decide_replaces_when_held_by_other_but_expired() {
        let now = Utc::now();
        let lease = lease_with("ns/other-plan/hash", now - Duration::seconds(200), 90);
        assert_eq!(
            decide(Some(&lease), "ns/plan/hash", now),
            LeaseDecision::Replace {
                resource_version: "42".into()
            }
        );
    }

    #[test]
    fn decide_treats_missing_renew_time_as_expired() {
        let now = Utc::now();
        let lease = Lease {
            metadata: ObjectMeta {
                name: Some("some-lease".into()),
                resource_version: Some("42".into()),
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some("ns/other-plan/hash".into()),
                ..Default::default()
            }),
        };
        assert_eq!(
            decide(Some(&lease), "ns/plan/hash", now),
            LeaseDecision::Replace {
                resource_version: "42".into()
            }
        );
    }

    #[test]
    fn lease_name_is_deterministic_and_dns_safe() {
        let a = lease_name("worker-1");
        let b = lease_name("worker-1");
        let c = lease_name("192.168.1.42");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-'));
    }

    #[test]
    fn jiff_chrono_roundtrip_preserves_seconds() {
        let now = Utc::now();
        let now = DateTime::from_timestamp(now.timestamp(), 0).unwrap();
        let roundtripped = jiff_to_chrono(&chrono_to_jiff(now)).unwrap();
        assert_eq!(now, roundtripped);
    }
}
