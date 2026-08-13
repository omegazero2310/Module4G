use super::*;

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
            syncing: AtomicBool::new(false),
            audio_gate: AsyncMutex::new(()),
            pending_audio_manifest: Mutex::new(None),
            sync_state: RwLock::new(AudioSyncState::Pending),
            catalog_ready: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn audio_sync_state(&self) -> AudioSyncState {
        *self
            .sync_state
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
    }

    pub fn audio_catalog_ready(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.catalog_ready)
    }

    pub fn mark_audio_sync_pending(&self) {
        self.catalog_ready.store(false, Ordering::Release);
        *self
            .sync_state
            .write()
            .unwrap_or_else(|lock| lock.into_inner()) = AudioSyncState::Pending;
    }

    pub fn current_audio(&self) -> Result<Option<UploadedAudioRecord>, ModemError> {
        self.store.current_audio()
    }

    pub fn list_audio(&self) -> Result<Vec<UploadedAudioRecord>, ModemError> {
        self.store.list_audio()
    }

    pub async fn select_audio(&self, id: &str) -> Result<UploadedAudioRecord, ModemError> {
        if self.call_active() || self.uploading.load(Ordering::Acquire) {
            return Err(ModemError::Busy);
        }
        let _gate = self.audio_gate.lock().await;
        let selected = self
            .store
            .list_audio()?
            .into_iter()
            .find(|audio| audio.id == id)
            .ok_or_else(|| ModemError::Validation("audio file was not found".into()))?;
        self.store.select_audio(id)?;
        if let Err(error) = self.persist_selection_manifest(id).await {
            eprintln!("audio manifest selection persistence deferred: {error}");
        }
        Ok(UploadedAudioRecord {
            is_current: true,
            ..selected
        })
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

    pub fn sms_sync_allowed(&self) -> bool {
        !self.call_active()
            && !self.uploading.load(Ordering::Acquire)
            && !self.syncing.load(Ordering::Acquire)
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
        let _audio_gate = self.audio_gate.lock().await;
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
                Some(data.clone()),
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
        if replaced
            .as_ref()
            .is_some_and(|old| !is_managed_path(&old.module_path))
        {
            let _ = delete_module_file(&self.command_tx, &target_name).await;
            return Err(ModemError::Validation(
                "an external modem audio file already uses that name".into(),
            ));
        }
        if let Err(error) = self
            .store
            .replace_and_select_audio(&audio, replaced.as_ref().map(|old| old.id.as_str()))
        {
            let _ = delete_module_file(&self.command_tx, &target_name).await;
            return Err(error);
        }
        if let Err(error) = self
            .persist_uploaded_manifest(&audio, replaced.as_ref(), &data)
            .await
        {
            eprintln!("audio manifest upload persistence deferred: {error}");
        }
        if let Some(previous) = replaced.as_ref() {
            if is_managed_path(&previous.module_path) {
                if let Some(previous_name) = previous.module_path.rsplit('/').next() {
                    if let Err(error) = delete_module_file(&self.command_tx, previous_name).await {
                        eprintln!("audio replacement cleanup deferred: {error}");
                    }
                }
            }
        }
        Ok(audio)
    }

    pub async fn make_call(
        self: &Arc<Self>,
        destination: String,
        audio_id: String,
    ) -> Result<CallRecord, ModemError> {
        self.make_call_with_id(ulid::Ulid::new().to_string(), destination, audio_id)
            .await
    }

    pub async fn make_call_with_id(
        self: &Arc<Self>,
        id: String,
        destination: String,
        audio_id: String,
    ) -> Result<CallRecord, ModemError> {
        if self.uploading.load(Ordering::Acquire) {
            return Err(ModemError::Busy);
        }
        let _audio_gate = self.audio_gate.lock().await;
        let audio = self
            .store
            .current_audio()?
            .filter(|audio| audio.id == audio_id)
            .ok_or_else(|| ModemError::Validation("select the current uploaded audio".into()))?;
        if !is_safe_audio_path(&audio.module_path) {
            return Err(ModemError::Validation(
                "current audio has an invalid modem path".into(),
            ));
        }
        if self.audio_sync_state() == AudioSyncState::Ready {
            verify_module_audio(&self.command_tx, &audio).await?;
        }
        let record = CallRecord {
            id,
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
