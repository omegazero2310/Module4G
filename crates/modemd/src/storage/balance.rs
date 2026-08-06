use super::*;

impl Store {
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
}
