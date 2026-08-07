use crate::{ModemError, settings::Settings};
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, sync::Mutex};

mod delivery;
mod models;
use delivery::*;
mod audio;
mod balance;
mod calls;
mod integration;
mod schema;
mod sms;
pub use models::{
    BalanceRecord, CallRecord, CommunicationReservation, IntegrationSettings, RestCommunication,
    SmsRecord, UploadedAudioRecord, WebhookAttempt,
};

pub struct Store(Mutex<Connection>);

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ModemError> {
        let connection = Connection::open(path).map_err(db_error)?;
        let store = Self(Mutex::new(connection));
        store.migrate()?;
        Ok(store)
    }

    pub fn memory() -> Result<Self, ModemError> {
        Self::open(":memory:")
    }

    pub fn load_settings(&self) -> Result<Settings, ModemError> {
        let json: Option<String> = self
            .connection()?
            .query_row("SELECT json FROM settings WHERE id=1", [], |row| row.get(0))
            .optional()
            .map_err(db_error)?;
        json.map(|value| serde_json::from_str(&value).map_err(db_error))
            .transpose()
            .map(|value| value.unwrap_or_default())
    }

    pub fn save_settings(&self, settings: &Settings, now_ms: i64) -> Result<(), ModemError> {
        settings.validate()?;
        let json = serde_json::to_string(settings).map_err(db_error)?;
        self.connection()?.execute("INSERT INTO settings(id,json,updated_at_ms) VALUES(1,?1,?2) ON CONFLICT(id) DO UPDATE SET json=excluded.json,updated_at_ms=excluded.updated_at_ms", params![json, now_ms]).map_err(db_error)?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64, ModemError> {
        self.connection()?
            .query_row("SELECT max(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .map_err(db_error)
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ModemError> {
        self.0
            .lock()
            .map_err(|_| ModemError::Persistence("database lock poisoned".into()))
    }
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>, ModemError> {
    let mut s = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(db_error)?;
    s.query_map([], |r| r.get(1))
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)
}

fn audio_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UploadedAudioRecord> {
    Ok(UploadedAudioRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        format: row.get(2)?,
        size: row.get(3)?,
        module_path: row.get(4)?,
        duration_ms: row.get(5)?,
        created_at_ms: row.get(6)?,
        state: "ready".into(),
        is_current: row.get(7)?,
    })
}

fn db_error(error: impl std::fmt::Display) -> ModemError {
    ModemError::Persistence(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migrates_and_round_trips_settings() {
        let store = Store::memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), 10);
        let mut expected = Settings::default();
        expected.port_override = Some("COM6".into());
        store.save_settings(&expected, 42).unwrap();
        assert_eq!(store.load_settings().unwrap(), expected);
    }
    #[test]
    fn sms_sync_deduplicates_retains_and_handles_index_reuse() {
        let s = Store::memory().unwrap();
        let mut a = SmsRecord {
            id: "a".into(),
            source: "sim".into(),
            storage: "SM".into(),
            storage_index: 1,
            fingerprint: "one".into(),
            present_on_modem: true,
            ..Default::default()
        };
        s.sync_sms(&[a.clone()], 1).unwrap();
        s.sync_sms(&[a.clone()], 2).unwrap();
        assert_eq!(s.list_sms(10).unwrap().len(), 1);
        a.id = "b".into();
        a.fingerprint = "two".into();
        s.sync_sms(&[a], 3).unwrap();
        let x = s.list_sms(10).unwrap();
        assert_eq!(x.len(), 2);
        assert_eq!(x.iter().filter(|r| r.present_on_modem).count(), 1);
    }
    #[test]
    fn read_state_transition_updates_the_canonical_row() {
        let store = Store::memory().unwrap();
        let mut record = SmsRecord {
            id: "unread".into(),
            source: "sim".into(),
            storage: "SM".into(),
            storage_index: 4,
            fingerprint: "immutable".into(),
            state: "unread".into(),
            modem_status: "REC UNREAD".into(),
            storage_indices: vec![4],
            ..Default::default()
        };
        store.sync_sms(&[record.clone()], 1).unwrap();
        record.id = "read-refresh".into();
        record.state = "read".into();
        record.modem_status = "REC READ".into();
        store.sync_sms(&[record], 2).unwrap();
        let rows = store.list_sms(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "read");
        assert_eq!(rows[0].modem_status, "REC READ");
    }

    #[test]
    fn stored_sent_copy_is_suppressed_in_favor_of_app_submission() {
        let store = Store::memory().unwrap();
        store
            .save_sms(&SmsRecord {
                id: "app".into(),
                direction: "outbound".into(),
                peer: "+84912345678".into(),
                body: "hello".into(),
                state: "submitted".into(),
                message_reference: "42".into(),
                kind: "submitted".into(),
                source: "app".into(),
                created_at_ms: 10,
                ..Default::default()
            })
            .unwrap();
        store
            .sync_sms(
                &[SmsRecord {
                    id: "copy".into(),
                    direction: "outbound".into(),
                    peer: "0912345678".into(),
                    body: "hello".into(),
                    state: "submitted".into(),
                    message_reference: "42".into(),
                    kind: "stored".into(),
                    source: "sim".into(),
                    storage: "SM".into(),
                    storage_index: 5,
                    storage_indices: vec![5],
                    modem_status: "STO SENT".into(),
                    fingerprint: "copy".into(),
                    ..Default::default()
                }],
                20,
            )
            .unwrap();
        let rows = store.list_sms(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "app");
        assert!(rows[0].present_on_modem);
    }

    #[test]
    fn startup_recovery_marks_sending_as_unknown() {
        let store = Store::memory().unwrap();
        store
            .save_sms(&SmsRecord {
                id: "attempt".into(),
                state: "sending".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(store.recover_interrupted_sms().unwrap(), 1);
        assert_eq!(store.list_sms(1).unwrap()[0].state, "send-unknown");
    }

    #[test]
    fn sms_reset_migration_preserves_balance_calls_and_audio() {
        let store = Store::memory().unwrap();
        store
            .save_sms(&SmsRecord {
                id: "old-sms".into(),
                ..Default::default()
            })
            .unwrap();
        store
            .save_balance(&BalanceRecord {
                id: "balance".into(),
                sms_id: "old-sms".into(),
                ..Default::default()
            })
            .unwrap();
        store
            .save_call(&CallRecord {
                id: "call".into(),
                ..Default::default()
            })
            .unwrap();
        store
            .save_current_audio(&UploadedAudioRecord {
                id: "audio".into(),
                name: "call.amr".into(),
                module_path: "call.amr".into(),
                ..Default::default()
            })
            .unwrap();
        store
            .connection()
            .unwrap()
            .execute("DELETE FROM schema_migrations WHERE version=7", [])
            .unwrap();
        store.migrate().unwrap();
        assert!(store.list_sms(10).unwrap().is_empty());
        assert_eq!(store.list_balances(10).unwrap()[0].sms_id, "");
        assert_eq!(store.list_calls(10).unwrap().len(), 1);
        assert_eq!(store.list_audio().unwrap().len(), 1);
    }

    #[test]
    fn request_id_migration_backfills_communications_and_preserves_outbox() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY);
             INSERT INTO schema_migrations(version) VALUES(9);
             CREATE TABLE rest_communications(
               id TEXT PRIMARY KEY,record_id TEXT NOT NULL UNIQUE,channel TEXT NOT NULL,owner TEXT NOT NULL,
               destination TEXT NOT NULL,content TEXT NOT NULL,encrypted INTEGER NOT NULL,
               payload_fingerprint TEXT NOT NULL,status TEXT NOT NULL,created_at_ms INTEGER NOT NULL,
               sent_at_ms INTEGER,delivered_at_ms INTEGER,failed_at_ms INTEGER,failure_reason TEXT NOT NULL DEFAULT '');
             CREATE TABLE webhook_outbox(
               id INTEGER PRIMARY KEY AUTOINCREMENT,communication_id TEXT NOT NULL,event_type TEXT NOT NULL,
               payload TEXT NOT NULL,attempt_count INTEGER NOT NULL DEFAULT 0,next_attempt_at_ms INTEGER NOT NULL,
               last_error TEXT NOT NULL DEFAULT '',completed_at_ms INTEGER,
               UNIQUE(communication_id,event_type));
             INSERT INTO rest_communications(id,record_id,channel,owner,destination,content,encrypted,payload_fingerprint,status,created_at_ms)
               VALUES('old-id','old-id','sms','desk','+84912345678','hello',0,'fp','sent',1);
             INSERT INTO webhook_outbox(communication_id,event_type,payload,next_attempt_at_ms)
               VALUES('old-id','communication.sent','{}',1);",
        ).unwrap();
        let store = Store(Mutex::new(connection));
        store.migrate().unwrap();
        assert_eq!(store.schema_version().unwrap(), 10);
        assert_eq!(
            store
                .connection()
                .unwrap()
                .query_row(
                    "SELECT request_id FROM rest_communications WHERE id='old-id'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "old-id"
        );
        assert_eq!(
            store
                .connection()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM webhook_outbox", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn balance_links_sms() {
        let s = Store::memory().unwrap();
        s.save_balance(&BalanceRecord {
            id: "b".into(),
            raw: "multi\nline".into(),
            sms_id: "s".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.list_balances(10).unwrap()[0].sms_id, "s");
    }

    #[test]
    fn multipart_sync_suppresses_only_matching_legacy_fragments() {
        let s = Store::memory().unwrap();
        let legacy = |id: &str, index, body: &str| SmsRecord {
            id: id.into(),
            source: "sim".into(),
            storage: "SM".into(),
            storage_index: index,
            storage_indices: vec![index],
            peer: "191".into(),
            body: body.into(),
            encoding: "UCS2".into(),
            dcs: 8,
            modem_timestamp: "26/08/03,10:00:00+28".into(),
            fingerprint: id.into(),
            part_count: 1,
            parts_received: 1,
            multipart_complete: true,
            ..Default::default()
        };
        s.sync_sms(
            &[
                legacy("old1", 7, "00580069006E0020"),
                legacy("old2", 8, "006300680061006F"),
            ],
            1,
        )
        .unwrap();
        let logical = SmsRecord {
            id: "new".into(),
            source: "sim".into(),
            storage: "SM".into(),
            storage_index: 7,
            storage_indices: vec![7, 8],
            part_payloads: vec!["Xin ".into(), "chao".into()],
            part_timestamps: vec!["26/08/03,10:00:00".into(), "26/08/03,10:00:00".into()],
            peer: "191".into(),
            body: "Xin chao".into(),
            encoding: "UCS2".into(),
            dcs: 8,
            modem_timestamp: "26/08/03,10:00:00".into(),
            fingerprint: "logical".into(),
            part_count: 2,
            parts_received: 2,
            multipart_complete: true,
            ..Default::default()
        };
        s.sync_sms(&[logical], 2).unwrap();
        let visible = s.list_sms(10).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].body, "Xin chao");
        let reused = legacy("reuse", 8, "004E00650077");
        s.sync_sms(&[reused], 3).unwrap();
        assert!(s.list_sms(10).unwrap().iter().any(|x| x.id == "reuse"));
    }

    #[test]
    fn multipart_identity_survives_newly_discovered_fragments() {
        let store = Store::memory().unwrap();
        let mut record = SmsRecord {
            id: "partial".into(),
            source: "sim".into(),
            storage: "SM".into(),
            storage_index: 8,
            storage_indices: vec![8],
            fingerprint: "concat-9".into(),
            body: "two".into(),
            part_count: 2,
            parts_received: 1,
            multipart_complete: false,
            ..Default::default()
        };
        store.sync_sms(&[record.clone()], 1).unwrap();
        record.id = "complete".into();
        record.storage_index = 7;
        record.storage_indices = vec![7, 8];
        record.body = "onetwo".into();
        record.parts_received = 2;
        record.multipart_complete = true;
        store.sync_sms(&[record], 2).unwrap();
        let rows = store.list_sms(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body, "onetwo");
        assert!(rows[0].multipart_complete);
    }

    #[test]
    fn delivery_reports_update_latest_matching_outbound_message() {
        let s = Store::memory().unwrap();
        s.save_sms(&SmsRecord {
            id: "sent".into(),
            direction: "outbound".into(),
            peer: "+84912345678".into(),
            message_reference: "42".into(),
            state: "submitted".into(),
            kind: "submitted".into(),
            source: "app".into(),
            created_at_ms: 10,
            ..Default::default()
        })
        .unwrap();
        let report = |id: &str, index, peer: &str, status: &str| SmsRecord {
            id: id.into(),
            direction: "inbound".into(),
            peer: peer.into(),
            message_reference: "42".into(),
            state: "status-report".into(),
            kind: "status-report".into(),
            source: "sim".into(),
            storage: "SM".into(),
            storage_index: index,
            storage_indices: vec![index],
            delivery_status: status.into(),
            fingerprint: id.into(),
            part_count: 1,
            parts_received: 1,
            multipart_complete: true,
            ..Default::default()
        };
        s.sync_sms(&[report("pending", 1, "0912345678", "0x20")], 20)
            .unwrap();
        assert_eq!(s.list_sms(10).unwrap().len(), 1);
        assert_eq!(
            s.list_sms(10)
                .unwrap()
                .iter()
                .find(|x| x.id == "sent")
                .unwrap()
                .state,
            "delivery-pending"
        );
        s.sync_sms(&[report("delivered", 2, "0084912345678", "0x00")], 30)
            .unwrap();
        let sent = s
            .list_sms(10)
            .unwrap()
            .into_iter()
            .find(|x| x.id == "sent")
            .unwrap();
        assert_eq!(sent.state, "delivered");
        assert_eq!(sent.delivery_status, "0x00");
    }

    #[test]
    fn archived_modem_sms_remains_in_history_but_is_no_longer_marked_present() {
        let store = Store::memory().unwrap();
        let record = SmsRecord {
            id: "received".into(),
            direction: "inbound".into(),
            peer: "+66812345678".into(),
            body: "hello".into(),
            state: "read".into(),
            kind: "received".into(),
            source: "sim".into(),
            storage: "SM".into(),
            storage_index: 4,
            storage_indices: vec![4],
            fingerprint: "received-fingerprint".into(),
            present_on_modem: true,
            ..Default::default()
        };
        store.sync_sms(std::slice::from_ref(&record), 10).unwrap();
        store.mark_sms_archived(&[record]).unwrap();

        let archived = store.list_sms(1).unwrap().pop().unwrap();
        assert_eq!(archived.body, "hello");
        assert!(!archived.present_on_modem);
    }

    #[test]
    fn permanent_status_report_is_delivery_failure() {
        assert_eq!(delivery_state("0x00"), "delivered");
        assert_eq!(delivery_state("0x01"), "delivery-unknown");
        assert_eq!(delivery_state("0x1F"), "delivery-unknown");
        assert_eq!(delivery_state("0x20"), "delivery-pending");
        assert_eq!(delivery_state("0x3F"), "delivery-pending");
        assert_eq!(delivery_state("0x40"), "delivery-failed");
        assert_eq!(delivery_state("0x7F"), "delivery-failed");
        assert_eq!(delivery_state("0x80"), "delivery-unknown");
        assert_eq!(delivery_state("0xFF"), "delivery-unknown");
        assert_eq!(delivery_state("malformed"), "delivery-unknown");
    }

    #[test]
    fn pending_delivery_expires_but_late_terminal_report_wins() {
        let store = Store::memory().unwrap();
        store
            .save_sms(&SmsRecord {
                id: "sent".into(),
                direction: "outbound".into(),
                peer: "+66812345678".into(),
                message_reference: "42".into(),
                state: "delivery-pending".into(),
                kind: "submitted".into(),
                source: "app".into(),
                created_at_ms: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(store.expire_delivery_reports(86_400_001).unwrap(), 1);
        assert_eq!(store.list_sms(1).unwrap()[0].state, "delivery-unknown");
        store
            .apply_direct_delivery_report(
                &SmsRecord {
                    id: "late".into(),
                    direction: "inbound".into(),
                    peer: "+66812345678".into(),
                    message_reference: "42".into(),
                    kind: "status-report".into(),
                    delivery_status: "0x00".into(),
                    synchronized_at_ms: 86_400_002,
                    ..Default::default()
                },
                86_400_002,
            )
            .unwrap();
        assert_eq!(store.list_sms(1).unwrap()[0].state, "delivered");
    }

    #[test]
    fn ambiguous_reused_message_reference_is_not_correlated() {
        let store = Store::memory().unwrap();
        for (id, created_at_ms) in [("first", 1_000), ("second", 2_000)] {
            store
                .save_sms(&SmsRecord {
                    id: id.into(),
                    direction: "outbound".into(),
                    peer: "+66812345678".into(),
                    message_reference: "7".into(),
                    state: "submitted".into(),
                    kind: "submitted".into(),
                    source: "app".into(),
                    created_at_ms,
                    ..Default::default()
                })
                .unwrap();
        }
        store
            .sync_sms(
                &[SmsRecord {
                    id: "report".into(),
                    direction: "inbound".into(),
                    peer: "+66812345678".into(),
                    message_reference: "7".into(),
                    kind: "status-report".into(),
                    source: "sim".into(),
                    storage: "SM".into(),
                    storage_index: 8,
                    storage_indices: vec![8],
                    delivery_status: "0x00".into(),
                    fingerprint: "report".into(),
                    part_count: 1,
                    parts_received: 1,
                    multipart_complete: true,
                    ..Default::default()
                }],
                3_000,
            )
            .unwrap();
        assert!(
            store
                .list_sms(10)
                .unwrap()
                .iter()
                .all(|sms| sms.state == "submitted")
        );
    }

    #[test]
    fn current_audio_replacement_and_call_history_survive_queries() {
        let store = Store::memory().unwrap();
        let audio = |id: &str, created_at_ms| UploadedAudioRecord {
            id: id.into(),
            name: format!("{id}.amr"),
            format: "AMR-NB".into(),
            size: 42,
            module_path: format!("c:/call_{id}.amr"),
            duration_ms: 20,
            created_at_ms,
            state: "ready".into(),
            is_current: false,
        };
        store.save_current_audio(&audio("old", 1)).unwrap();
        store.save_current_audio(&audio("new", 2)).unwrap();
        assert_eq!(store.current_audio().unwrap().unwrap().id, "new");
        assert_eq!(store.list_audio().unwrap().len(), 2);
        assert_eq!(store.select_audio("old").unwrap().id, "old");

        let mut replacement = audio("replacement", 3);
        replacement.name = " OLD.AMR ".into();
        let replaced = store.audio_named("old.amr").unwrap().unwrap();
        store
            .replace_and_select_audio(&replacement, Some(&replaced.id))
            .unwrap();
        let library = store.list_audio().unwrap();
        assert_eq!(library.len(), 2);
        assert_eq!(library[0].id, "replacement");
        assert!(library.iter().any(|entry| entry.id == "new"));

        let call = CallRecord {
            id: "call".into(),
            peer: "+66000000000".into(),
            state: "playing".into(),
            audio_id: "replacement".into(),
            created_at_ms: 3,
            ..Default::default()
        };
        store.save_call(&call).unwrap();
        assert_eq!(store.recover_interrupted_calls(4).unwrap(), 1);
        let recovered = &store.list_calls(10).unwrap()[0];
        assert_eq!(recovered.state, "failed");
        assert_eq!(recovered.end_reason, "modem-lost");
    }
}
