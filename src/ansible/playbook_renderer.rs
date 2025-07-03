use serde_yaml::{Mapping, Sequence, Value};

use crate::resources::playbookplan::PlaybookPlanSpec;

pub fn render_playbook(spec: &PlaybookPlanSpec) -> Result<String, super::RenderError> {
    let mut plays: Sequence = serde_yaml::from_str(&spec.template)?;

    for play in &mut plays {
        configure_ansible_play(play.as_mapping_mut().expect("expected a yaml map"));
    }

    Ok(serde_yaml::to_string(&plays)?)
}

fn configure_ansible_play(play: &mut Mapping) {
    play.insert(Value::String("hosts".into()), Value::String("all".into()));
}
