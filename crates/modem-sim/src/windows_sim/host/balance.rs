use super::*;
use serde_json::{Value, json};

fn active(check: &Value) -> bool {
    matches!(
        check["status"].as_str(),
        Some("queued" | "sending" | "waiting_reply" | "send_unknown")
    )
}

pub(super) fn response(command: &str, request: &Value, state: &mut SimState) -> Value {
    match command {
        "set_balance_scenario" => {
            let scenario = request["scenario"].as_str().unwrap_or_default();
            if !matches!(
                scenario,
                "success"
                    | "zero"
                    | "timeout"
                    | "send-unknown"
                    | "malformed"
                    | "unrelated"
                    | "ambiguous"
                    | "restart"
            ) {
                return json!({"ok":false,"error":"Unknown balance scenario."});
            }
            state.balance_scenario = scenario.into();
            if scenario == "restart" {
                if let Some(check) = state.balance_checks.iter_mut().find(|check| active(check)) {
                    check["status"] = "send_unknown".into();
                }
            }
            json!({"ok":true,"data":null})
        }
        "start_balance_check" => {
            let key = request["request_id"].as_str().unwrap_or_default();
            if key.trim().is_empty() || key.len() > 256 {
                return json!({"ok":false,"error":"request_id must contain 1 to 256 bytes"});
            }
            if let Some(check) = state
                .balance_checks
                .iter()
                .find(|check| check["request_id"] == key)
            {
                return json!({"ok":true,"data":check});
            }
            if state.balance_checks.iter().any(active) {
                return json!({"ok":false,"error":"A balance check is already active. Refresh to view its progress."});
            }
            if state.balance_cooldown > 0 {
                return json!({"ok":false,"error":"Balance check cooldown is active."});
            }
            state.balance_polls = 0;
            let check = json!({"id":format!("sim-balance-check-{}",state.balance_checks.len()+1),"request_id":key,"status":"queued","created_at":"2026-10-08T08:00:00.000Z","updated_at":"2026-10-08T08:00:00.000Z","completed_at":null,"balance":null,"failure_reason":null});
            state.balance_checks.push(check.clone());
            json!({"ok":true,"data":check})
        }
        "get_balance_check" => {
            let Some(index) = state
                .balance_checks
                .iter()
                .position(|check| check["id"] == request["id"])
            else {
                return json!({"ok":false,"error":"Balance check not found."});
            };
            if active(&state.balance_checks[index]) {
                state.balance_polls += 1;
                let unsuccessful = matches!(
                    state.balance_scenario.as_str(),
                    "timeout" | "send-unknown" | "malformed" | "unrelated" | "ambiguous"
                );
                let status = match state.balance_polls {
                    _ if state.balance_scenario == "send-unknown" && state.balance_polls < 4 => {
                        "send_unknown"
                    }
                    1 => "sending",
                    2 => "waiting_reply",
                    3 if unsuccessful => "waiting_reply",
                    _ if unsuccessful => "timed_out",
                    _ => "succeeded",
                };
                let check = &mut state.balance_checks[index];
                check["status"] = status.into();
                if status == "succeeded" {
                    let amount = if state.balance_scenario == "zero" {
                        0
                    } else {
                        85500
                    };
                    check["balance"] =
                        json!({"amount_vnd":amount,"observed_at":"2026-10-08T08:00:18.000Z"});
                    state.balance_history.insert(0,json!({"id":check["id"],"raw":format!("TK goc: {amount}d"),"value":amount,"currency":"VND","error":"","createdAtMs":1791446418000_i64,"smsId":"sim-carrier-reply"}));
                } else if status == "timed_out" {
                    check["failure_reason"] =
                        "no confirmed carrier reply before the deadline".into();
                }
                if matches!(status, "succeeded" | "timed_out") {
                    check["completed_at"] = "2026-10-08T08:00:18.000Z".into();
                    state.balance_cooldown = if status == "succeeded" { 60 } else { 300 };
                }
            }
            json!({"ok":true,"data":state.balance_checks[index]})
        }
        "get_latest_balance" => {
            let latest = state
                .balance_checks
                .iter()
                .rev()
                .find(|check| check["status"] == "succeeded")
                .map(|check| check["balance"].clone());
            let active_id = state
                .balance_checks
                .iter()
                .find(|check| active(check))
                .map(|check| check["id"].clone());
            let freshness = if latest.is_none() {
                "unavailable"
            } else if state
                .balance_checks
                .last()
                .is_some_and(|check| check["status"] == "timed_out")
            {
                "stale"
            } else {
                "fresh"
            };
            let remaining = state.balance_cooldown;
            state.balance_cooldown = remaining.saturating_sub(2);
            json!({"ok":true,"data":{"balance":latest,"freshness":freshness,"active_check_id":active_id,"retry_after_seconds":remaining}})
        }
        "list_balances" => json!({"ok":true,"data":state.balance_history}),
        _ => unreachable!("balance command routing"),
    }
}
