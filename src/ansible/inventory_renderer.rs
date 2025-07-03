use std::collections::BTreeMap;

use serde_yaml::{Mapping, Value};

pub fn render_inventory(
    inventory: &BTreeMap<String, Vec<String>>,
) -> Result<String, super::RenderError> {
    let mut yaml_inventory = Mapping::new();

    for (group_name, hostnames) in inventory.iter() {
        let mut hosts = Mapping::new();

        for hostname in hostnames {
            hosts.insert(
                Value::String(hostname.into()),
                Value::Mapping(Mapping::new()),
            );
        }

        let mut group = Mapping::new();
        group.insert(Value::String("hosts".into()), Value::Mapping(hosts));

        yaml_inventory.insert(Value::String(group_name.into()), Value::Mapping(group));
    }

    Ok(serde_yaml::to_string(&yaml_inventory)?)
}
