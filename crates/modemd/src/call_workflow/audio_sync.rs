use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const MANIFEST_FILES: [&str; 2] = ["a7670_audio_0.txt", "a7670_audio_1.txt"];
const LEGACY_MANIFEST_FILES: [&str; 2] = ["a7670_audio_0.json", "a7670_audio_1.json"];
const MANIFEST_MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct AudioManifest {
    version: u32,
    generation: u64,
    current_id: String,
    audio: Vec<ManifestAudio>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
struct ManifestAudio {
    id: String,
    name: String,
    module_path: String,
    size: u64,
    duration_ms: u64,
    created_at_ms: i64,
    sha256: String,
    owned: bool,
}

impl CallManager {
    pub async fn reconcile_audio(&self) -> Result<usize, ModemError> {
        if self.call_active() || self.uploading.load(Ordering::Acquire) {
            return Err(ModemError::Busy);
        }
        self.syncing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ModemError::Busy)?;
        let _sync_guard = SyncGuard(&self.syncing);
        self.set_sync_state(AudioSyncState::Syncing);
        let result = self.reconcile_audio_inner().await;
        self.set_sync_state(if result.is_ok() {
            AudioSyncState::Ready
        } else {
            AudioSyncState::Deferred
        });
        result
    }

    async fn reconcile_audio_inner(&self) -> Result<usize, ModemError> {
        let _gate = self.audio_gate.lock().await;
        let manifest = self.best_manifest().await?;
        let existing = self.store.list_audio()?;
        let existing_by_path = existing
            .iter()
            .map(|audio| (audio.module_path.to_ascii_lowercase(), audio))
            .collect::<HashMap<_, _>>();
        let manifest_by_path = manifest
            .audio
            .iter()
            .map(|audio| (audio.module_path.to_ascii_lowercase(), audio))
            .collect::<HashMap<_, _>>();
        let mut files = list_amr_files(&self.command_tx).await?;
        files.sort_by_key(|name| name.to_ascii_lowercase());
        let max_bytes = self.settings().max_audio_bytes;
        let mut entries = Vec::new();
        let mut ids = HashSet::new();
        for filename in files {
            let modem_path = format!("c:/{filename}");
            let size = match module_file_size(&self.command_tx, &filename).await {
                Ok(size) if size > 0 && size as usize <= max_bytes => size,
                Ok(_) | Err(ModemError::CommandRejected(_)) => continue,
                Err(error) => return Err(error),
            };
            let key = modem_path.to_ascii_lowercase();
            let saved = manifest_by_path.get(&key).filter(|saved| {
                saved.size == size
                    && !saved.sha256.is_empty()
                    && is_safe_audio_path(&saved.module_path)
            });
            let mut entry = if let Some(saved) = saved {
                (*saved).clone()
            } else {
                let data = match actor_data(
                    &self.command_tx,
                    format!("AT+CFTRANTX=\"{modem_path}\""),
                    max_bytes,
                )
                .await
                {
                    Ok(data) => data,
                    Err(ModemError::CommandRejected(_) | ModemError::Validation(_)) => continue,
                    Err(error) => return Err(error),
                };
                let Ok(info) = inspect_amr(&data, max_bytes) else {
                    continue;
                };
                let digest = sha256(&data);
                if let Some(saved) = existing_by_path
                    .get(&key)
                    .filter(|saved| saved.size == size)
                {
                    let mut recovered =
                        ManifestAudio::from_record(saved, digest, is_managed_path(&modem_path));
                    recovered.duration_ms = info.duration.as_millis() as u64;
                    recovered
                } else {
                    ManifestAudio {
                        id: recovered_id(&modem_path),
                        name: filename.clone(),
                        module_path: modem_path.clone(),
                        size,
                        duration_ms: info.duration.as_millis() as u64,
                        created_at_ms: managed_created_at(&filename).unwrap_or_else(now_ms),
                        sha256: digest,
                        owned: is_managed_path(&modem_path),
                    }
                }
            };
            entry.module_path = modem_path;
            entry.size = size;
            if validate_audio_name(&entry.name).is_err() {
                entry.name = filename;
            }
            if module_path(&entry.id).is_err() || !ids.insert(entry.id.clone()) {
                entry.id = recovered_id(&entry.module_path);
                if !ids.insert(entry.id.clone()) {
                    continue;
                }
            }
            entries.push(entry);
        }

        make_names_unique(&mut entries);
        let current_id = choose_current(&manifest.current_id, &existing, &entries);
        let mut records = entries
            .iter()
            .map(|entry| entry.to_record(entry.id == current_id))
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            right
                .is_current
                .cmp(&left.is_current)
                .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
                .then_with(|| right.id.cmp(&left.id))
        });
        let reconciled_manifest = AudioManifest {
            version: 1,
            generation: manifest.generation,
            current_id,
            audio: entries,
        };
        self.store.replace_audio_inventory(&records)?;
        if !same_manifest_content(&manifest, &reconciled_manifest) {
            if let Err(error) = self.queue_manifest(reconciled_manifest).await {
                eprintln!("audio manifest reconciliation persistence deferred: {error}");
            }
        }
        Ok(records.len())
    }

    pub(super) async fn persist_selection_manifest(&self, id: &str) -> Result<(), ModemError> {
        let mut manifest = self.manifest_from_store().await?;
        if !manifest.audio.iter().any(|audio| audio.id == id) {
            return Err(ModemError::Validation("audio file was not found".into()));
        }
        manifest.current_id = id.into();
        self.queue_manifest(manifest).await
    }

    pub(super) async fn persist_uploaded_manifest(
        &self,
        audio: &UploadedAudioRecord,
        replaced: Option<&UploadedAudioRecord>,
        data: &[u8],
    ) -> Result<(), ModemError> {
        let mut manifest = self.manifest_from_store().await?;
        if let Some(replaced) = replaced {
            manifest.audio.retain(|entry| entry.id != replaced.id);
        }
        manifest
            .audio
            .push(ManifestAudio::from_record(audio, sha256(data), true));
        manifest.current_id = audio.id.clone();
        self.queue_manifest(manifest).await
    }

    async fn manifest_from_store(&self) -> Result<AudioManifest, ModemError> {
        let saved = self.best_manifest().await?;
        let hashes = saved
            .audio
            .into_iter()
            .map(|entry| (entry.module_path.to_ascii_lowercase(), entry.sha256))
            .collect::<HashMap<_, _>>();
        let records = self.store.list_audio()?;
        Ok(AudioManifest {
            version: 1,
            generation: saved.generation,
            current_id: records
                .iter()
                .find(|audio| audio.is_current)
                .map(|audio| audio.id.clone())
                .unwrap_or_default(),
            audio: records
                .iter()
                .map(|audio| {
                    ManifestAudio::from_record(
                        audio,
                        hashes
                            .get(&audio.module_path.to_ascii_lowercase())
                            .cloned()
                            .unwrap_or_default(),
                        is_managed_path(&audio.module_path),
                    )
                })
                .collect(),
        })
    }

    async fn load_manifest(&self) -> Result<AudioManifest, ModemError> {
        let mut best = None;
        for filename in MANIFEST_FILES.into_iter().chain(LEGACY_MANIFEST_FILES) {
            let size = match module_file_size(&self.command_tx, filename).await {
                Ok(size) if size > 0 && size as usize <= MANIFEST_MAX_BYTES => size,
                Ok(_) | Err(ModemError::CommandRejected(_) | ModemError::Validation(_)) => continue,
                Err(error) => return Err(error),
            };
            let data = match actor_data(
                &self.command_tx,
                format!("AT+CFTRANTX=\"c:/{filename}\""),
                size as usize,
            )
            .await
            {
                Ok(data) => data,
                Err(ModemError::CommandRejected(_) | ModemError::Validation(_)) => continue,
                Err(error) => return Err(error),
            };
            let manifest = match serde_json::from_slice::<AudioManifest>(&data) {
                Ok(value) if value.version == 1 => value,
                _ => continue,
            };
            if best
                .as_ref()
                .is_none_or(|current: &AudioManifest| manifest.generation > current.generation)
            {
                best = Some(manifest);
            }
        }
        Ok(best.unwrap_or_default())
    }

    async fn best_manifest(&self) -> Result<AudioManifest, ModemError> {
        if let Some(pending) = self
            .pending_audio_manifest
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone()
        {
            return Ok(pending);
        }
        self.load_manifest().await
    }

    async fn queue_manifest(&self, mut manifest: AudioManifest) -> Result<(), ModemError> {
        manifest.version = 1;
        manifest.generation = manifest.generation.saturating_add(1);
        *self
            .pending_audio_manifest
            .lock()
            .unwrap_or_else(|lock| lock.into_inner()) = Some(manifest);
        self.persist_pending_manifest().await
    }

    async fn persist_pending_manifest(&self) -> Result<(), ModemError> {
        let manifest = self
            .pending_audio_manifest
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .clone()
            .ok_or_else(|| ModemError::Validation("no audio manifest is pending".into()))?;
        let slot = manifest.generation as usize % MANIFEST_FILES.len();
        let filename = MANIFEST_FILES[slot];
        let data = serde_json::to_vec(&manifest).map_err(|error| {
            ModemError::Persistence(format!("audio manifest serialization failed: {error}"))
        })?;
        if data.len() > MANIFEST_MAX_BYTES {
            return Err(ModemError::Validation("audio manifest is too large".into()));
        }
        let _ = delete_module_file(&self.command_tx, filename).await;
        actor_lines(
            &self.command_tx,
            format!("AT+CFTRANRX=\"c:/{filename}\",{}", data.len()),
            Some(data.clone()),
            PayloadMode::Raw {
                pacing: Duration::from_millis(u64::from(self.settings().upload_pacing_ms)),
            },
        )
        .await?;
        verify_uploaded_size(&self.command_tx, filename, data.len() as u64).await?;
        let mut pending = self
            .pending_audio_manifest
            .lock()
            .unwrap_or_else(|lock| lock.into_inner());
        if pending
            .as_ref()
            .is_some_and(|value| value.generation == manifest.generation)
        {
            *pending = None;
        }
        Ok(())
    }

    pub async fn retry_pending_audio_manifest(&self) -> Result<bool, ModemError> {
        if self.call_active()
            || self.uploading.load(Ordering::Acquire)
            || self.syncing.load(Ordering::Acquire)
        {
            return Err(ModemError::Busy);
        }
        if self
            .pending_audio_manifest
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .is_none()
        {
            return Ok(false);
        }
        let _gate = self.audio_gate.lock().await;
        self.persist_pending_manifest().await?;
        Ok(true)
    }

    fn set_sync_state(&self, state: AudioSyncState) {
        self.catalog_ready
            .store(state == AudioSyncState::Ready, Ordering::Release);
        *self
            .sync_state
            .write()
            .unwrap_or_else(|lock| lock.into_inner()) = state;
    }
}

