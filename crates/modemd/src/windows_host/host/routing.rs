use super::*;

pub(super) async fn handle_client(
    server: tokio::net::windows::named_pipe::NamedPipeServer,
    context: Arc<RuntimeContext>,
) -> io::Result<()> {
    let device_state = Arc::clone(&context.device_state);
    let delivery_capability = Arc::clone(&context.delivery_capability);
    let delivery_configuration = Arc::clone(&context.delivery_configuration);
    let command_tx = context.command_tx.clone();
    let store = Arc::clone(&context.store);
    let call_manager = Arc::clone(&context.call_manager);
    let mut stream = BufReader::new(server);
    let mut line = String::new();
    while stream.read_line(&mut line).await? != 0 {
        let request = line.trim();
        let response = if request.starts_with('{') {
            handle_json(
                request,
                &command_tx,
                &store,
                &call_manager,
                &delivery_capability,
                &delivery_configuration,
            )
            .await
        } else if request == "STATUS" {
            let state = device_state.read().unwrap_or_else(|lock| lock.into_inner());
            let state = match &*state {
                hardware::HardwareState::Ready { port_name } => format!("Ready\t{port_name}"),
                hardware::HardwareState::PortBusy { port_name } => {
                    format!("Port busy\t{port_name}")
                }
                hardware::HardwareState::Disconnected => "Disconnected\t".to_owned(),
            };
            let capability = delivery_capability
                .read()
                .unwrap_or_else(|lock| lock.into_inner());
            format!(
                "STATUS\t0.1.0\t{state}\tUNKNOWN\tNot registered\t0\t{}\t{}\n",
                capability.available,
                capability.error.replace(['\t', '\r', '\n'], " ")
            )
        } else if let Some(rest) = request.strip_prefix("SMS|") {
            let mut fields = rest.splitn(2, '|');
            let destination = fields.next().unwrap_or_default();
            let body = fields.next().and_then(decode_hex);
            match (modemd::sms::normalize_sms_destination(destination), body) {
                (Ok(destination), Some(body)) => {
                    run_actor(
                        &command_tx,
                        format!("AT+CMGS=\"{destination}\""),
                        Some(body),
                        false,
                    )
                    .await
                }
                (Err(error), _) => format!("ERROR: {error}\n"),
                (_, None) => "ERROR: invalid SMS payload\n".into(),
            }
        } else if request == "BALANCE" {
            match check_viettel_balance(&command_tx, &store).await {
                Ok((body, _)) => format!("{body}\n"),
                Err(error) => format!("ERROR: {error}\n"),
            }
        } else if let Some(code) = request.strip_prefix("USSD|") {
            if code.is_empty() || code.len() > 64 {
                "ERROR: invalid USSD code\n".into()
            } else {
                run_actor(&command_tx, format!("AT+CUSD=1,\"{code}\",15"), None, false).await
            }
        } else if let Some(number) = request.strip_prefix("DIAL|") {
            match modemd::sms::normalize_call_destination(number) {
                Ok(number) => dial_with_release_retry(&command_tx, &number).await,
                Err(error) => format!("ERROR: {error}\n"),
            }
        } else if request == "HANGUP" {
            run_actor(&command_tx, "ATH".into(), None, false).await
        } else if request == "CALLSTATUS" {
            run_actor(&command_tx, "AT+CLCC".into(), None, false).await
        } else if request == "CALLCAUSE" {
            run_actor(&command_tx, "AT+CEER".into(), None, false).await
        } else if request.starts_with("AT") {
            match modemd::at::validate_console(request, false) {
                Err(error) => format!("ERROR: {error}\n"),
                Ok(command) => {
                    let (reply, response) = tokio::sync::oneshot::channel();
                    if command_tx
                        .send(hardware::AtRequest {
                            command,
                            payload: None,
                            guarded: true,
                            payload_mode: PayloadMode::Sms,
                            batch: Vec::new(),
                            finalizer: None,
                            reply,
                        })
                        .is_err()
                    {
                        "ERROR: modem command actor unavailable\n".to_owned()
                    } else {
                        match tokio::time::timeout(Duration::from_secs(3), response).await {
                            Ok(Ok(Ok(lines))) => format!("{}\n", lines.join("\r\n")),
                            Ok(Ok(Err(error))) => format!("ERROR: {error}\n"),
                            Ok(Err(_)) => "ERROR: modem command actor stopped\n".to_owned(),
                            Err(_) => "ERROR: command timed out\n".to_owned(),
                        }
                    }
                }
            }
        } else {
            "ERROR: unsupported request\n".to_owned()
        };
        stream.get_mut().write_all(response.as_bytes()).await?;
        stream.get_mut().flush().await?;
        line.clear();
    }
    Ok(())
}

