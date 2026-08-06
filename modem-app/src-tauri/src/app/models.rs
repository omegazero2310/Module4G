use super::*;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Status {
    pub(super) service_version: String,
    pub(super) state: String,
    pub(super) port: String,
    pub(super) sim_state: String,
    pub(super) registration: String,
    pub(super) signal_rssi: i32,
    pub(super) last_error: String,
    pub(super) delivery_tracking_available: bool,
    pub(super) delivery_tracking_error: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Port {
    pub(super) name: String,
    pub(super) vid: u16,
    pub(super) pid: u16,
    pub(super) label: String,
    pub(super) available: bool,
    pub(super) dedicated_at: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Settings {
    pub(super) usb_vid: u16,
    pub(super) usb_pid: u16,
    pub(super) port_override: String,
    pub(super) baud: u32,
    pub(super) call_timeout_seconds: u32,
    pub(super) upload_pacing_ms: u32,
    pub(super) max_audio_bytes: usize,
    pub(super) ussd_code: String,
    pub(super) ussd_timeout_seconds: u32,
    pub(super) currency: String,
    pub(super) low_balance_threshold: f64,
    pub(super) balance_regex: String,
}

impl From<CoreSettings> for Settings {
    fn from(value: CoreSettings) -> Self {
        Self {
            usb_vid: value.usb_vid,
            usb_pid: value.usb_pid,
            port_override: value.port_override.unwrap_or_default(),
            baud: value.baud,
            call_timeout_seconds: value.call_timeout_seconds,
            upload_pacing_ms: value.upload_pacing_ms,
            max_audio_bytes: value.max_audio_bytes,
            ussd_code: value.ussd_code,
            ussd_timeout_seconds: value.ussd_timeout_seconds,
            currency: value.currency,
            low_balance_threshold: value.low_balance_threshold,
            balance_regex: value.balance_regex.unwrap_or_default(),
        }
    }
}
impl From<Settings> for CoreSettings {
    fn from(value: Settings) -> Self {
        Self {
            usb_vid: value.usb_vid,
            usb_pid: value.usb_pid,
            port_override: (!value.port_override.trim().is_empty())
                .then(|| value.port_override.trim().to_owned()),
            baud: value.baud,
            call_timeout_seconds: value.call_timeout_seconds,
            upload_pacing_ms: value.upload_pacing_ms,
            max_audio_bytes: value.max_audio_bytes,
            ussd_code: value.ussd_code,
            ussd_timeout_seconds: value.ussd_timeout_seconds,
            currency: value.currency,
            low_balance_threshold: value.low_balance_threshold,
            balance_regex: (!value.balance_regex.is_empty()).then_some(value.balance_regex),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub(super) struct Record {
    pub(super) id: String,
    pub(super) peer: String,
    pub(super) body: String,
    pub(super) state: String,
    pub(super) detail: String,
    pub(super) created_at_ms: u64,
    pub(super) answer_classification: String,
    pub(super) end_reason: String,
    pub(super) alerting_at_ms: u64,
    pub(super) release_cause: String,
    pub(super) direction: String,
    pub(super) kind: String,
    pub(super) source: String,
    pub(super) storage: String,
    pub(super) storage_index: i32,
    pub(super) storage_indices: Vec<i32>,
    pub(super) part_count: i32,
    pub(super) parts_received: i32,
    pub(super) multipart_complete: bool,
    pub(super) modem_status: String,
    pub(super) modem_timestamp: String,
    pub(super) encoding: String,
    pub(super) dcs: i32,
    pub(super) length: i32,
    pub(super) service_center: String,
    pub(super) message_reference: String,
    pub(super) delivery_status: String,
    pub(super) delivery_report_requested: bool,
    pub(super) delivery_report_scts: String,
    pub(super) delivery_report_discharge_time: String,
    pub(super) delivery_tracking_error: String,
    pub(super) synchronized_at_ms: i64,
    pub(super) present_on_modem: bool,
    pub(super) sms_id: String,
    pub(super) audio_id: String,
    pub(super) error: String,
    pub(super) duration_seconds: u32,
    pub(super) connected_at_ms: i64,
    pub(super) ended_at_ms: i64,
    #[serde(skip)]
    pub(super) voice_begin_seen: bool,
}

pub(super) struct AppState {
    pub(super) settings: Mutex<CoreSettings>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UploadedAudio {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) format: String,
    pub(super) size: u64,
    pub(super) module_path: String,
    pub(super) duration_ms: u64,
    pub(super) created_at_ms: i64,
    pub(super) state: String,
    pub(super) is_current: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CallData {
    pub(super) calls: Vec<Record>,
    pub(super) audio: Vec<UploadedAudio>,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            id: String::new(),
            peer: String::new(),
            body: String::new(),
            state: String::new(),
            detail: String::new(),
            created_at_ms: 0,
            answer_classification: String::new(),
            end_reason: String::new(),
            alerting_at_ms: 0,
            release_cause: String::new(),
            direction: String::new(),
            kind: String::new(),
            source: String::new(),
            storage: String::new(),
            storage_index: -1,
            storage_indices: Vec::new(),
            part_count: 1,
            parts_received: 1,
            multipart_complete: true,
            modem_status: String::new(),
            modem_timestamp: String::new(),
            encoding: String::new(),
            dcs: -1,
            length: 0,
            service_center: String::new(),
            message_reference: String::new(),
            delivery_status: String::new(),
            delivery_report_requested: false,
            delivery_report_scts: String::new(),
            delivery_report_discharge_time: String::new(),
            delivery_tracking_error: String::new(),
            synchronized_at_ms: 0,
            present_on_modem: false,
            sms_id: String::new(),
            audio_id: String::new(),
            error: String::new(),
            duration_seconds: 0,
            connected_at_ms: 0,
            ended_at_ms: 0,
            voice_begin_seen: false,
        }
    }
}
