use crate::{ModemError, settings::Settings};
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, sync::Mutex};

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SmsRecord {
    pub id: String,
    pub direction: String,
    pub peer: String,
    pub body: String,
    pub state: String,
    pub message_reference: String,
    pub cause: String,
    pub created_at_ms: i64,
    pub kind: String,
    pub source: String,
    pub storage: String,
    pub storage_index: i32,
    pub storage_indices: Vec<i32>,
    pub part_count: i32,
    pub parts_received: i32,
    pub multipart_complete: bool,
    #[serde(skip)]
    pub part_payloads: Vec<String>,
    #[serde(skip)]
    pub part_timestamps: Vec<String>,
    pub modem_status: String,
    pub modem_timestamp: String,
    pub encoding: String,
    pub dcs: i32,
    pub length: i32,
    pub service_center: String,
    pub delivery_status: String,
    pub synchronized_at_ms: i64,
    pub present_on_modem: bool,
    pub fingerprint: String,
}
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BalanceRecord {
    pub id: String,
    pub raw: String,
    pub value: Option<f64>,
    pub currency: String,
    pub error: String,
    pub created_at_ms: i64,
    pub sms_id: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UploadedAudioRecord {
    pub id: String,
    pub name: String,
    pub format: String,
    pub size: u64,
    pub module_path: String,
    pub duration_ms: u64,
    pub created_at_ms: i64,
    pub state: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CallRecord {
    pub id: String,
    pub peer: String,
    pub state: String,
    pub audio_id: String,
    pub error: String,
    pub duration_seconds: u32,
    pub created_at_ms: i64,
    pub answer_classification: String,
    pub end_reason: String,
    pub connected_at_ms: i64,
    pub ended_at_ms: i64,
    pub release_cause: String,
}

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

    fn migrate(&self) -> Result<(), ModemError> {
        self.connection()?.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY);
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (1);
             CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY CHECK(id=1), json TEXT NOT NULL, updated_at_ms INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS sms(id TEXT PRIMARY KEY, direction TEXT NOT NULL, peer TEXT NOT NULL, body TEXT NOT NULL, state TEXT NOT NULL, message_reference TEXT NOT NULL DEFAULT '', cause TEXT NOT NULL DEFAULT '', created_at_ms INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS sms_created ON sms(created_at_ms DESC, id DESC);
             CREATE TABLE IF NOT EXISTS calls(id TEXT PRIMARY KEY, peer TEXT NOT NULL, state TEXT NOT NULL, audio_id TEXT NOT NULL, error TEXT NOT NULL DEFAULT '', duration_seconds INTEGER NOT NULL DEFAULT 0, created_at_ms INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS calls_created ON calls(created_at_ms DESC, id DESC);
             CREATE TABLE IF NOT EXISTS balance_checks(id TEXT PRIMARY KEY, raw TEXT NOT NULL, value REAL, currency TEXT NOT NULL, error TEXT NOT NULL DEFAULT '', created_at_ms INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS balance_created ON balance_checks(created_at_ms DESC, id DESC);
             CREATE TABLE IF NOT EXISTS uploaded_audio(id TEXT PRIMARY KEY, name TEXT NOT NULL, format TEXT NOT NULL, size INTEGER NOT NULL, module_path TEXT NOT NULL, created_at_ms INTEGER NOT NULL);"
        ).map_err(db_error)?;
        let connection = self.connection()?;
        let columns = {
            let mut statement = connection
                .prepare("PRAGMA table_info(calls)")
                .map_err(db_error)?;
            statement
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(db_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_error)?
        };
        for (name, definition) in [
            ("answer_classification", "TEXT NOT NULL DEFAULT 'unknown'"),
            ("end_reason", "TEXT NOT NULL DEFAULT 'none'"),
            ("forwarding_notification_seen", "INTEGER NOT NULL DEFAULT 0"),
            ("connected_at_ms", "INTEGER"),
            ("ended_at_ms", "INTEGER"),
            ("release_cause", "TEXT NOT NULL DEFAULT ''"),
        ] {
            if !columns.iter().any(|column| column == name) {
                connection
                    .execute(
                        &format!("ALTER TABLE calls ADD COLUMN {name} {definition}"),
                        [],
                    )
                    .map_err(db_error)?;
            }
        }
        connection
            .execute(
                "INSERT OR IGNORE INTO schema_migrations(version) VALUES (2)",
                [],
            )
            .map_err(db_error)?;
        let sms_columns = table_columns(&connection, "sms")?;
        for (name, definition) in [
            ("kind", "TEXT NOT NULL DEFAULT ''"),
            ("source", "TEXT NOT NULL DEFAULT 'app'"),
            ("storage", "TEXT NOT NULL DEFAULT ''"),
            ("storage_index", "INTEGER NOT NULL DEFAULT -1"),
            ("modem_status", "TEXT NOT NULL DEFAULT ''"),
            ("modem_timestamp", "TEXT NOT NULL DEFAULT ''"),
            ("encoding", "TEXT NOT NULL DEFAULT ''"),
            ("dcs", "INTEGER NOT NULL DEFAULT -1"),
            ("length", "INTEGER NOT NULL DEFAULT 0"),
            ("service_center", "TEXT NOT NULL DEFAULT ''"),
            ("delivery_status", "TEXT NOT NULL DEFAULT ''"),
            ("synchronized_at_ms", "INTEGER NOT NULL DEFAULT 0"),
            ("present_on_modem", "INTEGER NOT NULL DEFAULT 0"),
            ("fingerprint", "TEXT NOT NULL DEFAULT ''"),
            ("storage_indices", "TEXT NOT NULL DEFAULT '[]'"),
            ("part_count", "INTEGER NOT NULL DEFAULT 1"),
            ("parts_received", "INTEGER NOT NULL DEFAULT 1"),
            ("multipart_complete", "INTEGER NOT NULL DEFAULT 1"),
            ("superseded", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            if !sms_columns.iter().any(|x| x == name) {
                connection
                    .execute(
                        &format!("ALTER TABLE sms ADD COLUMN {name} {definition}"),
                        [],
                    )
                    .map_err(db_error)?;
            }
        }
        let balance_columns = table_columns(&connection, "balance_checks")?;
        if !balance_columns.iter().any(|x| x == "sms_id") {
            connection
                .execute(
                    "ALTER TABLE balance_checks ADD COLUMN sms_id TEXT NOT NULL DEFAULT ''",
                    [],
                )
                .map_err(db_error)?;
        }
        connection.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS sms_sim_fingerprint ON sms(storage,storage_index,fingerprint) WHERE source='sim'; CREATE TABLE IF NOT EXISTS sms_parts(logical_id TEXT NOT NULL,storage_index INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(logical_id,storage_index)); INSERT OR IGNORE INTO schema_migrations(version) VALUES (3); INSERT OR IGNORE INTO schema_migrations(version) VALUES (4);").map_err(db_error)?;
        let audio_columns = table_columns(&connection, "uploaded_audio")?;
        for (name, definition) in [
            ("duration_ms", "INTEGER NOT NULL DEFAULT 0"),
            ("is_current", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            if !audio_columns.iter().any(|column| column == name) {
                connection
                    .execute(
                        &format!("ALTER TABLE uploaded_audio ADD COLUMN {name} {definition}"),
                        [],
                    )
                    .map_err(db_error)?;
            }
        }
        connection.execute_batch(
            "UPDATE uploaded_audio SET is_current=1 WHERE id=(SELECT id FROM uploaded_audio ORDER BY created_at_ms DESC,id DESC LIMIT 1) AND NOT EXISTS(SELECT 1 FROM uploaded_audio WHERE is_current=1);
             CREATE UNIQUE INDEX IF NOT EXISTS uploaded_audio_one_current ON uploaded_audio(is_current) WHERE is_current=1;
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (5);",
        ).map_err(db_error)?;
        Ok(())
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

    pub fn current_audio(&self) -> Result<Option<UploadedAudioRecord>, ModemError> {
        self.connection()?
            .query_row(
                "SELECT id,name,format,size,module_path,duration_ms,created_at_ms FROM uploaded_audio WHERE is_current=1 LIMIT 1",
                [],
                |row| {
                    Ok(UploadedAudioRecord {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        format: row.get(2)?,
                        size: row.get(3)?,
                        module_path: row.get(4)?,
                        duration_ms: row.get(5)?,
                        created_at_ms: row.get(6)?,
                        state: "ready".into(),
                    })
                },
            )
            .optional()
            .map_err(db_error)
    }

    pub fn save_current_audio(&self, audio: &UploadedAudioRecord) -> Result<(), ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "UPDATE uploaded_audio SET is_current=0 WHERE is_current=1",
                [],
            )
            .map_err(db_error)?;
        transaction.execute(
            "INSERT INTO uploaded_audio(id,name,format,size,module_path,duration_ms,created_at_ms,is_current) VALUES(?1,?2,?3,?4,?5,?6,?7,1)",
            params![audio.id,audio.name,audio.format,audio.size,audio.module_path,audio.duration_ms,audio.created_at_ms],
        ).map_err(db_error)?;
        transaction.commit().map_err(db_error)
    }

    pub fn save_call(&self, call: &CallRecord) -> Result<(), ModemError> {
        self.connection()?.execute(
            "INSERT INTO calls(id,peer,state,audio_id,error,duration_seconds,created_at_ms,answer_classification,end_reason,connected_at_ms,ended_at_ms,release_cause) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,NULLIF(?10,0),NULLIF(?11,0),?12) ON CONFLICT(id) DO UPDATE SET state=excluded.state,error=excluded.error,duration_seconds=excluded.duration_seconds,answer_classification=excluded.answer_classification,end_reason=excluded.end_reason,connected_at_ms=excluded.connected_at_ms,ended_at_ms=excluded.ended_at_ms,release_cause=excluded.release_cause",
            params![call.id,call.peer,call.state,call.audio_id,call.error,call.duration_seconds,call.created_at_ms,call.answer_classification,call.end_reason,call.connected_at_ms,call.ended_at_ms,call.release_cause],
        ).map_err(db_error)?;
        Ok(())
    }

    pub fn list_calls(&self, limit: usize) -> Result<Vec<CallRecord>, ModemError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,peer,state,audio_id,error,duration_seconds,created_at_ms,answer_classification,end_reason,COALESCE(connected_at_ms,0),COALESCE(ended_at_ms,0),release_cause FROM calls ORDER BY created_at_ms DESC,id DESC LIMIT ?1",
        ).map_err(db_error)?;
        statement
            .query_map([limit as i64], |row| {
                Ok(CallRecord {
                    id: row.get(0)?,
                    peer: row.get(1)?,
                    state: row.get(2)?,
                    audio_id: row.get(3)?,
                    error: row.get(4)?,
                    duration_seconds: row.get(5)?,
                    created_at_ms: row.get(6)?,
                    answer_classification: row.get(7)?,
                    end_reason: row.get(8)?,
                    connected_at_ms: row.get(9)?,
                    ended_at_ms: row.get(10)?,
                    release_cause: row.get(11)?,
                })
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)
    }

    pub fn recover_interrupted_calls(&self, now_ms: i64) -> Result<usize, ModemError> {
        self.connection()?.execute(
            "UPDATE calls SET state='failed',error='daemon restarted during call',end_reason='modem-lost',ended_at_ms=?1 WHERE state IN ('waiting-for-answer','playback-delay','playing')",
            [now_ms],
        ).map_err(db_error)
    }

    pub fn save_sms(&self, r: &SmsRecord) -> Result<(), ModemError> {
        self.connection()?.execute("INSERT INTO sms(id,direction,peer,body,state,message_reference,cause,created_at_ms,kind,source,storage,storage_index,modem_status,modem_timestamp,encoding,dcs,length,service_center,delivery_status,synchronized_at_ms,present_on_modem,fingerprint,storage_indices,part_count,parts_received,multipart_complete) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26) ON CONFLICT(id) DO UPDATE SET state=excluded.state,delivery_status=excluded.delivery_status,synchronized_at_ms=excluded.synchronized_at_ms,present_on_modem=excluded.present_on_modem",params![r.id,r.direction,r.peer,r.body,r.state,r.message_reference,r.cause,r.created_at_ms,r.kind,r.source,r.storage,r.storage_index,r.modem_status,r.modem_timestamp,r.encoding,r.dcs,r.length,r.service_center,r.delivery_status,r.synchronized_at_ms,r.present_on_modem,r.fingerprint,serde_json::to_string(&r.storage_indices).map_err(db_error)?,r.part_count,r.parts_received,r.multipart_complete]).map_err(db_error)?;
        Ok(())
    }
    pub fn sync_sms(&self, records: &[SmsRecord], now: i64) -> Result<(), ModemError> {
        let mut c = self.connection()?;
        let tx = c.transaction().map_err(db_error)?;
        tx.execute("UPDATE sms SET present_on_modem=0,synchronized_at_ms=?1 WHERE source='sim' AND storage='SM'",[now]).map_err(db_error)?;
        for r in records {
            let indices = serde_json::to_string(&r.storage_indices).map_err(db_error)?;
            tx.execute("INSERT INTO sms(id,direction,peer,body,state,message_reference,cause,created_at_ms,kind,source,storage,storage_index,modem_status,modem_timestamp,encoding,dcs,length,service_center,delivery_status,synchronized_at_ms,present_on_modem,fingerprint,storage_indices,part_count,parts_received,multipart_complete,superseded) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,1,?21,?22,?23,?24,?25,0) ON CONFLICT(storage,storage_index,fingerprint) WHERE source='sim' DO UPDATE SET direction=excluded.direction,peer=excluded.peer,body=excluded.body,state=excluded.state,kind=excluded.kind,modem_status=excluded.modem_status,modem_timestamp=excluded.modem_timestamp,encoding=excluded.encoding,dcs=excluded.dcs,length=excluded.length,service_center=excluded.service_center,delivery_status=excluded.delivery_status,synchronized_at_ms=excluded.synchronized_at_ms,present_on_modem=1,storage_indices=excluded.storage_indices,part_count=excluded.part_count,parts_received=excluded.parts_received,multipart_complete=excluded.multipart_complete,superseded=0",params![r.id,r.direction,r.peer,r.body,r.state,r.message_reference,r.cause,r.created_at_ms,r.kind,r.source,r.storage,r.storage_index,r.modem_status,r.modem_timestamp,r.encoding,r.dcs,r.length,r.service_center,r.delivery_status,now,r.fingerprint,indices,r.part_count,r.parts_received,r.multipart_complete]).map_err(db_error)?;
            let logical_id:String=tx.query_row("SELECT id FROM sms WHERE source='sim' AND storage=?1 AND storage_index=?2 AND fingerprint=?3",params![r.storage,r.storage_index,r.fingerprint],|row|row.get(0)).map_err(db_error)?;
            tx.execute("DELETE FROM sms_parts WHERE logical_id=?1", [&logical_id])
                .map_err(db_error)?;
            for (index, payload) in r.storage_indices.iter().zip(&r.part_payloads) {
                tx.execute(
                    "INSERT INTO sms_parts(logical_id,storage_index,payload) VALUES(?1,?2,?3)",
                    params![logical_id, index, payload],
                )
                .map_err(db_error)?;
            }
            // Suppress only exact legacy physical fragments. Index, peer,
            // timestamp and normalized payload all participate, protecting
            // unrelated messages created after SIM index reuse.
            reconcile_legacy_parts(&tx, r, &logical_id)?;
        }
        apply_delivery_reports(&tx, records)?;
        tx.commit().map_err(db_error)
    }
    pub fn list_sms(&self, limit: usize) -> Result<Vec<SmsRecord>, ModemError> {
        let c = self.connection()?;
        let mut s=c.prepare("SELECT id,direction,peer,body,state,message_reference,cause,created_at_ms,kind,source,storage,storage_index,modem_status,modem_timestamp,encoding,dcs,length,service_center,delivery_status,synchronized_at_ms,present_on_modem,fingerprint,storage_indices,part_count,parts_received,multipart_complete FROM sms WHERE superseded=0 ORDER BY created_at_ms DESC,id DESC LIMIT ?1").map_err(db_error)?;
        s.query_map([limit as i64], |r| {
            Ok(SmsRecord {
                id: r.get(0)?,
                direction: r.get(1)?,
                peer: r.get(2)?,
                body: r.get(3)?,
                state: r.get(4)?,
                message_reference: r.get(5)?,
                cause: r.get(6)?,
                created_at_ms: r.get(7)?,
                kind: r.get(8)?,
                source: r.get(9)?,
                storage: r.get(10)?,
                storage_index: r.get(11)?,
                modem_status: r.get(12)?,
                modem_timestamp: r.get(13)?,
                encoding: r.get(14)?,
                dcs: r.get(15)?,
                length: r.get(16)?,
                service_center: r.get(17)?,
                delivery_status: r.get(18)?,
                synchronized_at_ms: r.get(19)?,
                present_on_modem: r.get(20)?,
                fingerprint: r.get(21)?,
                storage_indices: serde_json::from_str(&r.get::<_, String>(22)?).unwrap_or_default(),
                part_count: r.get(23)?,
                parts_received: r.get(24)?,
                multipart_complete: r.get(25)?,
                part_payloads: Vec::new(),
                part_timestamps: Vec::new(),
            })
        })
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)
    }
    pub fn save_balance(&self, r: &BalanceRecord) -> Result<(), ModemError> {
        self.connection()?.execute("INSERT INTO balance_checks(id,raw,value,currency,error,created_at_ms,sms_id) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![r.id,r.raw,r.value,r.currency,r.error,r.created_at_ms,r.sms_id]).map_err(db_error)?;
        Ok(())
    }
    pub fn list_balances(&self, limit: usize) -> Result<Vec<BalanceRecord>, ModemError> {
        let c = self.connection()?;
        let mut s=c.prepare("SELECT id,raw,value,currency,error,created_at_ms,sms_id FROM balance_checks ORDER BY created_at_ms DESC,id DESC LIMIT ?1").map_err(db_error)?;
        s.query_map([limit as i64], |r| {
            Ok(BalanceRecord {
                id: r.get(0)?,
                raw: r.get(1)?,
                value: r.get(2)?,
                currency: r.get(3)?,
                error: r.get(4)?,
                created_at_ms: r.get(5)?,
                sms_id: r.get(6)?,
            })
        })
        .map_err(db_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_error)
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ModemError> {
        self.0
            .lock()
            .map_err(|_| ModemError::Persistence("database lock poisoned".into()))
    }
}

fn reconcile_legacy_parts(
    tx: &rusqlite::Transaction<'_>,
    logical: &SmsRecord,
    logical_id: &str,
) -> Result<(), ModemError> {
    if logical.storage_indices.len() <= 1 {
        return Ok(());
    }
    for (part, index) in logical.storage_indices.iter().enumerate() {
        let mut statement=tx.prepare("SELECT id,modem_timestamp,body,encoding,dcs FROM sms WHERE source='sim' AND id<>?1 AND storage=?2 AND storage_index=?3 AND peer=?4 AND superseded=0").map_err(db_error)?;
        let candidates = statement
            .query_map(
                params![logical_id, logical.storage, index, logical.peer],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i32>(4)?,
                    ))
                },
            )
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        drop(statement);
        let expected_payload = logical
            .part_payloads
            .get(part)
            .map(String::as_str)
            .unwrap_or_default();
        let expected_timestamp = logical
            .part_timestamps
            .get(part)
            .map(String::as_str)
            .unwrap_or(&logical.modem_timestamp);
        for (id, timestamp, body, encoding, dcs) in candidates {
            let explicit =
                encoding.eq_ignore_ascii_case("UCS2") || (dcs >= 0 && sms_dcs_uses_ucs2(dcs as u8));
            let normalized = crate::sms::decode_ucs2_body(&body, explicit).unwrap_or(body);
            if normalize_modem_timestamp(&timestamp)
                == normalize_modem_timestamp(expected_timestamp)
                && normalized == expected_payload
            {
                tx.execute("UPDATE sms SET superseded=1 WHERE id=?1", [id])
                    .map_err(db_error)?;
            }
        }
    }
    Ok(())
}

