//! Durable Viettel balance checks shared by REST and the desktop transport.

use crate::{
    ModemError, hardware::HardwareState, integration::CommunicationDispatcher, storage::Store,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

pub const REPLY_TIMEOUT_MS: i64 = 120_000;
pub const COOLDOWN_MS: i64 = 60_000;
pub const QUARANTINE_MS: i64 = 300_000;
pub const FRESHNESS_MS: i64 = 300_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BalanceCheck {
    pub id: String,
    pub request_id: String,
    pub status: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub completed_at_ms: Option<i64>,
    pub dispatched_at_ms: Option<i64>,
    pub deadline_ms: Option<i64>,
    pub amount_vnd: Option<i64>,
    pub observed_at_ms: Option<i64>,
    pub reply_sms_id: Option<String>,
    pub failure_reason: Option<String>,
    pub baseline: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum BalanceReservation {
    New(BalanceCheck),
    Replay(BalanceCheck),
    Active(BalanceCheck),
    Cooldown(i64),
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BalanceObservation {
    pub amount_vnd: i64,
    pub observed_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BalanceCheckData {
    pub id: String,
    pub request_id: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub balance: Option<BalanceObservation>,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LatestBalance {
    pub balance: Option<BalanceObservation>,
    pub freshness: String,
    pub active_check_id: Option<String>,
    pub retry_after_seconds: u64,
}

fn timestamp(stamp: i64) -> String {
    chrono::DateTime::from_timestamp_millis(stamp)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl From<BalanceCheck> for BalanceCheckData {
    fn from(check: BalanceCheck) -> Self {
        Self {
            id: check.id,
            request_id: check.request_id,
            status: check.status,
            created_at: timestamp(check.created_at_ms),
            updated_at: timestamp(check.updated_at_ms),
            completed_at: check.completed_at_ms.map(timestamp),
            balance: check
                .amount_vnd
                .zip(check.observed_at_ms)
                .map(|(amount_vnd, stamp)| BalanceObservation {
                    amount_vnd,
                    observed_at: timestamp(stamp),
                }),
            failure_reason: check.failure_reason,
        }
    }
}

pub struct BalanceService {
    pub store: Arc<Store>,
    dispatcher: Arc<dyn CommunicationDispatcher>,
    hardware: Arc<RwLock<HardwareState>>,
}

impl BalanceService {
    pub fn new(
        store: Arc<Store>,
        dispatcher: Arc<dyn CommunicationDispatcher>,
        hardware: Arc<RwLock<HardwareState>>,
    ) -> Self {
        Self {
            store,
            dispatcher,
            hardware,
        }
    }
    pub fn start(&self, request_id: &str) -> Result<BalanceReservation, ModemError> {
        let ready = matches!(
            *self
                .hardware
                .read()
                .unwrap_or_else(|lock| lock.into_inner()),
            HardwareState::Ready { .. }
        );
        self.store
            .reserve_balance_check(request_id, crate::integration::now_ms(), ready)
    }
    /// Only the service worker dispatches, after a durable claim.
    pub async fn tick(&self) -> Result<(), ModemError> {
        // A previous dispatch may have returned before its state could be saved.
        // This single worker has no in-flight send at the beginning of a tick.
        self.store
            .recover_balance_checks(crate::integration::now_ms())?;
        self.store
            .reconcile_balance_check(crate::integration::now_ms())?;
        let ready = matches!(
            *self
                .hardware
                .read()
                .unwrap_or_else(|lock| lock.into_inner()),
            HardwareState::Ready { .. }
        );
        if !ready
            || self
                .store
                .active_balance_check()?
                .is_none_or(|check| check.status != "queued")
        {
            return Ok(());
        }
        if self.dispatcher.prepare_balance_check().await.is_err() {
            return Ok(());
        }
        let Some(check) = self
            .store
            .claim_balance_dispatch(crate::integration::now_ms())?
        else {
            return Ok(());
        };
        let _ = self
            .dispatcher
            .send_balance_sms(
                check.id.clone(),
                check.deadline_ms.expect("claimed check has deadline"),
            )
            .await;
        let outgoing = self
            .store
            .list_sms(usize::MAX)?
            .into_iter()
            .find(|sms| sms.id == check.id);
        let status = match outgoing.as_ref().map(|sms| sms.state.as_str()) {
            Some(
                "submitted" | "delivery-pending" | "delivered" | "delivery-failed"
                | "delivery-unknown",
            ) => "waiting_reply",
            Some("send-failed") => "failed",
            _ => "send_unknown",
        };
        self.store
            .mark_balance_submission(&check.id, status, crate::integration::now_ms())?;
        self.store
            .reconcile_balance_check(crate::integration::now_ms())
    }
    pub async fn run(self: Arc<Self>) {
        loop {
            if self.tick().await.is_err() {
                eprintln!("balance workflow deferred (details redacted)");
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

pub fn parse_main_balance(body: &str) -> Option<i64> {
    use std::sync::OnceLock;
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    static LABEL: OnceLock<regex::Regex> = OnceLock::new();
    let folded: String = body
        .to_lowercase()
        .chars()
        .map(|ch| match ch {
            'á' | 'à' | 'ả' | 'ã' | 'ạ' | 'ă' | 'ắ' | 'ằ' | 'ẳ' | 'ẵ' | 'ặ' | 'â' | 'ấ' | 'ầ'
            | 'ẩ' | 'ẫ' | 'ậ' => 'a',
            'é' | 'è' | 'ẻ' | 'ẽ' | 'ẹ' | 'ê' | 'ế' | 'ề' | 'ể' | 'ễ' | 'ệ' => {
                'e'
            }
            'í' | 'ì' | 'ỉ' | 'ĩ' | 'ị' => 'i',
            'ó' | 'ò' | 'ỏ' | 'õ' | 'ọ' | 'ô' | 'ố' | 'ồ' | 'ổ' | 'ỗ' | 'ộ' | 'ơ' | 'ớ' | 'ờ'
            | 'ở' | 'ỡ' | 'ợ' => 'o',
            'ú' | 'ù' | 'ủ' | 'ũ' | 'ụ' | 'ư' | 'ứ' | 'ừ' | 'ử' | 'ữ' | 'ự' => {
                'u'
            }
            'ý' | 'ỳ' | 'ỷ' | 'ỹ' | 'ỵ' => 'y',
            'đ' => 'd',
            _ => ch,
        })
        .collect();
    let label = LABEL.get_or_init(|| {
        regex::Regex::new(r"\b(?:tk|tai\s+khoan)\s+(?:goc|chinh)\b")
            .expect("constant balance label")
    });
    if label.find_iter(&folded).count() != 1 {
        return None;
    }
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"\b(?:tk|tai\s+khoan)\s+(?:goc|chinh)\s*[:=]\s*([0-9][0-9.,]*)\s*(?:vnd|dong|d)\b",
        )
        .expect("constant balance pattern")
    });
    let mut matches = pattern.captures_iter(&folded);
    let matched = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let number = &matched[1];
    if number.contains('.') || number.contains(',') {
        let separator = if number.contains('.') { '.' } else { ',' };
        let groups: Vec<_> = number.split(separator).collect();
        if groups[0].is_empty()
            || groups[0].len() > 3
            || groups
                .iter()
                .skip(1)
                .any(|group| group.len() != 3 || !group.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
    }
    number
        .replace(['.', ','], "")
        .parse::<i64>()
        .ok()
        .filter(|amount| *amount <= 9_007_199_254_740_991)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_unambiguous_main_monetary_balance() {
        for (body, amount) in [
            ("TK goc: 85.500d; tai khoan khuyen mai: 89.174d", 85500),
            ("Tài khoản gốc: 125,000 VND", 125000),
            ("TK chinh: 0d", 0),
            ("TK goc: 1.234.567 đ", 1234567),
        ] {
            assert_eq!(parse_main_balance(body), Some(amount), "{body}");
        }
        for body in [
            "TK khuyen mai: 10000d",
            "Data: 500 MB",
            "Cuoc tra sau: 100000d",
            "TK goc: 1.23d",
            "TK goc: -100d",
            "TK goc: 100 MB",
            "TK goc: 100d; TK chinh: 200d",
            "TK goc: 999999999999999999999d",
            "TK goc: 100d; TK chinh: unavailable",
            "TK goc: 9007199254740993d",
        ] {
            assert_eq!(parse_main_balance(body), None, "{body}");
        }
    }
}
