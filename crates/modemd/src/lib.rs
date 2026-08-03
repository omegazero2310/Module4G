#![forbid(unsafe_code)]

pub mod at;
pub mod audio;
pub mod hardware;
pub mod settings;
pub mod sms;
pub mod storage;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModemError {
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("operation is not allowed while the modem is busy")]
    Busy,
    #[error("modem disconnected")]
    Disconnected,
    #[error("command timed out")]
    Timeout,
    #[error("SIM unavailable")]
    SimUnavailable,
    #[error("network unavailable")]
    NetworkUnavailable,
    #[error("persistence failed: {0}")]
    Persistence(String),
}

impl From<ModemError> for tonic::Status {
    fn from(error: ModemError) -> Self {
        match error {
            ModemError::Validation(message) => tonic::Status::invalid_argument(message),
            ModemError::Busy => tonic::Status::resource_exhausted(error.to_string()),
            ModemError::Disconnected => tonic::Status::unavailable(error.to_string()),
            ModemError::Timeout => tonic::Status::deadline_exceeded(error.to_string()),
            ModemError::SimUnavailable | ModemError::NetworkUnavailable => tonic::Status::failed_precondition(error.to_string()),
            ModemError::Persistence(_) => tonic::Status::internal(error.to_string()),
        }
    }
}
