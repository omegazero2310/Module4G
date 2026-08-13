use super::*;

pub(super) fn json_response(request: &str, state: &mut SimState) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(request.trim()).ok()?;
    let command = value.get("command")?.as_str()?;
    let result = match command {
        "get_settings" => {
            serde_json::json!({"ok":true,"data":state.settings.clone().unwrap_or_else(|| serde_json::json!({
                "usb_vid":7694,"usb_pid":36881,"port_override":null,"baud":115200,
                "call_timeout_seconds":90,"upload_pacing_ms":10,"max_audio_bytes":204800,
                "ussd_code":"*101#","ussd_timeout_seconds":30,"currency":"",
                "low_balance_threshold":0.0,"balance_regex":null
            }))})
        }
        "update_settings" => {
            let settings = value.get("settings").cloned().unwrap_or_default();
            state.settings = Some(settings.clone());
            serde_json::json!({"ok":true,"data":settings})
        }
        "get_integration_settings" => {
            serde_json::json!({"ok":true,"data":state.integration_settings.clone().unwrap_or_else(||serde_json::json!({
                "restEnabled":false,"restBindAddress":"0.0.0.0:5069","webhookUrl":"http://10.1.11.117:5068/api/v1/webhooks/receive","hasRestToken":false,"hasWebhookToken":false
            }))})
        }
        "update_integration_settings" => {
            match atomic_integration_update(value.get("settings"), state) {
                Ok(settings) => serde_json::json!({"ok":true,"data":settings}),
                Err(error) => serde_json::json!({"ok":false,"error":error}),
            }
        }
        "replace_rest_token"
        | "clear_rest_token"
        | "replace_webhook_token"
        | "clear_webhook_token" => {
            let mut settings=state.integration_settings.clone().unwrap_or_else(||serde_json::json!({"restEnabled":false,"restBindAddress":"0.0.0.0:5069","webhookUrl":"http://10.1.11.117:5068/api/v1/webhooks/receive"}));
            if command.contains("rest_token") {
                settings["hasRestToken"] = (command == "replace_rest_token").into();
            } else {
                settings["hasWebhookToken"] = (command == "replace_webhook_token").into();
            }
            state.integration_settings = Some(settings.clone());
            serde_json::json!({"ok":true,"data":settings})
        }
        "get_current_audio" => serde_json::json!({"ok":true,"data":state.current_audio}),
        "list_audio" => serde_json::json!({"ok":true,"data":state.audio}),
        "get_call_data" => {
            advance_calls(state);
            serde_json::json!({"ok":true,"data":{"calls":state.calls,"audio":state.audio,"audioSyncState":"ready"}})
        }
        "select_audio" => {
            let id = value
                .get("audioId")
                .and_then(|x| x.as_str())
                .unwrap_or_default();
            if let Some(index) = state.audio.iter().position(|audio| audio["id"] == id) {
                for audio in &mut state.audio {
                    audio["isCurrent"] = false.into();
                }
                state.audio[index]["isCurrent"] = true.into();
                state.current_audio = Some(state.audio[index].clone());
                serde_json::json!({"ok":true,"data":state.audio[index]})
            } else {
                serde_json::json!({"ok":false,"error":"audio file was not found"})
            }
        }
        "upload_audio" => {
            let name = value
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("call.amr");
            let size = value
                .get("data")
                .and_then(|x| x.as_array())
                .map_or(0, Vec::len);
            state.audio.retain(|audio| {
                !audio["name"]
                    .as_str()
                    .is_some_and(|old| old.trim().eq_ignore_ascii_case(name.trim()))
            });
            for existing in &mut state.audio {
                existing["isCurrent"] = false.into();
            }
            state.audio_sequence += 1;
            let id = format!("sim-audio-{}", state.audio_sequence);
            let audio = serde_json::json!({
                "id":id,"name":name,"format":"AMR-NB","size":size,
                "modulePath":format!("c:/call_{id}.amr"),"durationMs":2000,"createdAtMs":1785722400000_i64 + state.audio.len() as i64,"state":"ready","isCurrent":true
            });
            state.audio.insert(0, audio.clone());
            state.current_audio = Some(audio.clone());
            serde_json::json!({"ok":true,"data":audio})
        }
        "make_call" => {
            if state.current_audio.is_none() {
                serde_json::json!({"ok":false,"error":"select the current uploaded audio"})
            } else {
                let peer = value
                    .get("destination")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default();
                state.call_scenario = match &peer[peer.len().saturating_sub(2)..] {
                    "02" => "no-answer",
                    "03" => "busy",
                    "07" => "playback-failure",
                    "08" => "missing-completion",
                    "09" => "early-remote-hang-up",
                    "10" => "manual-cancellation",
                    "11" => "delayed-answer",
                    "12" => "stuck-release",
                    _ => "success",
                }
                .into();
                state.polls = 0;
                state.hang_attempts = 0;
                let audio_id = state
                    .current_audio
                    .as_ref()
                    .and_then(|audio| audio["id"].as_str())
                    .unwrap_or_default();
                let call = serde_json::json!({
                    "id":"sim-call","peer":peer,"state":"waiting-for-answer","audioId":audio_id,
                    "error":"","durationSeconds":0,"createdAtMs":1785722401000_i64,
                    "answerClassification":"unknown","endReason":"none","connectedAtMs":0,
                    "endedAtMs":0,"releaseCause":""
                });
                state.calls.insert(0, call.clone());
                serde_json::json!({"ok":true,"data":call})
            }
        }
        "list_calls" => {
            state.polls += 1;
            if let Some(call) = state.calls.first_mut() {
                let (status, answer, reason, error) =
                    match (state.call_scenario.as_str(), state.polls) {
                        ("no-answer", 3..) => ("ended", "not-answered", "no-answer", ""),
                        ("busy", 2..) => ("ended", "not-answered", "busy", ""),
                        ("playback-failure", 4..) => {
                            ("failed", "answered", "call-error", "playback start failed")
                        }
                        ("missing-completion", 10..) => (
                            "failed",
                            "answered",
                            "call-error",
                            "playback completion timed out",
                        ),
                        ("early-remote-hang-up", 2..) => {
                            ("ended", "answered", "remote-hang-up", "")
                        }
                        ("manual-cancellation", 3..) => ("playing", "answered", "none", ""),
                        ("stuck-release", _) if call["state"] == "hang-up-failed" => (
                            "hang-up-failed",
                            "answered",
                            "none",
                            "release confirmation timed out",
                        ),
                        ("delayed-answer", 1..=5) => ("waiting-for-answer", "unknown", "none", ""),
                        (_, 1) => ("playback-delay", "answered", "none", ""),
                        (_, poll) if (2..=4).contains(&poll) => ("playing", "answered", "none", ""),
                        (_, poll) if poll >= 5 => ("ended", "answered", "local-hang-up", ""),
                        _ => ("waiting-for-answer", "unknown", "none", ""),
                    };
                call["state"] = status.into();
                call["answerClassification"] = answer.into();
                call["endReason"] = reason.into();
                call["error"] = error.into();
            }
            serde_json::json!({"ok":true,"data":state.calls})
        }
        "hang_up" => {
            state.hang_attempts += 1;
            if let Some(call) = state.calls.first_mut() {
                if state.call_scenario == "stuck-release" && state.hang_attempts == 1 {
                    call["state"] = "hang-up-failed".into();
                    call["error"] = "release confirmation timed out; retry Hang Up".into();
                } else {
                    call["state"] = "ended".into();
                    call["endReason"] = "local-hang-up".into();
                    call["error"] = "".into();
                }
            }
            serde_json::json!({"ok":true,"data":null})
        }
        "send_sms" => {
            let peer = value
                .get("destination")
                .and_then(|x| x.as_str())
                .unwrap_or_default();
            let body = value
                .get("body")
                .and_then(|x| x.as_str())
                .unwrap_or_default();
            let state_name = if peer.ends_with("91") {
                "send-failed"
            } else if peer.ends_with("92") {
                "send-unknown"
            } else if peer.ends_with("95") {
                "submitted"
            } else {
                "delivery-pending"
            };
            let record = sms_record(
                &format!("sim-sms-{}", state.sms.len() + 1),
                "outbound",
                peer,
                body,
                state_name,
                "submitted",
                "app",
                "42",
                "",
                1785726000000,
            );
            state.sms.insert(0, record.clone());
            if matches!(state_name, "submitted" | "delivery-pending") {
                serde_json::json!({"ok":true,"data":record})
            } else {
                serde_json::json!({"ok":false,"error":if state_name=="send-failed"{"modem rejected command: +CMS ERROR: 500"}else{"modem command timed out; send result unknown"}})
            }
        }
        "sync_sms" => {
            state.sms_polls += 1;
            for sms in &mut state.sms {
                if sms["state"] == "unread" {
                    sms["state"] = "read".into();
                    sms["modemStatus"] = "REC READ".into();
                }
                let peer = sms["peer"].as_str().unwrap_or_default();
                let delayed = peer.ends_with("96");
                let missing = peer.ends_with("93");
                if sms["state"] == "delivery-pending"
                    && !missing
                    && state.sms_polls >= if delayed { 4 } else { 2 }
                {
                    sms["state"] = if sms["peer"]
                        .as_str()
                        .is_some_and(|peer| peer.ends_with("94"))
                    {
                        "delivery-failed".into()
                    } else {
                        "delivered".into()
                    };
                    sms["deliveryStatus"] = if sms["state"] == "delivered" {
                        "0x00".into()
                    } else {
                        "0x40".into()
                    };
                }
            }
            serde_json::json!({"ok":true,"data":{"count":state.sms.len()}})
        }
        "list_sms" => {
            if state.sms.is_empty() {
                state.sms.push(sms_record(
                    "sim-volte",
                    "inbound",
                    "191",
                    "Quy khach da dang ky thanh cong dich vu VoLTE",
                    "unread",
                    "received",
                    "sim",
                    "",
                    "26/08/03,10:00:00+28",
                    1785726000000,
                ));
            }
            serde_json::json!({"ok":true,"data":state.sms})
        }
        "check_balance" => {
            let sms = sms_record(
                "sim-balance-sms",
                "inbound",
                "191",
                "TK goc: 85.500d; tai khoan khuyen mai: 89.174d",
                "unread",
                "received",
                "sim",
                "",
                "26/08/03,10:01:00+28",
                1785726060000,
            );
            if !state.sms.iter().any(|item| item["id"] == "sim-balance-sms") {
                state.sms.insert(0, sms);
            }
            serde_json::json!({"ok":true,"data":{"id":"balance-1","raw":"TK goc: 85.500d; tai khoan khuyen mai: 89.174d","value":null,"currency":"","error":"","createdAtMs":1785726060000_i64,"smsId":"sim-balance-sms"}})
        }
        _ => return None,
    };
    Some(result.to_string() + "\n")
}

