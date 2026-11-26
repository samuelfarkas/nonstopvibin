//! Banked rate-limit resets (`src/server/reset-credits.ts`): listing a subscription's resets and
//! redeeming one. Codex lists credits; Claude lists the reset grants Claude Code 2.1.332 offers
//! (`cedar_ember`), and only to Claude Code, so Claude calls present the identity the core does.
//! A redemption keeps its request id in the store until the outcome is known, so a retry after
//! a failure, an ambiguous answer or a restart redeems the same reset once.

use std::borrow::Cow;
use std::sync::Arc;

use nsv_core::upstream::CLAUDE_CODE_USER_AGENT;
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::js;
use nsv_domain::registry::{Provider, ResetResultCode, registry};
use nsv_domain::time;
use nsv_domain::wire::{AccountKind, QuotaStatus, ResetCredit, ResetCredits, ResetResult};
use nsv_store::Store;
use serde_json::{Map, Value, json};

use crate::quota::{
    CLAUDE_API, CODEX_API, QuotaError, QuotaSource, Quotas, RefreshQuota, UpstreamRequest,
    call_upstream, status_text,
};
use crate::source::PoolSource;
use crate::task::Exclusive;

/// Claude Code's name for the reset program whose grants Claude lists and redeems.
const CLAUDE_PROGRAM: &str = "cedar_ember";

/// A whole, non-negative count that fits a `u32`.
fn whole(value: &Value) -> Option<u32> {
    let count = value.as_f64()?;
    if count.fract() != 0.0 || count < 0.0 {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked whole and non-negative; u32 bounds it"
    )]
    u32::try_from(count as u64).ok()
}

/// zod's `resetCreditsSchema` (`OpenAI`'s `backend-client/src/types.rs`).
#[must_use]
pub fn reset_credits(value: &Value) -> Option<ResetCredits> {
    let object = value.as_object()?;
    let available_count = whole(object.get("available_count")?)?;
    let credits = object
        .get("credits")?
        .as_array()?
        .iter()
        .map(reset_credit)
        .collect::<Option<Vec<_>>>()?;
    Some(ResetCredits {
        available_count,
        credits,
        note: None,
    })
}

fn reset_credit(value: &Value) -> Option<ResetCredit> {
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?;
    if !(1..=500).contains(&js::utf16_len(id)) {
        return None;
    }
    let expires_at = match object.get("expires_at")? {
        Value::Null => None,
        Value::String(time) if time::is_offset_datetime(time) => Some(time.clone()),
        _ => return None,
    };
    // The wire keeps an absent field apart from a `null` one.
    let wire = |field| match field {
        Nullable::Absent => None,
        Nullable::Null => Some(None),
        Nullable::Text(text) => Some(Some(text)),
    };
    Some(ResetCredit {
        id: id.to_owned(),
        reset_type: object.get("reset_type")?.as_str()?.to_owned(),
        status: object.get("status")?.as_str()?.to_owned(),
        expires_at,
        title: wire(nullable_text(object, "title")?),
        description: wire(nullable_text(object, "description")?),
    })
}

/// An optional, nullable string field.
enum Nullable {
    Absent,
    Null,
    Text(String),
}

/// The field `key` as [`Nullable`]; `None` for anything else, which drops the credit.
fn nullable_text(object: &Map<String, Value>, key: &str) -> Option<Nullable> {
    match object.get(key) {
        None => Some(Nullable::Absent),
        Some(Value::Null) => Some(Nullable::Null),
        Some(Value::String(text)) => Some(Nullable::Text(text.clone())),
        Some(_) => None,
    }
}

/// zod's `resetResultSchema`: a known result code.
#[must_use]
pub fn reset_result(value: &Value) -> Option<ResetResultCode> {
    ResetResultCode::from_id(js::text(js::field(Some(value), "code"))?)
}

/// A Claude grant id as Claude Code accepts one.
fn grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
}

