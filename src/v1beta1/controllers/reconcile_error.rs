use crate::v1beta1::ansible;

#[derive(thiserror::Error, Debug)]
pub enum ReconcileError {
    #[error(transparent)]
    KubeError(#[from] kube::Error),

    #[error("Precondition failed: {0}")]
    PreconditionFailed(&'static str),

    #[error(transparent)]
    RenderError(#[from] ansible::RenderError),

    #[error(transparent)]
    JsonSerializationError(#[from] serde_json::Error),

    #[error(transparent)]
    YamlSerializationError(#[from] serde_yaml::Error),
}