struct SyncGuard<'a>(&'a AtomicBool);

impl Drop for SyncGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl ManifestAudio {
    fn from_record(audio: &UploadedAudioRecord, sha256: String, owned: bool) -> Self {
        Self {
            id: audio.id.clone(),
            name: audio.name.clone(),
            module_path: audio.module_path.clone(),
            size: audio.size,
            duration_ms: audio.duration_ms,
            created_at_ms: audio.created_at_ms,
            sha256,
            owned,
        }
    }

    fn to_record(&self, is_current: bool) -> UploadedAudioRecord {
        UploadedAudioRecord {
            id: self.id.clone(),
            name: self.name.clone(),
            format: "AMR-NB".into(),
            size: self.size,
            module_path: self.module_path.clone(),
            duration_ms: self.duration_ms,
            created_at_ms: self.created_at_ms,
            state: "ready".into(),
            is_current,
        }
    }
}

pub(super) fn is_safe_audio_path(path: &str) -> bool {
    let Some(filename) = path
        .strip_prefix("c:/")
        .or_else(|| path.strip_prefix("C:/"))
    else {
        return false;
    };
    !filename.is_empty()
        && path.len() <= 55
        && filename.to_ascii_lowercase().ends_with(".amr")
        && filename.bytes().all(|byte| {
            byte.is_ascii() && !byte.is_ascii_control() && !b"\\/:*?\"<>|".contains(&byte)
        })
}

