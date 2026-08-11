use crate::{
    ModemError,
    hardware::HardwareState,
    storage::{CommunicationReservation, IntegrationSettings, RestCommunication, Store},
};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_WEBHOOK_URL: &str = "http://10.1.11.117:5068/api/v1/webhooks/receive";
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const INTEGRATION_DIAGNOSTICS_CAPACITY: usize = 200;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationDiagnosticEvent {
    pub timestamp: String,
    pub source: String,
    pub phase: String,
    pub outcome: String,
    pub http_status: Option<u16>,
    pub request_id: Option<String>,
    pub communication_id: Option<String>,
    pub channel: Option<String>,
    pub byte_count: Option<usize>,
    pub payload_sha256: Option<String>,
    pub elapsed_ms: Option<u128>,
    pub summary: String,
}

#[derive(Debug)]
pub struct IntegrationDiagnostics {
    enabled: bool,
    events: Mutex<VecDeque<IntegrationDiagnosticEvent>>,
}

impl IntegrationDiagnostics {
    pub fn from_environment() -> Self {
        Self::new(matches!(std::env::var("MODEMD_INTEGRATION_DEBUG"), Ok(value) if value == "1"))
    }

    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            events: Mutex::new(VecDeque::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn record(&self, event: IntegrationDiagnosticEvent) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "integration diagnostic: {} {} {} {}",
            event.source, event.phase, event.outcome, event.summary
        );
        let mut events = self.events.lock().unwrap_or_else(|lock| lock.into_inner());
        if events.len() == INTEGRATION_DIAGNOSTICS_CAPACITY {
            events.pop_front();
        }
        events.push_back(event);
    }

    pub fn snapshot(&self) -> Vec<IntegrationDiagnosticEvent> {
        if !self.enabled {
            return Vec::new();
        }
        self.events
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .iter()
            .rev()
            .cloned()
            .collect()
    }
}

fn diagnostic_event(
    source: &str,
    phase: &str,
    outcome: &str,
    http_status: Option<u16>,
    request_id: Option<String>,
    communication_id: Option<String>,
    channel: Option<String>,
    byte_count: Option<usize>,
    payload_sha256: Option<String>,
    elapsed_ms: Option<u128>,
    summary: &str,
) -> IntegrationDiagnosticEvent {
    IntegrationDiagnosticEvent {
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        source: source.into(),
        phase: phase.into(),
        outcome: outcome.into(),
        http_status,
        request_id,
        communication_id,
        channel,
        byte_count,
        payload_sha256,
        elapsed_ms,
        summary: summary.into(),
    }
}

fn payload_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PublicIntegrationSettings {
    pub rest_enabled: bool,
    pub rest_bind_address: String,
    pub webhook_url: String,
    pub has_rest_token: bool,
    pub has_webhook_token: bool,
}

