use super::*;

#[tauri::command]
pub(super) async fn get_integration_settings() -> Result<IntegrationSettings, String> {
    log_event("APP", "Integration settings opened (secrets not requested)");
    #[cfg(windows)]
    {
        request_json(serde_json::json!({"command":"get_integration_settings"})).await
    }
    #[cfg(not(windows))]
    {
        Ok(IntegrationSettings::default())
    }
}

#[tauri::command]
pub(super) async fn update_integration_settings(
    settings: IntegrationSettings,
) -> Result<IntegrationSettings, String> {
    log_event(
        "APP",
        "Integration settings save requested (secrets not logged)",
    );
    #[cfg(windows)]
    {
        let mut current: IntegrationSettings = request_json(serde_json::json!({
            "command":"update_integration_settings",
            "settings":{
                "restEnabled":settings.rest_enabled,
                "restBindAddress":settings.rest_bind_address,
                "webhookUrl":settings.webhook_url,
                "restToken":settings.rest_token,
                "webhookToken":settings.webhook_token,
                "clearRestToken":settings.clear_rest_token,
                "clearWebhookToken":settings.clear_webhook_token
            }
        }))
        .await?;
        current.rest_token = String::new();
        current.webhook_token = String::new();
        Ok(current)
    }
    #[cfg(not(windows))]
    {
        Ok(settings)
    }
}

#[tauri::command]
pub(super) async fn list_integration_diagnostics() -> Result<Vec<IntegrationDiagnosticEvent>, String>
{
    #[cfg(windows)]
    {
        request_json(serde_json::json!({"command":"list_integration_diagnostics"})).await
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}
