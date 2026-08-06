#[cfg(windows)]
pub mod host {
    mod json;
    mod legacy;
    mod pipe;
    mod state;
    use json::*;
    use legacy::*;
    pub use pipe::run;
    use state::*;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::ServerOptions;

    const PIPE: &str = r"\\.\pipe\a7670-modemd-v1";

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn legacy_routes_preserve_exact_status_and_sms_shapes() {
            let mut scenario = CallScenario::default();
            assert_eq!(
                response("STATUS\n", &mut scenario),
                "STATUS\t0.1.0-sim\tReady\tSIMULATED\tREADY\tRegistered\t20\n"
            );
            assert_eq!(response("SMS|191|TK\n", &mut scenario), "+CMGS: 42\r\nOK\n");
            assert_eq!(response("UNKNOWN\n", &mut scenario), "ERROR\n");
        }

        #[test]
        fn json_routes_preserve_envelope_and_deterministic_ids() {
            let mut state = SimState::default();
            let sent = json_response(
                r#"{"command":"send_sms","destination":"190","body":"TK"}"#,
                &mut state,
            )
            .expect("send_sms response");
            let sent: serde_json::Value = serde_json::from_str(sent.trim()).unwrap();
            assert_eq!(sent["ok"], true);
            assert_eq!(sent["data"]["id"], "sim-sms-1");
            assert_eq!(sent["data"]["peer"], "190");
            assert_eq!(sent["data"]["body"], "TK");

            assert!(json_response(r#"{"command":"unknown"}"#, &mut state).is_none());
        }

        #[test]
        fn integration_settings_update_is_atomic_and_persistent() {
            let mut state = SimState::default();
            let updated = json_response(
                r#"{"command":"update_integration_settings","settings":{"restEnabled":true,"restBindAddress":"127.0.0.1:5069","webhookUrl":"https://example.test/hook","restToken":"secret","webhookToken":"","clearRestToken":false,"clearWebhookToken":false}}"#,
                &mut state,
            )
            .unwrap();
            let updated: serde_json::Value = serde_json::from_str(updated.trim()).unwrap();
            assert_eq!(updated["ok"], true);
            assert_eq!(updated["data"]["hasRestToken"], true);
            assert!(updated["data"].get("restToken").is_none());

            let read =
                json_response(r#"{"command":"get_integration_settings"}"#, &mut state).unwrap();
            assert!(read.contains(r#""hasRestToken":true"#));
            assert!(!read.contains("secret"));

            let before = state.integration_settings.clone();
            let rejected = json_response(
                r#"{"command":"update_integration_settings","settings":{"restEnabled":true,"restBindAddress":"invalid","webhookUrl":"https://example.test/hook","restToken":"replacement","clearRestToken":true}}"#,
                &mut state,
            )
            .unwrap();
            assert!(rejected.contains(r#""ok":false"#));
            assert_eq!(state.integration_settings, before);

            let cleared = json_response(
                r#"{"command":"update_integration_settings","settings":{"restEnabled":false,"restBindAddress":"127.0.0.1:5069","webhookUrl":"https://example.test/hook","clearRestToken":true}}"#,
                &mut state,
            )
            .unwrap();
            assert!(cleared.contains(r#""hasRestToken":false"#));
        }

        #[test]
        fn desktop_smoke_covers_workflows_and_failure_scenarios() {
            let mut state = SimState::default();
            let mut legacy = CallScenario::default();
            assert!(response("STATUS\n", &mut legacy).contains("\tReady\tSIMULATED\t"));
            assert!(response("AT+CSQ\n", &mut legacy).contains("+CSQ: 20,99"));

            let settings = json_response(
                r#"{"command":"update_settings","settings":{"baud":115200,"portOverride":"SIMULATED"}}"#,
                &mut state,
            )
            .unwrap();
            assert!(settings.contains(r#""portOverride":"SIMULATED""#));
            assert!(
                json_response(r#"{"command":"get_settings"}"#, &mut state)
                    .unwrap()
                    .contains(r#""baud":115200"#)
            );

            let sent = json_response(
                r#"{"command":"send_sms","destination":"190","body":"smoke"}"#,
                &mut state,
            )
            .unwrap();
            assert!(sent.contains(r#""state":"delivery-pending""#));
            json_response(r#"{"command":"sync_sms"}"#, &mut state).unwrap();
            json_response(r#"{"command":"sync_sms"}"#, &mut state).unwrap();
            assert!(
                json_response(r#"{"command":"list_sms"}"#, &mut state)
                    .unwrap()
                    .contains(r#""state":"delivered""#)
            );
            assert!(
                json_response(
                    r#"{"command":"send_sms","destination":"091","body":"failure"}"#,
                    &mut state,
                )
                .unwrap()
                .contains("+CMS ERROR: 500")
            );

            assert!(
                json_response(
                    r#"{"command":"make_call","destination":"190","audioId":"missing"}"#,
                    &mut state,
                )
                .unwrap()
                .contains("select the current uploaded audio")
            );
            let uploaded = json_response(
                r#"{"command":"upload_audio","name":"smoke.amr","data":[35,33,65,77,82]}"#,
                &mut state,
            )
            .unwrap();
            assert!(uploaded.contains("sim-audio-1"));
            assert!(
                json_response(r#"{"command":"list_audio"}"#, &mut state)
                    .unwrap()
                    .contains(r#""isCurrent":true"#)
            );
            assert!(
                json_response(
                    r#"{"command":"select_audio","audioId":"sim-audio-1"}"#,
                    &mut state,
                )
                .unwrap()
                .contains(r#""ok":true"#)
            );
            assert!(
                json_response(
                    r#"{"command":"make_call","destination":"112","audioId":"sim-audio-1"}"#,
                    &mut state,
                )
                .unwrap()
                .contains("waiting-for-answer")
            );
            json_response(r#"{"command":"hang_up"}"#, &mut state).unwrap();
            assert!(
                json_response(r#"{"command":"list_calls"}"#, &mut state)
                    .unwrap()
                    .contains("hang-up-failed")
            );
            json_response(r#"{"command":"hang_up"}"#, &mut state).unwrap();
            assert_eq!(state.calls[0]["endReason"], "local-hang-up");
            assert!(
                json_response(r#"{"command":"get_call_data"}"#, &mut state)
                    .unwrap()
                    .contains(r#""audio""#)
            );

            let balance = json_response(r#"{"command":"check_balance"}"#, &mut state).unwrap();
            assert!(balance.contains("sim-balance-sms"));
            assert!(balance.contains("85.500d"));
        }
    }
}