/// Claude's reset grants, read from the `cedar_ember` block of its usage answer as Claude Code
/// 2.1.332 offers them: one credit per kind of reset (Fable, full, 5-hour, weekly) that has
/// resets left, `available` when one of its grants can be used now, which is the one a claim
/// redeems. An ineligible subscription lists none and says why in `note`; no block lists none.
/// `None` for a block that does not match the schema; a grant that does not is skipped.
#[must_use]
pub fn claude_credits(value: &Value) -> Option<ResetCredits> {
    let mut credits = ResetCredits {
        available_count: 0,
        credits: Vec::new(),
        note: None,
    };
    let status = match value.as_object()?.get(CLAUDE_PROGRAM) {
        None | Some(Value::Null) => return Some(credits),
        Some(status) => status.as_object()?,
    };
    if !status.get("eligible")?.as_bool()? {
        credits.note = Some(ineligible(js::text(status.get("ineligible_reason"))).to_owned());
        return Some(credits);
    }
    // Each kind's grants in Claude's order, and the first usable one: what Claude Code offers.
    let mut kinds: Vec<(Grant, Option<Grant>, u32)> = Vec::new();
    let grants = status.get("grants").and_then(Value::as_array);
    for grant in grants
        .into_iter()
        .flatten()
        .filter_map(|grant| Grant::read(grant, status))
    {
        let Some((_, target, left)) = kinds
            .iter_mut()
            .find(|(first, ..)| first.kind == grant.kind)
        else {
            let target = grant.why.is_none().then(|| grant.clone());
            let left = grant.left;
            kinds.push((grant, target, left));
            continue;
        };
        *left = left.saturating_add(grant.left);
        if target.is_none() && grant.why.is_none() {
            *target = Some(grant);
        }
    }
    for (first, target, left) in kinds {
        if left == 0 {
            continue;
        }
        if target.is_some() {
            credits.available_count = credits.available_count.saturating_add(left);
        }
        credits.credits.push(target.unwrap_or(first).credit(left));
    }
    Some(credits)
}

/// Why Claude offers a subscription no resets (`ineligible_reason`), in words.
fn ineligible(reason: Option<&str>) -> &'static str {
    match reason {
        Some("surface") => "Claude offers resets only to Claude Code and claude.ai.",
        Some("cli_version") => {
            "Claude offers resets only to a newer Claude Code. Update NonstopVibin to see them."
        }
        Some("unavailable") => "Claude could not check resets. Try again later.",
        _ => "Claude offers this subscription no resets right now.",
    }
}

/// A Claude reset grant as Claude Code reads one.
#[derive(Clone)]
struct Grant {
    id: String,
    left: u32,
    /// Claude Code's name for its kind.
    kind: &'static str,
    /// The limits it refills, in words.
    limits: String,
    expires_at: Option<String>,
    /// Why it cannot be used now; `None` when it can.
    why: Option<&'static str>,
}

impl Grant {
    /// A grant Claude Code would list: unpaused, with resets left or usable. `None` otherwise
    /// and for one that does not match the schema.
    fn read(value: &Value, status: &Map<String, Value>) -> Option<Self> {
        let grant = value.as_object()?;
        let id = grant.get("id")?.as_str().filter(|id| grant_id(id))?;
        let left = whole(grant.get("resets_left")?)?;
        let flag = |key, default| grant.get(key).and_then(Value::as_bool).unwrap_or(default);
        let usable = flag("usable_now", false);
        if flag("paused", false) || (left == 0 && !usable) {
            return None;
        }
        let clears = windows(grant.get("clears"));
        let fable = flag("gives_back_weekly", false);
        let kind = match (fable, js::text(grant.get("type"))) {
            (true, _) => "Fable reset",
            (false, Some("full")) => "Full reset",
            (false, Some("session")) => "5-hour reset",
            (false, Some("weekly")) => "Weekly reset",
            _ => match (
                clears.contains(&"five_hour"),
                clears.iter().any(|window| *window != "five_hour"),
            ) {
                (true, false) => "5-hour reset",
                (false, true) => "Weekly reset",
                _ => "Full reset",
            },
        };
        let why = if usable {
            None
        } else {
            Some(unusable(grant, status, &clears, fable))
        };
        Some(Self {
            id: id.to_owned(),
            left,
            kind,
            limits: refills(&clears, fable),
            expires_at: grant
                .get("ends_at")
                .and_then(Value::as_str)
                .filter(|time| time::is_offset_datetime(time))
                .map(str::to_owned),
            why,
        })
    }

