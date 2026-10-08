use modemd::{
    balance::*,
    storage::{SmsRecord, Store},
};

fn reply(id: &str, body: &str, stamp: i64) -> SmsRecord {
    SmsRecord {
        id: id.into(),
        fingerprint: id.into(),
        direction: "inbound".into(),
        source: "sim".into(),
        peer: "191".into(),
        body: body.into(),
        multipart_complete: true,
        created_at_ms: stamp,
        synchronized_at_ms: stamp,
        ..Default::default()
    }
}

#[test]
fn reservation_replays_before_hardware_and_preserves_single_active_check() {
    let store = Store::memory().unwrap();
    let check = match store.reserve_balance_check("key", 1000, true).unwrap() {
        BalanceReservation::New(check) => check,
        _ => panic!("expected new"),
    };
    assert!(
        matches!(store.reserve_balance_check("key", 1001, false).unwrap(), BalanceReservation::Replay(found) if found.id == check.id)
    );
    assert!(
        matches!(store.reserve_balance_check("different", 1002, true).unwrap(), BalanceReservation::Active(found) if found.id == check.id)
    );
    assert!(store.reserve_balance_check(" ", 1003, true).is_err());
}

#[test]
fn reply_is_correlated_once_and_completed_atomically() {
    let store = Store::memory().unwrap();
    store.save_sms(&reply("old", "TK goc: 100d", 900)).unwrap();
    store.reserve_balance_check("key", 1000, true).unwrap();
    let check = store.claim_balance_dispatch(1100).unwrap().unwrap();
    store
        .mark_balance_submission(&check.id, "waiting_reply", 1200)
        .unwrap();
    store
        .save_sms(&reply("new", "TK goc: 0d; TK khuyen mai: 100d", 1300))
        .unwrap();
    store.reconcile_balance_check(1400).unwrap();
    let done = store.balance_check(&check.id).unwrap().unwrap();
    assert_eq!(done.status, "succeeded");
    assert_eq!(done.amount_vnd, Some(0));
    assert_eq!(done.reply_sms_id.as_deref(), Some("new"));
    store.reconcile_balance_check(1500).unwrap();
    assert_eq!(store.list_balances(10).unwrap().len(), 1);
    assert!(matches!(
        store.reserve_balance_check("next", 1500, true).unwrap(),
        BalanceReservation::Cooldown(_)
    ));
}

#[test]
fn rejects_incomplete_old_wrong_sender_and_ambiguous_replies() {
    let store = Store::memory().unwrap();
    store.reserve_balance_check("key", 1000, true).unwrap();
    let check = store.claim_balance_dispatch(1100).unwrap().unwrap();
    store
        .mark_balance_submission(&check.id, "waiting_reply", 1200)
        .unwrap();
    let mut incomplete = reply("incomplete", "TK goc: 100d", 1300);
    incomplete.multipart_complete = false;
    let mut other = reply("other", "TK goc: 100d", 1300);
    other.peer = "untrusted".into();
    for r in [
        incomplete,
        other,
        reply("late-old", "TK goc: 100d", 800),
        reply("data", "Data: 500 MB", 1300),
    ] {
        store.save_sms(&r).unwrap();
    }
    store.reconcile_balance_check(1400).unwrap();
    assert_eq!(
        store.balance_check(&check.id).unwrap().unwrap().status,
        "waiting_reply"
    );
    for id in ["one", "two"] {
        store.save_sms(&reply(id, "TK goc: 100d", 1500)).unwrap();
    }
    store.reconcile_balance_check(1600).unwrap();
    assert_eq!(
        store.balance_check(&check.id).unwrap().unwrap().status,
        "waiting_reply"
    );
    store
        .reconcile_balance_check(check.deadline_ms.unwrap() + 1)
        .unwrap();
    assert_eq!(
        store.balance_check(&check.id).unwrap().unwrap().status,
        "timed_out"
    );
}

#[test]
fn restart_does_not_redispatch_unknown_submission_and_processes_durable_reply() {
    let store = Store::memory().unwrap();
    store.reserve_balance_check("key", 1000, true).unwrap();
    let check = store.claim_balance_dispatch(1100).unwrap().unwrap();
    store.recover_balance_checks(1200).unwrap();
    assert!(store.claim_balance_dispatch(1300).unwrap().is_none());
    assert_eq!(
        store.balance_check(&check.id).unwrap().unwrap().status,
        "send_unknown"
    );
    store
        .save_sms(&reply("reply", "TK goc: 123.000d", 1400))
        .unwrap();
    store
        .reconcile_balance_check(check.deadline_ms.unwrap() + 1)
        .unwrap();
    assert_eq!(
        store.balance_check(&check.id).unwrap().unwrap().amount_vnd,
        Some(123000)
    );
}

#[test]
fn migration_retains_legacy_balance_and_pending_operations() {
    let store = Store::memory().unwrap();
    store
        .save_balance(&modemd::storage::BalanceRecord {
            id: "legacy".into(),
            raw: "unchanged".into(),
            ..Default::default()
        })
        .unwrap();
    store.reserve_balance_check("key", 1000, true).unwrap();
    assert_eq!(store.schema_version().unwrap(), 11);
    assert_eq!(store.list_balances(10).unwrap()[0].raw, "unchanged");
    assert!(store.active_balance_check().unwrap().is_some());
}

#[test]
fn never_dispatches_a_queued_operation_after_its_deadline() {
    let store = Store::memory().unwrap();
    store.reserve_balance_check("key", 1000, true).unwrap();
    assert!(
        store
            .claim_balance_dispatch(1000 + REPLY_TIMEOUT_MS + 1)
            .unwrap()
            .is_none()
    );
    assert!(store.active_balance_check().unwrap().is_none());
}

#[test]
fn failed_refresh_marks_the_previous_snapshot_stale_without_losing_zero() {
    let store = Store::memory().unwrap();
    store.reserve_balance_check("first", 1000, true).unwrap();
    let first = store.claim_balance_dispatch(1100).unwrap().unwrap();
    store
        .mark_balance_submission(&first.id, "waiting_reply", 1200)
        .unwrap();
    store.save_sms(&reply("reply", "TK goc: 0d", 1300)).unwrap();
    store.reconcile_balance_check(1400).unwrap();
    assert_eq!(store.latest_balance(1500).unwrap().freshness, "fresh");
    store
        .reserve_balance_check("next", 1400 + COOLDOWN_MS, true)
        .unwrap();
    let next = store
        .claim_balance_dispatch(1500 + COOLDOWN_MS)
        .unwrap()
        .unwrap();
    store
        .mark_balance_submission(&next.id, "failed", 1600 + COOLDOWN_MS)
        .unwrap();
    let latest = store.latest_balance(1700 + COOLDOWN_MS).unwrap();
    assert_eq!(latest.freshness, "stale");
    assert_eq!(latest.balance.unwrap().amount_vnd, 0);
}

#[test]
fn concurrent_duplicate_requests_reserve_one_operation() {
    let store = std::sync::Arc::new(Store::memory().unwrap());
    let joins: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || store.reserve_balance_check("same-key", 1000, true).unwrap())
        })
        .collect();
    let results: Vec<_> = joins.into_iter().map(|join| join.join().unwrap()).collect();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, BalanceReservation::New(_)))
            .count(),
        1
    );
}
