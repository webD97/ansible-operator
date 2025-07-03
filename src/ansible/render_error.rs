#[derive(thiserror::Error, Debug)]
pub enum RenderError {
    #[error(transparent)]
    SerializationError(#[from] serde_yaml::Error),
}
