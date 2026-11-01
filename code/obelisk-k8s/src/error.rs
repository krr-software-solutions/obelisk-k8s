/// Every error obelisk-k8s can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ObeliskK8sError {
    #[error("{0}")]
    Invalid(String),
}