    /// The credit for this grant's kind, with `left` resets across the kind.
    fn credit(self, left: u32) -> ResetCredit {
        let mut description = format!("Refills your {} · {left} left", self.limits);
        if let Some(why) = self.why {
            description = format!("{description} · {why}");
        }
        ResetCredit {
            id: self.id,
            reset_type: self.kind.to_owned(),
            status: if self.why.is_none() {
                "available"
            } else {
                "unavailable"
            }
            .to_owned(),
            expires_at: self.expires_at,
            title: None,
            description: Some(Some(description)),
        }
    }
}

/// A usage window list (`clears`, `blocking`, `exhausted`); anything else is empty.
fn windows(value: Option<&Value>) -> Vec<&str> {
    value
        .and_then(Value::as_array)
        .map_or_default(|windows| windows.iter().filter_map(Value::as_str).collect())
}

/// The limits a grant refills, as Claude Code names them: "Fable and 5-hour limits".
fn refills(clears: &[&str], fable: bool) -> String {
    let mut names: Vec<&str> = Vec::new();
    let mut add = |name| {
        if !names.contains(&name) {
            names.push(name);
        }
    };
    if fable {
        add("Fable");
    }
    for window in clears {
        match *window {
            "five_hour" => add("5-hour"),
            "seven_day" => add("weekly"),
            "seven_day_opus" => add("Opus weekly"),
            "seven_day_sonnet" => add("Sonnet weekly"),
            "seven_day_overage_included" => add(if fable { "Fable" } else { "weekly" }),
            _ => {}
        }
    }
    match names.split_last() {
        None => "limits".to_owned(),
        Some((last, [])) => format!("{last} limit"),
        Some((last, rest)) => format!("{} and {last} limits", rest.join(", ")),
    }
}

/// Why a grant cannot be used now, in Claude Code's words: Claude's own reason, else one read
/// from the status the way Claude Code reads it.
fn unusable(
    grant: &Map<String, Value>,
    status: &Map<String, Value>,
    clears: &[&str],
    fable: bool,
) -> &'static str {
    let refills = |window: &str| clears.contains(&window) || (window == "seven_day" && fable);
    let reason = js::text(grant.get("unavailable_reason")).unwrap_or_else(|| {
        if js::text(status.get("cooldown_until")).is_some() {
            return "cooldown";
        }
        let blocking = windows(grant.get("blocking"));
        if let Some(window) = blocking.iter().find_map(|window| match *window {
            "five_hour" => Some("blocked_five_hour"),
            "seven_day" => Some("blocked_seven_day"),
            "seven_day_opus" => Some("blocked_opus"),
            "seven_day_sonnet" => Some("blocked_sonnet"),
            _ => None,
        }) {
            return window;
        }
        if !blocking.is_empty() {
            return "blocked_other";
        }
        let at_limit = status.get("at_limit").and_then(Value::as_bool) == Some(true);
        if at_limit
            && !windows(status.get("exhausted"))
                .iter()
                .any(|window| refills(window))
        {
            return "cannot_clear";
        }
        if grant.get("use_requires_limit").and_then(Value::as_bool) == Some(false) {
            "unavailable"
        } else {
            "needs_limit"
        }
    });
    match reason {
        "cooldown" => "try again in a minute",
        "blocked_five_hour" => "can't be used while you're at your 5-hour limit",
        "blocked_seven_day" => "can't be used while you're at your weekly limit",
        "blocked_opus" => "can't be used while you're at your Opus weekly limit",
        "blocked_sonnet" => "can't be used while you're at your Sonnet weekly limit",
        "blocked_other" => "can't be used while you're at another limit it doesn't refill",
        "cannot_clear" => "doesn't refill the limit you've reached",
        "needs_limit" => "usable once you reach a limit it refills",
        _ => "can't be used right now",
    }
}

/// Claude's answer to a claim (`result`), as Claude Code reads it.
///
/// # Errors
/// 409 while another reset is going through, 502 for an unknown outcome: Claude Code cannot
/// tell whether an `unavailable` claim used a reset either.
fn claude_result(value: &Value) -> Result<ResetResultCode, AppError> {
    match js::text(js::field(Some(value), "result")) {
        Some("reset") => Ok(ResetResultCode::Reset),
        Some("already_used") => Ok(ResetResultCode::AlreadyRedeemed),
        Some("not_limited") => Ok(ResetResultCode::NothingToReset),
        Some("ineligible") => Ok(ResetResultCode::NoCredit),
        Some("cooldown") => Err(AppError::new(
            "Another reset is still going through for this subscription. If your limits are not refilled in a minute, retry this reset.",
            409,
        )),
        _ => Err(unknown_outcome()),
    }
}