fn atomic_integration_update(
    requested: Option<&serde_json::Value>,
    state: &mut SimState,
) -> Result<serde_json::Value, String> {
    let requested = requested
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "invalid integration settings payload".to_owned())?;
    let string = |name: &str| {
        requested
            .get(name)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "invalid integration settings payload".to_owned())
    };
    let boolean = |name: &str| {
        requested
            .get(name)
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| "invalid integration settings payload".to_owned())
    };
    let rest_enabled = boolean("restEnabled")?;
    let rest_bind_address = string("restBindAddress")?.trim();
    let webhook_url = string("webhookUrl")?.trim();
    let rest_token = requested
        .get("restToken")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    let webhook_token = requested
        .get("webhookToken")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    let clear_rest = requested
        .get("clearRestToken")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let clear_webhook = requested
        .get("clearWebhookToken")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if clear_rest && !rest_token.is_empty() {
        return Err("REST token cannot be replaced and cleared in the same request".into());
    }
    if clear_webhook && !webhook_token.is_empty() {
        return Err("webhook token cannot be replaced and cleared in the same request".into());
    }
    if rest_bind_address.parse::<std::net::SocketAddr>().is_err() {
        return Err("REST bind address must be an IP address and port".into());
    }
    let valid_webhook = ["http://", "https://"].iter().any(|prefix| {
        webhook_url
            .strip_prefix(prefix)
            .is_some_and(|remainder| !remainder.is_empty() && !remainder.starts_with('/'))
    });
    if !valid_webhook {
        return Err("webhook URL must use http or https".into());
    }

    let old = state.integration_settings.as_ref();
    let mut has_rest_token = old
        .and_then(|settings| settings.get("hasRestToken"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut has_webhook_token = old
        .and_then(|settings| settings.get("hasWebhookToken"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if clear_rest {
        has_rest_token = false;
    } else if !rest_token.is_empty() {
        has_rest_token = true;
    }
    if clear_webhook {
        has_webhook_token = false;
    } else if !webhook_token.is_empty() {
        has_webhook_token = true;
    }
    if rest_enabled && !has_rest_token {
        return Err("REST requires a bearer token before it can be enabled".into());
    }
    let settings = serde_json::json!({
        "restEnabled":rest_enabled,
        "restBindAddress":rest_bind_address,
        "webhookUrl":webhook_url,
        "hasRestToken":has_rest_token,
        "hasWebhookToken":has_webhook_token
    });
    state.integration_settings = Some(settings.clone());
    Ok(settings)
}

pub(super) fn sms_record(
    id: &str,
    direction: &str,
    peer: &str,
    body: &str,
    state: &str,
    kind: &str,
    source: &str,
    message_reference: &str,
    modem_timestamp: &str,
    created_at_ms: i64,
) -> serde_json::Value {
    serde_json::json!({"id":id,"direction":direction,"peer":peer,"body":body,"state":state,"detail":"","cause":"","createdAtMs":created_at_ms,"answerClassification":"","endReason":"","releaseCause":"","kind":kind,"source":source,"storage":if source=="sim"{"SM"}else{""},"storageIndex":if source=="sim"{1}else{-1},"storageIndices":if source=="sim"{vec![1]}else{vec![]},"partCount":1,"partsReceived":1,"multipartComplete":true,"modemStatus":if state=="unread"{"REC UNREAD"}else{""},"modemTimestamp":modem_timestamp,"encoding":"GSM-7","dcs":0,"length":body.chars().count(),"serviceCenter":"","messageReference":message_reference,"deliveryStatus":"","deliveryReportRequested":source=="app"&&state!="submitted","deliveryReportScts":"","deliveryReportDischargeTime":"","deliveryTrackingError":if source=="app"&&state=="submitted"{"simulated tracking configuration degradation"}else{""},"synchronizedAtMs":created_at_ms,"presentOnModem":source=="sim","smsId":"","audioId":"","error":"","durationSeconds":0,"connectedAtMs":0,"endedAtMs":0})
}
