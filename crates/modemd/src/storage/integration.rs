use super::*;

impl Store {
    pub fn load_integration_settings(&self) -> Result<IntegrationSettings, ModemError> {
        let json: Option<String> = self
            .connection()?
            .query_row(
                "SELECT json FROM integration_settings WHERE id=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        json.map(|value| serde_json::from_str(&value).map_err(db_error))
            .transpose()
            .map(|value| value.unwrap_or_default())
    }

    pub fn save_integration_settings(
        &self,
        settings: &IntegrationSettings,
        now_ms: i64,
    ) -> Result<(), ModemError> {
        crate::integration::validate_settings(settings)?;
        let json = serde_json::to_string(settings).map_err(db_error)?;
        self.connection()?
            .execute(
                "INSERT INTO integration_settings(id,json,updated_at_ms) VALUES(1,?1,?2)
             ON CONFLICT(id) DO UPDATE SET json=excluded.json,updated_at_ms=excluded.updated_at_ms",
                params![json, now_ms],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn reserve_rest_communication(
        &self,
        communication: &RestCommunication,
    ) -> Result<CommunicationReservation, ModemError> {
        let connection = self.connection()?;
        let inserted = connection.execute(
            "INSERT OR IGNORE INTO rest_communications(id,record_id,channel,owner,destination,content,encrypted,payload_fingerprint,status,created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![communication.id,communication.record_id,communication.channel,communication.owner,
                communication.destination,communication.content,communication.encrypted,
                communication.payload_fingerprint,communication.status,communication.created_at_ms],
        ).map_err(db_error)?;
        if inserted == 1 {
            return Ok(CommunicationReservation::New(communication.clone()));
        }
        let existing = communication_by_id(&connection, &communication.id)?.ok_or_else(|| {
            ModemError::Persistence("communication reservation disappeared".into())
        })?;
        if existing.payload_fingerprint == communication.payload_fingerprint {
            Ok(CommunicationReservation::Replay(existing))
        } else {
            Ok(CommunicationReservation::Conflict)
        }
    }

    pub fn rest_communication(&self, id: &str) -> Result<Option<RestCommunication>, ModemError> {
        let connection = self.connection()?;
        communication_by_id(&connection, id)
    }

    pub fn reconcile_rest_communications(&self, now_ms: i64) -> Result<usize, ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        let mut statement = transaction.prepare(
            "SELECT r.id,r.record_id,r.channel,r.owner,r.destination,r.content,r.encrypted,
                    r.payload_fingerprint,r.status,r.created_at_ms,COALESCE(r.sent_at_ms,0),
                    COALESCE(r.delivered_at_ms,0),COALESCE(r.failed_at_ms,0),r.failure_reason,
                    CASE WHEN r.channel='sms' THEN COALESCE(s.state,'') ELSE COALESCE(c.state,'') END,
                    CASE WHEN r.channel='sms' THEN COALESCE(s.cause,'') ELSE COALESCE(c.error,'') END,
                    COALESCE(c.answer_classification,''),COALESCE(c.end_reason,''),
                    CASE WHEN r.channel='sms' THEN 0 ELSE COALESCE(c.connected_at_ms,0) END,
                    CASE WHEN r.channel='sms' THEN 0 ELSE COALESCE(c.ended_at_ms,0) END
             FROM rest_communications r
             LEFT JOIN sms s ON r.channel='sms' AND s.id=r.record_id
             LEFT JOIN calls c ON r.channel='call' AND c.id=r.record_id"
        ).map_err(db_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    communication_from_row(row)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, String>(15)?,
                    row.get::<_, String>(16)?,
                    row.get::<_, String>(17)?,
                    row.get::<_, i64>(18)?,
                    row.get::<_, i64>(19)?,
                ))
            })
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)?;
        drop(statement);
        let mut changed = 0;
        for (mut communication, source_state, source_error, answer, end_reason, connected, ended) in
            rows
        {
            let (status, reason) = crate::integration::normalize_source_state(
                &communication.channel,
                &source_state,
                &source_error,
                &answer,
                &end_reason,
            );
            if status == communication.status {
                continue;
            }
            communication.status = status.to_owned();
            match status {
                "sent" => communication.sent_at_ms = now_ms,
                "delivered" => {
                    if communication.sent_at_ms == 0 {
                        communication.sent_at_ms = connected.max(now_ms);
                    }
                    communication.delivered_at_ms = if ended > 0 { ended } else { now_ms };
                    communication.failed_at_ms = 0;
                    communication.failure_reason.clear();
                }
                "failed" | "expired" | "missed" => {
                    communication.failed_at_ms = if ended > 0 { ended } else { now_ms };
                    communication.failure_reason = reason;
                }
                _ => {}
            }
            transaction.execute(
                "UPDATE rest_communications SET status=?2,sent_at_ms=NULLIF(?3,0),delivered_at_ms=NULLIF(?4,0),failed_at_ms=NULLIF(?5,0),failure_reason=?6 WHERE id=?1",
                params![communication.id,communication.status,communication.sent_at_ms,communication.delivered_at_ms,
                    communication.failed_at_ms,communication.failure_reason],
            ).map_err(db_error)?;
            if let Some(event_type) = crate::integration::event_for_status(status) {
                let payload = crate::integration::webhook_payload(&communication, event_type)?;
                transaction.execute(
                    "INSERT OR IGNORE INTO webhook_outbox(communication_id,event_type,payload,next_attempt_at_ms) VALUES(?1,?2,?3,?4)",
                    params![communication.id,event_type,payload,now_ms],
                ).map_err(db_error)?;
            }
            changed += 1;
        }
        transaction.commit().map_err(db_error)?;
        Ok(changed)
    }

    pub fn mark_rest_dispatch_failed(
        &self,
        id: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<(), ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction.execute(
            "UPDATE rest_communications SET status='failed',failed_at_ms=?2,failure_reason=?3 WHERE id=?1 AND status='sending'",
            params![id,now_ms,crate::integration::safe_failure_reason(reason)],
        ).map_err(db_error)?;
        if let Some(communication) = communication_by_id(&transaction, id)? {
            let payload =
                crate::integration::webhook_payload(&communication, "communication.failed")?;
            transaction.execute(
                "INSERT OR IGNORE INTO webhook_outbox(communication_id,event_type,payload,next_attempt_at_ms) VALUES(?1,'communication.failed',?2,?3)",
                params![id,payload,now_ms],
            ).map_err(db_error)?;
        }
        transaction.commit().map_err(db_error)
    }

    pub fn next_webhook(&self, now_ms: i64) -> Result<Option<WebhookAttempt>, ModemError> {
        self.connection()?.query_row(
            "SELECT o.id,o.communication_id,o.event_type,o.payload,o.attempt_count,o.next_attempt_at_ms
             FROM webhook_outbox o WHERE o.completed_at_ms IS NULL AND o.next_attempt_at_ms<=?1
             AND NOT EXISTS(SELECT 1 FROM webhook_outbox prior WHERE prior.communication_id=o.communication_id
               AND prior.id<o.id AND prior.completed_at_ms IS NULL)
             ORDER BY o.next_attempt_at_ms,o.id LIMIT 1", [now_ms],
            |row| Ok(WebhookAttempt { id:row.get(0)?,communication_id:row.get(1)?,event_type:row.get(2)?,
                payload:row.get(3)?,attempt_count:row.get(4)?,next_attempt_at_ms:row.get(5)? })
        ).optional().map_err(db_error)
    }

    pub fn complete_webhook(&self, id: i64, now_ms: i64) -> Result<(), ModemError> {
        self.connection()?
            .execute(
                "UPDATE webhook_outbox SET completed_at_ms=?2,last_error='' WHERE id=?1",
                params![id, now_ms],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn retry_webhook(
        &self,
        id: i64,
        attempt_count: u32,
        next_ms: i64,
        error: &str,
    ) -> Result<(), ModemError> {
        self.connection()?.execute(
            "UPDATE webhook_outbox SET attempt_count=?2,next_attempt_at_ms=?3,last_error=?4 WHERE id=?1",
            params![id,attempt_count,next_ms,crate::integration::safe_delivery_error(error)],
        ).map_err(db_error)?;
        Ok(())
    }
}

fn communication_by_id(
    connection: &Connection,
    id: &str,
) -> Result<Option<RestCommunication>, ModemError> {
    connection.query_row(
        "SELECT id,record_id,channel,owner,destination,content,encrypted,payload_fingerprint,status,created_at_ms,
                COALESCE(sent_at_ms,0),COALESCE(delivered_at_ms,0),COALESCE(failed_at_ms,0),failure_reason
         FROM rest_communications WHERE id=?1", [id], communication_from_row,
    ).optional().map_err(db_error)
}

fn communication_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RestCommunication> {
    Ok(RestCommunication {
        id: row.get(0)?,
        record_id: row.get(1)?,
        channel: row.get(2)?,
        owner: row.get(3)?,
        destination: row.get(4)?,
        content: row.get(5)?,
        encrypted: row.get(6)?,
        payload_fingerprint: row.get(7)?,
        status: row.get(8)?,
        created_at_ms: row.get(9)?,
        sent_at_ms: row.get(10)?,
        delivered_at_ms: row.get(11)?,
        failed_at_ms: row.get(12)?,
        failure_reason: row.get(13)?,
    })
}