pub(super) fn is_managed_path(path: &str) -> bool {
    is_safe_audio_path(path)
        && path
            .rsplit_once('/')
            .is_some_and(|(_, name)| name.to_ascii_lowercase().starts_with("call_"))
}

async fn list_amr_files(tx: &mpsc::Sender<AtRequest>) -> Result<Vec<String>, ModemError> {
    let lines = actor_batch_lines(tx, vec!["AT+FSCD=C:".into(), "AT+FSLS=2".into()]).await?;
    let mut in_files = false;
    let mut reported_files = false;
    let mut files = Vec::new();
    for line in lines {
        if line == "+FSLS: FILES:" {
            in_files = true;
            reported_files = true;
        } else if line.starts_with("+FSLS:") {
            in_files = false;
        } else if in_files {
            let path = format!("c:/{line}");
            if is_safe_audio_path(&path) {
                files.push(line);
            }
        }
    }
    if !reported_files {
        return Err(ModemError::CommandRejected(
            "modem did not report the file catalog".into(),
        ));
    }
    Ok(files)
}

async fn module_file_size(tx: &mpsc::Sender<AtRequest>, filename: &str) -> Result<u64, ModemError> {
    let lines = actor_batch_lines(
        tx,
        vec!["AT+FSCD=C:".into(), format!("AT+FSATTRI=\"{filename}\"")],
    )
    .await?;
    lines
        .iter()
        .find_map(|line| line.strip_prefix("+FSATTRI:"))
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| ModemError::CommandRejected("modem did not report a file size".into()))
}

