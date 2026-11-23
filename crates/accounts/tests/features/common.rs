//! Stubs and helpers shared by the feature tests.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::future::{Future, ready};
use std::path::Path;
use std::sync::{Arc, Mutex};

use nsv_accounts::fetch::{Fetch, Request};
use nsv_accounts::quota::QuotaSource;
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::wire::Account;
use nsv_net::{HeaderMap, Loopback, NetError, Response, StatusCode};
use nsv_store::files::owner_only_directory;
use nsv_store::{Store, Vault};
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

pub(crate) async fn open_store(directory: &Path) -> Store {
    owner_only_directory(directory, || Ok::<(), std::io::Error>(())).unwrap();
    Store::open(directory, Vault::open(directory).unwrap())
        .await
        .unwrap()
}

pub(crate) async fn profile(store: &Store, name: &str) -> ProfileId {
    let name = name.to_owned();
    store
        .call(move |db| Ok(db.create_profile(&name)?.id))
        .await
        .unwrap()
}

pub(crate) fn response(status: u16, body: impl Into<Vec<u8>>) -> Response {
    Response {
        status: StatusCode::from_u16(status).unwrap(),
        headers: HeaderMap::new(),
        body: body.into(),
    }
}

/// What a stub fetch was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) request: Request,
    pub(crate) loopback: Option<(Loopback, String)>,
}

type Answer = Box<dyn Fn(&str) -> Result<Response, NetError> + Send + Sync>;

/// Answers each request by its URL and records it; a gate holds answers until it opens.
pub(crate) struct StubFetch {
    pub(crate) seen: Mutex<Vec<Seen>>,
    answer: Mutex<Answer>,
    gate: Option<Arc<Semaphore>>,
}

impl StubFetch {
    pub(crate) fn new(
        answer: impl Fn(&str) -> Result<Response, NetError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            answer: Mutex::new(Box::new(answer)),
            gate: None,
        }
    }

    /// Holds every answer until a permit is added to the returned gate.
    pub(crate) fn gated(
        answer: impl Fn(&str) -> Result<Response, NetError> + Send + Sync + 'static,
    ) -> (Self, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        let mut stub = Self::new(answer);
        stub.gate = Some(Arc::clone(&gate));
        (stub, gate)
    }

    pub(crate) fn answer(
        &self,
        answer: impl Fn(&str) -> Result<Response, NetError> + Send + Sync + 'static,
    ) {
        *self.answer.lock().unwrap() = Box::new(answer);
    }

    pub(crate) fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    async fn reply(&self, seen: Seen) -> Result<Response, NetError> {
        let url = seen.request.url.clone();
        self.seen.lock().unwrap().push(seen);
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        (self.answer.lock().unwrap())(&url)
    }
}

impl Fetch for StubFetch {
    async fn https(&self, request: Request) -> Result<Response, NetError> {
        self.reply(Seen {
            request,
            loopback: None,
        })
        .await
    }

    async fn loopback(
        &self,
        to: Loopback,
        target: String,
        request: Request,
    ) -> Result<Response, NetError> {
        self.reply(Seen {
            request,
            loopback: Some((to, target)),
        })
        .await
    }
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
