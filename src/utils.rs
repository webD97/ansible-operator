use std::fmt::Debug;

use kube::api::{Patch, PatchParams, PostParams};
use serde::{Serialize, de::DeserializeOwned};

pub async fn create_or_update<K>(
    api: &kube::Api<K>,
    field_manager: &str,
    resource_name: &str,
    resource: K,
    mutate_fn: impl FnOnce(K, &mut K),
) -> Result<(), kube::Error>
where
    K: DeserializeOwned + Serialize + Clone + Debug,
{
    if let Some(existing_resource) = api.get_opt(resource_name).await? {
        let mut updated_resource = resource.clone();
        mutate_fn(existing_resource, &mut updated_resource);

        api.patch(
            resource_name,
            &PatchParams::apply(field_manager),
            &Patch::Apply(serde_yaml::to_value(&updated_resource).unwrap()),
        )
        .await?;
    } else {
        api.create(
            &PostParams {
                field_manager: Some(field_manager.into()),
                ..Default::default()
            },
            &resource,
        )
        .await?;
    }

    Ok(())
}

pub trait Condition {
    fn type_(&self) -> &str;
    fn status(&self) -> &str;
    fn reason(&self) -> Option<&str>;
}

pub fn upsert_condition<T: Condition>(conditions: &mut Vec<T>, new_condition: T) {
    if let Some(existing_condition) = conditions
        .iter_mut()
        .find(|c| c.type_() == new_condition.type_())
    {
        // Skip change if we can't see a difference in the new value
        if existing_condition.status() == new_condition.status()
            && existing_condition.reason() == new_condition.reason()
        {
            return;
        }

        *existing_condition = new_condition;
    } else {
        conditions.push(new_condition);
    }
}

fn encode_base36(mut num: u64) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

    if num == 0 {
        return "a".repeat(6); // return "aaaaaa" if input is zero, fixed length
    }
    let base = ALPHABET.len() as u64;
    let mut chars = Vec::new();

    while num > 0 {
        let rem = (num % base) as usize;
        chars.push(ALPHABET[rem] as char);
        num /= base;
    }

    chars.reverse();
    chars.into_iter().collect()
}

/// Generate a short Kubernetes-like ID for use in resource names
pub fn generate_id(num: u64) -> String {
    const LEN: usize = 5;

    let encoded = encode_base36(num);

    if encoded.len() == LEN {
        encoded
    } else if encoded.len() > LEN {
        encoded[encoded.len() - LEN..].to_string()
    } else {
        let padding = "a".repeat(LEN - encoded.len());
        format!("{padding}{encoded}")
    }
}