pub(super) async fn handle_json(
    request: &str,
    tx: &mpsc::Sender<hardware::AtRequest>,
    store: &Store,
    call_manager: &Arc<CallManager>,
    delivery_capability: &Arc<RwLock<DeliveryCapability>>,
    delivery_configuration: &Arc<tokio::sync::Mutex<()>>,
) -> String {
    let value: serde_json::Value = match serde_json::from_str(request) {
        Ok(v) => v,
        Err(e) => return json_error(e),
    };
    let command = value
        .get("command")
        .and_then(|x| x.as_str())
        .unwrap_or_default();
    let result: Result<serde_json::Value, String> = match command {
        "list_sms" => store
            .list_sms(1000)
            .map(|mut records| {
                // Normalize rows synchronized by older versions too, so an
                // upgrade fixes the visible list without deleting history.
                for record in &mut records {
                    let explicit = record.encoding.eq_ignore_ascii_case("UCS2")
                        || (record.dcs >= 0
                            && ((record.dcs as u8 & 0xc0 == 0 && record.dcs as u8 & 0x0c == 0x08)
                                || record.dcs as u8 & 0xf0 == 0xe0));
                    if let Some(body) = modemd::sms::decode_ucs2_body(&record.body, explicit) {
                        record.body = body;
                        record.encoding = "UCS2".into();
                        record.length = record.body.chars().count() as i32;
                    }
                }
                serde_json::to_value(records).unwrap()
            })
            .map_err(|e| e.to_string()),
        "list_balances" => store
            .list_balances(1000)
            .map(|x| serde_json::to_value(x).unwrap())
            .map_err(|e| e.to_string()),
        "send_sms" => send_sms_json(
            value,
            tx,
            store,
            delivery_capability,
            delivery_configuration,
        )
        .await
        .map(|x| serde_json::to_value(x).unwrap()),
        "sync_sms" => sync_sms_json(tx, store)
            .await
            .map(|x| serde_json::json!({"count":x})),
        "check_balance" => balance_json(tx, store)
            .await
            .map(|x| serde_json::to_value(x).unwrap()),
        "get_current_audio" => call_manager
            .current_audio()
            .map(|audio| serde_json::to_value(audio).unwrap())
            .map_err(|e| e.to_string()),
        "list_audio" => call_manager
            .list_audio()
            .map(|audio| serde_json::to_value(audio).unwrap())
            .map_err(|e| e.to_string()),
        "get_call_data" => call_manager
            .list_calls(1000)
            .and_then(|calls| {
                call_manager
                    .list_audio()
                    .map(|audio| serde_json::json!({"calls": calls, "audio": audio}))
            })
            .map_err(|e| e.to_string()),
        "select_audio" => call_manager
            .select_audio(
                value
                    .get("audioId")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default(),
            )
            .map(|audio| serde_json::to_value(audio).unwrap())
            .map_err(|e| e.to_string()),
        "upload_audio" => upload_audio_json(value, call_manager)
            .await
            .map(|audio| serde_json::to_value(audio).unwrap()),
        "make_call" => make_call_json(value, call_manager)
            .await
            .map(|call| serde_json::to_value(call).unwrap()),
        "hang_up" => call_manager
            .hang_up()
            .await
            .map(|()| serde_json::Value::Null)
            .map_err(|e| e.to_string()),
        "list_calls" => call_manager
            .list_calls(1000)
            .map(|calls| serde_json::to_value(calls).unwrap())
            .map_err(|e| e.to_string()),
        "get_settings" => Ok(serde_json::to_value(call_manager.settings()).unwrap()),
        "update_settings" => serde_json::from_value::<Settings>(
            value
                .get("settings")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|_| "invalid settings payload".to_owned())
        .and_then(|settings| {
            call_manager
                .update_settings(settings)
                .map_err(|error| error.to_string())
        })
        .map(|settings| serde_json::to_value(settings).unwrap()),
        _ => Err("unknown JSON command".into()),
    };
    match result {
        Ok(data) => serde_json::json!({"ok":true,"data":data}).to_string() + "\n",
        Err(error) => serde_json::json!({"ok":false,"error":error}).to_string() + "\n",
    }
}
pub(super) fn json_error(e: impl std::fmt::Display) -> String {
    serde_json::json!({"ok":false,"error":e.to_string()}).to_string() + "\n"
}
pub(super) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(super) async fn upload_audio_json(
    value: serde_json::Value,
    manager: &CallManager,
) -> Result<modemd::storage::UploadedAudioRecord, String> {
    let name = value
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();
    let data: Vec<u8> = serde_json::from_value(
        value
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    )
    .map_err(|_| "invalid audio payload".to_owned())?;
    manager
        .upload_audio(name, data)
        .await
        .map_err(|e| e.to_string())
}