impl From<&IntegrationSettings> for PublicIntegrationSettings {
    fn from(value: &IntegrationSettings) -> Self {
        Self {
            rest_enabled: value.rest_enabled,
            rest_bind_address: value.rest_bind_address.clone(),
            webhook_url: value.webhook_url.clone(),
            has_rest_token: !value.rest_token.is_empty(),
            has_webhook_token: !value.webhook_token.is_empty(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunicationRequest {
    pub request_id: String,
    #[serde(rename = "from")]
    pub owner: String,
    pub to: String,
    pub channel: Channel,
    pub content: String,
    #[serde(default)]
    pub encrypted: bool,
    #[serde(default)]
    pub send_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Sms,
    Call,
}

impl Channel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sms => "sms",
            Self::Call => "call",
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CommunicationData {
    pub id: String,
    pub request_id: String,
    pub owner: String,
    pub contact: String,
    pub channel: String,
    pub content: String,
    pub encrypted: bool,
    pub status: String,
    pub created_at: String,
    pub sent_at: Option<String>,
    pub delivered_at: Option<String>,
    pub failed_at: Option<String>,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HttpSmsEnvelope<T> {
    pub status: String,
    pub message: String,
    pub data: Option<T>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommunicationWebhookRequest {
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: CommunicationWebhookData,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommunicationWebhookData {
    pub id: String,
    pub request_id: String,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub enum DispatchError {
    Validation(String),
    Unavailable(String),
    Failed(String),
}

#[async_trait]
pub trait CommunicationDispatcher: Send + Sync {
    async fn send_sms(
        &self,
        id: String,
        destination: String,
        body: String,
    ) -> Result<(), DispatchError>;
    async fn make_call(
        &self,
        id: String,
        destination: String,
        audio_id: String,
    ) -> Result<(), DispatchError>;
}

#[derive(Clone)]
pub struct RestState {
    pub store: Arc<Store>,
    pub settings: Arc<RwLock<IntegrationSettings>>,
    pub hardware_state: Arc<RwLock<HardwareState>>,
    pub dispatcher: Arc<dyn CommunicationDispatcher>,
    pub diagnostics: Arc<IntegrationDiagnostics>,
}

pub fn router(state: RestState) -> Router {
    Router::new()
        .route("/api/v1/health", get(get_health))
        .route("/api/v1/communications", post(post_communication))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn get_health(State(state): State<RestState>, headers: HeaderMap) -> Response {
    let settings = state
        .settings
        .read()
        .unwrap_or_else(|lock| lock.into_inner())
        .clone();
    if !settings.rest_enabled
        || settings.rest_token.is_empty()
        || !authorized(&headers, &settings.rest_token)
    {
        return error(StatusCode::UNAUTHORIZED, "authentication required");
    }

    let hardware_ready = matches!(
        &*state
            .hardware_state
            .read()
            .unwrap_or_else(|lock| lock.into_inner()),
        HardwareState::Ready { .. }
    );
    let cutoff_ms = now_ms().saturating_sub(86_400_000);
    let communication_healthy = state
        .store
        .latest_outbound_health_evidence(cutoff_ms)
        .unwrap_or(Some(false))
        .unwrap_or(true);
    let healthy = hardware_ready && communication_healthy;
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(healthy)).into_response()
}

pub async fn serve(listener: tokio::net::TcpListener, state: RestState) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

async fn post_communication(
    State(state): State<RestState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let started = Instant::now();
    let settings = state
        .settings
        .read()
        .unwrap_or_else(|lock| lock.into_inner())
        .clone();
    if !settings.rest_enabled
        || settings.rest_token.is_empty()
        || !authorized(&headers, &settings.rest_token)
    {
        state.diagnostics.record(diagnostic_event(
            "api",
            "request",
            "received",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "REST communication request received",
        ));
        state.diagnostics.record(diagnostic_event(
            "api",
            "response",
            "authentication-failed",
            Some(StatusCode::UNAUTHORIZED.as_u16()),
            None,
            None,
            None,
            None,
            None,
            Some(started.elapsed().as_millis()),
            "REST authentication rejected",
        ));
        return error(StatusCode::UNAUTHORIZED, "authentication required");
    }
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            state.diagnostics.record(diagnostic_event(
                "api",
                "request",
                "received",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                "REST communication request received",
            ));
            state.diagnostics.record(diagnostic_event(
                "api",
                "response",
                "payload-too-large",
                Some(StatusCode::PAYLOAD_TOO_LARGE.as_u16()),
                None,
                None,
                None,
                None,
                None,
                Some(started.elapsed().as_millis()),
                "REST request exceeded the body limit",
            ));
            return error(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds 64 KiB");
        }
    };
    let byte_count = bytes.len();
    let hash = payload_sha256(&bytes);
    let request: CommunicationRequest = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(parse_error) => {
            let status = if matches!(parse_error.classify(), serde_json::error::Category::Data) {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            };
            state.diagnostics.record(diagnostic_event(
                "api",
                "request",
                "received",
                None,
                None,
                None,
                None,
                Some(byte_count),
                Some(hash.clone()),
                None,
                "REST communication request received",
            ));
            state.diagnostics.record(diagnostic_event(
                "api",
                "response",
                "invalid-json",
                Some(status.as_u16()),
                None,
                None,
                None,
                Some(byte_count),
                Some(hash),
                Some(started.elapsed().as_millis()),
                "REST request JSON was rejected",
            ));
            return error(status, "invalid JSON request");
        }
    };
    let request_id = request.request_id.clone();
    let channel = request.channel.as_str().to_owned();
    state.diagnostics.record(diagnostic_event(
        "api",
        "request",
        "received",
        None,
        Some(request_id.clone()),
        None,
        Some(channel.clone()),
        Some(byte_count),
        Some(hash),
        None,
        "REST communication request received",
    ));
    match process_request(&state, request).await {
        Ok((status, data, replay)) => {
            let outcome = if data.status == "failed" {
                "dispatch-failed"
            } else {
                "success"
            };
            let summary = if data.status == "failed" {
                "REST communication dispatch failed"
            } else {
                "REST communication processed"
            };
            state.diagnostics.record(diagnostic_event(
                "api",
                "response",
                outcome,
                Some(status.as_u16()),
                Some(data.request_id.clone()),
                Some(data.id.clone()),
                Some(data.channel.clone()),
                Some(byte_count),
                None,
                Some(started.elapsed().as_millis()),
                summary,
            ));
            (
                status,
                Json(HttpSmsEnvelope {
                    status: "success".into(),
                    message: if replay {
                        "communication already exists".into()
                    } else {
                        "communication processed".into()
                    },
                    data: Some(data),
                }),
            )
                .into_response()
        }
        Err((status, message)) => {
            state.diagnostics.record(diagnostic_event(
                "api",
                "response",
                "validation-or-dispatch-failed",
                Some(status.as_u16()),
                Some(request_id),
                None,
                Some(channel),
                Some(byte_count),
                None,
                Some(started.elapsed().as_millis()),
                "REST communication request was rejected",
            ));
            error(status, &message)
        }
    }
}

async fn process_request(
    state: &RestState,
    request: CommunicationRequest,
) -> Result<(StatusCode, CommunicationData, bool), (StatusCode, String)> {
    if request.send_at.is_some_and(|stamp| stamp > Utc::now()) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "future send_at values are not supported".into(),
        ));
    }
    if request.request_id.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "request_id must be a non-empty string".into(),
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let destination = match request.channel {
        Channel::Sms => crate::sms::normalize_sms_destination(&request.to),
        Channel::Call => crate::sms::normalize_call_destination(&request.to),
    }
    .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
    if matches!(request.channel, Channel::Sms) {
        crate::sms::validate_body(&request.content)
            .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))?;
    }
    let audio_id = if matches!(request.channel, Channel::Call) {
        let audio = if request.content.trim().is_empty() {
            state.store.current_audio().map_err(internal)?
        } else {
            state
                .store
                .audio_named(&request.content)
                .map_err(internal)?
        };
        Some(
            audio
                .ok_or_else(|| {
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "audio file was not found".into(),
                    )
                })?
                .id,
        )
    } else {
        None
    };
    let fingerprint = fingerprint(
        request.channel,
        &request.owner,
        &destination,
        &request.content,
        request.encrypted,
        request.send_at,
    );
    let stamp = now_ms();
    let communication = RestCommunication {
        id: id.clone(),
        request_id: request.request_id,
        record_id: id.clone(),
        channel: request.channel.as_str().into(),
        owner: request.owner,
        destination: destination.clone(),
        content: request.content.clone(),
        encrypted: request.encrypted,
        payload_fingerprint: fingerprint,
        status: "sending".into(),
        created_at_ms: stamp,
        ..Default::default()
    };
    match state
        .store
        .reserve_rest_communication(&communication)
        .map_err(internal)?
    {
        CommunicationReservation::Conflict => {
            return Err((StatusCode::CONFLICT, "request_id was already used".into()));
        }
        CommunicationReservation::New(_) => {}
    }
    let dispatched = match request.channel {
        Channel::Sms => {
            state
                .dispatcher
                .send_sms(id.clone(), destination, request.content)
                .await
        }
        Channel::Call => {
            state
                .dispatcher
                .make_call(id.clone(), destination, audio_id.unwrap_or_default())
                .await
        }
    };
    if let Err(dispatch_error) = dispatched {
        let reason = match dispatch_error {
            DispatchError::Validation(x)
            | DispatchError::Unavailable(x)
            | DispatchError::Failed(x) => x,
        };
        state
            .store
            .mark_rest_dispatch_failed(&id, &reason, now_ms())
            .map_err(internal)?;
    }
    state
        .store
        .reconcile_rest_communications(now_ms())
        .map_err(internal)?;
    let result = state
        .store
        .rest_communication(&id)
        .map_err(internal)?
        .ok_or_else(|| {
            internal(ModemError::Persistence(
                "communication was not found".into(),
            ))
        })?;
    Ok((StatusCode::CREATED, communication_data(&result), false))
}