/// A Claude reset call as the Claude Code the core presents itself as.
fn claude_code(method: &'static str, url: String) -> UpstreamRequest {
    let mut request = UpstreamRequest::claude(method, url);
    request
        .header
        .push(("User-Agent", CLAUDE_CODE_USER_AGENT.to_owned()));
    request
}

fn unknown_outcome() -> AppError {
    AppError::new("Reset outcome is unknown. Retry the same reset.", 502)
}

/// Whose resets a subscription redeems.
enum Seat {
    /// The `ChatGPT` account id.
    Codex(String),
    /// The Claude seat: a user's account in an organization.
    Claude {
        organization: String,
        account: String,
    },
}

/// A subscription that can redeem resets.
struct Subscription {
    auth_index: String,
    seat: Seat,
    /// The provider's name in messages.
    label: String,
}

impl Subscription {
    /// What redemptions run one at a time for and keep their request ids under: the seat.
    fn key(&self) -> String {
        match &self.seat {
            Seat::Codex(identity) => identity.clone(),
            Seat::Claude {
                organization,
                account,
            } => format!("claude:{account}:{organization}"),
        }
    }

    /// The request that lists the subscription's resets.
    fn listing(&self) -> UpstreamRequest {
        match &self.seat {
            Seat::Codex(identity) => UpstreamRequest::codex(
                "GET",
                format!("{CODEX_API}/rate-limit-reset-credits"),
                Some(identity),
            ),
            Seat::Claude { .. } => claude_code(
                "GET",
                format!("{CLAUDE_API}/oauth/usage?{CLAUDE_PROGRAM}=1&skip_spend=1"),
            ),
        }
    }

    /// The request that redeems `credit`; the upstream redeems it once for `request_id`.
    fn redemption(&self, credit: &str, request_id: &str) -> UpstreamRequest {
        let (mut request, data) = match &self.seat {
            Seat::Codex(identity) => (
                UpstreamRequest::codex(
                    "POST",
                    format!("{CODEX_API}/rate-limit-reset-credits/consume"),
                    Some(identity),
                ),
                json!({ "credit_id": credit, "redeem_request_id": request_id }),
            ),
            Seat::Claude { organization, .. } => (
                claude_code(
                    "POST",
                    format!("{CLAUDE_API}/organizations/{organization}/reset_rate_limits"),
                ),
                json!({ "program": CLAUDE_PROGRAM, "grant_id": credit, "request_id": request_id }),
            ),
        };
        request.data = Some(data.to_string());
        request
    }
}

/// Redemptions, one at a time per seat, through `source`'s cores; a redemption refreshes the
/// account's quota through `quotas`.
pub struct Resets<S = PoolSource, Q = Quotas<PoolSource>> {
    store: Store,
    source: S,
    quotas: Arc<Q>,
    redeeming: Exclusive<String>,
}

impl<S: QuotaSource, Q: RefreshQuota> Resets<S, Q> {
    #[must_use]
    pub fn new(store: Store, source: S, quotas: Arc<Q>) -> Self {
        Self {
            store,
            source,
            quotas,
            redeeming: Exclusive::new(),
        }
    }

    async fn subscription(
        &self,
        profile: &ProfileId,
        account_id: &AccountId,
    ) -> Result<Subscription, QuotaError> {
        let source = &self.source;
        if !source.usable(profile) {
            return Err(AppError::not_started().into());
        }
        let account = source
            .accounts(profile)
            .await?
            .into_iter()
            .find(|account| account.kind == AccountKind::OAuth && &account.id == account_id)
            .ok_or_else(AppError::account_not_found)?;
        let provider = Provider::from_id(&account.provider)
            .filter(|provider| registry().providers.get(*provider).banked_resets)
            .ok_or_else(|| {
                AppError::new("Banked resets require a Claude or Codex subscription.", 400)
            })?;
        let label = registry().label(&account.provider);
        let seat = match provider {
            Provider::Claude => account.organization_uuid.zip(account.account_uuid).map(
                |(organization, account)| Seat::Claude {
                    organization,
                    account,
                },
            ),
            _ => source
                .chatgpt_account_id(profile, account_id)
                .filter(|id| !id.is_empty())
                .map(Seat::Codex),
        };
        match (seat, account.auth_index.filter(|index| !index.is_empty())) {
            (Some(seat), Some(auth_index)) => Ok(Subscription {
                auth_index,
                seat,
                label,
            }),
            _ => Err(AppError::new(
                "Reconnect this subscription to verify its account identity.",
                409,
            )
            .into()),
        }
    }

