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
        let id = ulid::Ulid::new().to_string();
        let target_path = module_path(&id)?;
        let previous = self.store.current_audio()?;
        actor_lines(
            &self.command_tx,
            format!("AT+CFTRANRX=\"{target_path}\",{}", data.len()),
            Some(data),
            PayloadMode::Raw {
                pacing: Duration::from_millis(u64::from(settings.upload_pacing_ms)),
            },
        )
        .await?;

        let audio = UploadedAudioRecord {
            id,
            name,
            format: "AMR-NB".into(),
            size,
            module_path: target_path,
            duration_ms: info.duration.as_millis() as u64,
            created_at_ms: now_ms(),
            state: "ready".into(),
        };
        self.store.save_current_audio(&audio)?;

        if let Some(previous) = previous.filter(|old| old.id != audio.id) {
            if module_path(&previous.id).as_deref() == Ok(previous.module_path.as_str()) {
                let _ = actor_lines(
                    &self.command_tx,
                    format!("AT+FSDEL=\"{}\"", previous.module_path),
                    None,
                    PayloadMode::Sms,
                )
                .await;
            }
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
        if was_playing {
            let _ = actor_lines(
                &self.command_tx,
                "AT+CCMXSTOP".into(),
                None,
                PayloadMode::Sms,
            )
            .await;
        }
        let hang_result = actor_lines(&self.command_tx, "ATH".into(), None, PayloadMode::Sms).await;
        self.finish(&active, "ended", "local-hang-up", "", true);
        self.clear_active(&active.id);
        hang_result.map(|_| ())
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
                self.clear_active(&active.id);
                return;
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
                let hang =
                    actor_lines(&self.command_tx, "ATH".into(), None, PayloadMode::Sms).await;
                if let Err(error) = hang {
                    drop(_gate);
                    self.fail_safely(
                        &active,
                        true,
                        format!("hang-up after playback failed: {error}"),
                    )
                    .await;
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
        if playback_started {
            let _ = actor_lines(
                &self.command_tx,
                "AT+CCMXSTOP".into(),
                None,
                PayloadMode::Sms,
            )
            .await;
        }
        let _ = actor_lines(&self.command_tx, "ATH".into(), None, PayloadMode::Sms).await;
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
                    Vec::new()
                };
                let _ = request.reply.send(Ok(lines));
                if command == "ATH" {
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
        assert_eq!(seen.iter().filter(|command| *command == "ATH").count(), 1);
    }

    #[tokio::test]
    async fn manual_hang_up_during_delay_cancels_future_playback() {
        let (manager, rx, store) = manager_fixture();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let actor_seen = Arc::clone(&seen);
        let actor = thread::spawn(move || {
            let mut began = false;
            while let Ok(request) = rx.recv() {
                let command = request.command.clone();
                actor_seen.lock().unwrap().push(command.clone());
                let lines = if command == "AT+CLCC" && !began {
                    began = true;
                    vec!["VOICE CALL: BEGIN".into()]
                } else {
                    Vec::new()
                };
                let _ = request.reply.send(Ok(lines));
                if command == "ATH" {
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
        assert_eq!(seen.iter().filter(|command| *command == "ATH").count(), 1);
    }

    #[tokio::test]
    async fn rejected_playback_fails_call_and_hangs_up() {
        let (manager, rx, store) = manager_fixture();
        let actor = thread::spawn(move || {
            let mut began = false;
            while let Ok(request) = rx.recv() {
                let command = request.command.clone();
                let result = if command == "AT+CLCC" && !began {
                    began = true;
                    Ok(vec!["VOICE CALL: BEGIN".into()])
                } else if command.starts_with("AT+CCMXPLAY") {
                    Err(ModemError::CommandRejected("ERROR".into()))
                } else {
                    Ok(Vec::new())
                };
                let _ = request.reply.send(result);
                if command == "ATH" {
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
    async fn failed_replacement_retains_previous_current_audio() {
        let (manager, rx, store) = manager_fixture();
        let actor = thread::spawn(move || {
            let request = rx.recv().unwrap();
            assert!(request.command.starts_with("AT+CFTRANRX="));
            let _ = request
                .reply
                .send(Err(ModemError::CommandRejected("upload rejected".into())));
        });
        let mut amr = crate::audio::AMR_NB_MAGIC.to_vec();
        amr.push(0x04);
        amr.extend([0_u8; 12]);
        assert!(manager.upload_audio("new.amr".into(), amr).await.is_err());
        actor.join().unwrap();
        assert_eq!(store.current_audio().unwrap().unwrap().id, "audio");
    }
}