pub(super) async fn verify_module_audio(
    tx: &mpsc::Sender<AtRequest>,
    audio: &UploadedAudioRecord,
) -> Result<(), ModemError> {
    let filename = audio
        .module_path
        .rsplit_once('/')
        .map(|(_, filename)| filename)
        .ok_or_else(|| ModemError::Validation("current audio has an invalid modem path".into()))?;
    let actual = module_file_size(tx, filename).await?;
    if actual != audio.size {
        return Err(ModemError::Validation(
            "current audio is not synchronized with the modem".into(),
        ));
    }
    Ok(())
}

fn recovered_id(path: &str) -> String {
    if let Some(id) = path
        .rsplit_once('/')
        .map(|(_, name)| name)
        .and_then(|name| {
            name.strip_prefix("call_")
                .or_else(|| name.strip_prefix("CALL_"))
        })
        .and_then(|name| {
            name.strip_suffix(".amr")
                .or_else(|| name.strip_suffix(".AMR"))
        })
        .filter(|id| module_path(id).is_ok())
    {
        return id.into();
    }
    format!(
        "module-{}",
        &sha256(path.to_ascii_lowercase().as_bytes())[..32]
    )
}

fn managed_created_at(filename: &str) -> Option<i64> {
    let id = filename.strip_prefix("call_")?.strip_suffix(".amr")?;
    ulid::Ulid::from_string(id)
        .ok()
        .map(|value| value.timestamp_ms() as i64)
}

fn choose_current(
    manifest_current: &str,
    existing: &[UploadedAudioRecord],
    entries: &[ManifestAudio],
) -> String {
    if entries.iter().any(|entry| entry.id == manifest_current) {
        return manifest_current.into();
    }
    if let Some(current) = existing.iter().find(|audio| {
        audio.is_current
            && entries
                .iter()
                .any(|entry| entry.module_path.eq_ignore_ascii_case(&audio.module_path))
    }) {
        return current.id.clone();
    }
    entries
        .iter()
        .filter(|entry| entry.owned)
        .max_by_key(|entry| managed_created_at(entry.module_path.rsplit('/').next().unwrap_or("")))
        .or_else(|| entries.first())
        .map(|entry| entry.id.clone())
        .unwrap_or_default()
}

fn make_names_unique(entries: &mut [ManifestAudio]) {
    let mut used = HashSet::new();
    for entry in entries {
        let key = entry.name.trim().to_ascii_lowercase();
        if used.insert(key) {
            continue;
        }
        let filename = entry.module_path.rsplit('/').next().unwrap_or("audio.amr");
        let stem = filename
            .strip_suffix(".amr")
            .or_else(|| filename.strip_suffix(".AMR"))
            .unwrap_or(filename);
        let suffix = entry.id.chars().take(8).collect::<String>();
        entry.name = format!("{stem} [{suffix}].amr");
        used.insert(entry.name.to_ascii_lowercase());
    }
}

