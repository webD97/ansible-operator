use serde_yaml::{Mapping, Value};

use crate::v1beta1::ResolvedHosts;

pub fn render_inventory(inventory: &[ResolvedHosts]) -> Result<String, super::RenderError> {
    let mut yaml_inventory = Mapping::new();

    for group in inventory.iter() {
        let mut hosts = Mapping::new();

        for hostname in &group.hosts {
            hosts.insert(
                Value::String(hostname.into()),
                Value::Mapping(Mapping::new()),
            );
        }

        let mut yaml_group = Mapping::new();
        yaml_group.insert(Value::String("hosts".into()), Value::Mapping(hosts));

        yaml_inventory.insert(
            Value::String(group.name.to_owned()),
            Value::Mapping(yaml_group),
        );
    }

    Ok(serde_yaml::to_string(&yaml_inventory)?)
}
