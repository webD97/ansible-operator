use serde_yaml::{Mapping, Value};

use crate::resources::playbookplan::PlaybookPlanSpec;

pub fn render_playbook(spec: &PlaybookPlanSpec) -> Result<String, super::RenderError> {
    let mut playbook = Vec::new();

    for play_spec in &spec.templates {
        let mut play = Mapping::new();

        play.insert(
            Value::String("hosts".into()),
            Value::String(play_spec.hosts.clone()),
        );

        if let Some(handlers) = &play_spec.handlers.clone() {
            play.insert(
                Value::String("handlers".into()),
                serde_yaml::from_str(handlers)?,
            );
        }

        if let Some(pre_tasks) = &play_spec.pre_tasks.clone() {
            play.insert(
                Value::String("pre_tasks".into()),
                serde_yaml::from_str(pre_tasks)?,
            );
        }

        play.insert(
            Value::String("tasks".into()),
            serde_yaml::from_str(&play_spec.tasks.clone())?,
        );

        if let Some(post_tasks) = &play_spec.post_tasks.clone() {
            play.insert(
                Value::String("post_tasks".into()),
                serde_yaml::from_str(post_tasks)?,
            );
        }

        playbook.push(play);
    }

    Ok(serde_yaml::to_string(&playbook)?)
}