    /// Sends `request` as the subscription and answers the unwrapped body of its 200.
    async fn send(
        &self,
        profile: &ProfileId,
        subscription: &Subscription,
        request: &UpstreamRequest,
    ) -> Result<Value, QuotaError> {
        let label = &subscription.label;
        let answer = call_upstream(
            &self.source,
            profile,
            Some(&subscription.auth_index),
            request,
        )
        .await?;
        if answer.status != Some(200.0) {
            return Err(AppError::new(
                format!(
                    "{label} returned HTTP {}. Retry the same reset; reconnect if authorization has expired.",
                    status_text(answer.status)
                ),
                502,
            )
            .into());
        }
        js::unwrap(&answer.body).map(Cow::into_owned).map_err(|_| {
            AppError::new(
                format!("{label} returned an unreadable reset response. Retry the same reset."),
                502,
            )
            .into()
        })
    }

    /// The subscription's banked resets.
    ///
    /// # Errors
    /// 409 when the profile is stopped or the subscription's identity is unknown, 404 for an
    /// unknown account, 400 for one that is not a Claude or Codex subscription, 502 for an
    /// upstream failure or an answer that does not match the schema.
    pub async fn credits(
        &self,
        profile: &ProfileId,
        account: &AccountId,
    ) -> Result<ResetCredits, QuotaError> {
        let subscription = self.subscription(profile, account).await?;
        let body = self
            .send(profile, &subscription, &subscription.listing())
            .await?;
        match subscription.seat {
            Seat::Codex(_) => reset_credits(&body),
            Seat::Claude { .. } => claude_credits(&body),
        }
        .ok_or_else(|| {
            AppError::new(
                format!("{} reset information is unavailable.", subscription.label),
                502,
            )
            .into()
        })
    }

    /// Redeems `credit_id`, then refreshes the account's quota. A Codex request id is cleared
    /// only when the upstream says nothing was redeemed (`nothing_to_reset`, `no_credit`): a
    /// credit is used once. A Claude grant holds several resets, so any outcome Claude names
    /// clears it and the next reset gets a new one. After an ambiguous answer a retry reuses it.
    ///
    /// # Errors
    /// As [`Resets::credits`], 400 for a Claude grant id Claude would refuse, 409 while another
    /// redemption for the seat runs or Claude is still applying one, and 502 for an unknown
    /// outcome.
    pub async fn consume(
        &self,
        profile: &ProfileId,
        account: &AccountId,
        credit_id: &str,
    ) -> Result<ResetResult, QuotaError> {
        let subscription = self.subscription(profile, account).await?;
        let claude = matches!(subscription.seat, Seat::Claude { .. });
        if claude && !grant_id(credit_id) {
            return Err(AppError::new("This reset is not available.", 400).into());
        }
        let key = subscription.key();
        let Some(_redeeming) = self.redeeming.begin(key.clone()) else {
            return Err(AppError::new(
                "A reset is already in progress for this subscription.",
                409,
            )
            .into());
        };
        let request_id = {
            let (key, credit) = (key.clone(), credit_id.to_owned());
            self.store
                .call(move |db| db.reset_request_id(&key, &credit))
                .await?
        };
        let request = subscription.redemption(credit_id, &request_id);
        let body = self.send(profile, &subscription, &request).await?;
        let code = if claude {
            claude_result(&body)?
        } else {
            reset_result(&body).ok_or_else(unknown_outcome)?
        };
        if claude
            || matches!(
                code,
                ResetResultCode::NothingToReset | ResetResultCode::NoCredit
            )
        {
            let credit = credit_id.to_owned();
            self.store
                .call(move |db| db.clear_reset_request(&key, &credit))
                .await?;
        }
        // The redemption is settled even if a concurrent refresh or an account removal wins.
        let quota_refreshed = self
            .quotas
            .refresh(profile, account)
            .await
            .is_ok_and(|quota| quota.status == QuotaStatus::Available);
        Ok(ResetResult {
            code,
            quota_refreshed,
        })
    }
}
