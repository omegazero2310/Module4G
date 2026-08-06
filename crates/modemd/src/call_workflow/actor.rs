use crate::{
    ModemError,
    hardware::{AtRequest, PayloadMode},
};
use std::sync::mpsc;

pub(super) async fn actor_lines(
    tx: &mpsc::Sender<AtRequest>,
    command: String,
    payload: Option<Vec<u8>>,
    payload_mode: PayloadMode,
) -> Result<Vec<String>, ModemError> {
    let (reply, response) = tokio::sync::oneshot::channel();
    tx.send(AtRequest {
        command,
        payload,
        guarded: false,
        payload_mode,
        batch: Vec::new(),
        finalizer: None,
        reply,
    })
    .map_err(|_| ModemError::Disconnected)?;
    response.await.map_err(|_| ModemError::Disconnected)?
}

pub(super) async fn actor_batch_lines(
    tx: &mpsc::Sender<AtRequest>,
    batch: Vec<String>,
) -> Result<Vec<String>, ModemError> {
    let (reply, response) = tokio::sync::oneshot::channel();
    tx.send(AtRequest {
        command: String::new(),
        payload: None,
        guarded: false,
        payload_mode: PayloadMode::Sms,
        batch,
        finalizer: None,
        reply,
    })
    .map_err(|_| ModemError::Disconnected)?;
    response.await.map_err(|_| ModemError::Disconnected)?
}