fn internal(error: ModemError) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(HttpSmsEnvelope::<CommunicationData> {
            status: "error".into(),
            message: message.into(),
            data: None,
        }),
    )
        .into_response()
}
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|v| v.as_bytes() == token.as_bytes())
}

pub fn validate_settings(settings: &IntegrationSettings) -> Result<(), ModemError> {
    settings
        .rest_bind_address
        .parse::<SocketAddr>()
        .map_err(|_| {
            ModemError::Validation("REST bind address must be an IP address and port".into())
        })?;
    let url = reqwest::Url::parse(&settings.webhook_url)
        .map_err(|_| ModemError::Validation("webhook URL is invalid".into()))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ModemError::Validation(
            "webhook URL must use http or https".into(),
        ));
    }
    if settings.rest_enabled && settings.rest_token.trim().is_empty() {
        return Err(ModemError::Validation(
            "REST requires a bearer token before it can be enabled".into(),
        ));
    }
    Ok(())
}

pub fn normalize_source_state(
    channel: &str,
    state: &str,
    error: &str,
    answer: &str,
    end_reason: &str,
) -> (&'static str, String) {
    if channel == "sms" {
        return match state {
            "submitted" | "delivery-pending" => ("sent", String::new()),
            "delivered" => ("delivered", String::new()),
            "send-failed" | "delivery-failed" => (
                "failed",
                safe_failure_reason(if error.is_empty() {
                    "SMS operation failed"
                } else {
                    error
                }),
            ),
            "send-unknown" | "delivery-unknown" => (
                "expired",
                safe_failure_reason(if error.is_empty() {
                    "SMS outcome is unknown"
                } else {
                    error
                }),
            ),
            _ => ("sending", String::new()),
        };
    }
    // A daemon-initiated hang-up is not a failed delivery. Before pickup the
    // accepted dial remains sent; after pickup the answered classification
    // below keeps it delivered.
    if end_reason == "local-hang-up" && answer != "answered" {
        return ("sent", String::new());
    }
    if answer == "not-answered" || matches!(end_reason, "no-answer" | "busy" | "signaling-timeout")
    {
        return (
            "failed",
            safe_failure_reason(if error.is_empty() { end_reason } else { error }),
        );
    }
    if state == "failed" || state == "hang-up-failed" {
        return (
            "failed",
            safe_failure_reason(if error.is_empty() {
                "call failed"
            } else {
                error
            }),
        );
    }
    if answer == "answered" {
        return ("delivered", String::new());
    }
    if state == "ended" {
        return (
            "failed",
            safe_failure_reason(if error.is_empty() { end_reason } else { error }),
        );
    }
    if matches!(state, "waiting-for-answer" | "playback-delay" | "playing") {
        return ("sent", String::new());
    }
    ("sending", String::new())
}
pub fn event_for_status(status: &str) -> Option<&'static str> {
    match status {
        "sent" => Some("communication.sent"),
        "delivered" => Some("communication.delivered"),
        "failed" => Some("communication.failed"),
        "expired" => Some("communication.expired"),
        "missed" => Some("communication.missed"),
        _ => None,
    }
}
pub fn webhook_payload(
    communication: &RestCommunication,
    event_type: &str,
) -> Result<String, ModemError> {
    serde_json::to_string(&CommunicationWebhookRequest {
        event_type: event_type.into(),
        data: CommunicationWebhookData {
            id: communication.id.clone(),
            request_id: communication.request_id.clone(),
            failure_reason: matches!(
                event_type,
                "communication.failed" | "communication.expired" | "communication.missed"
            )
            .then(|| communication.failure_reason.clone()),
        },
    })
    .map_err(|e| ModemError::Persistence(e.to_string()))
}
pub fn safe_failure_reason(value: &str) -> String {
    let clean = value.replace(['\r', '\n'], " ");
    clean.chars().take(512).collect()
}
pub fn safe_delivery_error(value: &str) -> String {
    safe_failure_reason(value).chars().take(200).collect()
}
pub fn retry_delay_ms(attempt: u32) -> i64 {
    match attempt {
        0 => 1_000,
        1 => 5_000,
        2 => 30_000,
        3 => 120_000,
        4 => 600_000,
        _ => 3_600_000,
    }
}

