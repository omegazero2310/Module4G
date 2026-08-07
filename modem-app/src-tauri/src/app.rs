use modemd::{at::validate_console, settings::Settings as CoreSettings};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

mod balance_handlers;
mod call_handlers;
mod client;
mod console;
mod integration_handlers;
mod logging;
mod models;
mod settings_handlers;
mod sms_handlers;
mod status;
#[cfg(windows)]
use client::{request_json, request_line};
use logging::log_event;
#[cfg(test)]
use logging::{logged_request, logged_response, suppress_successful_poll_log};
use models::*;
#[cfg(test)]
use status::parse_status_response;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parser_accepts_legacy_and_extended_responses() {
        let legacy =
            parse_status_response("STATUS\t0.1.0\tReady\tCOM6\tREADY\tRegistered\t18\n").unwrap();
        assert!(!legacy.delivery_tracking_available);
        assert!(legacy.delivery_tracking_error.is_empty());

        let extended =
            parse_status_response("STATUS\t0.1.0\tReady\tCOM6\tREADY\tRegistered\t18\ttrue\t\n")
                .unwrap();
        assert!(extended.delivery_tracking_available);
        assert!(extended.delivery_tracking_error.is_empty());
    }

    #[test]
    fn console_logging_redacts_private_request_data() {
        assert_eq!(
            logged_request("SMS|191|48656C6C6F"),
            "SMS send (destination and message redacted)"
        );
        assert_eq!(
            logged_request("DIAL|+84912345678"),
            "DIAL (destination redacted)"
        );
        assert_eq!(
            logged_response("AT+CLCC", "+CLCC: 1,0,0,0,0,\"+84912345678\",145\r\nOK\n"),
            "+CLCC: 1,0,0,0,0,\"+<redacted-number>\",145 | OK"
        );
        assert_eq!(
            logged_response("BALANCE", "Your balance is 100000 VND"),
            "balance response received (content redacted)"
        );
        assert_eq!(
            logged_response(
                "{\"command\":\"list_sms\"}",
                "{\"ok\":true,\"data\":[{\"body\":\"secret\"}]}"
            ),
            "JSON response received (SMS and balance content redacted)"
        );
        let integration_request = r#"{"command":"update_integration_settings","settings":{"restToken":"never-log-this","webhookToken":"also-secret"}}"#;
        let logged = logged_request(integration_request);
        assert_eq!(
            logged,
            "JSON update_integration_settings (SMS and balance content redacted)"
        );
        assert!(!logged.contains("never-log-this"));
        assert!(!logged.contains("also-secret"));
    }

    #[test]
    fn successful_high_frequency_polls_are_suppressed() {
        assert!(suppress_successful_poll_log("STATUS"));
        assert!(suppress_successful_poll_log(r#"{"command":"list_calls"}"#));
        assert!(suppress_successful_poll_log(
            r#"{"command":"get_current_audio"}"#
        ));
        assert!(!suppress_successful_poll_log(
            r#"{"command":"upload_audio"}"#
        ));
    }
}

pub fn run() {
    log_event("APP", "A7670 Modem application starting");
    tauri::Builder::default()
        .manage(AppState {
            settings: Mutex::new(CoreSettings::default()),
        })
        .invoke_handler(tauri::generate_handler![
            status::get_status,
            settings_handlers::get_settings,
            settings_handlers::update_settings,
            settings_handlers::list_ports,
            integration_handlers::get_integration_settings,
            integration_handlers::update_integration_settings,
            integration_handlers::list_integration_diagnostics,
            console::execute_at,
            sms_handlers::send_sms,
            sms_handlers::sync_sms,
            sms_handlers::list_sms,
            call_handlers::get_current_audio,
            call_handlers::list_audio,
            call_handlers::get_call_data,
            call_handlers::select_audio,
            call_handlers::upload_audio,
            call_handlers::make_call,
            call_handlers::hang_up,
            call_handlers::list_calls,
            balance_handlers::check_balance,
            balance_handlers::list_balance_checks
        ])
        .run(tauri::generate_context!())
        .expect("failed to run modem app");
}
