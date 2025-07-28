use crate::v1beta1::{self, PlaybookVariableSource};

pub fn render_ansible_command(
    plan: &v1beta1::PlaybookPlan,
    hostname: &str,
    extra_vars_filepaths: Vec<&String>,
) -> Vec<String> {
    let static_vars_filenames: Vec<String> = plan
        .spec
        .template
        .variables
        .as_ref()
        .map(|variables| {
            variables
                .iter()
                .filter_map(|source| match source {
                    PlaybookVariableSource::SecretRef { secret_ref: _ } => None,
                    PlaybookVariableSource::Inline { inline: _ } => Some(()),
                })
                .enumerate()
                .map(|(index, _)| format!("static-variables-{index}.yml"))
                .collect()
        })
        .unwrap_or_default();

    let mut ansible_command = vec!["ansible-playbook".into()];

    ansible_command.extend(
        static_vars_filenames
            .iter()
            .flat_map(|path| ["--extra-vars".into(), format!("@{path}")]),
    );

    ansible_command.extend(extra_vars_filepaths.iter().flat_map(|path| {
        [
            "--extra-vars".into(),
            format!("@/run/ansible-operator/vars/{path}/variables.yaml"),
        ]
    }));

    let connection_args = match &plan.spec.connection_strategy {
        v1beta1::ConnectionStrategy::Chroot {} => vec!["-i".into(), "/mnt/host,".into()],
        v1beta1::ConnectionStrategy::Ssh { ssh } => vec![
            "--ssh-common-args='-o UserKnownHostsFile=/ssh/known_hosts'".into(),
            "--private-key".into(),
            "/ssh/id_rsa".into(),
            "--user".into(),
            ssh.user.clone(),
            "-i".into(),
            "inventory.yml".into(),
            "-l".into(),
            format!("{hostname},"),
        ],
    };

    ansible_command.extend(connection_args);
    ansible_command.push("playbook.yml".into());

    ansible_command
}
