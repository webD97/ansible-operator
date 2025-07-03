#[derive(thiserror::Error, Debug)]
pub enum RenderError {
    #[error(transparent)]
    SerializationError(#[from] serde_yaml::Error),
}

#[cfg(test)]
mod test {
    use crate::{
        ansible::playbook_renderer::render_playbook,
        resources::playbookplan::{PlaybookPlanSpec, Template},
    };

    #[test]
    pub fn test_render_playbook() {
        let spec = &PlaybookPlanSpec {
            templates: vec![Template {
                hosts: "all".into(),
                tasks: r#"
- name: Ensure httpd installed
  ansible.builtin.dnf:
    name: httpd
    state: installed
"#
                .into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        println!("{}", render_playbook(spec).unwrap());
    }
}
