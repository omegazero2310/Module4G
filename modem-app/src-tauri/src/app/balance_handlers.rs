use super::*;

#[tauri::command]
pub(super) async fn start_balance_check(
    request_id: String,
) -> Result<modemd::balance::BalanceCheckData, String> {
    #[cfg(windows)]
    {
        request_json(serde_json::json!({"command":"start_balance_check","request_id":request_id}))
            .await
    }
    #[cfg(not(windows))]
    {
        let _ = request_id;
        Err("Balance checks are available only on Windows.".into())
    }
}
#[tauri::command]
pub(super) async fn get_balance_check(
    id: String,
) -> Result<modemd::balance::BalanceCheckData, String> {
    #[cfg(windows)]
    {
        request_json(serde_json::json!({"command":"get_balance_check","id":id})).await
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Err("Balance checks are available only on Windows.".into())
    }
}
#[tauri::command]
pub(super) async fn get_latest_balance() -> Result<modemd::balance::LatestBalance, String> {
    #[cfg(windows)]
    {
        request_json(serde_json::json!({"command":"get_latest_balance"})).await
    }
    #[cfg(not(windows))]
    {
        Ok(modemd::balance::LatestBalance {
            balance: None,
            freshness: "unavailable".into(),
            active_check_id: None,
            retry_after_seconds: 0,
        })
    }
}

#[cfg(windows)]
#[tauri::command]
pub(super) async fn check_balance(_state: tauri::State<'_, AppState>) -> Result<Record, String> {
    let wire: BalanceWire = request_json(serde_json::json!({"command":"check_balance"})).await?;
    Ok(wire.into())
}
#[cfg(not(windows))]
#[tauri::command]
pub(super) async fn check_balance(_state: tauri::State<'_, AppState>) -> Result<Record, String> {
    Err("Balance checks are available only on Windows.".into())
}
#[tauri::command]
pub(super) async fn list_balance_checks(
    _state: tauri::State<'_, AppState>,
) -> Result<Vec<Record>, String> {
    #[cfg(windows)]
    {
        Ok(
            request_json::<Vec<BalanceWire>>(serde_json::json!({"command":"list_balances"}))
                .await?
                .into_iter()
                .map(Into::into)
                .collect(),
        )
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BalanceWire {
    pub(super) id: String,
    pub(super) raw: String,
    pub(super) created_at_ms: i64,
    pub(super) sms_id: String,
}
impl From<BalanceWire> for Record {
    fn from(x: BalanceWire) -> Self {
        Record {
            id: x.id,
            peer: "191".into(),
            body: x.raw.clone(),
            detail: x.raw,
            state: "received".into(),
            created_at_ms: x.created_at_ms as u64,
            sms_id: x.sms_id,
            ..Default::default()
        }
    }
}
