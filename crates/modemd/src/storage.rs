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
    pub delivery_report_requested: bool,
    pub delivery_report_scts: String,
    pub delivery_report_discharge_time: String,
    pub delivery_tracking_error: String,
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
    pub is_current: bool,
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
        connection.execute_batch(
            "DELETE FROM uploaded_audio WHERE is_current=0 AND NOT EXISTS(SELECT 1 FROM schema_migrations WHERE version=6);
             CREATE UNIQUE INDEX IF NOT EXISTS uploaded_audio_name_nocase ON uploaded_audio(lower(trim(name)));
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (6);",
        ).map_err(db_error)?;
        connection.execute_batch(
            "DELETE FROM sms_parts WHERE NOT EXISTS(SELECT 1 FROM schema_migrations WHERE version=7);
             DELETE FROM sms WHERE NOT EXISTS(SELECT 1 FROM schema_migrations WHERE version=7);
             UPDATE balance_checks SET sms_id='' WHERE NOT EXISTS(SELECT 1 FROM schema_migrations WHERE version=7);
             DROP INDEX IF EXISTS sms_sim_fingerprint;
             CREATE UNIQUE INDEX sms_sim_fingerprint ON sms(storage,fingerprint) WHERE source='sim';
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (7);",
        ).map_err(db_error)?;
        let sms_columns = table_columns(&connection, "sms")?;
        for (name, definition) in [
            ("delivery_report_requested", "INTEGER NOT NULL DEFAULT 0"),
            ("delivery_report_scts", "TEXT NOT NULL DEFAULT ''"),
            ("delivery_report_discharge_time", "TEXT NOT NULL DEFAULT ''"),
            ("delivery_tracking_error", "TEXT NOT NULL DEFAULT ''"),
            ("matched_sms_id", "TEXT NOT NULL DEFAULT ''"),
            ("delivery_event_ms", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            if !sms_columns.iter().any(|column| column == name) {
                connection
                    .execute(
                        &format!("ALTER TABLE sms ADD COLUMN {name} {definition}"),
                        [],
                    )
                    .map_err(db_error)?;
            }
        }
        connection
            .execute(
                "INSERT OR IGNORE INTO schema_migrations(version) VALUES (8)",
                [],
            )
            .map_err(db_error)?;
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
                "SELECT id,name,format,size,module_path,duration_ms,created_at_ms,is_current FROM uploaded_audio WHERE is_current=1 LIMIT 1",
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
                        is_current: row.get(7)?,
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
            "INSERT INTO uploaded_audio(id,name,format,size,module_path,duration_ms,created_at_ms,is_current) VALUES(?1,?2,?3,?4,?5,?6,?7,1) ON CONFLICT(id) DO UPDATE SET name=excluded.name,format=excluded.format,size=excluded.size,module_path=excluded.module_path,duration_ms=excluded.duration_ms,created_at_ms=excluded.created_at_ms,is_current=1",
            params![audio.id,audio.name,audio.format,audio.size,audio.module_path,audio.duration_ms,audio.created_at_ms],
        ).map_err(db_error)?;
        transaction.commit().map_err(db_error)
    }

    pub fn list_audio(&self) -> Result<Vec<UploadedAudioRecord>, ModemError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,name,format,size,module_path,duration_ms,created_at_ms,is_current FROM uploaded_audio ORDER BY is_current DESC,created_at_ms DESC,id DESC",
        ).map_err(db_error)?;
        statement
            .query_map([], audio_from_row)
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)
    }

    pub fn audio_named(&self, name: &str) -> Result<Option<UploadedAudioRecord>, ModemError> {
        self.connection()?.query_row(
            "SELECT id,name,format,size,module_path,duration_ms,created_at_ms,is_current FROM uploaded_audio WHERE lower(trim(name))=lower(trim(?1)) LIMIT 1",
            [name], audio_from_row,
        ).optional().map_err(db_error)
    }

    pub fn replace_and_select_audio(
        &self,
        audio: &UploadedAudioRecord,
        replaced_id: Option<&str>,
    ) -> Result<(), ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "UPDATE uploaded_audio SET is_current=0 WHERE is_current=1",
                [],
            )
            .map_err(db_error)?;
        if let Some(id) = replaced_id {
            transaction
                .execute("DELETE FROM uploaded_audio WHERE id=?1", [id])
                .map_err(db_error)?;
        }
        transaction.execute(
            "INSERT INTO uploaded_audio(id,name,format,size,module_path,duration_ms,created_at_ms,is_current) VALUES(?1,?2,?3,?4,?5,?6,?7,1)",
            params![audio.id,audio.name,audio.format,audio.size,audio.module_path,audio.duration_ms,audio.created_at_ms],
        ).map_err(db_error)?;
        transaction.commit().map_err(db_error)
    }

    pub fn select_audio(&self, id: &str) -> Result<UploadedAudioRecord, ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        let exists: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM uploaded_audio WHERE id=?1)",
                [id],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if !exists {
            return Err(ModemError::Validation("audio file was not found".into()));
        }
        transaction
            .execute(
                "UPDATE uploaded_audio SET is_current=0 WHERE is_current=1",
                [],
            )
            .map_err(db_error)?;
        transaction
            .execute("UPDATE uploaded_audio SET is_current=1 WHERE id=?1", [id])
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        drop(connection);
        self.current_audio()?
            .ok_or_else(|| ModemError::Validation("audio file was not found".into()))
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

    pub fn recover_interrupted_sms(&self) -> Result<usize, ModemError> {
        self.connection()?.execute(
            "UPDATE sms SET state='send-unknown',cause='daemon restarted while modem acceptance was unknown' WHERE state='sending'",
            [],
        ).map_err(db_error)
    }

    pub fn next_sms_sync_delay_ms(&self, now_ms: i64) -> Result<u64, ModemError> {
        let _ = now_ms;
        Ok(300_000)
    }

    pub fn expire_delivery_reports(&self, now_ms: i64) -> Result<usize, ModemError> {
        self.connection()?.execute(
            "UPDATE sms SET state='delivery-unknown',cause='no final delivery report was received within the 24-hour validity period' WHERE source='app' AND direction='outbound' AND state='delivery-pending' AND created_at_ms<=?1",
            [now_ms.saturating_sub(86_400_000)],
        ).map_err(db_error)
    }

    pub fn apply_direct_delivery_report(
        &self,
        report: &SmsRecord,
        now_ms: i64,
    ) -> Result<bool, ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        let matched = apply_delivery_reports(&transaction, std::slice::from_ref(report), now_ms)?;
        transaction.commit().map_err(db_error)?;
        Ok(matched != 0)
    }

    pub fn save_sms(&self, r: &SmsRecord) -> Result<(), ModemError> {
        self.connection()?.execute("INSERT INTO sms(id,direction,peer,body,state,message_reference,cause,created_at_ms,kind,source,storage,storage_index,modem_status,modem_timestamp,encoding,dcs,length,service_center,delivery_status,synchronized_at_ms,present_on_modem,fingerprint,storage_indices,part_count,parts_received,multipart_complete,delivery_report_requested,delivery_report_scts,delivery_report_discharge_time,delivery_tracking_error) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30) ON CONFLICT(id) DO UPDATE SET state=excluded.state,message_reference=excluded.message_reference,cause=excluded.cause,delivery_status=excluded.delivery_status,synchronized_at_ms=excluded.synchronized_at_ms,present_on_modem=excluded.present_on_modem,delivery_report_requested=excluded.delivery_report_requested,delivery_report_scts=excluded.delivery_report_scts,delivery_report_discharge_time=excluded.delivery_report_discharge_time,delivery_tracking_error=excluded.delivery_tracking_error",params![r.id,r.direction,r.peer,r.body,r.state,r.message_reference,r.cause,r.created_at_ms,r.kind,r.source,r.storage,r.storage_index,r.modem_status,r.modem_timestamp,r.encoding,r.dcs,r.length,r.service_center,r.delivery_status,r.synchronized_at_ms,r.present_on_modem,r.fingerprint,serde_json::to_string(&r.storage_indices).map_err(db_error)?,r.part_count,r.parts_received,r.multipart_complete,r.delivery_report_requested,r.delivery_report_scts,r.delivery_report_discharge_time,r.delivery_tracking_error]).map_err(db_error)?;
        Ok(())
    }
    pub fn sync_sms(&self, records: &[SmsRecord], now: i64) -> Result<(), ModemError> {
        let mut c = self.connection()?;
        let tx = c.transaction().map_err(db_error)?;
        tx.execute("UPDATE sms SET present_on_modem=0,synchronized_at_ms=?1 WHERE source='sim' AND storage='SM'",[now]).map_err(db_error)?;
        for r in records {
            let indices = serde_json::to_string(&r.storage_indices).map_err(db_error)?;
            tx.execute("INSERT INTO sms(id,direction,peer,body,state,message_reference,cause,created_at_ms,kind,source,storage,storage_index,modem_status,modem_timestamp,encoding,dcs,length,service_center,delivery_status,synchronized_at_ms,present_on_modem,fingerprint,storage_indices,part_count,parts_received,multipart_complete,superseded,delivery_report_scts,delivery_report_discharge_time) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,1,?21,?22,?23,?24,?25,0,?26,?27) ON CONFLICT(storage,fingerprint) WHERE source='sim' DO UPDATE SET direction=excluded.direction,peer=excluded.peer,body=excluded.body,state=excluded.state,kind=excluded.kind,storage_index=excluded.storage_index,modem_status=excluded.modem_status,modem_timestamp=excluded.modem_timestamp,encoding=excluded.encoding,dcs=excluded.dcs,length=excluded.length,service_center=excluded.service_center,delivery_status=excluded.delivery_status,synchronized_at_ms=excluded.synchronized_at_ms,present_on_modem=1,storage_indices=excluded.storage_indices,part_count=excluded.part_count,parts_received=excluded.parts_received,multipart_complete=excluded.multipart_complete,delivery_report_scts=excluded.delivery_report_scts,delivery_report_discharge_time=excluded.delivery_report_discharge_time,superseded=0",params![r.id,r.direction,r.peer,r.body,r.state,r.message_reference,r.cause,r.created_at_ms,r.kind,r.source,r.storage,r.storage_index,r.modem_status,r.modem_timestamp,r.encoding,r.dcs,r.length,r.service_center,r.delivery_status,now,r.fingerprint,indices,r.part_count,r.parts_received,r.multipart_complete,r.delivery_report_scts,r.delivery_report_discharge_time]).map_err(db_error)?;
            let logical_id: String = tx
                .query_row(
                    "SELECT id FROM sms WHERE source='sim' AND storage=?1 AND fingerprint=?2",
                    params![r.storage, r.fingerprint],
                    |row| row.get(0),
                )
                .map_err(db_error)?;
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
        apply_delivery_reports(&tx, records, now)?;
        reconcile_stored_submissions(&tx, records)?;
        tx.commit().map_err(db_error)
    }
    pub fn list_sms(&self, limit: usize) -> Result<Vec<SmsRecord>, ModemError> {
        let c = self.connection()?;
        let mut s=c.prepare("SELECT id,direction,peer,body,state,message_reference,cause,created_at_ms,kind,source,storage,storage_index,modem_status,modem_timestamp,encoding,dcs,length,service_center,delivery_status,synchronized_at_ms,present_on_modem,fingerprint,storage_indices,part_count,parts_received,multipart_complete,delivery_report_requested,delivery_report_scts,delivery_report_discharge_time,delivery_tracking_error FROM sms WHERE superseded=0 AND kind<>'status-report' ORDER BY created_at_ms DESC,storage_index DESC,id DESC LIMIT ?1").map_err(db_error)?;
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
                delivery_report_requested: r.get(26)?,
                delivery_report_scts: r.get(27)?,
                delivery_report_discharge_time: r.get(28)?,
                delivery_tracking_error: r.get(29)?,
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
    synchronized_at_ms: i64,
) -> Result<usize, ModemError> {
    let mut matched_count = 0;
    for report in records.iter().filter(|record| {
        record.kind == "status-report"
            && !record.peer.is_empty()
            && !record.message_reference.is_empty()
    }) {
        let state = delivery_state(&report.delivery_status);
        let scts_ms = modem_timestamp_ms(&report.delivery_report_scts);
        let report_sync = if report.synchronized_at_ms == 0 {
            synchronized_at_ms
        } else {
            report.synchronized_at_ms
        };
        let event_ms =
            modem_timestamp_ms(&report.delivery_report_discharge_time).unwrap_or(report_sync);

        // Once a report has been linked, an idempotent replay with the same
        // SCTS follows that link even if TP-MR has since wrapped around.
        let existing_link: Option<String> = tx
            .query_row(
                "SELECT matched_sms_id FROM sms WHERE kind='status-report' AND message_reference=?1 AND peer=?2 AND delivery_report_scts=?3 AND matched_sms_id<>'' LIMIT 1",
                params![report.message_reference, report.peer, report.delivery_report_scts],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let mut statement = tx.prepare(
            "SELECT id,peer,created_at_ms FROM sms WHERE source='app' AND direction='outbound' AND message_reference=?1 AND kind<>'status-report'",
        ).map_err(db_error)?;
        let candidates = statement
            .query_map([&report.message_reference], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        drop(statement);
        let eligible = candidates
            .into_iter()
            .filter(|(_, peer, created)| {
                if normalize_peer(peer) != normalize_peer(&report.peer) {
                    return false;
                }
                scts_ms.map_or_else(
                    || *created <= report_sync,
                    |scts| (*created - scts).abs() <= 600_000,
                )
            })
            .collect::<Vec<_>>();
        let matched =
            existing_link.or_else(|| (eligible.len() == 1).then(|| eligible[0].0.clone()));
        if let Some(id) = matched {
            tx.execute(
                "UPDATE sms SET state=?1,delivery_status=?2,delivery_report_scts=?3,delivery_report_discharge_time=?4,delivery_event_ms=?5 WHERE id=?6 AND (delivery_event_ms<?5 OR (delivery_event_ms=?5 AND state IN ('submitted','delivery-pending','delivery-unknown') AND ?1 IN ('delivered','delivery-failed')))",
                params![state, report.delivery_status, report.delivery_report_scts, report.delivery_report_discharge_time, event_ms, id],
            )
            .map_err(db_error)?;
            tx.execute(
                "UPDATE sms SET matched_sms_id=?1 WHERE id=?2",
                params![id, report.id],
            )
            .map_err(db_error)?;
            matched_count += 1;
        }
    }
    Ok(matched_count)
}

fn reconcile_stored_submissions(
    tx: &rusqlite::Transaction<'_>,
    records: &[SmsRecord],
) -> Result<(), ModemError> {
    for stored in records.iter().filter(|record| {
        record.direction == "outbound"
            && record.modem_status == "STO SENT"
            && !record.body.is_empty()
    }) {
        let normalized_peer = normalize_peer(&stored.peer);
        let mut statement = tx.prepare(
            "SELECT id,peer FROM sms WHERE source='app' AND direction='outbound' AND body=?1 AND (?2='' OR message_reference=?2) ORDER BY created_at_ms DESC,id DESC",
        ).map_err(db_error)?;
        let candidates = statement
            .query_map(params![stored.body, stored.message_reference], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        drop(statement);
        if let Some((app_id, _)) = candidates
            .into_iter()
            .find(|(_, peer)| normalize_peer(peer) == normalized_peer)
        {
            tx.execute(
                "UPDATE sms SET superseded=1 WHERE source='sim' AND storage=?1 AND storage_index=?2 AND fingerprint=?3",
                params![stored.storage, stored.storage_index, stored.fingerprint],
            ).map_err(db_error)?;
            tx.execute(
                "UPDATE sms SET modem_status=?1,synchronized_at_ms=?2,present_on_modem=1 WHERE id=?3",
                params![stored.modem_status, stored.synchronized_at_ms, app_id],
            ).map_err(db_error)?;
        }
    }
    Ok(())
}

fn normalize_peer(value: &str) -> String {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    digits.strip_prefix("00").unwrap_or(&digits).to_owned()
}

fn delivery_state(status: &str) -> &'static str {
    let code = status
        .strip_prefix("0x")
        .or_else(|| status.strip_prefix("0X"))
        .and_then(|value| u8::from_str_radix(value, 16).ok());
    match code {
        Some(0x00) => "delivered",
        Some(0x01..=0x1f) => "delivery-unknown",
        Some(0x20..=0x3f) => "delivery-pending",
        Some(0x40..=0x7f) => "delivery-failed",
        Some(0x80..=0xff) => "delivery-unknown",
        None => "delivery-unknown",
    }
}

fn modem_timestamp_ms(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[2] != b'/'
        || bytes[5] != b'/'
        || bytes[8] != b','
        || bytes[11] != b':'
        || bytes[14] != b':'
        || !matches!(bytes[17], b'+' | b'-')
    {
        return None;
    }
    let number = |start: usize| {
        std::str::from_utf8(&bytes[start..start + 2])
            .ok()?
            .parse::<i64>()
            .ok()
    };
    let (year, month, day) = (2000 + number(0)?, number(3)?, number(6)?);
    let (hour, minute, second, quarters) = (number(9)?, number(12)?, number(15)?, number(18)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
        || quarters > 79
    {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let yoe = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted_month + 2) / 5 + day - 1;
    let days = era * 146097 + (yoe * 365 + yoe / 4 - yoe / 100) + doy - 719468;
    let seconds = days * 86400 + hour * 3600 + minute * 60 + second;
    let offset = quarters * 900 * if bytes[17] == b'-' { -1 } else { 1 };
    Some((seconds - offset) * 1000)
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
        assert_eq!(store.schema_version().unwrap(), 8);
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
                peer: "+66812345678".into(),
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
                    peer: "+66812345678".into(),
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
