//! Usage records from a core's usage queue, as `usage.ts` stored them: accounting metadata
//! only, never keys, prompts or upstream error bodies.

use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::js::{nullish, number, or, record, string, stringify, text, truthy};
use nsv_domain::sha256_hex;
use nsv_domain::time::iso_string;
use nsv_domain::wire::{PricingTokens, UsageRecord};
use serde_json::Value;

/// Usage records per page of the core's queue; a full page means more are waiting.
pub const PAGE: usize = 500;

/// One page of the core's usage queue as stored rows, `now` (Unix milliseconds) stamping a
/// row without a timestamp.
///
/// # Errors
/// 502 when the page is not an array, 500 when an entry is text that is not JSON.
pub fn parse_page(
    profile: &ProfileId,
    page: &Value,
    now: i64,
) -> Result<Vec<UsageRecord>, AppError> {
    let entries = page
        .as_array()
        .ok_or_else(|| AppError::new("Unexpected usage queue format.", 502))?;
    entries
        .iter()
        .map(|entry| {
            // Some management endpoints wrap a JSON document inside a JSON string.
            let parsed;
            let entry = match entry {
                Value::String(wrapped) => {
                    parsed = serde_json::from_str::<Value>(wrapped).map_err(|error| {
                        AppError::new(format!("JSON Parse error: {error}"), 500)
                    })?;
                    &parsed
                }
                other => other,
            };
            Ok(usage_record(profile, entry, now))
        })
        .collect()
}

fn usage_record(profile: &ProfileId, entry: &Value, now: i64) -> UsageRecord {
    let r = record(Some(entry));
    let tokens = record(r.get("tokens"));
    let fail = record(r.get("fail"));
    let counter = |key: &str| whole(number(tokens.get(key)).unwrap_or(0.0));
    let request = or(r.get("request_id"), None).map_or_else(
        || sha256_hex(stringify(&Value::Object(r.clone())).as_bytes()),
        |value| string(Some(value)).into_owned(),
    );
    let failed = r.get("failed");
    let status = number(fail.get("status_code"))
        .filter(|status| *status != 0.0)
        .unwrap_or(if truthy(failed) { 502.0 } else { 200.0 });
    UsageRecord {
        id: format!("{}:{request}", profile.as_str()),
        profile_id: profile.clone(),
        timestamp: text(r.get("timestamp")).map_or_else(|| iso_string(now), str::to_owned),
        provider: nullish(r.get("provider")).map_or_else(
            || "unknown".to_owned(),
            |value| string(Some(value)).into_owned(),
        ),
        model: or(r.get("alias"), r.get("model"))
            .filter(|model| truthy(Some(model)))
            .map_or_else(
                || "unknown".to_owned(),
                |value| string(Some(value)).into_owned(),
            ),
        upstream_model: text(r.get("model")).map(str::to_owned),
        pricing_tokens: Some(pricing_tokens(r.get("token_breakdown"))),
        account: nullish(r.get("auth_index"))
            .map_or_else(String::new, |value| string(Some(value)).into_owned()),
        input_tokens: counter("input_tokens"),
        output_tokens: counter("output_tokens"),
        cached_tokens: whole(
            number(tokens.get("cache_read_tokens"))
                .unwrap_or_else(|| number(tokens.get("cached_tokens")).unwrap_or(0.0).max(0.0)),
        ),
        reasoning_tokens: counter("reasoning_tokens"),
        cache_write_tokens: counter("cache_creation_tokens"),
        total_tokens: counter("total_tokens"),
        latency_ms: whole(number(r.get("latency_ms")).unwrap_or(0.0)),
        failed: failed == Some(&Value::Bool(true)),
        status_code: status_code(status),
        stream: r.get("stream") == Some(&Value::Bool(true)),
    }
}

/// A counter as the stored row's integer: negatives are 0, fractions are dropped.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a finite non-negative counter; Rust's cast saturates at u64::MAX"
)]
fn whole(value: f64) -> u64 {
    value.max(0.0) as u64
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "an HTTP status; anything outside u16 saturates"
)]
fn status_code(value: f64) -> u16 {
    value as u16
}

/// The provider's own token breakdown when it is complete and adds up; `None` (unknown, never
/// free) otherwise.
#[must_use]
#[expect(clippy::float_cmp, reason = "zod's z.literal compares numbers exactly")]
pub fn pricing_tokens(value: Option<&Value>) -> Option<PricingTokens> {
    let breakdown = value?.as_object()?;
    let counter = |parent: &serde_json::Map<String, Value>, key: &str| -> Option<u64> {
        // z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER)
        let number = parent.get(key)?.as_number()?;
        let value = number.as_f64()?;
        let safe = 9_007_199_254_740_991.0;
        (value.fract() == 0.0 && (0.0..=safe).contains(&value)).then(|| whole(value))
    };
    let schema = breakdown.get("schema_version")?.as_f64()?;
    let unclassified = breakdown.get("unclassified_tokens")?.as_f64()?;
    if schema != 2.0 || breakdown.get("quality")?.as_str()? != "complete" || unclassified != 0.0 {
        return None;
    }
    let total = counter(breakdown, "total_tokens")?;
    let input = breakdown.get("input")?.as_object()?;
    let output = breakdown.get("output")?.as_object()?;
    let input_total = counter(input, "total_tokens")?;
    let tokens = PricingTokens {
        input: counter(input, "uncached_tokens")?,
        cached: counter(input, "cache_read_tokens")?,
        cache_write: counter(input, "cache_write_tokens")?,
        output: counter(output, "non_reasoning_tokens")?,
        reasoning: counter(output, "reasoning_tokens")?,
    };
    let output_total = counter(output, "total_tokens")?;
    let adds_up = input_total
        == tokens
            .input
            .checked_add(tokens.cached)?
            .checked_add(tokens.cache_write)?
        && output_total == tokens.output.checked_add(tokens.reasoning)?
        && total == input_total.checked_add(output_total)?;
    adds_up.then_some(tokens)
}
