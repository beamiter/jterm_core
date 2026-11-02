//! Provider-reported usage only. No price table, character/token estimate or
//! subscription-to-API billing conversion is performed. Codex thread totals
//! replace earlier totals; Claude result records accumulate once per turn.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_REPORTS: usize = 64;
const USD_SCALE: u32 = 18;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AgentUsageSource {
    CodexThreadTotal,
    ClaudeResult,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AgentUsageScope {
    ProviderThread,
    ObservedNativeRun,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentUsage {
    pub source: AgentUsageSource,
    pub scope: AgentUsageScope,
    pub provider_session_id: Option<String>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// Exact decimal sum of reported USD values when every observed record
    /// provided one. None means unavailable, not zero or free execution.
    pub reported_cost_usd: Option<String>,
    pub reports: u64,
    pub partial: bool,
    #[serde(default)]
    report_ids: Vec<String>,
}

impl AgentUsage {
    /// Audited Codex 0.147.0 `thread/tokenUsage/updated` shape:
    /// https://github.com/openai/codex/blob/rust-v0.147.0/codex-rs/app-server-protocol/src/protocol/v2/thread.rs
    /// The caller verifies active thread/turn correlation before retaining it.
    pub(crate) fn codex(params: &Map<String, Value>) -> Option<Self> {
        let total = params.get("tokenUsage")?.get("total")?.as_object()?;
        let id = bounded_identity(params.get("threadId")?)?;
        let usage = Self {
            source: AgentUsageSource::CodexThreadTotal,
            scope: AgentUsageScope::ProviderThread,
            provider_session_id: Some(id),
            input_tokens: unsigned(total, "inputTokens"),
            cached_input_tokens: unsigned(total, "cachedInputTokens"),
            cache_write_input_tokens: unsigned(total, "cacheWriteInputTokens"),
            output_tokens: unsigned(total, "outputTokens"),
            reasoning_output_tokens: unsigned(total, "reasoningOutputTokens"),
            total_tokens: unsigned(total, "totalTokens"),
            reported_cost_usd: None,
            reports: 1,
            partial: false,
            report_ids: Vec::new(),
        };
        usage.has_measurement().then_some(usage)
    }

    pub(crate) fn claude_result(value: &Value) -> Option<Self> {
        if value.get("type").and_then(Value::as_str) != Some("result") {
            return None;
        }
        let counts = value.get("usage").and_then(Value::as_object);
        let count = |key| counts.and_then(|map| unsigned(map, key));
        let cost = value
            .get("total_cost_usd")
            .and_then(Value::as_number)
            .and_then(|number| decimal_atoms(&number.to_string()))
            .map(format_atoms);
        let report_id = value.get("uuid").and_then(bounded_identity);
        let usage = Self {
            source: AgentUsageSource::ClaudeResult,
            scope: AgentUsageScope::ObservedNativeRun,
            provider_session_id: value.get("session_id").and_then(bounded_identity),
            input_tokens: count("input_tokens"),
            cached_input_tokens: count("cache_read_input_tokens"),
            cache_write_input_tokens: count("cache_creation_input_tokens"),
            output_tokens: count("output_tokens"),
            reasoning_output_tokens: None,
            // Claude result usage may omit a total; cached input semantics
            // differ from Codex, so do not synthesize one by adding fields.
            total_tokens: count("total_tokens"),
            reported_cost_usd: cost,
            reports: 1,
            partial: false,
            report_ids: report_id.into_iter().collect(),
        };
        usage.has_measurement().then_some(usage)
    }

    fn has_measurement(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cached_input_tokens.is_some()
            || self.cache_write_input_tokens.is_some()
            || self.reasoning_output_tokens.is_some()
            || self.total_tokens.is_some()
            || self.reported_cost_usd.is_some()
    }

    /// Add a Claude result exactly once. A process-local turn ID is the
    /// fallback when this provider version omits a result UUID. Bounded IDs
    /// remain in the durable snapshot, so repeated observations never add cost.
    pub(crate) fn accumulate(&mut self, mut next: Self, fallback_id: &str) -> bool {
        if self.source != AgentUsageSource::ClaudeResult
            || next.source != self.source
            || self.scope != next.scope
            || self.provider_session_id != next.provider_session_id
        {
            self.partial = true;
            return false;
        }
        next.ensure_report_id(fallback_id);
        if next
            .report_ids
            .iter()
            .any(|id| self.report_ids.contains(id))
        {
            return false;
        }
        if self.report_ids.len() >= MAX_REPORTS {
            self.partial = true;
            return false;
        }
        self.input_tokens = add(self.input_tokens, next.input_tokens);
        self.cached_input_tokens = add(self.cached_input_tokens, next.cached_input_tokens);
        self.cache_write_input_tokens =
            add(self.cache_write_input_tokens, next.cache_write_input_tokens);
        self.output_tokens = add(self.output_tokens, next.output_tokens);
        self.reasoning_output_tokens =
            add(self.reasoning_output_tokens, next.reasoning_output_tokens);
        self.total_tokens = add(self.total_tokens, next.total_tokens);
        self.reported_cost_usd = self
            .reported_cost_usd
            .as_deref()
            .zip(next.reported_cost_usd.as_deref())
            .and_then(|(a, b)| decimal_atoms(a)?.checked_add(decimal_atoms(b)?))
            .map(format_atoms);
        self.reports = self.reports.saturating_add(1);
        self.partial |= next.partial;
        self.report_ids.extend(next.report_ids);
        true
    }

    pub(crate) fn ensure_report_id(&mut self, fallback_id: &str) {
        if self.report_ids.is_empty() {
            self.report_ids.push(format!("local-turn:{fallback_id}"));
        }
    }

    pub fn summary(&self) -> String {
        let count = |value: Option<u64>| {
            value
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unavailable".into())
        };
        let source = match self.source {
            AgentUsageSource::CodexThreadTotal => "Codex reported",
            AgentUsageSource::ClaudeResult => "Claude reported",
        };
        let scope = match self.scope {
            AgentUsageScope::ProviderThread => "thread total",
            AgentUsageScope::ObservedNativeRun => "observed native run",
        };
        let cost = self
            .reported_cost_usd
            .as_deref()
            .map(|value| format!("USD {value}"))
            .unwrap_or_else(|| "cost unavailable".into());
        format!(
            "{source} · {scope} · input {} · output {} · {cost}{}",
            count(self.input_tokens),
            count(self.output_tokens),
            if self.partial { " · partial" } else { "" }
        )
    }
}

fn unsigned(map: &Map<String, Value>, key: &str) -> Option<u64> {
    map.get(key).and_then(Value::as_u64)
}
fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    a.zip(b).and_then(|(a, b)| a.checked_add(b))
}
fn bounded_identity(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    (!text.is_empty() && text.len() <= 512 && crate::review_input::validate(text).is_ok())
        .then(|| text.to_owned())
}

/// Exact decimal arithmetic at up to 18 fractional places; unrepresentable or
/// overflowing provider numbers remain unknown instead of being rounded.
fn decimal_atoms(text: &str) -> Option<u128> {
    if text.is_empty() || text.starts_with('-') || text.len() > 80 {
        return None;
    }
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(index) => (&text[..index], text[index + 1..].parse::<i32>().ok()?),
        None => (text, 0),
    };
    if exponent.unsigned_abs() > 64 {
        return None;
    }
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}").parse::<u128>().ok()?;
    let power = i64::from(USD_SCALE) + i64::from(exponent) - i64::try_from(fraction.len()).ok()?;
    if power >= 0 {
        digits.checked_mul(10u128.checked_pow(u32::try_from(power).ok()?)?)
    } else {
        let divisor = 10u128.checked_pow(u32::try_from(-power).ok()?)?;
        (digits % divisor == 0).then_some(digits / divisor)
    }
}
fn format_atoms(value: u128) -> String {
    let divisor = 10u128.pow(USD_SCALE);
    let fraction = value % divisor;
    if fraction == 0 {
        return (value / divisor).to_string();
    }
    let fraction = format!("{fraction:018}");
    format!("{}.{}", value / divisor, fraction.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn codex_cumulative_snapshot_is_reported_not_inferred_or_billed() {
        let params = json!({"threadId":"fixture-thread","turnId":"turn-1","tokenUsage":{"total":{
            "totalTokens":120,"inputTokens":100,"cachedInputTokens":60,"cacheWriteInputTokens":5,
            "outputTokens":20,"reasoningOutputTokens":10}}});
        let value = AgentUsage::codex(params.as_object().unwrap()).unwrap();
        assert_eq!(value.total_tokens, Some(120));
        assert_eq!(value.reported_cost_usd, None);
        assert_eq!(value.scope, AgentUsageScope::ProviderThread);
        assert!(value.summary().contains("cost unavailable"));
    }
    #[test]
    fn claude_reports_accumulate_once_and_do_not_turn_missing_fields_into_zero() {
        let make = |uuid: &str, input: u64, output: u64, cost: f64| {
            AgentUsage::claude_result(&json!({
            "type":"result","uuid":uuid,"session_id":"fixture-session","total_cost_usd":cost,
            "usage":{"input_tokens":input,"output_tokens":output}}))
            .unwrap()
        };
        let mut total = make("a", 10, 5, 0.01);
        assert!(total.accumulate(make("b", 20, 7, 0.02), "turn-b"));
        assert!(!total.accumulate(make("b", 20, 7, 0.02), "turn-b"));
        assert_eq!(total.input_tokens, Some(30));
        assert_eq!(total.output_tokens, Some(12));
        assert_eq!(total.total_tokens, None);
        assert_eq!(total.cached_input_tokens, None);
        assert_eq!(total.reported_cost_usd.as_deref(), Some("0.03"));
        let no_cost = AgentUsage::claude_result(&json!({"type":"result","uuid":"c","session_id":"fixture-session","usage":{"input_tokens":1}})).unwrap();
        assert!(total.accumulate(no_cost, "turn-c"));
        assert_eq!(total.reported_cost_usd, None);
        assert_eq!(total.output_tokens, None);
    }
    #[test]
    fn malformed_negative_and_unrepresentable_usage_is_not_displayed_as_zero() {
        assert!(
            AgentUsage::claude_result(&json!({"type":"assistant","usage":{"input_tokens":1}}))
                .is_none()
        );
        assert!(AgentUsage::claude_result(
            &json!({"type":"result","usage":{"input_tokens":-1},"total_cost_usd":-0.1})
        )
        .is_none());
        assert_eq!(
            decimal_atoms("1.25e-3").map(format_atoms).as_deref(),
            Some("0.00125")
        );
        assert!(decimal_atoms("1e-20").is_none());
        assert!(decimal_atoms("1e100").is_none());
    }
}
