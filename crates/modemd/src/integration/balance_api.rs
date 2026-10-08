use super::*;
use crate::balance::{BalanceCheckData, BalanceReservation, BalanceService};
use axum::extract::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    request_id: String,
}

fn authenticated(state: &RestState, headers: &HeaderMap) -> bool {
    let settings = state
        .settings
        .read()
        .unwrap_or_else(|lock| lock.into_inner());
    settings.rest_enabled
        && !settings.rest_token.is_empty()
        && authorized(headers, &settings.rest_token)
}
fn response<T: Serialize>(status: StatusCode, message: &str, data: T) -> Response {
    (
        status,
        Json(HttpSmsEnvelope {
            status: if status.is_success() {
                "success"
            } else {
                "error"
            }
            .into(),
            message: message.into(),
            data: Some(data),
        }),
    )
        .into_response()
}

pub(super) async fn start(
    State(state): State<RestState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !authenticated(&state, &headers) {
        return error(StatusCode::UNAUTHORIZED, "authentication required");
    }
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds 64 KiB"),
    };
    let request: StartRequest = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(err) => {
            return error(
                if err.is_data() {
                    StatusCode::UNPROCESSABLE_ENTITY
                } else {
                    StatusCode::BAD_REQUEST
                },
                "invalid JSON request",
            );
        }
    };
    let service = BalanceService::new(
        state.store.clone(),
        state.dispatcher.clone(),
        state.hardware_state.clone(),
    );
    match service.start(&request.request_id) {
        Ok(BalanceReservation::New(check)) => response(
            StatusCode::ACCEPTED,
            "balance check queued",
            BalanceCheckData::from(check),
        ),
        Ok(BalanceReservation::Replay(check)) => response(
            StatusCode::OK,
            "balance check already exists",
            BalanceCheckData::from(check),
        ),
        Ok(BalanceReservation::Active(check)) => response(
            StatusCode::CONFLICT,
            "a balance check is already active",
            serde_json::json!({"active_check_id":check.id}),
        ),
        Ok(BalanceReservation::Cooldown(ms)) => {
            let seconds = (ms + 999) / 1000;
            let mut result = response(
                StatusCode::TOO_MANY_REQUESTS,
                "balance check cooldown is active",
                serde_json::json!({"retry_after_seconds":seconds}),
            );
            if let Ok(value) = seconds.to_string().parse() {
                result
                    .headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, value);
            }
            result
        }
        Ok(BalanceReservation::Unavailable) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "modem is unavailable")
        }
        Err(ModemError::Validation(_)) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "request_id must contain 1 to 256 bytes",
        ),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "balance operation could not be stored",
        ),
    }
}
pub(super) async fn get_check(
    State(state): State<RestState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !authenticated(&state, &headers) {
        return error(StatusCode::UNAUTHORIZED, "authentication required");
    }
    match state.store.balance_check(&id) {
        Ok(Some(check)) => response(
            StatusCode::OK,
            "balance check retrieved",
            BalanceCheckData::from(check),
        ),
        Ok(None) => error(StatusCode::NOT_FOUND, "balance check not found"),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "balance check could not be read",
        ),
    }
}
pub(super) async fn latest(State(state): State<RestState>, headers: HeaderMap) -> Response {
    if !authenticated(&state, &headers) {
        return error(StatusCode::UNAUTHORIZED, "authentication required");
    }
    match state.store.latest_balance(now_ms()) {
        Ok(data) => response(StatusCode::OK, "latest balance retrieved", data),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "latest balance could not be read",
        ),
    }
}