fn sha256(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn same_manifest_content(left: &AudioManifest, right: &AudioManifest) -> bool {
    left.version == right.version
        && left.current_id == right.current_id
        && left.audio == right.audio
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        hardware::{AtResponse, PayloadMode},
        settings::Settings,
        storage::Store,
    };
    use std::{sync::Arc, thread};

    fn entry(id: &str, name: &str, path: &str, owned: bool) -> ManifestAudio {
        ManifestAudio {
            id: id.into(),
            name: name.into(),
            module_path: path.into(),
            owned,
            ..Default::default()
        }
    }

    #[test]
    fn accepts_safe_external_amr_paths_but_not_nested_or_quoted_paths() {
        assert!(is_safe_audio_path("c:/greeting.amr"));
        assert!(is_safe_audio_path("C:/CALL_01ABC.AMR"));
        assert!(!is_safe_audio_path("c:/folder/greeting.amr"));
        assert!(!is_safe_audio_path("c:/bad\"name.amr"));
        assert!(!is_safe_audio_path("d:/greeting.amr"));
    }

    #[test]
    fn external_ids_are_stable_and_managed_ids_are_preserved() {
        assert_eq!(
            recovered_id("c:/call_01J00000000000000000000000.amr"),
            "01J00000000000000000000000"
        );
        assert_eq!(
            recovered_id("c:/Greeting.amr"),
            recovered_id("c:/greeting.amr")
        );
    }

    #[test]
    fn selection_prefers_manifest_then_existing_then_newest_managed() {
        let old = entry(
            "01J00000000000000000000000",
            "old.amr",
            "c:/call_01J00000000000000000000000.amr",
            true,
        );
        let new = entry(
            "01K00000000000000000000000",
            "new.amr",
            "c:/call_01K00000000000000000000000.amr",
            true,
        );
        assert_eq!(choose_current("", &[], &[old.clone(), new.clone()]), new.id);
        assert_eq!(
            choose_current(&old.id, &[], &[old.clone(), new.clone()]),
            old.id
        );
    }

    #[test]
    fn duplicate_display_names_receive_stable_amr_suffixes() {
        let mut entries = [
            entry("one", "voice.amr", "c:/one.amr", false),
            entry("two", "VOICE.AMR", "c:/two.amr", false),
        ];
        make_names_unique(&mut entries);
        assert_eq!(entries[0].name, "voice.amr");
        assert_eq!(entries[1].name, "two [two].amr");
    }

    #[tokio::test]
    async fn fresh_database_imports_and_selects_valid_external_amr() {
        let store = Arc::new(Store::memory().unwrap());
        let (tx, rx) = mpsc::channel();
        let manager = CallManager::new(
            tx,
            Arc::clone(&store),
            Arc::new(RwLock::new(Settings::default())),
        );
        let mut amr = crate::audio::AMR_NB_MAGIC.to_vec();
        amr.push(0x04);
        amr.extend([0_u8; 12]);
        let actor = thread::spawn(move || {
            let mut manifest_size = 0;
            for index in 0..10 {
                let request = rx.recv().unwrap();
                let result = match index {
                    0..=3 => Err(ModemError::CommandRejected("ERROR".into())),
                    4 => Ok(AtResponse::Lines(vec![
                        "+FSLS: FILES:".into(),
                        "legacy.amr".into(),
                    ])),
                    5 => Ok(AtResponse::Lines(vec!["+FSATTRI: 19".into()])),
                    6 => {
                        assert_eq!(
                            request.payload_mode,
                            PayloadMode::Download { max_bytes: 204_800 }
                        );
                        Ok(AtResponse::Data(amr.clone()))
                    }
                    7 => Ok(AtResponse::Lines(Vec::new())),
                    8 => {
                        manifest_size = request.payload.as_ref().map_or(0, Vec::len);
                        assert!(request.command.contains("a7670_audio_1.txt"));
                        Ok(AtResponse::Lines(Vec::new()))
                    }
                    9 => Ok(AtResponse::Lines(vec![format!(
                        "+FSATTRI: {manifest_size}"
                    )])),
                    _ => unreachable!(),
                };
                let _ = request.reply.send(result);
            }
        });

        assert_eq!(manager.reconcile_audio().await.unwrap(), 1);
        actor.join().unwrap();
        let imported = store.current_audio().unwrap().unwrap();
        assert_eq!(imported.name, "legacy.amr");
        assert_eq!(imported.duration_ms, 20);
        assert!(imported.id.starts_with("module-"));
        assert_eq!(manager.audio_sync_state(), AudioSyncState::Ready);
    }

    #[tokio::test]
    async fn rejected_manifest_is_non_fatal_and_retry_only_persists_txt_metadata() {
        let store = Arc::new(Store::memory().unwrap());
        let (tx, rx) = mpsc::channel();
        let manager = CallManager::new(
            tx,
            Arc::clone(&store),
            Arc::new(RwLock::new(Settings::default())),
        );
        let mut amr = crate::audio::AMR_NB_MAGIC.to_vec();
        amr.push(0x04);
        amr.extend([0_u8; 12]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let actor_seen = Arc::clone(&seen);
        let actor = thread::spawn(move || {
            let mut retry_size = 0;
            for index in 0..12 {
                let request = rx.recv().unwrap();
                actor_seen
                    .lock()
                    .unwrap()
                    .push(if request.command.is_empty() {
                        request.batch.join(" ")
                    } else {
                        request.command.clone()
                    });
                let response = match index {
                    0..=3 => Err(ModemError::CommandRejected("ERROR".into())),
                    4 => Ok(AtResponse::Lines(vec![
                        "+FSLS: FILES:".into(),
                        "greeting.amr".into(),
                    ])),
                    5 => Ok(AtResponse::Lines(vec!["+FSATTRI: 19".into()])),
                    6 => Ok(AtResponse::Data(amr.clone())),
                    7 => Ok(AtResponse::Lines(Vec::new())),
                    8 => Err(ModemError::CommandRejected("ERROR".into())),
                    9 => Ok(AtResponse::Lines(Vec::new())),
                    10 => {
                        retry_size = request.payload.as_ref().map_or(0, Vec::len);
                        Ok(AtResponse::Lines(Vec::new()))
                    }
                    11 => Ok(AtResponse::Lines(vec![format!("+FSATTRI: {retry_size}")])),
                    _ => unreachable!(),
                };
                let _ = request.reply.send(response);
            }
        });

        assert_eq!(manager.reconcile_audio().await.unwrap(), 1);
        assert_eq!(manager.audio_sync_state(), AudioSyncState::Ready);
        assert_eq!(store.list_audio().unwrap().len(), 1);
        assert!(manager.retry_pending_audio_manifest().await.unwrap());
        actor.join().unwrap();

        let seen = seen.lock().unwrap();
        let retry = &seen[9..];
        assert_eq!(retry.len(), 3);
        assert!(
            retry
                .iter()
                .all(|command| command.contains("a7670_audio_1.txt"))
        );
        assert!(!retry.iter().any(|command| {
            command.contains("FSLS")
                || command.contains("CFTRANTX")
                || command.contains("greeting.amr")
        }));
    }

    #[tokio::test]
    async fn transport_failure_preserves_previous_inventory() {
        let store = Arc::new(Store::memory().unwrap());
        store
            .save_current_audio(&UploadedAudioRecord {
                id: "existing".into(),
                name: "existing.amr".into(),
                format: "AMR-NB".into(),
                size: 19,
                module_path: "c:/existing.amr".into(),
                duration_ms: 20,
                created_at_ms: 1,
                state: "ready".into(),
                is_current: true,
            })
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let manager = CallManager::new(
            tx,
            Arc::clone(&store),
            Arc::new(RwLock::new(Settings::default())),
        );
        let actor = thread::spawn(move || {
            for index in 0..6 {
                let request = rx.recv().unwrap();
                let response = match index {
                    0..=3 => Err(ModemError::CommandRejected("ERROR".into())),
                    4 => Ok(AtResponse::Lines(vec![
                        "+FSLS: FILES:".into(),
                        "existing.amr".into(),
                    ])),
                    5 => Err(ModemError::Timeout),
                    _ => unreachable!(),
                };
                let _ = request.reply.send(response);
            }
        });

        assert_eq!(manager.reconcile_audio().await, Err(ModemError::Timeout));
        actor.join().unwrap();
        assert_eq!(manager.audio_sync_state(), AudioSyncState::Deferred);
        assert_eq!(store.current_audio().unwrap().unwrap().id, "existing");
    }

    #[tokio::test]
    async fn manifest_loading_uses_highest_generation_across_txt_and_legacy_json() {
        let store = Arc::new(Store::memory().unwrap());
        let (tx, rx) = mpsc::channel();
        let manager = CallManager::new(tx, store, Arc::new(RwLock::new(Settings::default())));
        let manifests = [
            ("txt-zero", 2_u64),
            ("txt-one", 6),
            ("json-zero", 8),
            ("json-one", 4),
        ]
        .map(|(current_id, generation)| {
            serde_json::to_vec(&AudioManifest {
                version: 1,
                generation,
                current_id: current_id.into(),
                audio: Vec::new(),
            })
            .unwrap()
        });
        let actor = thread::spawn(move || {
            for data in manifests {
                let size_request = rx.recv().unwrap();
                assert!(size_request.command.is_empty());
                let _ = size_request.reply.send(Ok(AtResponse::Lines(vec![format!(
                    "+FSATTRI: {}",
                    data.len()
                )])));
                let download_request = rx.recv().unwrap();
                assert!(matches!(
                    download_request.payload_mode,
                    PayloadMode::Download { .. }
                ));
                let _ = download_request.reply.send(Ok(AtResponse::Data(data)));
            }
        });

        let loaded = manager.load_manifest().await.unwrap();
        actor.join().unwrap();
        assert_eq!(loaded.generation, 8);
        assert_eq!(loaded.current_id, "json-zero");
    }
}
