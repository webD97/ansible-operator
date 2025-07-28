use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
};

use k8s_openapi::ByteString;

use crate::v1beta1::{self, controllers::reconcile_error::ReconcileError};

/// Returns an iterator over hosts where the PlaybookPlan needs to be (re)applied.
pub fn find_outdated_hosts<'a>(
    status: &'a v1beta1::PlaybookPlanStatus,
    execution_hash: u64,
) -> Result<Box<dyn Iterator<Item = &'a String> + Send + 'a>, ReconcileError> {
    // If we have no eligible hosts, we don't need to execute the playbook anywhere
    let Some(hosts) = &status.eligible_hosts else {
        return Ok(Box::new(std::iter::empty()));
    };

    // If we don't have any hosts_status yet, simply return all hosts for execution
    let Some(hosts_status) = &status.hosts_status else {
        return Ok(Box::new(hosts.values().flatten()));
    };

    // For each host, check if it already has the current execution hash in the PlaybookPlan's status
    let outdated_hosts = hosts.values().flatten().filter(move |host| {
        let host_status = hosts_status.get(*host);

        // We don't have a status for this host yet so we must execute the playbook
        if host_status.is_none() {
            return true;
        }

        let host_status = host_status.unwrap();

        // Otherwise just compare the hashes
        host_status.last_applied_hash != execution_hash.to_string()
    });

    Ok(Box::new(outdated_hosts))
}

/// Given a playbook and some secrets, calculate a hash that only changes if the inputs change.
/// With regards to the secrets, the hash is order-insensitive.
pub fn calculate_execution_hash<'a, T: IntoIterator<Item = &'a BTreeMap<String, ByteString>>>(
    playbook: &str,
    secrets: T,
) -> u64 {
    std::iter::once({
        let mut hasher = twox_hash::XxHash3_64::new();
        playbook.hash(&mut hasher);
        hasher.finish()
    })
    .chain(secrets.into_iter().map(|secret| {
        let mut hasher = twox_hash::XxHash3_64::new();

        for (key, value) in secret {
            key.hash(&mut hasher);
            value.0.hash(&mut hasher);
        }

        hasher.finish()
    }))
    .fold(0u64, |prev, next| prev ^ next)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::v1beta1::{HostStatus, PlaybookPlanStatus};

    use super::*;

    #[test]
    pub fn test_must_execute_returns_none_when_eligible_hosts_empty() {
        // Given
        let status = PlaybookPlanStatus {
            eligible_hosts: None,
            ..Default::default()
        };

        // When
        let to_execute = find_outdated_hosts(&status, 1u64);

        // Then
        assert_eq!(to_execute.unwrap().count(), 0);
    }

    #[test]
    pub fn test_must_execute_returns_all_when_hosts_status_empty() {
        // Given
        let status = PlaybookPlanStatus {
            eligible_hosts: Some(BTreeMap::from_iter(vec![(
                "test-inventory".into(),
                vec!["host-1".into(), "host-2".into(), "host-3".into()],
            )])),
            hosts_status: None,
            ..Default::default()
        };

        // When
        let to_execute = find_outdated_hosts(&status, 1u64);

        // Then
        let expected_hostnames = [
            "host-1".to_owned(),
            "host-2".to_owned(),
            "host-3".to_owned(),
        ];
        let expected: Vec<&String> = expected_hostnames.iter().collect();
        let actual: Vec<&String> = to_execute.unwrap().collect();

        assert!(expected.eq(&actual));
    }

    #[test]
    pub fn test_must_execute_returns_correct_hosts() {
        // Given
        let status = PlaybookPlanStatus {
            eligible_hosts: Some(BTreeMap::from_iter(vec![(
                "test-inventory".into(),
                vec!["host-1".into(), "host-2".into(), "host-3".into()],
            )])),
            hosts_status: Some(BTreeMap::from_iter(vec![
                (
                    "host-1".to_owned(),
                    HostStatus {
                        last_applied_hash: "1".to_owned(),
                    },
                ),
                (
                    "host-2".to_owned(),
                    HostStatus {
                        last_applied_hash: "2".to_owned(),
                    },
                ),
                (
                    "host-3".to_owned(),
                    HostStatus {
                        last_applied_hash: "1".to_owned(),
                    },
                ),
            ])),
            ..Default::default()
        };

        // When
        let to_execute = find_outdated_hosts(&status, 2u64);

        // Then
        let expected_hostnames = ["host-1".to_owned(), "host-3".to_owned()];
        let expected: Vec<&String> = expected_hostnames.iter().collect();
        let actual: Vec<&String> = to_execute.unwrap().collect();

        assert_eq!(expected, actual);
    }

    #[test]
    pub fn test_calculate_execution_hash_is_order_insensitive() {
        // Given
        let playbook = "awesome playbook here";
        let secret1_data = BTreeMap::from_iter(vec![
            ("key-1".to_string(), ByteString(b"data-1".to_vec())),
            ("key-2".to_string(), ByteString(b"value-2".to_vec())),
        ]);
        let secret2_data = BTreeMap::from_iter(vec![(
            "meaningful_number".to_string(),
            ByteString(b"73".to_vec()),
        )]);
        let secret3_data = BTreeMap::from_iter(vec![(
            "answer".to_string(),
            ByteString(b"forty-two".to_vec()),
        )]);

        // When
        let hashed_1 =
            calculate_execution_hash(playbook, [&secret1_data, &secret2_data, &secret3_data]);
        let hashed_2 =
            calculate_execution_hash(playbook, [&secret2_data, &secret1_data, &secret3_data]);
        let hashed_3 =
            calculate_execution_hash(playbook, [&secret3_data, &secret2_data, &secret1_data]);

        // Then
        assert_eq!(hashed_1, hashed_2);
        assert_eq!(hashed_2, hashed_3);
    }
}
