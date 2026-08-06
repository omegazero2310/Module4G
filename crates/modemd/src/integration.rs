use crate::{
    ModemError,
    storage::{CommunicationReservation, IntegrationSettings, RestCommunication, Store},
};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_WEBHOOK_URL: &str = "http://10.1.11.117:5068/api/v1/webhooks/receive";
pub const MAX_BODY_BYTES: usize = 64 * 1024;

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
    pub request_id: Option<String>,
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
    pub owner: String,
    pub to: String,
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

#[derive(Clone, Debug, Serialize)]
pub struct CommunicationWebhookRequest {
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: CommunicationData,
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
    pub dispatcher: Arc<dyn CommunicationDispatcher>,
}

pub fn router(state: RestState) -> Router {
    Router::new()
        .route("/api/v1/communications", post(post_communication))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

pub async fn serve(listener: tokio::net::TcpListener, state: RestState) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

async fn post_communication(
    State(state): State<RestState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
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
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds 64 KiB"),
    };
    let request: CommunicationRequest = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(parse_error) => {
            let status = if matches!(parse_error.classify(), serde_json::error::Category::Data) {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            };
            return error(status, "invalid JSON request");
        }
    };
    match process_request(&state, request).await {
        Ok((status, data, replay)) => (
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
            .into_response(),
        Err((status, message)) => error(status, &message),
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
    let id = match request.request_id {
        Some(value) => uuid::Uuid::parse_str(&value)
            .map_err(|_| {
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "request_id must be a UUID".into(),
                )
            })?
            .to_string(),
        None => uuid::Uuid::new_v4().to_string(),
    };
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
        Some(
            state
                .store
                .audio_named(&request.content)
                .map_err(internal)?
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
            return Err((
                StatusCode::CONFLICT,
                "request_id was already used with a different payload".into(),
            ));
        }
        CommunicationReservation::Replay(existing) => {
            return Ok((StatusCode::OK, communication_data(&existing), true));
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
    if answer == "not-answered" || matches!(end_reason, "no-answer" | "busy" | "signaling-timeout")
    {
        return (
            "missed",
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
        data: communication_data(communication),
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
        owner: value.owner.clone(),
        to: value.destination.clone(),
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

pub async fn deliver_webhooks(store: Arc<Store>, settings: Arc<RwLock<IntegrationSettings>>) {
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
                }
                Ok(response) => {
                    let count = attempt.attempt_count + 1;
                    let _ = store.retry_webhook(
                        attempt.id,
                        count,
                        now_ms() + retry_delay_ms(attempt.attempt_count),
                        &format!("HTTP {}", response.status().as_u16()),
                    );
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
    use crate::storage::SmsRecord;
    use axum::http::{Request, header};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    struct MockDispatcher {
        store: Arc<Store>,
        sends: AtomicUsize,
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
            _id: String,
            _destination: String,
            _audio_id: String,
        ) -> Result<(), DispatchError> {
            Err(DispatchError::Validation("audio unavailable".into()))
        }
    }

    fn fixture() -> (Router, Arc<MockDispatcher>) {
        let store = Arc::new(Store::memory().unwrap());
        let dispatcher = Arc::new(MockDispatcher {
            store: Arc::clone(&store),
            sends: AtomicUsize::new(0),
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
                dispatcher: dispatcher.clone(),
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
    async fn uuid_is_idempotent_and_conflicts_on_changed_payload() {
        let (app, dispatcher) = fixture();
        let id = "d2719dd7-246b-4e20-9c17-d40f0567b217";
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
        assert_eq!(json["data"]["id"], id);
        assert_eq!(json["data"]["status"], "sent");
        assert!(json["data"]["created_at"].as_str().unwrap().ends_with('Z'));
        assert!(json["data"].get("createdAt").is_none());
        assert_eq!(
            app.clone()
                .oneshot(request(payload, true))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(dispatcher.sends.load(Ordering::SeqCst), 1);
        let conflict = serde_json::json!({"request_id":id,"from":"desk","to":"0912345678","channel":"sms","content":"different"});
        assert_eq!(
            app.oneshot(request(conflict, true)).await.unwrap().status(),
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn rejects_invalid_uuid_future_work_and_unknown_channel() {
        let (app, _) = fixture();
        for payload in [
            serde_json::json!({"request_id":"bad","from":"desk","to":"0912345678","channel":"sms","content":"hello"}),
            serde_json::json!({"from":"desk","to":"0912345678","channel":"email","content":"hello"}),
            serde_json::json!({"from":"desk","to":"0912345678","channel":"sms","content":"hello","send_at":"2999-01-01T00:00:00Z"}),
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

    #[test]
    fn lifecycle_and_retry_mapping_is_stable() {
        assert_eq!(
            normalize_source_state("sms", "delivery-unknown", "timeout", "", "").0,
            "expired"
        );
        assert_eq!(
            normalize_source_state("call", "ended", "", "not-answered", "no-answer").0,
            "missed"
        );
        assert_eq!(
            normalize_source_state("call", "failed", "timeout", "unknown", "signaling-timeout").0,
            "missed"
        );
        assert_eq!(
            (0..7).map(retry_delay_ms).collect::<Vec<_>>(),
            vec![1_000, 5_000, 30_000, 120_000, 600_000, 3_600_000, 3_600_000]
        );
    }

    #[test]
    fn outbox_preserves_order_and_late_delivery_replaces_expiration() {
        let store = Store::memory().unwrap();
        let id = "bf63c10f-8085-4748-ae06-c8238c604f03";
        store
            .reserve_rest_communication(&RestCommunication {
                id: id.into(),
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
        sms.state = "delivery-unknown".into();
        sms.cause = "timeout".into();
        store.save_sms(&sms).unwrap();
        store.reconcile_rest_communications(3).unwrap();
        assert_eq!(store.next_webhook(3).unwrap().unwrap().id, sent.id);
        store.complete_webhook(sent.id, 3).unwrap();
        let expired = store.next_webhook(3).unwrap().unwrap();
        assert_eq!(expired.event_type, "communication.expired");
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
