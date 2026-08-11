use super::*;

impl Store {
    /// Returns whether the newest classified outbound attempt since `cutoff_ms`
    /// is healthy. Attempts that have not established a dispatch outcome are
    /// deliberately omitted.
    pub fn latest_outbound_health_evidence(
        &self,
        cutoff_ms: i64,
    ) -> Result<Option<bool>, ModemError> {
        self.connection()?
            .query_row(
                "SELECT healthy FROM (
                   SELECT created_at_ms, id, CASE
                     WHEN state IN ('send-failed','send-unknown') THEN 0
                     ELSE 1
                   END AS healthy
                   FROM sms
                   WHERE direction='outbound' AND source='app' AND superseded=0
                     AND created_at_ms>=?1
                     AND state IN ('send-failed','send-unknown','submitted','delivery-pending',
                                   'delivered','delivery-failed','delivery-unknown')
                   UNION ALL
                   SELECT created_at_ms, id, CASE
                     WHEN state IN ('failed','hang-up-failed')
                       AND end_reason IN ('modem-lost','call-error') THEN 0
                     ELSE 1
                   END AS healthy
                   FROM calls
                   WHERE created_at_ms>=?1 AND (
                     (state IN ('failed','hang-up-failed')
                       AND end_reason IN ('modem-lost','call-error','signaling-timeout'))
                     OR state='ended'
                     OR answer_classification='answered'
                     OR end_reason IN ('busy','no-answer','local-hang-up')
                   )
                   UNION ALL
                   SELECT r.created_at_ms, r.id, 0 AS healthy
                   FROM rest_communications r
                   WHERE r.created_at_ms>=?1 AND r.status='failed'
                     AND NOT EXISTS (SELECT 1 FROM sms s WHERE r.channel='sms' AND s.id=r.record_id)
                     AND NOT EXISTS (SELECT 1 FROM calls c WHERE r.channel='call' AND c.id=r.record_id)
                 ) evidence
                 ORDER BY created_at_ms DESC, id DESC
                 LIMIT 1",
                [cutoff_ms],
                |row| row.get::<_, bool>(0),
            )
            .optional()
            .map_err(db_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sms(id: &str, state: &str, created_at_ms: i64) -> SmsRecord {
        SmsRecord {
            id: id.into(),
            direction: "outbound".into(),
            state: state.into(),
            source: "app".into(),
            kind: "submitted".into(),
            created_at_ms,
            ..Default::default()
        }
    }

    fn call(id: &str, state: &str, reason: &str, created_at_ms: i64) -> CallRecord {
        CallRecord {
            id: id.into(),
            state: state.into(),
            end_reason: reason.into(),
            answer_classification: "unknown".into(),
            created_at_ms,
            ..Default::default()
        }
    }

    #[test]
    fn newest_classified_sms_evidence_wins_and_old_or_in_progress_rows_are_ignored() {
        let store = Store::memory().unwrap();
        store
            .save_sms(&sms("old-failure", "send-failed", 10))
            .unwrap();
        store.save_sms(&sms("sending", "sending", 30)).unwrap();
        assert_eq!(store.latest_outbound_health_evidence(11).unwrap(), None);

        store.save_sms(&sms("failure", "send-unknown", 40)).unwrap();
        assert_eq!(
            store.latest_outbound_health_evidence(11).unwrap(),
            Some(false)
        );
        store
            .save_sms(&sms("success", "delivery-failed", 50))
            .unwrap();
        assert_eq!(
            store.latest_outbound_health_evidence(11).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn call_outcomes_distinguish_modem_failures_from_completed_command_handling() {
        let store = Store::memory().unwrap();
        for (index, reason) in ["busy", "no-answer", "local-hang-up", "signaling-timeout"]
            .into_iter()
            .enumerate()
        {
            let state = if reason == "signaling-timeout" {
                "failed"
            } else {
                "ended"
            };
            store
                .save_call(&call(
                    &format!("healthy-{index}"),
                    state,
                    reason,
                    index as i64 + 1,
                ))
                .unwrap();
            assert_eq!(
                store.latest_outbound_health_evidence(0).unwrap(),
                Some(true)
            );
        }
        store
            .save_call(&call("lost", "failed", "modem-lost", 10))
            .unwrap();
        assert_eq!(
            store.latest_outbound_health_evidence(0).unwrap(),
            Some(false)
        );
        store
            .save_call(&call("error", "hang-up-failed", "call-error", 11))
            .unwrap();
        assert_eq!(
            store.latest_outbound_health_evidence(0).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn answered_calls_are_healthy_and_missing_dispatch_records_are_failures() {
        let store = Store::memory().unwrap();
        let mut answered = call("answered", "playing", "none", 20);
        answered.answer_classification = "answered".into();
        store.save_call(&answered).unwrap();
        assert_eq!(
            store.latest_outbound_health_evidence(0).unwrap(),
            Some(true)
        );

        let communication = RestCommunication {
            id: "dispatch-failed".into(),
            request_id: "request-dispatch-failed".into(),
            record_id: "dispatch-failed".into(),
            channel: "call".into(),
            payload_fingerprint: "fingerprint".into(),
            status: "sending".into(),
            created_at_ms: 21,
            ..Default::default()
        };
        store.reserve_rest_communication(&communication).unwrap();
        store
            .mark_rest_dispatch_failed("dispatch-failed", "dispatch failed", 22)
            .unwrap();
        assert_eq!(
            store.latest_outbound_health_evidence(0).unwrap(),
            Some(false)
        );
    }
}
