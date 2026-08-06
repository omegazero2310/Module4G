use super::*;

impl Store {
    pub(super) fn migrate(&self) -> Result<(), ModemError> {
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
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS integration_settings(id INTEGER PRIMARY KEY CHECK(id=1),json TEXT NOT NULL,updated_at_ms INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS rest_communications(
               id TEXT PRIMARY KEY,record_id TEXT NOT NULL UNIQUE,channel TEXT NOT NULL,owner TEXT NOT NULL,
               destination TEXT NOT NULL,content TEXT NOT NULL,encrypted INTEGER NOT NULL,
               payload_fingerprint TEXT NOT NULL,status TEXT NOT NULL,created_at_ms INTEGER NOT NULL,
               sent_at_ms INTEGER,delivered_at_ms INTEGER,failed_at_ms INTEGER,failure_reason TEXT NOT NULL DEFAULT '');
             CREATE TABLE IF NOT EXISTS webhook_outbox(
               id INTEGER PRIMARY KEY AUTOINCREMENT,communication_id TEXT NOT NULL,event_type TEXT NOT NULL,
               payload TEXT NOT NULL,attempt_count INTEGER NOT NULL DEFAULT 0,next_attempt_at_ms INTEGER NOT NULL,
               last_error TEXT NOT NULL DEFAULT '',completed_at_ms INTEGER,
               UNIQUE(communication_id,event_type),
               FOREIGN KEY(communication_id) REFERENCES rest_communications(id));
             CREATE INDEX IF NOT EXISTS webhook_outbox_due ON webhook_outbox(completed_at_ms,next_attempt_at_ms,id);
             INSERT OR IGNORE INTO schema_migrations(version) VALUES (9);",
        ).map_err(db_error)?;
        Ok(())
    }
}
