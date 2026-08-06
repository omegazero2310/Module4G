#[cfg(windows)]
mod windows_sim {
    use std::io;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;

    const PIPE: &str = r"\\.\pipe\a7670-modemd-v1";

    #[derive(Clone, Copy, Default)]
    enum CallScenario {
        #[default]
        Answered,
        NoAnswer,
        Busy,
        Unreachable,
        NetworkError,
        MissingTerminal,
    }

    #[derive(Default)]
    struct SimState {
        legacy_scenario: CallScenario,
        current_audio: Option<serde_json::Value>,
        audio: Vec<serde_json::Value>,
        audio_sequence: u64,
        calls: Vec<serde_json::Value>,
        call_scenario: String,
        polls: u32,
        settings: Option<serde_json::Value>,
        hang_attempts: u32,
        sms: Vec<serde_json::Value>,
        sms_polls: u32,
    }

    fn advance_calls(state: &mut SimState) {
        state.polls += 1;
        if let Some(call) = state.calls.first_mut() {
            let (status, answer, reason, error) = match (state.call_scenario.as_str(), state.polls)
            {
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
                ("early-remote-hang-up", 2..) => ("ended", "answered", "remote-hang-up", ""),
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
    }

    fn json_response(request: &str, state: &mut SimState) -> Option<String> {
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
            "get_current_audio" => serde_json::json!({"ok":true,"data":state.current_audio}),
            "list_audio" => serde_json::json!({"ok":true,"data":state.audio}),
            "get_call_data" => {
                advance_calls(state);
                serde_json::json!({"ok":true,"data":{"calls":state.calls,"audio":state.audio}})
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
                            ("delayed-answer", 1..=5) => {
                                ("waiting-for-answer", "unknown", "none", "")
                            }
                            (_, 1) => ("playback-delay", "answered", "none", ""),
                            (_, poll) if (2..=4).contains(&poll) => {
                                ("playing", "answered", "none", "")
                            }
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

    fn sms_record(
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

    fn response(command: &str, scenario: &mut CallScenario) -> &'static str {
        let command = command.trim();
        if command.contains("\"command\":\"sync_sms\"") {
            return "{\"ok\":true,\"data\":{\"count\":7}}\n";
        }
        if command.contains("\"command\":\"list_sms\"") {
            return "{\"ok\":true,\"data\":[{\"id\":\"sim-unread\",\"direction\":\"inbound\",\"peer\":\"191\",\"body\":\"Balance line one\\nBalance line two\",\"state\":\"unread\",\"detail\":\"\",\"createdAtMs\":1785722400000,\"answerClassification\":\"\",\"endReason\":\"\",\"alertingAtMs\":0,\"releaseCause\":\"\",\"kind\":\"received\",\"source\":\"sim\",\"storage\":\"SM\",\"storageIndex\":1,\"modemStatus\":\"REC UNREAD\",\"modemTimestamp\":\"26/08/03,10:00:00+28\",\"encoding\":\"GSM\",\"dcs\":0,\"length\":33,\"serviceCenter\":\"\",\"messageReference\":\"\",\"deliveryStatus\":\"\",\"synchronizedAtMs\":1785722400000,\"presentOnModem\":true,\"smsId\":\"\"}]}\n";
        }
        if command.contains("\"command\":\"list_balances\"") {
            return "{\"ok\":true,\"data\":[]}\n";
        }
        if command.contains("\"command\":\"check_balance\"") {
            return "{\"ok\":true,\"data\":{\"id\":\"balance-1\",\"raw\":\"Tai khoan goc: 85.500d\\nKhuyen mai: 89.174d\",\"value\":null,\"currency\":\"\",\"error\":\"\",\"createdAtMs\":1785722400000,\"smsId\":\"balance-sms-1\"}}\n";
        }
        if command.contains("\"command\":\"send_sms\"") {
            return "{\"ok\":true,\"data\":{\"id\":\"sent-1\",\"direction\":\"outbound\",\"peer\":\"+84912345678\",\"body\":\"simulated\",\"state\":\"submitted\",\"detail\":\"\",\"createdAtMs\":1785722400000,\"answerClassification\":\"\",\"endReason\":\"\",\"alertingAtMs\":0,\"releaseCause\":\"\",\"kind\":\"submitted\",\"source\":\"app\",\"storage\":\"\",\"storageIndex\":-1,\"modemStatus\":\"\",\"modemTimestamp\":\"\",\"encoding\":\"GSM-7\",\"dcs\":-1,\"length\":9,\"serviceCenter\":\"\",\"messageReference\":\"42\",\"deliveryStatus\":\"\",\"synchronizedAtMs\":0,\"presentOnModem\":false,\"smsId\":\"\"}}\n";
        }
        if command.starts_with("SMS|") {
            return "+CMGS: 42\r\nOK\n";
        }
        if command == "BALANCE" {
            return "Thuê bao: 84XXXXXXXXX (HISCL):\r\n- TK gốc: 85.500đ, HSD: 00:00:00 02-10-2026.\r\n- TK tiền di động: 863đ, HSD: 00:00:00 01-01-2100.\r\n- TK tiền khuyến mại: 89.174đ.\r\nĐể tìm hiểu và đăng ký các gói data ưu đãi, truy cập https://vietteltelecom.vn/goidatahot\n";
        }
        if command.starts_with("USSD|") {
            return "+CUSD: 0,\"Balance 125.50 THB\",15\r\nOK\n";
        }
        if let Some(number) = command.strip_prefix("DIAL|") {
            *scenario = match &number[number.len().saturating_sub(2)..] {
                "02" => CallScenario::NoAnswer,
                "03" => CallScenario::Busy,
                "04" => CallScenario::Unreachable,
                "05" => CallScenario::NetworkError,
                "06" => CallScenario::MissingTerminal,
                _ => CallScenario::Answered,
            };
            return "OK\n";
        }
        if command == "HANGUP" {
            return "OK\n";
        }
        if command == "CALLCAUSE" {
            return match scenario {
                CallScenario::Unreachable => "+CEER: 20 Subscriber absent\n",
                CallScenario::NetworkError => "+CEER: 34 No circuit/channel available\n",
                _ => "+CEER: 16 Normal call clearing\n",
            };
        }
        if command == "CALLSTATUS" {
            return match scenario {
                CallScenario::Answered => {
                    "+CLCC: 1,0,0,0,0 | VOICE CALL: BEGIN | VOICE CALL: END | NO CARRIER\n"
                }
                CallScenario::NoAnswer => "NO ANSWER | VOICE CALL: END | NO CARRIER\n",
                CallScenario::Busy => "BUSY | NO CARRIER\n",
                CallScenario::Unreachable | CallScenario::NetworkError => "NO CARRIER\n",
                CallScenario::MissingTerminal => "+CLCC: 1,0,2,0,0\n",
            };
        }
        match command {
            "STATUS" => "STATUS\t0.1.0-sim\tReady\tSIMULATED\tREADY\tRegistered\t20\n",
            "AT" | "AT+CMEE=2" | "AT+CVHU=0" | "AT+CLCC=1" | "AT+CMGF=1" | "AT+CNMI=2,1,0,1,0"
            | "AT+CSMP=49,167,0,0" => "OK\n",
            "AT+CNMI?" => "+CNMI: 2,1,0,1,0\r\nOK\n",
            "AT+CSMP?" => "+CSMP: 49,167,0,0\r\nOK\n",
            "ATI" => "SIMCOM_Ltd\r\nSIMCOM_SIM7600G-H\r\nRevision: A7670M7_V1.11\r\nOK\n",
            "AT+CSQ" => "+CSQ: 20,99\r\nOK\n",
            "AT+CPIN?" => "+CPIN: READY\r\nOK\n",
            "AT+CREG?" => "+CREG: 0,1\r\nOK\n",
            _ => "ERROR\n",
        }
    }

    async fn serve_connection(
        server: tokio::net::windows::named_pipe::NamedPipeServer,
        state: Arc<Mutex<SimState>>,
    ) -> io::Result<()> {
        server.connect().await?;
        let mut line = String::new();
        let mut stream = BufReader::new(server);
        while stream.read_line(&mut line).await? != 0 {
            let reply = {
                let mut state = state.lock().unwrap_or_else(|lock| lock.into_inner());
                json_response(&line, &mut state)
                    .unwrap_or_else(|| response(&line, &mut state.legacy_scenario).to_owned())
            };
            stream.get_mut().write_all(reply.as_bytes()).await?;
            stream.get_mut().flush().await?;
            line.clear();
        }
        Ok(())
    }

    pub async fn run() -> io::Result<()> {
        eprintln!("modem-sim listening on {PIPE}");
        let state = Arc::new(Mutex::new(SimState::default()));
        loop {
            let server = ServerOptions::new().create(PIPE)?;
            if let Err(error) = serve_connection(server, Arc::clone(&state)).await {
                eprintln!("simulator client error: {error}");
            }
        }
    }
}

#[cfg(windows)]
#[tokio::main]
async fn main() -> std::io::Result<()> {
    windows_sim::run().await
}

#[cfg(not(windows))]
fn main() {
    eprintln!("modem-sim named-pipe mode is available only on Windows");
}
