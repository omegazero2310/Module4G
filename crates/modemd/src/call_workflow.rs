use crate::{
    ModemError,
    audio::{inspect_amr, module_path, playback_deadline, validate_audio_name},
    call::{AnswerClassification, EndReason, classify_ceer, parse_urc, sanitize_cause},
    hardware::{AtRequest, PayloadMode},
    settings::Settings,
    storage::{CallRecord, Store, UploadedAudioRecord},
};
use std::{
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex as AsyncMutex;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const RELEASE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct ActiveCall {
    id: String,
    cancelled: Arc<AtomicBool>,
    command_gate: Arc<AsyncMutex<()>>,
    record: Arc<Mutex<CallRecord>>,
}

pub struct CallManager {
    command_tx: mpsc::Sender<AtRequest>,
    store: Arc<Store>,
    settings: Arc<RwLock<Settings>>,
    active: Mutex<Option<ActiveCall>>,
    uploading: AtomicBool,
}

impl CallManager {
    pub fn new(
        command_tx: mpsc::Sender<AtRequest>,
        store: Arc<Store>,
        settings: Arc<RwLock<Settings>>,
    ) -> Self {
        Self {
            command_tx,
            store,
            settings,
            active: Mutex::new(None),
            uploading: AtomicBool::new(false),
        }
    }

    pub fn current_audio(&self) -> Result<Option<UploadedAudioRecord>, ModemError> {
        self.store.current_audio()
    }

    pub fn list_audio(&self) -> Result<Vec<UploadedAudioRecord>, ModemError> {
        self.store.list_audio()
    }

    pub fn select_audio(&self, id: &str) -> Result<UploadedAudioRecord, ModemError> {
        if self.call_active() || self.uploading.load(Ordering::Acquire) {
            return Err(ModemError::Busy);
        }
        self.store.select_audio(id)
    }

    pub fn settings(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone()
    }

    pub fn update_settings(&self, settings: Settings) -> Result<Settings, ModemError> {
        settings.validate()?;
        self.store.save_settings(&settings, now_ms())?;
        *self
            .settings
            .write()
            .unwrap_or_else(|lock| lock.into_inner()) = settings.clone();
        Ok(settings)
    }

    pub fn list_calls(&self, limit: usize) -> Result<Vec<CallRecord>, ModemError> {
        self.store.list_calls(limit)
    }

    pub fn call_active(&self) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .is_some()
    }

    pub async fn upload_audio(
        &self,
        name: String,
        data: Vec<u8>,
    ) -> Result<UploadedAudioRecord, ModemError> {
        if self.call_active() {
            return Err(ModemError::Busy);
        }
        self.uploading
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ModemError::Busy)?;
        let _guard = UploadGuard(&self.uploading);
        if self.call_active() {
            return Err(ModemError::Busy);
        }

        let name = validate_audio_name(&name)?;
        let settings = self
            .settings
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone();
        let info = inspect_amr(&data, settings.max_audio_bytes)?;
        let size = data.len() as u64;
        eprintln!(
            "upload_validation status=ok bytes={} amr_frames={} duration_ms={}",
            size,
            info.frames,
            info.duration.as_millis()
        );
        let mut id = ulid::Ulid::new().to_string();
        let mut target_path = module_path(&id)?;
        let mut target_name = module_filename(&id)?;
        let replaced = self.store.audio_named(&name)?;
        let configured_pacing = Duration::from_millis(u64::from(settings.upload_pacing_ms));
        let first_result = actor_lines(
            &self.command_tx,
            format!("AT+CFTRANRX=\"{target_path}\",{}", data.len()),
            Some(data.clone()),
            PayloadMode::Raw {
                pacing: configured_pacing,
            },
        )
        .await;
        if let Err(error) = first_result {
            if !recoverable_upload_timeout(&error) {
                return Err(upload_stage_error("transfer", error));
            }

            eprintln!(
                "upload_retry reason=recoverable_timeout parser_resynchronized=true next_pacing_ms={} ",
                configured_pacing.max(Duration::from_millis(50)).as_millis()
            );
            let _ = delete_module_file(&self.command_tx, &target_name).await;
            id = ulid::Ulid::new().to_string();
            target_path = module_path(&id)?;
            target_name = module_filename(&id)?;
            let retry_result = actor_lines(
                &self.command_tx,
                format!("AT+CFTRANRX=\"{target_path}\",{}", data.len()),
                Some(data),
                PayloadMode::Raw {
                    pacing: configured_pacing.max(Duration::from_millis(50)),
                },
            )
            .await;
            if let Err(error) = retry_result {
                if recoverable_upload_timeout(&error)
                    || matches!(error, ModemError::CommandRejected(_))
                {
                    let _ = delete_module_file(&self.command_tx, &target_name).await;
                }
                return Err(upload_stage_error("retry transfer", error));
            }
        }

        if let Err(error) = verify_uploaded_size(&self.command_tx, &target_name, size).await {
            let _ = delete_module_file(&self.command_tx, &target_name).await;
            return Err(error);
        }

        let audio = UploadedAudioRecord {
            id,
            name,
            format: "AMR-NB".into(),
            size,
            module_path: target_path,
            duration_ms: info.duration.as_millis() as u64,
            created_at_ms: now_ms(),
            state: "ready".into(),
            is_current: true,
        };
        if let Some(previous) = replaced.as_ref() {
            if module_path(&previous.id).as_deref() == Ok(previous.module_path.as_str()) {
                let previous_name = module_filename(&previous.id)?;
                if let Err(error) = delete_module_file(&self.command_tx, &previous_name).await {
                    let _ = delete_module_file(&self.command_tx, &target_name).await;
                    return Err(upload_stage_error("replacement cleanup", error));
                }
            }
        }
        if let Err(error) = self
            .store
            .replace_and_select_audio(&audio, replaced.as_ref().map(|old| old.id.as_str()))
        {
            let _ = delete_module_file(&self.command_tx, &target_name).await;
            return Err(error);
        }
        Ok(audio)
    }

    pub async fn make_call(
        self: &Arc<Self>,
        destination: String,
        audio_id: String,
    ) -> Result<CallRecord, ModemError> {
        if self.uploading.load(Ordering::Acquire) {
            return Err(ModemError::Busy);
        }
        let audio = self
            .store
            .current_audio()?
            .filter(|audio| audio.id == audio_id)
            .ok_or_else(|| ModemError::Validation("select the current uploaded audio".into()))?;
        if module_path(&audio.id).as_deref() != Ok(audio.module_path.as_str()) {
            return Err(ModemError::Validation(
                "current audio has an invalid modem path".into(),
            ));
        }
        let record = CallRecord {
            id: ulid::Ulid::new().to_string(),
            peer: destination,
            state: "waiting-for-answer".into(),
            audio_id,
            created_at_ms: now_ms(),
            answer_classification: "unknown".into(),
            end_reason: "none".into(),
            ..Default::default()
        };
        let active = ActiveCall {
            id: record.id.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            command_gate: Arc::new(AsyncMutex::new(())),
            record: Arc::new(Mutex::new(record.clone())),
        };
        {
            let mut slot = self.active.lock().unwrap_or_else(|lock| lock.into_inner());
            if slot.is_some() {
                return Err(ModemError::Busy);
            }
            self.store.save_call(&record)?;
            *slot = Some(active.clone());
        }
        let manager = Arc::clone(self);
        tokio::spawn(async move { manager.run_call(active, audio).await });
        Ok(record)
    }

    pub async fn hang_up(&self) -> Result<(), ModemError> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone()
            .ok_or_else(|| ModemError::Validation("no call is active".into()))?;
        let _gate = active.command_gate.lock().await;
        active.cancelled.store(true, Ordering::Release);
        let was_playing = with_record(&active, |record| record.state == "playing");
        match self.release_and_confirm(&active, was_playing).await {
            Ok(()) => {
                self.finish(&active, "ended", "local-hang-up", "", true);
                self.clear_active(&active.id);
                Ok(())
            }
            Err(error) => {
                active.cancelled.store(false, Ordering::Release);
                self.mark_hang_up_failed(&active, &error.to_string());
                Err(error)
            }
        }
    }

    async fn run_call(self: Arc<Self>, active: ActiveCall, audio: UploadedAudioRecord) {
        if let Err(error) =
            dial_with_retry(&self.command_tx, &with_record(&active, |r| r.peer.clone())).await
        {
            self.fail_safely(&active, false, format!("dial failed: {error}"))
                .await;
            return;
        }

        let signaling_deadline = tokio::time::Instant::now()
            + Duration::from_secs(u64::from(
                self.settings
                    .read()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .call_timeout_seconds,
            ));
        let mut play_at = None;
        let mut completion_deadline = None;
        let mut playback_started = false;

        loop {
            if active.cancelled.load(Ordering::Acquire) {
                let still_active = self
                    .active
                    .lock()
                    .unwrap_or_else(|lock| lock.into_inner())
                    .as_ref()
                    .is_some_and(|call| call.id == active.id);
                if !still_active {
                    return;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }

            let lines =
                match actor_lines(&self.command_tx, "AT+CLCC".into(), None, PayloadMode::Sms).await
                {
                    Ok(lines) => lines,
                    Err(error) => {
                        self.fail_safely(
                            &active,
                            playback_started,
                            format!("modem polling failed: {error}"),
                        )
                        .await;
                        return;
                    }
                };

            let explicit = terminal_event(
                &lines,
                with_record(&active, |record| record.connected_at_ms > 0),
            );
            if with_record(&active, |record| record.state == "hang-up-failed") {
                if !lines
                    .iter()
                    .any(|line| matches!(parse_urc(line), Some(crate::call::CallUrc::Clcc { .. })))
                {
                    self.finish(&active, "ended", "local-hang-up", "", true);
                    self.clear_active(&active.id);
                    return;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            if let Some((classification, reason)) = explicit {
                self.finish_remote(&active, classification, reason).await;
                return;
            }
            if lines
                .iter()
                .any(|line| matches!(parse_urc(line), Some(crate::call::CallUrc::AudioPlayStop)))
                && playback_started
            {
                let _gate = active.command_gate.lock().await;
                if active.cancelled.load(Ordering::Acquire) {
                    continue;
                }
                if let Err(error) = self.release_and_confirm(&active, true).await {
                    self.mark_hang_up_failed(
                        &active,
                        &format!("hang-up after playback failed: {error}"),
                    );
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                } else {
                    self.finish(&active, "ended", "local-hang-up", "", true);
                    self.clear_active(&active.id);
                }
                return;
            }
            if play_at.is_none()
                && lines
                    .iter()
                    .any(|line| matches!(parse_urc(line), Some(crate::call::CallUrc::VoiceBegin)))
            {
                play_at = Some(tokio::time::Instant::now() + Duration::from_secs(1));
                with_record(&active, |record| {
                    record.state = "playback-delay".into();
                    record.answer_classification = "answered".into();
                    record.connected_at_ms = now_ms();
                    let _ = self.store.save_call(record);
                });
            }

            if !playback_started
                && play_at.is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
            {
                let _gate = active.command_gate.lock().await;
                if active.cancelled.load(Ordering::Acquire) {
                    continue;
                }
                let command = format!("AT+CCMXPLAY=\"{}\",1,0", audio.module_path);
                match actor_lines(&self.command_tx, command, None, PayloadMode::Sms).await {
                    Ok(_) => {
                        playback_started = true;
                        completion_deadline = Some(
                            tokio::time::Instant::now()
                                + playback_deadline(Duration::from_millis(audio.duration_ms)),
                        );
                        with_record(&active, |record| {
                            record.state = "playing".into();
                            let _ = self.store.save_call(record);
                        });
                    }
                    Err(error) => {
                        drop(_gate);
                        self.fail_safely(&active, false, format!("playback start failed: {error}"))
                            .await;
                        return;
                    }
                }
            }

            if completion_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                self.fail_safely(&active, true, "playback completion timed out".into())
                    .await;
                return;
            }
            if play_at.is_none() && tokio::time::Instant::now() >= signaling_deadline {
                self.fail_safely(&active, false, "call signaling timed out".into())
                    .await;
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn finish_remote(
        &self,
        active: &ActiveCall,
        classification: AnswerClassification,
        reason: EndReason,
    ) {
        if with_record(active, |record| record.state == "playing") {
            let _ = actor_lines(
                &self.command_tx,
                "AT+CCMXSTOP".into(),
                None,
                PayloadMode::Sms,
            )
            .await;
        }
        let cause = actor_lines(&self.command_tx, "AT+CEER".into(), None, PayloadMode::Sms)
            .await
            .ok()
            .and_then(|lines| lines.into_iter().find(|line| line.starts_with("+CEER:")))
            .map(|line| sanitize_cause(line.trim_start_matches("+CEER:").trim()))
            .unwrap_or_default();
        let reason = if matches!(reason, EndReason::CallError) && !cause.is_empty() {
            classify_ceer(&cause, classification == AnswerClassification::Answered)
        } else {
            reason
        };
        with_record(active, |record| {
            record.answer_classification = classification_label(classification).into();
            record.release_cause = cause;
        });
        self.finish(active, "ended", end_reason_label(reason), "", false);
        self.clear_active(&active.id);
    }

    async fn fail_safely(&self, active: &ActiveCall, playback_started: bool, error: String) {
        let _gate = active.command_gate.lock().await;
        if active.cancelled.load(Ordering::Acquire) {
            return;
        }
        if let Err(release_error) = self.release_and_confirm(active, playback_started).await {
            self.mark_hang_up_failed(
                active,
                &format!("{error}; release confirmation failed: {release_error}"),
            );
            drop(_gate);
            self.monitor_failed_release(active).await;
            return;
        }
        let reason = if error.contains("signaling timed out") {
            "signaling-timeout"
        } else if error.contains("disconnected") || error.contains("actor stopped") {
            "modem-lost"
        } else {
            "call-error"
        };
        self.finish(active, "failed", reason, &error, false);
        self.clear_active(&active.id);
    }

    async fn release_and_confirm(
        &self,
        _active: &ActiveCall,
        playback_started: bool,
    ) -> Result<(), ModemError> {
        if playback_started {
            let _ = actor_lines(
                &self.command_tx,
                "AT+CCMXSTOP".into(),
                None,
                PayloadMode::Sms,
            )
            .await;
        }
        actor_lines(&self.command_tx, "AT+CHUP".into(), None, PayloadMode::Sms).await?;
        let deadline = tokio::time::Instant::now() + RELEASE_CONFIRM_TIMEOUT;
        loop {
            let lines =
                actor_lines(&self.command_tx, "AT+CLCC".into(), None, PayloadMode::Sms).await?;
            if !lines
                .iter()
                .any(|line| matches!(parse_urc(line), Some(crate::call::CallUrc::Clcc { .. })))
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ModemError::Timeout);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    fn mark_hang_up_failed(&self, active: &ActiveCall, error: &str) {
        with_record(active, |record| {
            record.state = "hang-up-failed".into();
            record.error = error.into();
            let _ = self.store.save_call(record);
        });
    }

    async fn monitor_failed_release(&self, active: &ActiveCall) {
        loop {
            if active.cancelled.load(Ordering::Acquire) {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            let _gate = active.command_gate.lock().await;
            if active.cancelled.load(Ordering::Acquire) {
                continue;
            }
            if actor_lines(&self.command_tx, "AT+CLCC".into(), None, PayloadMode::Sms)
                .await
                .is_ok_and(|lines| {
                    !lines.iter().any(|line| {
                        matches!(parse_urc(line), Some(crate::call::CallUrc::Clcc { .. }))
                    })
                })
            {
                self.finish(active, "ended", "local-hang-up", "", true);
                self.clear_active(&active.id);
                return;
            }
            drop(_gate);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    fn finish(&self, active: &ActiveCall, state: &str, reason: &str, error: &str, local: bool) {
        with_record(active, |record| {
            record.state = state.into();
            record.end_reason = reason.into();
            record.error = error.into();
            record.ended_at_ms = now_ms();
            if local && record.connected_at_ms > 0 {
                record.answer_classification = "answered".into();
            }
            if record.connected_at_ms > 0 {
                record.duration_seconds = record
                    .ended_at_ms
                    .saturating_sub(record.connected_at_ms)
                    .div_euclid(1_000) as u32;
            }
            let _ = self.store.save_call(record);
        });
    }

    fn clear_active(&self, id: &str) {
        let mut active = self.active.lock().unwrap_or_else(|lock| lock.into_inner());
        if active.as_ref().is_some_and(|call| call.id == id) {
            *active = None;
        }
    }
}

struct UploadGuard<'a>(&'a AtomicBool);
impl Drop for UploadGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn with_record<T>(active: &ActiveCall, apply: impl FnOnce(&mut CallRecord) -> T) -> T {
    apply(
        &mut active
            .record
            .lock()
            .unwrap_or_else(|lock| lock.into_inner()),
    )
}

async fn actor_lines(
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

async fn actor_batch_lines(
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

fn module_filename(audio_id: &str) -> Result<String, ModemError> {
    module_path(audio_id)?
        .rsplit_once('/')
        .map(|(_, name)| name.to_owned())
        .ok_or_else(|| ModemError::Validation("invalid modem audio path".into()))
}

async fn verify_uploaded_size(
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

async fn delete_module_file(
    tx: &mpsc::Sender<AtRequest>,
    filename: &str,
) -> Result<Vec<String>, ModemError> {
    actor_batch_lines(
        tx,
        vec!["AT+FSCD=C:".into(), format!("AT+FSDEL=\"{filename}\"")],
    )
    .await
}

async fn dial_with_retry(
    tx: &mpsc::Sender<AtRequest>,
    destination: &str,
) -> Result<Vec<String>, ModemError> {
    let command = format!("ATD{destination};");
    for attempt in 0..5 {
        match actor_lines(tx, command.clone(), None, PayloadMode::Sms).await {
            Err(ModemError::CommandRejected(message))
                if attempt < 4
                    && message
                        .to_ascii_lowercase()
                        .contains("operation not allowed") =>
            {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            result => return result,
        }
    }
    unreachable!()
}

fn terminal_event(
    lines: &[String],
    previously_answered: bool,
) -> Option<(AnswerClassification, EndReason)> {
    if lines
        .iter()
        .any(|line| matches!(parse_urc(line), Some(crate::call::CallUrc::Busy)))
    {
        return Some((AnswerClassification::NotAnswered, EndReason::Busy));
    }
    if lines
        .iter()
        .any(|line| matches!(parse_urc(line), Some(crate::call::CallUrc::NoAnswer)))
    {
        return Some((AnswerClassification::NotAnswered, EndReason::NoAnswer));
    }
    let answered = previously_answered
        || lines
            .iter()
            .any(|line| matches!(parse_urc(line), Some(crate::call::CallUrc::VoiceBegin)));
    let ended = lines.iter().any(|line| {
        matches!(
            parse_urc(line),
            Some(crate::call::CallUrc::VoiceEnd | crate::call::CallUrc::NoCarrier)
        )
    });
    ended.then_some(if answered {
        (AnswerClassification::Answered, EndReason::RemoteHangUp)
    } else {
        (AnswerClassification::Unknown, EndReason::CallError)
    })
}

fn classification_label(value: AnswerClassification) -> &'static str {
    match value {
        AnswerClassification::Unknown => "unknown",
        AnswerClassification::Answered => "answered",
        AnswerClassification::NotAnswered => "not-answered",
    }
}

fn end_reason_label(value: EndReason) -> &'static str {
    match value {
        EndReason::None => "none",
        EndReason::LocalHangUp => "local-hang-up",
        EndReason::RemoteHangUp => "remote-hang-up",
        EndReason::Busy => "busy",
        EndReason::NoAnswer => "no-answer",
        EndReason::Unreachable => "unreachable",
        EndReason::NetworkError => "network-error",
        EndReason::SignalingTimeout => "signaling-timeout",
        EndReason::ModemLost => "modem-lost",
        EndReason::CallError => "call-error",
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn recoverable_upload_timeout(error: &ModemError) -> bool {
    matches!(
        error,
        ModemError::RawUploadTimeout {
            resynchronized: true,
            ..
        }
    )
}

fn upload_stage_error(stage: &str, error: ModemError) -> ModemError {
    ModemError::CommandRejected(format!("audio upload {stage} failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn explicit_terminal_codes_take_precedence() {
        let lines = vec!["VOICE CALL: END".into(), "NO CARRIER".into(), "BUSY".into()];
        assert_eq!(
            terminal_event(&lines, false),
            Some((AnswerClassification::NotAnswered, EndReason::Busy))
        );
    }

    #[test]
    fn audio_stop_is_not_a_remote_call_end() {
        assert_eq!(
            terminal_event(&["+AUDIOSTATE: audio play stop".into()], false),
            None
        );
    }

    fn manager_fixture() -> (Arc<CallManager>, mpsc::Receiver<AtRequest>, Arc<Store>) {
        let store = Arc::new(Store::memory().unwrap());
        store
            .save_current_audio(&UploadedAudioRecord {
                id: "audio".into(),
                name: "call.amr".into(),
                format: "AMR-NB".into(),
                size: 20,
                module_path: "c:/call_audio.amr".into(),
                duration_ms: 20,
                created_at_ms: 1,
                state: "ready".into(),
                is_current: true,
            })
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let manager = Arc::new(CallManager::new(
            tx,
            Arc::clone(&store),
            Arc::new(RwLock::new(Settings {
                call_timeout_seconds: 5,
                ..Default::default()
            })),
        ));
        (manager, rx, store)
    }

    async fn wait_for_state(store: &Store, state: &str, timeout: Duration) -> CallRecord {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let record = store.list_calls(1).unwrap().remove(0);
            if record.state == state {
                return record;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "last state was {}",
                record.state
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn answered_call_delays_once_plays_once_and_hangs_up_on_audio_stop() {
        let (manager, rx, store) = manager_fixture();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let actor_seen = Arc::clone(&seen);
        let actor = thread::spawn(move || {
            let mut began = false;
            let mut played = false;
            let mut polls_after_play = 0;
            let mut releasing = false;
            while let Ok(request) = rx.recv() {
                let command = request.command.clone();
                actor_seen.lock().unwrap().push(command.clone());
                let lines = if command.starts_with("ATD") {
                    Vec::new()
                } else if command == "AT+CLCC" && !began {
                    began = true;
                    vec!["VOICE CALL: BEGIN".into()]
                } else if command == "AT+CLCC" && played {
                    polls_after_play += 1;
                    if polls_after_play >= 2 {
                        vec!["+AUDIOSTATE: audio play stop".into()]
                    } else {
                        Vec::new()
                    }
                } else {
                    if command.starts_with("AT+CCMXPLAY") {
                        played = true;
                    }
                    if command == "AT+CHUP" {
                        releasing = true;
                    }
                    Vec::new()
                };
                let _ = request.reply.send(Ok(lines));
                if releasing && command == "AT+CLCC" {
                    break;
                }
            }
        });

        manager
            .make_call("+66812345678".into(), "audio".into())
            .await
            .unwrap();
        wait_for_state(&store, "playback-delay", Duration::from_secs(1)).await;
        let ended = wait_for_state(&store, "ended", Duration::from_secs(3)).await;
        actor.join().unwrap();
        assert_eq!(ended.answer_classification, "answered");
        assert_eq!(ended.end_reason, "local-hang-up");
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.iter()
                .filter(|command| command.starts_with("AT+CCMXPLAY"))
                .count(),
            1
        );
        assert_eq!(
            seen.iter().filter(|command| *command == "AT+CHUP").count(),
            1
        );
    }

    #[tokio::test]
    async fn manual_hang_up_during_delay_cancels_future_playback() {
        let (manager, rx, store) = manager_fixture();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let actor_seen = Arc::clone(&seen);
        let actor = thread::spawn(move || {
            let mut began = false;
            let mut releasing = false;
            while let Ok(request) = rx.recv() {
                let command = request.command.clone();
                actor_seen.lock().unwrap().push(command.clone());
                let lines = if command == "AT+CLCC" && !began {
                    began = true;
                    vec!["VOICE CALL: BEGIN".into()]
                } else {
                    if command == "AT+CHUP" {
                        releasing = true;
                    }
                    Vec::new()
                };
                let _ = request.reply.send(Ok(lines));
                if releasing && command == "AT+CLCC" {
                    break;
                }
            }
        });

        manager
            .make_call("+66812345678".into(), "audio".into())
            .await
            .unwrap();
        wait_for_state(&store, "playback-delay", Duration::from_secs(1)).await;
        manager.hang_up().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        actor.join().unwrap();
        let ended = store.list_calls(1).unwrap().remove(0);
        assert_eq!(ended.state, "ended");
        assert_eq!(ended.end_reason, "local-hang-up");
        let seen = seen.lock().unwrap();
        assert!(
            !seen
                .iter()
                .any(|command| command.starts_with("AT+CCMXPLAY"))
        );
        assert_eq!(
            seen.iter().filter(|command| *command == "AT+CHUP").count(),
            1
        );
    }

    #[tokio::test]
    async fn rejected_playback_fails_call_and_hangs_up() {
        let (manager, rx, store) = manager_fixture();
        let actor = thread::spawn(move || {
            let mut began = false;
            let mut releasing = false;
            while let Ok(request) = rx.recv() {
                let command = request.command.clone();
                let result = if command == "AT+CLCC" && !began {
                    began = true;
                    Ok(vec!["VOICE CALL: BEGIN".into()])
                } else if command.starts_with("AT+CCMXPLAY") {
                    Err(ModemError::CommandRejected("ERROR".into()))
                } else {
                    if command == "AT+CHUP" {
                        releasing = true;
                    }
                    Ok(Vec::new())
                };
                let _ = request.reply.send(result);
                if releasing && command == "AT+CLCC" {
                    break;
                }
            }
        });
        manager
            .make_call("+66812345678".into(), "audio".into())
            .await
            .unwrap();
        let failed = wait_for_state(&store, "failed", Duration::from_secs(3)).await;
        actor.join().unwrap();
        assert!(failed.error.contains("playback start failed"));
        assert_eq!(failed.end_reason, "call-error");
    }

    #[tokio::test]
    async fn unconfirmed_release_blocks_calls_until_hang_up_retry_succeeds() {
        let (manager, rx, store) = manager_fixture();
        let actor = thread::spawn(move || {
            let mut began = false;
            let mut release_attempts = 0;
            while let Ok(request) = rx.recv() {
                let command = request.command.clone();
                if command == "AT+CHUP" {
                    release_attempts += 1;
                }
                let lines = if command == "AT+CLCC" && !began {
                    began = true;
                    vec!["VOICE CALL: BEGIN".into(), "+CLCC: 1,0,0,0,0".into()]
                } else if command == "AT+CLCC" && release_attempts < 2 {
                    vec!["+CLCC: 1,0,0,0,0".into()]
                } else {
                    Vec::new()
                };
                let _ = request.reply.send(Ok(lines));
                if release_attempts >= 2 && command == "AT+CLCC" {
                    break;
                }
            }
        });

        manager
            .make_call("+66812345678".into(), "audio".into())
            .await
            .unwrap();
        wait_for_state(&store, "playback-delay", Duration::from_secs(1)).await;
        assert!(manager.hang_up().await.is_err());
        wait_for_state(&store, "hang-up-failed", Duration::from_secs(1)).await;
        assert!(matches!(
            manager
                .make_call("+66812345679".into(), "audio".into())
                .await,
            Err(ModemError::Busy)
        ));
        manager.hang_up().await.unwrap();
        let ended = wait_for_state(&store, "ended", Duration::from_secs(1)).await;
        actor.join().unwrap();
        assert_eq!(ended.end_reason, "local-hang-up");
        assert!(!manager.call_active());
    }

    #[tokio::test]
    async fn non_resynchronized_timeout_is_not_retried_and_retains_previous_audio() {
        let (manager, rx, store) = manager_fixture();
        let actor = thread::spawn(move || {
            let request = rx.recv().unwrap();
            assert!(request.command.starts_with("AT+CFTRANRX="));
            let _ = request.reply.send(Err(ModemError::RawUploadTimeout {
                phase: crate::RawUploadTimeoutPhase::Prompt,
                bytes_sent: 0,
                chunks_sent: 0,
                pacing_ms: 50,
                elapsed_ms: 2_000,
                resynchronized: false,
            }));
        });
        let mut amr = crate::audio::AMR_NB_MAGIC.to_vec();
        amr.push(0x04);
        amr.extend([0_u8; 12]);
        assert!(manager.upload_audio("new.amr".into(), amr).await.is_err());
        actor.join().unwrap();
        assert_eq!(store.current_audio().unwrap().unwrap().id, "audio");
    }

    #[tokio::test]
    async fn unreliable_fast_upload_retries_once_with_manual_recommended_pacing() {
        let (manager, rx, store) = manager_fixture();
        let mut settings = manager.settings();
        settings.upload_pacing_ms = 10;
        manager.update_settings(settings).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let actor_seen = Arc::clone(&seen);
        let actor = thread::spawn(move || {
            for index in 0..4 {
                let request = rx.recv().unwrap();
                actor_seen.lock().unwrap().push((
                    request.command.clone(),
                    request.payload_mode,
                    request.batch.clone(),
                ));
                let result = if index == 0 {
                    Err(ModemError::RawUploadTimeout {
                        phase: crate::RawUploadTimeoutPhase::FinalResult,
                        bytes_sent: 19,
                        chunks_sent: 1,
                        pacing_ms: 10,
                        elapsed_ms: 100,
                        resynchronized: true,
                    })
                } else if index == 3 {
                    Ok(vec!["+FSATTRI: 19".into()])
                } else {
                    Ok(Vec::new())
                };
                let _ = request.reply.send(result);
            }
        });
        let mut amr = crate::audio::AMR_NB_MAGIC.to_vec();
        amr.push(0x04);
        amr.extend([0_u8; 12]);
        let uploaded = manager.upload_audio("retry.amr".into(), amr).await.unwrap();
        actor.join().unwrap();
        assert_eq!(store.current_audio().unwrap().unwrap().id, uploaded.id);
        let seen = seen.lock().unwrap();
        let transfers: Vec<_> = seen
            .iter()
            .filter(|(command, _, _)| command.starts_with("AT+CFTRANRX="))
            .collect();
        assert_eq!(transfers.len(), 2);
        assert_ne!(transfers[0].0, transfers[1].0);
        assert_eq!(
            transfers[0].1,
            PayloadMode::Raw {
                pacing: Duration::from_millis(10)
            }
        );
        assert_eq!(
            transfers[1].1,
            PayloadMode::Raw {
                pacing: Duration::from_millis(50)
            }
        );
        assert_eq!(seen[1].2[0], "AT+FSCD=C:");
        assert!(seen[1].2[1].starts_with("AT+FSDEL=\"call_"));
        assert!(!seen[1].2[1].contains("C:/"));
        assert_eq!(seen[3].2[0], "AT+FSCD=C:");
        assert!(seen[3].2[1].starts_with("AT+FSATTRI=\"call_"));
        assert_eq!(store.list_audio().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn size_mismatch_keeps_previous_audio_and_deletes_relative_partial_file() {
        let (manager, rx, store) = manager_fixture();
        let seen = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let actor_seen = Arc::clone(&seen);
        let actor = thread::spawn(move || {
            for index in 0..3 {
                let request = rx.recv().unwrap();
                actor_seen.lock().unwrap().push(request.batch.clone());
                let response = if index == 1 {
                    Ok(vec!["+FSATTRI: 18".into()])
                } else {
                    Ok(Vec::new())
                };
                let _ = request.reply.send(response);
            }
        });
        let mut amr = crate::audio::AMR_NB_MAGIC.to_vec();
        amr.push(0x04);
        amr.extend([0_u8; 12]);
        let error = manager
            .upload_audio("mismatch.amr".into(), amr)
            .await
            .unwrap_err();
        actor.join().unwrap();
        assert!(error.to_string().contains("expected 19 byte(s)"));
        assert_eq!(store.current_audio().unwrap().unwrap().id, "audio");
        let seen = seen.lock().unwrap();
        assert_eq!(seen[1][0], "AT+FSCD=C:");
        assert!(seen[1][1].starts_with("AT+FSATTRI=\"call_"));
        assert_eq!(seen[2][0], "AT+FSCD=C:");
        assert!(seen[2][1].starts_with("AT+FSDEL=\"call_"));
        assert!(!seen[2][1].contains("C:/"));
    }
}
