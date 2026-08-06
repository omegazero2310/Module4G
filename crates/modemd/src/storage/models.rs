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
