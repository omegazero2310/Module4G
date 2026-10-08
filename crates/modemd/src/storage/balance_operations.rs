use super::*;
use crate::balance::*;

fn load_check(
    connection: &Connection,
    column: &str,
    value: &str,
) -> Result<Option<BalanceCheck>, ModemError> {
    let json: Option<String> = connection
        .query_row(
            &format!("SELECT json FROM balance_operations WHERE {column}=?1"),
            [value],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)?;
    json.map(|json| serde_json::from_str(&json).map_err(db_error))
        .transpose()
}
fn active(connection: &Connection) -> Result<Option<BalanceCheck>, ModemError> {
    let json: Option<String> = connection.query_row("SELECT json FROM balance_operations WHERE status IN ('queued','sending','waiting_reply','send_unknown') LIMIT 1", [], |row| row.get(0)).optional().map_err(db_error)?;
    json.map(|json| serde_json::from_str(&json).map_err(db_error))
        .transpose()
}
fn save(connection: &Connection, check: &BalanceCheck) -> Result<(), ModemError> {
    connection
        .execute(
            "UPDATE balance_operations SET status=?2,json=?3,reply_sms_id=?4 WHERE id=?1",
            params![
                check.id,
                check.status,
                serde_json::to_string(check).map_err(db_error)?,
                check.reply_sms_id
            ],
        )
        .map_err(db_error)?;
    Ok(())
}
fn cooldown(connection: &Connection, stamp: i64) -> Result<i64, ModemError> {
    let mut statement = connection.prepare("SELECT json FROM balance_operations WHERE status IN ('succeeded','failed','timed_out')").map_err(db_error)?;
    let jsons = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_error)?;
    let mut until = 0;
    for json in jsons {
        let check: BalanceCheck =
            serde_json::from_str(&json.map_err(db_error)?).map_err(db_error)?;
        let end = if check.status == "timed_out" {
            check
                .deadline_ms
                .unwrap_or(check.updated_at_ms)
                .saturating_add(QUARANTINE_MS)
        } else {
            check
                .completed_at_ms
                .unwrap_or(check.updated_at_ms)
                .saturating_add(COOLDOWN_MS)
        };
        until = until.max(end);
    }
    Ok(until.saturating_sub(stamp).max(0))
}