pub(super) async fn make_call_json(
    value: serde_json::Value,
    manager: &Arc<CallManager>,
) -> Result<modemd::storage::CallRecord, String> {
    let destination = modemd::sms::normalize_call_destination(
        value
            .get("destination")
            .and_then(|value| value.as_str())
            .unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    let audio_id = value
        .get("audioId")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();
    manager
        .make_call(destination, audio_id)
        .await
        .map_err(|e| e.to_string())
}
pub(super) async fn send_sms_json(
    v: serde_json::Value,
    tx: &mpsc::Sender<hardware::AtRequest>,
    store: &Store,
    delivery_capability: &Arc<RwLock<DeliveryCapability>>,
    delivery_configuration: &Arc<tokio::sync::Mutex<()>>,
) -> Result<SmsRecord, String> {
    let peer = modemd::sms::normalize_sms_destination(
        v.get("destination")
            .and_then(|x| x.as_str())
            .unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    let body = v
        .get("body")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_owned();
    modemd::sms::validate_body(&body).map_err(|e| e.to_string())?;
    let mut record = SmsRecord {
        id: ulid::Ulid::new().to_string(),
        direction: "outbound".into(),
        peer: peer.clone(),
        body: body.clone(),
        state: "sending".into(),
        created_at_ms: now(),
        kind: "submitted".into(),
        source: "app".into(),
        part_count: 1,
        parts_received: 1,
        multipart_complete: true,
        encoding: modemd::sms::validate_body(&body).unwrap_or("").into(),
        length: body.chars().count() as i32,
        storage_index: -1,
        ..Default::default()
    };
    if !delivery_capability
        .read()
        .unwrap_or_else(|lock| lock.into_inner())
        .attempted
    {
        let _configuration = delivery_configuration.lock().await;
        let still_needs_configuration = !delivery_capability
            .read()
            .unwrap_or_else(|lock| lock.into_inner())
            .attempted;
        if still_needs_configuration {
            let capability = configure_delivery_tracking(tx).await;
            *delivery_capability
                .write()
                .unwrap_or_else(|lock| lock.into_inner()) = capability;
        }
    }
    let capability = delivery_capability
        .read()
        .unwrap_or_else(|lock| lock.into_inner())
        .clone();
    record.delivery_report_requested = capability.report_request_available;
    record.delivery_tracking_error = capability.error.clone();
    store.save_sms(&record).map_err(|e| e.to_string())?;
    let result = actor_lines(
        tx,
        format!("AT+CMGS=\"{peer}\""),
        Some(body.clone().into_bytes()),
    )
    .await;
    let lines = match result {
        Ok(lines) => lines,
        Err(error) => {
            record.state = if is_explicit_send_rejection(&error) {
                "send-failed"
            } else {
                "send-unknown"
            }
            .into();
            record.cause = error.clone();
            store.save_sms(&record).map_err(|e| e.to_string())?;
            return Err(error);
        }
    };
    let mr = lines
        .iter()
        .find_map(|x| x.strip_prefix("+CMGS:").map(|v| v.trim().to_owned()))
        .unwrap_or_default();
    if mr.is_empty() {
        record.state = "send-unknown".into();
        record.cause = "modem returned OK without a +CMGS message reference".into();
        store.save_sms(&record).map_err(|e| e.to_string())?;
        return Err(record.cause.clone());
    }
    record.state =
        if capability.report_request_available && capability.report_reception_available {
            "delivery-pending"
        } else {
            "submitted"
        }
        .into();
    record.message_reference = mr;
    record.cause.clear();
    store.save_sms(&record).map_err(|e| e.to_string())?;
    Ok(record)
}

pub(super) fn is_explicit_send_rejection(error: &str) -> bool {
    let upper = error.to_ascii_uppercase();
    upper.contains("COMMAND REJECTED")
        || upper.contains("+CMS ERROR")
        || upper.contains("+CME ERROR")
        || upper.trim_end().ends_with("ERROR")
}
