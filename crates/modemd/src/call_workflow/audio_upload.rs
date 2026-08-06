use super::*;

pub(super) fn module_filename(audio_id: &str) -> Result<String, ModemError> {
    module_path(audio_id)?
        .rsplit_once('/')
        .map(|(_, name)| name.to_owned())
        .ok_or_else(|| ModemError::Validation("invalid modem audio path".into()))
}

pub(super) async fn verify_uploaded_size(
    tx: &mpsc::Sender<AtRequest>,
    filename: &str,
    expected: u64,
) -> Result<(), ModemError> {
    let lines = actor_batch_lines(
        tx,
        vec!["AT+FSCD=C:".into(), format!("AT+FSATTRI=\"{filename}\"")],
    )
    .await
    .map_err(|error| upload_stage_error("size verification", error))?;
    let actual = lines
        .iter()
        .find_map(|line| line.strip_prefix("+FSATTRI:"))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .ok_or_else(|| {
            ModemError::CommandRejected(
                "audio upload size verification failed: modem did not report a file size".into(),
            )
        })?;
    if actual != expected {
        return Err(ModemError::CommandRejected(format!(
            "audio upload size verification failed: expected {expected} byte(s), modem reported {actual}"
        )));
    }
    Ok(())
}

pub(super) async fn delete_module_file(
    tx: &mpsc::Sender<AtRequest>,
    filename: &str,
) -> Result<Vec<String>, ModemError> {
    actor_batch_lines(
        tx,
        vec!["AT+FSCD=C:".into(), format!("AT+FSDEL=\"{filename}\"")],
    )
    .await
}
