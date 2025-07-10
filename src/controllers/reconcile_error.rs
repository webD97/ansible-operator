use crate::ansible::RenderError;

#[derive(thiserror::Error, Debug)]
pub enum ReconcileError {
    #[error(transparent)]
    KubeError(#[from] kube::Error),

    #[error("Precondition failed: {0}")]
    PreconditionFailed(&'static str),

    #[error(transparent)]
    RenderError(#[from] RenderError),

    #[error(transparent)]
    JsonSerializationError(#[from] serde_json::Error),

    #[error(transparent)]
    YamlSerializationError(#[from] serde_yaml::Error),
}
