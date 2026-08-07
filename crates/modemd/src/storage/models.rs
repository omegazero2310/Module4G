#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SmsRecord {
    pub id: String,
    pub direction: String,
    pub peer: String,
    pub body: String,
    pub state: String,
    pub message_reference: String,
    pub cause: String,
    pub created_at_ms: i64,
    pub kind: String,
    pub source: String,
    pub storage: String,
    pub storage_index: i32,
    pub storage_indices: Vec<i32>,
    pub part_count: i32,
    pub parts_received: i32,
    pub multipart_complete: bool,
    #[serde(skip)]
    pub part_payloads: Vec<String>,
    #[serde(skip)]
    pub part_timestamps: Vec<String>,
    pub modem_status: String,
    pub modem_timestamp: String,
    pub encoding: String,
    pub dcs: i32,
    pub length: i32,
    pub service_center: String,
    pub delivery_status: String,
    pub delivery_report_requested: bool,
    pub delivery_report_scts: String,
    pub delivery_report_discharge_time: String,
    pub delivery_tracking_error: String,
    pub synchronized_at_ms: i64,
    pub present_on_modem: bool,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BalanceRecord {
    pub id: String,
    pub raw: String,
    pub value: Option<f64>,
    pub currency: String,
    pub error: String,
    pub created_at_ms: i64,
    pub sms_id: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UploadedAudioRecord {
    pub id: String,
    pub name: String,
    pub format: String,
    pub size: u64,
    pub module_path: String,
    pub duration_ms: u64,
    pub created_at_ms: i64,
    pub state: String,
    pub is_current: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CallRecord {
    pub id: String,
    pub peer: String,
    pub state: String,
    pub audio_id: String,
    pub error: String,
    pub duration_seconds: u32,
    pub created_at_ms: i64,
    pub answer_classification: String,
    pub end_reason: String,
    pub connected_at_ms: i64,
    pub ended_at_ms: i64,
    pub release_cause: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct IntegrationSettings {
    pub rest_enabled: bool,
    pub rest_bind_address: String,
    pub webhook_url: String,
    pub rest_token: String,
    pub webhook_token: String,
}

impl Default for IntegrationSettings {
    fn default() -> Self {
        Self {
            rest_enabled: false,
            rest_bind_address: "0.0.0.0:5069".into(),
            webhook_url: "http://10.1.11.117:5068/api/v1/webhooks/receive".into(),
            rest_token: String::new(),
            webhook_token: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestCommunication {
    pub id: String,
    pub request_id: String,
    pub record_id: String,
    pub channel: String,
    pub owner: String,
    pub destination: String,
    pub content: String,
    pub encrypted: bool,
    pub payload_fingerprint: String,
    pub status: String,
    pub created_at_ms: i64,
    pub sent_at_ms: i64,
    pub delivered_at_ms: i64,
    pub failed_at_ms: i64,
    pub failure_reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebhookAttempt {
    pub id: i64,
    pub communication_id: String,
    pub event_type: String,
    pub payload: String,
    pub attempt_count: u32,
    pub next_attempt_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommunicationReservation {
    New(RestCommunication),
    Conflict,
}
