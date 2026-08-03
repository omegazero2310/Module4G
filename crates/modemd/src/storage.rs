use crate::{ModemError, settings::Settings};
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, sync::Mutex};

pub struct Store(Mutex<Connection>);

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ModemError> {
        let connection = Connection::open(path).map_err(db_error)?;
        let store = Self(Mutex::new(connection));
        store.migrate()?;
        Ok(store)
    }

    pub fn memory() -> Result<Self, ModemError> { Self::open(":memory:") }

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
        Ok(())
    }

    pub fn load_settings(&self) -> Result<Settings, ModemError> {
        let json: Option<String> = self.connection()?.query_row("SELECT json FROM settings WHERE id=1", [], |row| row.get(0)).optional().map_err(db_error)?;
        json.map(|value| serde_json::from_str(&value).map_err(db_error)).transpose().map(|value| value.unwrap_or_default())
    }

    pub fn save_settings(&self, settings: &Settings, now_ms: i64) -> Result<(), ModemError> {
        settings.validate()?;
        let json = serde_json::to_string(settings).map_err(db_error)?;
        self.connection()?.execute("INSERT INTO settings(id,json,updated_at_ms) VALUES(1,?1,?2) ON CONFLICT(id) DO UPDATE SET json=excluded.json,updated_at_ms=excluded.updated_at_ms", params![json, now_ms]).map_err(db_error)?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64, ModemError> {
        self.connection()?.query_row("SELECT max(version) FROM schema_migrations", [], |row| row.get(0)).map_err(db_error)
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ModemError> {
        self.0.lock().map_err(|_| ModemError::Persistence("database lock poisoned".into()))
    }
}

fn db_error(error: impl std::fmt::Display) -> ModemError { ModemError::Persistence(error.to_string()) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migrates_and_round_trips_settings() {
        let store = Store::memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), 1);
        let mut expected = Settings::default();
        expected.port_override = Some("COM6".into());
        store.save_settings(&expected, 42).unwrap();
        assert_eq!(store.load_settings().unwrap(), expected);
    }
}
