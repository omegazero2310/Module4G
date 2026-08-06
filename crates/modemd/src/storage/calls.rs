use super::*;

impl Store {
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
}
