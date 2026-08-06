use super::*;

impl Store {
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

    pub fn mark_sms_archived(&self, records: &[SmsRecord]) -> Result<(), ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        for record in records.iter().filter(|record| record.source == "sim") {
            transaction
                .execute(
                    "UPDATE sms SET present_on_modem=0 WHERE source='sim' AND storage=?1 AND fingerprint=?2",
                    params![record.storage, record.fingerprint],
                )
                .map_err(db_error)?;
        }
        transaction.commit().map_err(db_error)
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
        s.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
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
}
