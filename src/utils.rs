use std::fmt::Debug;

use kube::api::{Patch, PatchParams, PostParams};
use serde::{Serialize, de::DeserializeOwned};

pub async fn create_or_update<K>(
    api: kube::Api<K>,
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