fn normalize_modem_timestamp(value: &str) -> &str {
    value.get(..17).unwrap_or(value)
}

fn apply_delivery_reports(
    tx: &rusqlite::Transaction<'_>,
    records: &[SmsRecord],
) -> Result<(), ModemError> {
    for report in records.iter().filter(|record| {
        record.kind == "status-report"
            && !record.peer.is_empty()
            && !record.message_reference.is_empty()
    }) {
        let state = delivery_state(&report.delivery_status);
        tx.execute(
            "UPDATE sms SET state=?1,delivery_status=?2 WHERE id=(SELECT id FROM sms WHERE direction='outbound' AND peer=?3 AND message_reference=?4 AND kind<>'status-report' ORDER BY created_at_ms DESC,id DESC LIMIT 1)",
            params![state, report.delivery_status, report.peer, report.message_reference],
        )
        .map_err(db_error)?;
    }
    Ok(())
}

fn delivery_state(status: &str) -> &'static str {
    let code = status
        .strip_prefix("0x")
        .or_else(|| status.strip_prefix("0X"))
        .and_then(|value| u8::from_str_radix(value, 16).ok());
    match code {
        Some(0x00..=0x1f) => "delivered",
        Some(0x20..=0x3f) => "delivery-pending",
        Some(_) => "delivery-failed",
        None => "delivery-unknown",
    }
}
fn sms_dcs_uses_ucs2(dcs: u8) -> bool {
    (dcs & 0xc0 == 0 && dcs & 0x0c == 8) || dcs & 0xf0 == 0xe0
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

fn db_error(error: impl std::fmt::Display) -> ModemError {
    ModemError::Persistence(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migrates_and_round_trips_settings() {
        let store = Store::memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), 5);
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
    fn delivery_reports_update_latest_matching_outbound_message() {
        let s = Store::memory().unwrap();
        s.save_sms(&SmsRecord {
            id: "sent".into(),
            direction: "outbound".into(),
            peer: "+66812345678".into(),
            message_reference: "42".into(),
            state: "submitted".into(),
            kind: "submitted".into(),
            source: "app".into(),
            created_at_ms: 10,
            ..Default::default()
        })
        .unwrap();
        let report = |id: &str, index, status: &str| SmsRecord {
            id: id.into(),
            direction: "inbound".into(),
            peer: "+66812345678".into(),
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
        s.sync_sms(&[report("pending", 1, "0x20")], 20).unwrap();
        assert_eq!(
            s.list_sms(10)
                .unwrap()
                .iter()
                .find(|x| x.id == "sent")
                .unwrap()
                .state,
            "delivery-pending"
        );
        s.sync_sms(&[report("delivered", 2, "0x00")], 30).unwrap();
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
    fn permanent_status_report_is_delivery_failure() {
        assert_eq!(delivery_state("0x40"), "delivery-failed");
        assert_eq!(delivery_state("malformed"), "delivery-unknown");
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
        };
        store.save_current_audio(&audio("old", 1)).unwrap();
        store.save_current_audio(&audio("new", 2)).unwrap();
        assert_eq!(store.current_audio().unwrap().unwrap().id, "new");

        let call = CallRecord {
            id: "call".into(),
            peer: "+66000000000".into(),
            state: "playing".into(),
            audio_id: "new".into(),
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
