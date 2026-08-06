#![forbid(unsafe_code)]

pub mod at;
pub mod audio;
pub mod call;
pub mod call_workflow;
pub mod hardware;
pub mod integration;
pub mod settings;
pub mod sms;
pub mod storage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawUploadTimeoutPhase {
    Prompt,
    FinalResult,
}

impl std::fmt::Display for RawUploadTimeoutPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Prompt => "before the data prompt",
            Self::FinalResult => "after the final payload byte",
        })
    }
}

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
    #[error("SMS submission timed out {phase}; parser resynchronized: {resynchronized}")]
    SmsSubmitTimeout {
        phase: &'static str,
        resynchronized: bool,
    },
    #[error(
        "raw upload timed out {phase} ({bytes_sent} byte(s), {chunks_sent} chunk(s), {pacing_ms} ms pacing, {elapsed_ms} ms elapsed; parser resynchronized: {resynchronized})"
    )]
    RawUploadTimeout {
        phase: RawUploadTimeoutPhase,
        bytes_sent: usize,
        chunks_sent: usize,
        pacing_ms: u64,
        elapsed_ms: u64,
        resynchronized: bool,
    },
    #[error("SIM unavailable")]
    SimUnavailable,
    #[error("network unavailable")]
    NetworkUnavailable,
    #[error("modem rejected command: {0}")]
    CommandRejected(String),
    #[error("persistence failed: {0}")]
    Persistence(String),
}

impl From<ModemError> for tonic::Status {
    fn from(error: ModemError) -> Self {
        match error {
            ModemError::Validation(message) => tonic::Status::invalid_argument(message),
            ModemError::Busy => tonic::Status::resource_exhausted(error.to_string()),
            ModemError::Disconnected => tonic::Status::unavailable(error.to_string()),
            ModemError::Timeout
            | ModemError::SmsSubmitTimeout { .. }
            | ModemError::RawUploadTimeout { .. } => {
                tonic::Status::deadline_exceeded(error.to_string())
            }
            ModemError::SimUnavailable
            | ModemError::NetworkUnavailable
            | ModemError::CommandRejected(_) => {
                tonic::Status::failed_precondition(error.to_string())
            }
            ModemError::Persistence(_) => tonic::Status::internal(error.to_string()),
        }
    }
}