fn fingerprint(
    channel: Channel,
    owner: &str,
    to: &str,
    content: &str,
    encrypted: bool,
    send_at: Option<DateTime<Utc>>,
) -> String {
    let canonical = serde_json::json!([
        channel.as_str(),
        owner,
        to,
        content,
        encrypted,
        send_at.map(|x| x.to_rfc3339_opts(SecondsFormat::Millis, true))
    ]);
    format!("{:x}", Sha256::digest(canonical.to_string().as_bytes()))
}
fn timestamp(ms: i64) -> Option<String> {
    (ms > 0).then(|| {
        DateTime::<Utc>::from_timestamp_millis(ms)
            .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    })
}
pub fn communication_data(value: &RestCommunication) -> CommunicationData {
    CommunicationData {
        id: value.id.clone(),
        request_id: value.request_id.clone(),
        owner: value.owner.clone(),
        contact: value.destination.clone(),
        channel: value.channel.clone(),
        content: value.content.clone(),
        encrypted: value.encrypted,
        status: value.status.clone(),
        created_at: timestamp(value.created_at_ms)
            .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into()),
        sent_at: timestamp(value.sent_at_ms),
        delivered_at: timestamp(value.delivered_at_ms),
        failed_at: timestamp(value.failed_at_ms),
        failure_reason: (!value.failure_reason.is_empty()).then(|| value.failure_reason.clone()),
    }
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub async fn deliver_webhooks(
    store: Arc<Store>,
    settings: Arc<RwLock<IntegrationSettings>>,
    diagnostics: Arc<IntegrationDiagnostics>,
) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(x) => x,
        Err(_) => return,
    };
    loop {
        let stamp = now_ms();
        let _ = store.reconcile_rest_communications(stamp);
        if let Ok(Some(attempt)) = store.next_webhook(stamp) {
            let started = Instant::now();
            let payload = attempt.payload.clone();
            let request_id = serde_json::from_str::<CommunicationWebhookRequest>(&payload)
                .ok()
                .map(|value| value.data.request_id);
            diagnostics.record(diagnostic_event(
                "webhook",
                "request",
                "attempt",
                None,
                request_id.clone(),
                Some(attempt.communication_id.clone()),
                None,
                Some(payload.len()),
                Some(payload_sha256(payload.as_bytes())),
                None,
                "Webhook delivery attempted",
            ));
            let config = settings.read().unwrap_or_else(|l| l.into_inner()).clone();
            let mut request = client
                .post(&config.webhook_url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(attempt.payload.clone());
            if !config.webhook_token.is_empty() {
                request = request.bearer_auth(&config.webhook_token);
            }
            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    let _ = store.complete_webhook(attempt.id, now_ms());
                    diagnostics.record(diagnostic_event(
                        "webhook",
                        "response",
                        "success",
                        Some(response.status().as_u16()),
                        request_id,
                        Some(attempt.communication_id),
                        None,
                        Some(payload.len()),
                        None,
                        Some(started.elapsed().as_millis()),
                        "Webhook delivery accepted",
                    ));
                }
                Ok(response) => {
                    let count = attempt.attempt_count + 1;
                    let _ = store.retry_webhook(
                        attempt.id,
                        count,
                        now_ms() + retry_delay_ms(attempt.attempt_count),
                        &format!("HTTP {}", response.status().as_u16()),
                    );
                    diagnostics.record(diagnostic_event(
                        "webhook",
                        "response",
                        "retry",
                        Some(response.status().as_u16()),
                        request_id,
                        Some(attempt.communication_id),
                        None,
                        Some(payload.len()),
                        None,
                        Some(started.elapsed().as_millis()),
                        "Webhook delivery will be retried",
                    ));
                }
                Err(error) => {
                    let count = attempt.attempt_count + 1;
                    let _ = store.retry_webhook(
                        attempt.id,
                        count,
                        now_ms() + retry_delay_ms(attempt.attempt_count),
                        if error.is_timeout() {
                            "request timed out"
                        } else {
                            "request failed"
                        },
                    );
                    diagnostics.record(diagnostic_event(
                        "webhook",
                        "response",
                        "transport-failure",
                        None,
                        request_id,
                        Some(attempt.communication_id),
                        None,
                        Some(payload.len()),
                        None,
                        Some(started.elapsed().as_millis()),
                        "Webhook transport failed and will be retried",
                    ));
                }
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{CallRecord, SmsRecord, Store, UploadedAudioRecord};
    use axum::http::{Request, header};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    #[test]
    fn diagnostics_are_bounded_newest_first_and_disabled_when_requested() {
        let enabled = IntegrationDiagnostics::new(true);
        for index in 0..=INTEGRATION_DIAGNOSTICS_CAPACITY {
            enabled.record(diagnostic_event(
                "api",
                "response",
                "success",
                Some(201),
                Some(format!("request-{index}")),
                None,
                Some("sms".into()),
                Some(1),
                Some("hash".into()),
                Some(2),
                "REST communication processed",
            ));
        }
        let events = enabled.snapshot();
        assert_eq!(events.len(), INTEGRATION_DIAGNOSTICS_CAPACITY);
        assert_eq!(
            events.first().and_then(|event| event.request_id.as_deref()),
            Some("request-200")
        );
        assert_eq!(
            events.last().and_then(|event| event.request_id.as_deref()),
            Some("request-1")
        );
        assert!(events.iter().all(|event| !event.summary.contains("secret")));

        let disabled = IntegrationDiagnostics::new(false);
        disabled.record(diagnostic_event(
            "api",
            "request",
            "received",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "REST communication request received",
        ));
        assert!(disabled.snapshot().is_empty());
    }

    struct MockDispatcher {
        store: Arc<Store>,
        sends: AtomicUsize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl CommunicationDispatcher for MockDispatcher {
        async fn send_sms(
            &self,
            id: String,
            destination: String,
            body: String,
        ) -> Result<(), DispatchError> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            self.store
                .save_sms(&SmsRecord {
                    id,
                    peer: destination,
                    body,
                    state: "submitted".into(),
                    direction: "outbound".into(),
                    source: "app".into(),
                    kind: "submitted".into(),
                    created_at_ms: now_ms(),
                    storage_index: -1,
                    ..Default::default()
                })
                .map_err(|error| DispatchError::Failed(error.to_string()))
        }

        async fn make_call(
            &self,
            id: String,
            destination: String,
            audio_id: String,
        ) -> Result<(), DispatchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.store
                .save_call(&CallRecord {
                    id,
                    peer: destination,
                    audio_id,
                    state: "waiting-for-answer".into(),
                    created_at_ms: now_ms(),
                    answer_classification: "unknown".into(),
                    end_reason: "none".into(),
                    ..Default::default()
                })
                .map_err(|error| DispatchError::Failed(error.to_string()))
        }
    }

    fn fixture() -> (Router, Arc<MockDispatcher>) {
        let store = Arc::new(Store::memory().unwrap());
        let dispatcher = Arc::new(MockDispatcher {
            store: Arc::clone(&store),
            sends: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        });
        let settings = Arc::new(RwLock::new(IntegrationSettings {
            rest_enabled: true,
            rest_token: "secret".into(),
            ..Default::default()
        }));
        (
            router(RestState {
                store,
                settings,
                hardware_state: Arc::new(RwLock::new(HardwareState::Ready {
                    port_name: "COM1".into(),
                })),
                dispatcher: dispatcher.clone(),
                diagnostics: Arc::new(IntegrationDiagnostics::new(false)),
            }),
            dispatcher,
        )
    }

    fn request(body: serde_json::Value, auth: bool) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/v1/communications")
            .header(header::CONTENT_TYPE, "application/json");
        if auth {
            builder = builder.header(header::AUTHORIZATION, "Bearer secret");
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    fn health_fixture(
        hardware: HardwareState,
    ) -> (
        Router,
        Arc<Store>,
        Arc<RwLock<IntegrationSettings>>,
        Arc<RwLock<HardwareState>>,
    ) {
        let store = Arc::new(Store::memory().unwrap());
        let dispatcher = Arc::new(MockDispatcher {
            store: Arc::clone(&store),
            sends: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        });
        let settings = Arc::new(RwLock::new(IntegrationSettings {
            rest_enabled: true,
            rest_token: "secret".into(),
            ..Default::default()
        }));
        let hardware_state = Arc::new(RwLock::new(hardware));
        let app = router(RestState {
            store: Arc::clone(&store),
            settings: Arc::clone(&settings),
            hardware_state: Arc::clone(&hardware_state),
            dispatcher,
            diagnostics: Arc::new(IntegrationDiagnostics::new(false)),
        });
        (app, store, settings, hardware_state)
    }

    fn health_request(auth: bool) -> Request<Body> {
        let mut builder = Request::builder().method("GET").uri("/api/v1/health");
        if auth {
            builder = builder.header(header::AUTHORIZATION, "Bearer secret");
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn health_response(app: Router, auth: bool) -> (StatusCode, String, String) {
        let response = app.oneshot(health_request(auth)).await.unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        (
            status,
            String::from_utf8(body.to_vec()).unwrap(),
            content_type,
        )
    }

    #[tokio::test]
    async fn health_requires_enabled_rest_and_valid_authentication() {
        let (app, _, settings, _) = health_fixture(HardwareState::Ready {
            port_name: "COM1".into(),
        });
        assert_eq!(
            health_response(app.clone(), false).await.0,
            StatusCode::UNAUTHORIZED
        );
        let invalid = Request::builder()
            .method("GET")
            .uri("/api/v1/health")
            .header(header::AUTHORIZATION, "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(invalid).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        *settings.write().unwrap() = IntegrationSettings {
            rest_enabled: false,
            rest_token: "secret".into(),
            ..Default::default()
        };
        assert_eq!(health_response(app, true).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn health_returns_exact_json_boolean_for_ready_disconnected_and_busy_hardware() {
        let (app, _, _, hardware) = health_fixture(HardwareState::Ready {
            port_name: "COM1".into(),
        });
        assert_eq!(
            health_response(app.clone(), true).await,
            (StatusCode::OK, "true".into(), "application/json".into())
        );
        *hardware.write().unwrap() = HardwareState::Disconnected;
        assert_eq!(
            health_response(app.clone(), true).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "false".into(),
                "application/json".into()
            )
        );
        *hardware.write().unwrap() = HardwareState::PortBusy {
            port_name: "COM1".into(),
        };
        assert_eq!(
            health_response(app, true).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn recent_failure_makes_health_false_and_a_newer_success_restores_it() {
        let (app, store, _, _) = health_fixture(HardwareState::Ready {
            port_name: "COM1".into(),
        });
        let stamp = now_ms();
        store
            .save_sms(&SmsRecord {
                id: "failed".into(),
                direction: "outbound".into(),
                state: "send-failed".into(),
                source: "app".into(),
                created_at_ms: stamp - 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            health_response(app.clone(), true).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        store
            .save_sms(&SmsRecord {
                id: "submitted".into(),
                direction: "outbound".into(),
                state: "submitted".into(),
                source: "app".into(),
                created_at_ms: stamp,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(health_response(app, true).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn requires_auth_and_preserves_nullable_error_data() {
        let (app, _) = fixture();
        let response = app
            .oneshot(request(serde_json::json!({}), false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], "error");
        assert!(json["data"].is_null());
    }

    #[tokio::test]
    async fn request_id_is_opaque_and_duplicates_are_rejected_without_redispatch() {
        let (app, dispatcher) = fixture();
        let id = "caller-correlation/42";
        let payload = serde_json::json!({"request_id":id,"from":"desk","to":"0912345678","channel":"sms","content":"hello","encrypted":true,"send_at":null});
        let response = app
            .clone()
            .oneshot(request(payload.clone(), true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let daemon_id = json["data"]["id"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(daemon_id).is_ok());
        assert_ne!(daemon_id, id);
        assert_eq!(json["data"]["request_id"], id);
        assert_eq!(json["data"]["contact"], "+84912345678");
        assert!(json["data"].get("to").is_none());
        assert_eq!(json["data"]["status"], "sent");
        assert!(json["data"]["created_at"].as_str().unwrap().ends_with('Z'));
        assert!(json["data"].get("createdAt").is_none());
        assert_eq!(
            app.clone()
                .oneshot(request(payload, true))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(dispatcher.sends.load(Ordering::SeqCst), 1);
        let conflict = serde_json::json!({"request_id":id,"from":"desk","to":"0912345678","channel":"sms","content":"different"});
        assert_eq!(
            app.oneshot(request(conflict, true)).await.unwrap().status(),
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn requires_non_empty_request_id_and_rejects_future_work_and_unknown_channel() {
        let (app, _) = fixture();
        for payload in [
            serde_json::json!({"from":"desk","to":"0912345678","channel":"sms","content":"hello"}),
            serde_json::json!({"request_id":"","from":"desk","to":"0912345678","channel":"sms","content":"hello"}),
            serde_json::json!({"request_id":"request-2","from":"desk","to":"0912345678","channel":"email","content":"hello"}),
            serde_json::json!({"request_id":"request-3","from":"desk","to":"0912345678","channel":"sms","content":"hello","send_at":"2999-01-01T00:00:00Z"}),
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(payload, true))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
    }

    #[tokio::test]
    async fn empty_call_content_uses_current_audio() {
        let (app, dispatcher) = fixture();
        dispatcher
            .store
            .save_current_audio(&UploadedAudioRecord {
                id: "default-audio".into(),
                name: "default.amr".into(),
                format: "AMR-NB".into(),
                size: 19,
                module_path: "call_default-audio.amr".into(),
                duration_ms: 20,
                created_at_ms: 1,
                state: "ready".into(),
                is_current: true,
            })
            .unwrap();
        let response = app
            .oneshot(request(
                serde_json::json!({"request_id":"call-request","from":"desk","to":"0912345678","channel":"call","content":""}),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            dispatcher.store.list_calls(1).unwrap()[0].audio_id,
            "default-audio"
        );
    }

    #[test]
    fn lifecycle_and_retry_mapping_is_stable() {
        assert_eq!(
            normalize_source_state("sms", "delivery-unknown", "timeout", "", "").0,
            "expired"
        );
        assert_eq!(
            normalize_source_state("call", "ended", "", "not-answered", "no-answer").0,
            "failed"
        );
        assert_eq!(
            normalize_source_state("call", "failed", "timeout", "unknown", "signaling-timeout").0,
            "failed"
        );
        for (state, error, answer, end_reason) in [
            ("ended", "", "not-answered", "busy"),
            ("ended", "", "not-answered", "no-answer"),
            ("failed", "timed out", "unknown", "signaling-timeout"),
            ("ended", "network unavailable", "unknown", "network-error"),
            ("ended", "remote ended", "unknown", "call-error"),
        ] {
            let (status, reason) = normalize_source_state("call", state, error, answer, end_reason);
            assert_eq!(status, "failed");
            assert!(!reason.is_empty());
        }
        assert_eq!(
            normalize_source_state("call", "ended", "", "answered", "remote-hang-up").0,
            "delivered"
        );
        assert_eq!(
            normalize_source_state("call", "ended", "", "unknown", "local-hang-up").0,
            "sent"
        );
        assert_eq!(
            normalize_source_state("call", "ended", "", "answered", "local-hang-up").0,
            "delivered"
        );
        assert_eq!(
            (0..7).map(retry_delay_ms).collect::<Vec<_>>(),
            vec![1_000, 5_000, 30_000, 120_000, 600_000, 3_600_000, 3_600_000]
        );
    }

    #[test]
    fn call_webhooks_use_sent_delivered_and_failed_without_local_hang_up_failure() {
        let store = Store::memory().unwrap();
        let reserve = |id: &str| {
            store
                .reserve_rest_communication(&RestCommunication {
                    id: id.into(),
                    request_id: format!("request-{id}"),
                    record_id: id.into(),
                    channel: "call".into(),
                    owner: "desk".into(),
                    destination: "+84912345678".into(),
                    content: "audio".into(),
                    payload_fingerprint: format!("fingerprint-{id}"),
                    status: "sending".into(),
                    created_at_ms: 1,
                    ..Default::default()
                })
                .unwrap();
        };
        let waiting = |id: &str| CallRecord {
            id: id.into(),
            state: "waiting-for-answer".into(),
            created_at_ms: 1,
            answer_classification: "unknown".into(),
            end_reason: "none".into(),
            ..Default::default()
        };

        reserve("busy");
        let mut busy = waiting("busy");
        store.save_call(&busy).unwrap();
        store.reconcile_rest_communications(2).unwrap();
        let sent = store.next_webhook(2).unwrap().unwrap();
        assert_eq!(sent.event_type, "communication.sent");
        store.complete_webhook(sent.id, 2).unwrap();
        busy.state = "ended".into();
        busy.answer_classification = "not-answered".into();
        busy.end_reason = "busy".into();
        busy.error = "busy".into();
        busy.ended_at_ms = 3;
        store.save_call(&busy).unwrap();
        store.reconcile_rest_communications(3).unwrap();
        let failed = store.next_webhook(3).unwrap().unwrap();
        assert_eq!(failed.event_type, "communication.failed");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&failed.payload).unwrap()["data"]["failure_reason"],
            "busy"
        );
        store.complete_webhook(failed.id, 3).unwrap();

        reserve("remote");
        let mut remote = waiting("remote");
        store.save_call(&remote).unwrap();
        store.reconcile_rest_communications(4).unwrap();
        let sent = store.next_webhook(4).unwrap().unwrap();
        assert_eq!(sent.event_type, "communication.sent");
        store.complete_webhook(sent.id, 4).unwrap();
        remote.state = "ended".into();
        remote.answer_classification = "answered".into();
        remote.end_reason = "remote-hang-up".into();
        remote.connected_at_ms = 4;
        remote.ended_at_ms = 5;
        store.save_call(&remote).unwrap();
        store.reconcile_rest_communications(5).unwrap();
        let delivered = store.next_webhook(5).unwrap().unwrap();
        assert_eq!(delivered.event_type, "communication.delivered");
        store.complete_webhook(delivered.id, 5).unwrap();
        assert!(store.next_webhook(5).unwrap().is_none());

        reserve("local");
        let mut local = waiting("local");
        store.save_call(&local).unwrap();
        store.reconcile_rest_communications(6).unwrap();
        let sent = store.next_webhook(6).unwrap().unwrap();
        assert_eq!(sent.event_type, "communication.sent");
        store.complete_webhook(sent.id, 6).unwrap();
        local.state = "ended".into();
        local.end_reason = "local-hang-up".into();
        local.ended_at_ms = 7;
        store.save_call(&local).unwrap();
        store.reconcile_rest_communications(7).unwrap();
        assert!(store.next_webhook(7).unwrap().is_none());
    }

    #[test]
    fn webhook_payload_uses_only_contract_fields_and_nullable_failure_reason() {
        let communication = RestCommunication {
            id: "daemon-id".into(),
            request_id: "external-id".into(),
            failure_reason: "not answered".into(),
            ..Default::default()
        };
        for (event_type, reason) in [
            ("communication.sent", serde_json::Value::Null),
            ("communication.delivered", serde_json::Value::Null),
            ("communication.failed", serde_json::json!("not answered")),
            ("communication.expired", serde_json::json!("not answered")),
            ("communication.missed", serde_json::json!("not answered")),
        ] {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(
                    &webhook_payload(&communication, event_type).unwrap()
                )
                .unwrap(),
                serde_json::json!({"type":event_type,"data":{"id":"daemon-id","request_id":"external-id","failure_reason":reason}})
            );
        }
    }

    #[test]
    fn outbox_preserves_order_and_late_delivery_replaces_expiration() {
        let store = Store::memory().unwrap();
        let id = "bf63c10f-8085-4748-ae06-c8238c604f03";
        store
            .reserve_rest_communication(&RestCommunication {
                id: id.into(),
                request_id: "external-request".into(),
                record_id: id.into(),
                channel: "sms".into(),
                owner: "desk".into(),
                destination: "+84912345678".into(),
                content: "hello".into(),
                payload_fingerprint: "fp".into(),
                status: "sending".into(),
                created_at_ms: 1,
                ..Default::default()
            })
            .unwrap();
        let mut sms = SmsRecord {
            id: id.into(),
            state: "delivery-pending".into(),
            direction: "outbound".into(),
            source: "app".into(),
            kind: "submitted".into(),
            created_at_ms: 1,
            storage_index: -1,
            ..Default::default()
        };
        store.save_sms(&sms).unwrap();
        store.reconcile_rest_communications(2).unwrap();
        let sent = store.next_webhook(2).unwrap().unwrap();
        assert_eq!(sent.event_type, "communication.sent");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&sent.payload).unwrap(),
            serde_json::json!({"type":"communication.sent","data":{"id":id,"request_id":"external-request","failure_reason":null}})
        );
        sms.state = "delivery-unknown".into();
        sms.cause = "timeout".into();
        store.save_sms(&sms).unwrap();
        store.reconcile_rest_communications(3).unwrap();
        assert_eq!(store.next_webhook(3).unwrap().unwrap().id, sent.id);
        store.complete_webhook(sent.id, 3).unwrap();
        let expired = store.next_webhook(3).unwrap().unwrap();
        assert_eq!(expired.event_type, "communication.expired");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&expired.payload).unwrap()["data"]["failure_reason"],
            "timeout"
        );
        sms.state = "delivered".into();
        sms.cause.clear();
        store.save_sms(&sms).unwrap();
        store.reconcile_rest_communications(4).unwrap();
        store.complete_webhook(expired.id, 4).unwrap();
        assert_eq!(
            store.next_webhook(4).unwrap().unwrap().event_type,
            "communication.delivered"
        );
        let communication = store.rest_communication(id).unwrap().unwrap();
        assert_eq!(communication.status, "delivered");
        assert_eq!(communication.failed_at_ms, 0);
        assert!(communication.failure_reason.is_empty());
    }
}
