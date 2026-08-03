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
        connection.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS sms_sim_fingerprint ON sms(storage,storage_index,fingerprint) WHERE source='sim'; INSERT OR IGNORE INTO schema_migrations(version) VALUES (3);").map_err(db_error)?;
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

    pub fn save_sms(&self, r: &SmsRecord) -> Result<(), ModemError> {
        self.connection()?.execute("INSERT INTO sms(id,direction,peer,body,state,message_reference,cause,created_at_ms,kind,source,storage,storage_index,modem_status,modem_timestamp,encoding,dcs,length,service_center,delivery_status,synchronized_at_ms,present_on_modem,fingerprint) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22) ON CONFLICT(id) DO UPDATE SET state=excluded.state,delivery_status=excluded.delivery_status,synchronized_at_ms=excluded.synchronized_at_ms,present_on_modem=excluded.present_on_modem",params![r.id,r.direction,r.peer,r.body,r.state,r.message_reference,r.cause,r.created_at_ms,r.kind,r.source,r.storage,r.storage_index,r.modem_status,r.modem_timestamp,r.encoding,r.dcs,r.length,r.service_center,r.delivery_status,r.synchronized_at_ms,r.present_on_modem,r.fingerprint]).map_err(db_error)?;
        Ok(())
    }
    pub fn sync_sms(&self, records: &[SmsRecord], now: i64) -> Result<(), ModemError> {
        let mut c = self.connection()?;
        let tx = c.transaction().map_err(db_error)?;
        tx.execute("UPDATE sms SET present_on_modem=0,synchronized_at_ms=?1 WHERE source='sim' AND storage='SM'",[now]).map_err(db_error)?;
        for r in records {
            tx.execute("INSERT INTO sms(id,direction,peer,body,state,message_reference,cause,created_at_ms,kind,source,storage,storage_index,modem_status,modem_timestamp,encoding,dcs,length,service_center,delivery_status,synchronized_at_ms,present_on_modem,fingerprint) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,1,?21) ON CONFLICT(storage,storage_index,fingerprint) WHERE source='sim' DO UPDATE SET direction=excluded.direction,peer=excluded.peer,body=excluded.body,state=excluded.state,kind=excluded.kind,modem_status=excluded.modem_status,modem_timestamp=excluded.modem_timestamp,encoding=excluded.encoding,dcs=excluded.dcs,length=excluded.length,service_center=excluded.service_center,delivery_status=excluded.delivery_status,synchronized_at_ms=excluded.synchronized_at_ms,present_on_modem=1",params![r.id,r.direction,r.peer,r.body,r.state,r.message_reference,r.cause,r.created_at_ms,r.kind,r.source,r.storage,r.storage_index,r.modem_status,r.modem_timestamp,r.encoding,r.dcs,r.length,r.service_center,r.delivery_status,now,r.fingerprint]).map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }
    pub fn list_sms(&self, limit: usize) -> Result<Vec<SmsRecord>, ModemError> {
        let c = self.connection()?;
        let mut s=c.prepare("SELECT id,direction,peer,body,state,message_reference,cause,created_at_ms,kind,source,storage,storage_index,modem_status,modem_timestamp,encoding,dcs,length,service_center,delivery_status,synchronized_at_ms,present_on_modem,fingerprint FROM sms ORDER BY created_at_ms DESC,id DESC LIMIT ?1").map_err(db_error)?;
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
        assert_eq!(store.schema_version().unwrap(), 3);
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
}
