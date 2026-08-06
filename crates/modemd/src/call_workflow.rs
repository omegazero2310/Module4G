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

mod actor;
mod audio_upload;
mod lifecycle;
mod release;
use actor::{actor_batch_lines, actor_lines};
use audio_upload::*;
use release::*;

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
