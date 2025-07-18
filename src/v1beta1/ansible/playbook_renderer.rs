use serde_yaml::Sequence;

use crate::v1beta1;

pub fn render_playbook(spec: &v1beta1::PlaybookPlanSpec) -> Result<String, super::RenderError> {
    let plays: Sequence = serde_yaml::from_str(&spec.template.playbook)?;
    Ok(serde_yaml::to_string(&plays)?)
}
