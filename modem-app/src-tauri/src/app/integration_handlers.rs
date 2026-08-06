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
        if !settings.rest_token.trim().is_empty() && !settings.clear_rest_token {
            let _: IntegrationSettings = request_json(
                serde_json::json!({"command":"replace_rest_token","token":settings.rest_token}),
            )
            .await?;
        }
        if !settings.webhook_token.trim().is_empty() && !settings.clear_webhook_token {
            let _: IntegrationSettings = request_json(serde_json::json!({"command":"replace_webhook_token","token":settings.webhook_token})).await?;
        }
        let mut current:IntegrationSettings=request_json(serde_json::json!({"command":"update_integration_settings","settings":{
            "restEnabled":settings.rest_enabled,"restBindAddress":settings.rest_bind_address,"webhookUrl":settings.webhook_url
        }})).await?;
        if settings.clear_rest_token {
            current = request_json(serde_json::json!({"command":"clear_rest_token"})).await?;
        }
        if settings.clear_webhook_token {
            current = request_json(serde_json::json!({"command":"clear_webhook_token"})).await?;
        }
        current.rest_token = String::new();
        current.webhook_token = String::new();
        Ok(current)
    }
    #[cfg(not(windows))]
    {
        Ok(settings)
    }
}
