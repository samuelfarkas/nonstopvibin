//! Stubs and helpers shared by the feature tests.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::future::{Future, ready};
use std::path::Path;
use std::sync::{Arc, Mutex};

use nsv_accounts::fetch::Request;
use nsv_accounts::quota::QuotaSource;
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::wire::Account;
use serde_json::Value;
use tokio::sync::Semaphore;

/// The recorder's fixed clock, `2026-11-22T12:00:00.000Z`.
pub(crate) const NOW: i64 = 1_795_348_800_000;
pub(crate) const NOW_ISO: &str = "2026-11-22T12:00:00.000Z";

pub(crate) fn fixed() -> i64 {
    NOW
}

pub(crate) fn cases(area: &str) -> Vec<Value> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../tests/golden/{area}.json"));
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

pub(crate) fn case<'a>(cases: &'a [Value], name: &str) -> &'a Value {
    cases
        .iter()
        .find(|case| case["name"] == name)
        .unwrap_or_else(|| panic!("no case {name}"))
}

/// `value` with each placeholder replaced in every string and key.
pub(crate) fn replaced(value: &Value, pairs: &[(&str, &str)]) -> Value {
    let mut text = value.to_string();
    for (placeholder, actual) in pairs {
        text = text.replace(placeholder, actual);
    }
    serde_json::from_str(&text).unwrap()
}

/// The core and account list as the golden recorder stubbed them: queued `/api-call`
/// answers, recorded bodies.
#[derive(Default)]
pub(crate) struct StubSource {
    pub(crate) accounts: Vec<Account>,
    pub(crate) chatgpt: HashMap<String, String>,
    pub(crate) stopped: bool,
    pub(crate) answers: Mutex<VecDeque<Value>>,
    pub(crate) calls: Mutex<Vec<Value>>,
    pub(crate) gate: Option<Arc<Semaphore>>,
}

impl StubSource {
    pub(crate) fn queue(&self, answers: impl IntoIterator<Item = Value>) {
        self.answers.lock().unwrap().extend(answers);
    }

    pub(crate) fn take_calls(&self) -> Vec<Value> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl QuotaSource for StubSource {
    fn accounts(
        &self,
        _: &ProfileId,
    ) -> impl Future<Output = Result<Vec<Account>, AppError>> + Send {
        ready(Ok(self.accounts.clone()))
    }

    fn chatgpt_account_id(&self, _: &ProfileId, account: &AccountId) -> Option<String> {
        self.chatgpt.get(account.as_str()).cloned()
    }

    fn usable(&self, _: &ProfileId) -> bool {
        !self.stopped
    }

    async fn api_call(&self, _: &ProfileId, body: Value) -> Result<Value, AppError> {
        self.calls.lock().unwrap().push(body);
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        let answer = self.answers.lock().unwrap().pop_front();
        answer.ok_or_else(|| AppError::new("no stub answer", 500))
    }
}

/// Header names in lowercase, as the recorder's `Headers` wrote them.
pub(crate) fn headers(request: &Request) -> Value {
    request
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), Value::String(value.clone())))
        .collect::<serde_json::Map<_, _>>()
        .into()
}