impl Store {
    pub fn balance_check(&self, id: &str) -> Result<Option<BalanceCheck>, ModemError> {
        load_check(&*self.connection()?, "id", id)
    }
    pub fn active_balance_check(&self) -> Result<Option<BalanceCheck>, ModemError> {
        active(&*self.connection()?)
    }
    pub fn reserve_balance_check(
        &self,
        request_id: &str,
        stamp: i64,
        ready: bool,
    ) -> Result<BalanceReservation, ModemError> {
        if request_id.trim().is_empty() || request_id.len() > 256 {
            return Err(ModemError::Validation(
                "request_id must contain 1 to 256 bytes".into(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        if let Some(check) = load_check(&transaction, "request_id", request_id)? {
            return Ok(BalanceReservation::Replay(check));
        }
        if let Some(check) = active(&transaction)? {
            return Ok(BalanceReservation::Active(check));
        }
        let remaining = cooldown(&transaction, stamp)?;
        if remaining > 0 {
            return Ok(BalanceReservation::Cooldown(remaining));
        }
        if !ready {
            return Ok(BalanceReservation::Unavailable);
        }
        let check = BalanceCheck {
            id: uuid::Uuid::new_v4().to_string(),
            request_id: request_id.into(),
            status: "queued".into(),
            created_at_ms: stamp,
            updated_at_ms: stamp,
            completed_at_ms: None,
            dispatched_at_ms: None,
            deadline_ms: Some(stamp.saturating_add(REPLY_TIMEOUT_MS)),
            amount_vnd: None,
            observed_at_ms: None,
            reply_sms_id: None,
            failure_reason: None,
            baseline: vec![],
        };
        transaction
            .execute(
                "INSERT INTO balance_operations(id,request_id,status,json) VALUES(?1,?2,?3,?4)",
                params![
                    check.id,
                    check.request_id,
                    check.status,
                    serde_json::to_string(&check).map_err(db_error)?
                ],
            )
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        Ok(BalanceReservation::New(check))
    }
    pub fn claim_balance_dispatch(&self, stamp: i64) -> Result<Option<BalanceCheck>, ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        let Some(mut check) = active(&transaction)? else {
            return Ok(None);
        };
        if check.status != "queued" {
            return Ok(None);
        }
        if check.deadline_ms.is_some_and(|deadline| stamp >= deadline) {
            check.status = "timed_out".into();
            check.completed_at_ms = Some(stamp);
            check.updated_at_ms = stamp;
            check.failure_reason =
                Some("balance query could not be scheduled before the deadline".into());
            save(&transaction, &check)?;
            transaction.commit().map_err(db_error)?;
            return Ok(None);
        }
        check.baseline = {
            let mut statement = transaction
                .prepare("SELECT fingerprint FROM sms WHERE direction='inbound'")
                .map_err(db_error)?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(db_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_error)?
        };
        check.status = "sending".into();
        check.dispatched_at_ms = Some(stamp);
        check.deadline_ms = Some(stamp.saturating_add(REPLY_TIMEOUT_MS));
        check.updated_at_ms = stamp;
        save(&transaction, &check)?;
        transaction.commit().map_err(db_error)?;
        Ok(Some(check))
    }
    pub fn mark_balance_submission(
        &self,
        id: &str,
        status: &str,
        stamp: i64,
    ) -> Result<(), ModemError> {
        if !matches!(status, "waiting_reply" | "failed" | "send_unknown") {
            return Err(ModemError::Validation("invalid balance transition".into()));
        }
        let connection = self.connection()?;
        let Some(mut check) = load_check(&connection, "id", id)? else {
            return Ok(());
        };
        if check.status != "sending" {
            return Ok(());
        }
        check.status = status.into();
        check.updated_at_ms = stamp;
        if status == "failed" {
            check.completed_at_ms = Some(stamp);
            check.failure_reason = Some("balance query SMS was rejected".into());
        }
        save(&connection, &check)
    }
    pub fn recover_balance_checks(&self, stamp: i64) -> Result<(), ModemError> {
        let connection = self.connection()?;
        if let Some(mut check) = active(&connection)? {
            if check.status == "sending" {
                check.status = "send_unknown".into();
                check.updated_at_ms = stamp;
                save(&connection, &check)?;
            }
        }
        Ok(())
    }
    pub fn reconcile_balance_check(&self, stamp: i64) -> Result<(), ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        let Some(mut check) = active(&transaction)? else {
            return Ok(());
        };
        if check.status == "sending" {
            return Ok(());
        }
        let candidates = if let Some(dispatched) = check.dispatched_at_ms {
            let mut statement = transaction.prepare("SELECT id,body,created_at_ms,fingerprint FROM sms WHERE direction='inbound' AND source='sim' AND peer='191' AND multipart_complete=1 AND superseded=0 AND created_at_ms>=?1 AND created_at_ms<=?2 AND NOT EXISTS(SELECT 1 FROM balance_operations WHERE reply_sms_id=sms.id)").map_err(db_error)?;
            let rows = statement
                .query_map(
                    params![dispatched / 1000 * 1000, check.deadline_ms],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .map_err(db_error)?;
            let mut candidates = Vec::new();
            for row in rows {
                let (id, body, observed, fingerprint) = row.map_err(db_error)?;
                if !check.baseline.contains(&fingerprint) {
                    if let Some(amount) = parse_main_balance(&body) {
                        candidates.push((id, body, observed, amount));
                    }
                }
            }
            candidates
        } else {
            Vec::new()
        };
        if candidates.len() == 1 {
            let (sms_id, raw, observed, amount) = &candidates[0];
            check.status = "succeeded".into();
            check.amount_vnd = Some(*amount);
            check.observed_at_ms = Some(*observed);
            check.reply_sms_id = Some(sms_id.clone());
            check.completed_at_ms = Some(stamp);
            check.updated_at_ms = stamp;
            transaction.execute("INSERT INTO balance_checks(id,raw,value,currency,error,created_at_ms,sms_id) VALUES(?1,?2,?3,'VND','',?4,?5)", params![check.id, raw, *amount as f64, observed, sms_id]).map_err(db_error)?;
            save(&transaction, &check)?;
        } else if check.deadline_ms.is_some_and(|deadline| stamp >= deadline) {
            check.failure_reason = Some(
                if check.status == "send_unknown" {
                    "no confirmed carrier reply; SMS submission remained unknown"
                } else if candidates.len() > 1 {
                    "carrier replies were ambiguous"
                } else {
                    "no confirmed carrier reply before the deadline"
                }
                .into(),
            );
            check.status = "timed_out".into();
            check.completed_at_ms = Some(stamp);
            check.updated_at_ms = stamp;
            save(&transaction, &check)?;
        }
        transaction.commit().map_err(db_error)
    }
    pub fn latest_balance(&self, stamp: i64) -> Result<LatestBalance, ModemError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT json FROM balance_operations")
            .map_err(db_error)?;
        let checks = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .map(|json| {
                serde_json::from_str::<BalanceCheck>(&json.map_err(db_error)?).map_err(db_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let latest_failure = checks
            .iter()
            .filter(|check| matches!(check.status.as_str(), "failed" | "timed_out"))
            .map(|check| check.created_at_ms)
            .max();
        let newest = checks
            .into_iter()
            .filter(|check| check.status == "succeeded")
            .max_by_key(|check| check.observed_at_ms);
        let freshness = match &newest {
            Some(check)
                if stamp.saturating_sub(check.observed_at_ms.unwrap_or(0)) < FRESHNESS_MS
                    && !latest_failure.is_some_and(|failed| failed > check.created_at_ms) =>
            {
                "fresh"
            }
            Some(_) => "stale",
            None => "unavailable",
        };
        Ok(LatestBalance {
            balance: newest.and_then(|check| BalanceCheckData::from(check).balance),
            freshness: freshness.into(),
            active_check_id: active(&connection)?.map(|check| check.id),
            retry_after_seconds: ((cooldown(&connection, stamp)? + 999) / 1000) as u64,
        })
    }
}
