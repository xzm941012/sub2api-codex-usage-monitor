use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use std::os::windows::process::CommandExt;

use crate::diagnose;
use crate::localization::Strings;
use crate::models::{AccountUsageDetail, AppUsageData, UsageData, UsageSection};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CREATE_NO_WINDOW: u32 = 0x08000000;

const MODEL_FALLBACK_CHAIN: &[&str] = &["claude-3-haiku-20240307", "claude-haiku-4-5-20251001"];

#[derive(Debug)]
pub enum PollError {
    AuthRequired,
    NoCredentials,
    TokenExpired,
    RequestFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialWatchMode {
    ActiveSource,
    AllSources,
}

pub type CredentialWatchSnapshot = Vec<String>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageMode {
    Codex,
    Sub2api,
}

impl Default for UsageMode {
    fn default() -> Self {
        Self::Codex
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Sub2apiConfig {
    pub base_url: String,
    pub admin_api_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<i64>,
    #[serde(default)]
    pub group_name: String,
    #[serde(default = "default_sub2api_platform")]
    pub platform: String,
    #[serde(default = "default_sub2api_account_type")]
    pub account_type: String,
    #[serde(default)]
    pub force_active_usage: bool,
}

#[derive(Clone, Debug)]
pub struct Sub2apiGroup {
    pub id: i64,
    pub name: String,
}

fn default_sub2api_platform() -> String {
    "openai".to_string()
}

fn default_sub2api_account_type() -> String {
    "oauth".to_string()
}

pub fn fetch_sub2api_groups(config: &Sub2apiConfig) -> Result<Vec<Sub2apiGroup>, PollError> {
    let base_url = config.base_url.trim().trim_end_matches('/');
    let admin_api_key = config.admin_api_key.trim();
    if base_url.is_empty() || admin_api_key.is_empty() {
        return Err(PollError::NoCredentials);
    }

    let agent = build_agent()?;
    let path = format!(
        "/api/v1/admin/groups/all?platform={}",
        url_encode_component(&config.platform)
    );
    let data = sub2api_get(&agent, base_url, admin_api_key, &path)?;
    let groups = data.as_array().ok_or(PollError::RequestFailed)?;
    let mut result = Vec::new();
    for group in groups {
        let Some(id) = group.get("id").and_then(|v| v.as_i64()) else {
            continue;
        };
        let name = group
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("#{id}"));
        result.push(Sub2apiGroup { id, name });
    }
    result.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(result)
}

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
}

#[derive(Deserialize)]
struct UsageBucket {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokenData>,
}

#[derive(Clone, Deserialize)]
struct CodexTokenData {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: Option<Option<Box<CodexRateLimitDetails>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitDetails {
    primary_window: Option<Option<Box<CodexRateLimitWindow>>>,
    secondary_window: Option<Option<Box<CodexRateLimitWindow>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitWindow {
    used_percent: f64,
    reset_at: i64,
}

#[derive(Deserialize)]
struct Sub2apiEnvelope {
    code: i64,
    data: Option<serde_json::Value>,
    message: Option<String>,
}

pub fn poll_with_mode(
    show_claude_code: bool,
    show_codex: bool,
    usage_mode: UsageMode,
    sub2api_config: Option<&Sub2apiConfig>,
) -> Result<AppUsageData, PollError> {
    let mut data = AppUsageData::default();

    if show_claude_code {
        data.claude_code = Some(poll_claude_code()?);
    }

    if show_codex {
        let codex_result = match usage_mode {
            UsageMode::Codex => poll_codex(),
            UsageMode::Sub2api => sub2api_config
                .ok_or(PollError::NoCredentials)
                .and_then(poll_sub2api_codex),
        };
        match codex_result {
            Ok(codex) => data.codex = Some(codex),
            Err(error) if !show_claude_code => return Err(error),
            Err(error) => diagnose::log(format!("Codex usage poll failed: {error:?}")),
        }
    }

    if data.claude_code.is_none() && data.codex.is_none() {
        Err(PollError::RequestFailed)
    } else {
        Ok(data)
    }
}

fn poll_sub2api_codex(config: &Sub2apiConfig) -> Result<UsageData, PollError> {
    poll_sub2api_codex_with_usage(config)
}

pub fn poll_sub2api_codex_quick(config: &Sub2apiConfig) -> Result<UsageData, PollError> {
    let base_url = config.base_url.trim().trim_end_matches('/');
    let admin_api_key = config.admin_api_key.trim();
    if base_url.is_empty() || admin_api_key.is_empty() {
        diagnose::log("Sub2API poll failed: missing base_url or admin_api_key");
        return Err(PollError::NoCredentials);
    }

    let agent = build_agent()?;
    diagnose::log("Sub2API poll: fetching accounts");
    let accounts = fetch_sub2api_accounts(&agent, base_url, admin_api_key, config)?;
    if accounts.is_empty() {
        diagnose::log("Sub2API poll failed: no accounts returned for configured filters");
        return Err(PollError::NoCredentials);
    }

    usage_data_from_accounts(
        accounts
            .iter()
            .filter_map(|account| account.cached_usage.as_ref().map(|usage| (account, usage))),
    )
}

pub fn poll_sub2api_codex_with_usage(config: &Sub2apiConfig) -> Result<UsageData, PollError> {
    let base_url = config.base_url.trim().trim_end_matches('/');
    let admin_api_key = config.admin_api_key.trim();
    if base_url.is_empty() || admin_api_key.is_empty() {
        diagnose::log("Sub2API poll failed: missing base_url or admin_api_key");
        return Err(PollError::NoCredentials);
    }

    let agent = build_agent()?;
    diagnose::log("Sub2API poll: fetching accounts");
    let accounts = fetch_sub2api_accounts(&agent, base_url, admin_api_key, config)?;
    if accounts.is_empty() {
        diagnose::log("Sub2API poll failed: no accounts returned for configured filters");
        return Err(PollError::NoCredentials);
    }

    diagnose::log(format!(
        "Sub2API poll: fetching account usage for accounts={}",
        accounts.len()
    ));
    let usage_results =
        fetch_sub2api_account_usages_parallel(accounts, base_url, admin_api_key, config.clone());

    usage_data_from_accounts(
        usage_results
            .iter()
            .filter_map(|(account, result)| match result {
                Ok(usage) => Some((account, usage)),
                Err(error) => {
                    diagnose::log(format!(
                        "Sub2API account usage failed for account {}: {error:?}",
                        account.id
                    ));
                    account.cached_usage.as_ref().map(|usage| (account, usage))
                }
            }),
    )
}

pub fn poll_sub2api_codex_usage_for_details(
    config: &Sub2apiConfig,
    base: &UsageData,
) -> Result<UsageData, PollError> {
    let base_url = config.base_url.trim().trim_end_matches('/');
    let admin_api_key = config.admin_api_key.trim();
    if base_url.is_empty() || admin_api_key.is_empty() {
        diagnose::log("Sub2API usage detail poll failed: missing base_url or admin_api_key");
        return Err(PollError::NoCredentials);
    }

    let accounts: Vec<Sub2apiAccountSummary> = base
        .account_details
        .iter()
        .map(|detail| Sub2apiAccountSummary {
            id: detail.id,
            name: detail.name.clone(),
            cached_usage: Some(Sub2apiUsageParts {
                session: Some(detail.session.clone()),
                weekly: Some(detail.weekly.clone()),
                session_stats: None,
                weekly_stats: None,
            }),
        })
        .collect();
    if accounts.is_empty() {
        return Err(PollError::RequestFailed);
    }

    diagnose::log(format!(
        "Sub2API poll: fetching account usage for accounts={}",
        accounts.len()
    ));
    let usage_results =
        fetch_sub2api_account_usages_parallel(accounts, base_url, admin_api_key, config.clone());

    usage_data_from_accounts(
        usage_results
            .iter()
            .filter_map(|(account, result)| match result {
                Ok(usage) => Some((account, usage)),
                Err(error) => {
                    diagnose::log(format!(
                        "Sub2API account usage failed for account {}: {error:?}",
                        account.id
                    ));
                    account.cached_usage.as_ref().map(|usage| (account, usage))
                }
            }),
    )
}

#[derive(Default)]
struct Sub2apiUsageParts {
    session: Option<UsageSection>,
    weekly: Option<UsageSection>,
    session_stats: Option<Sub2apiUsageStats>,
    weekly_stats: Option<Sub2apiUsageStats>,
}

struct Sub2apiAccountSummary {
    id: i64,
    name: String,
    cached_usage: Option<Sub2apiUsageParts>,
}

#[derive(Clone, Copy, Default)]
struct Sub2apiUsageStats {
    requests: u64,
    tokens: u64,
    account_cost: f64,
}

fn fetch_sub2api_accounts(
    agent: &ureq::Agent,
    base_url: &str,
    admin_api_key: &str,
    config: &Sub2apiConfig,
) -> Result<Vec<Sub2apiAccountSummary>, PollError> {
    let mut accounts = Vec::new();
    let mut page = 1;
    let page_size = 200;
    let group_id = resolve_sub2api_group_id(agent, base_url, admin_api_key, config)?;

    loop {
        let mut path = format!(
            "/api/v1/admin/accounts?page={page}&page_size={page_size}&platform={}&type={}&sort_by=name&sort_order=asc",
            url_encode_component(&config.platform),
            url_encode_component(&config.account_type)
        );
        if let Some(group_id) = group_id {
            path.push_str("&group=");
            path.push_str(&group_id.to_string());
        }

        let data = sub2api_get(agent, base_url, admin_api_key, &path)?;
        let items = data
            .get("items")
            .and_then(|v| v.as_array())
            .ok_or(PollError::RequestFailed)?;
        for item in items {
            if !sub2api_account_is_normal_or_rate_limited(item) {
                continue;
            }
            if let Some(id) = sub2api_account_id(item) {
                accounts.push(Sub2apiAccountSummary {
                    id,
                    name: sub2api_account_name(item, id),
                    cached_usage: sub2api_account_extra(item).and_then(parse_sub2api_extra_usage),
                });
            }
        }

        let total = data
            .get("total")
            .and_then(|v| v.as_i64())
            .unwrap_or(accounts.len() as i64);
        if accounts.len() as i64 >= total || items.is_empty() {
            break;
        }
        page += 1;
    }

    accounts.sort_by_key(|account| account.id);
    accounts.dedup_by_key(|account| account.id);
    diagnose::log(format!(
        "Sub2API poll: eligible normal/rate-limited accounts={}",
        accounts.len()
    ));
    Ok(accounts)
}

fn resolve_sub2api_group_id(
    agent: &ureq::Agent,
    base_url: &str,
    admin_api_key: &str,
    config: &Sub2apiConfig,
) -> Result<Option<i64>, PollError> {
    let group_name = config.group_name.trim();
    if let Some(group_id) = config.group_id.filter(|id| *id > 0) {
        return Ok(Some(group_id));
    }
    if group_name.is_empty() {
        return Ok(None);
    }

    let path = format!(
        "/api/v1/admin/groups/all?platform={}",
        url_encode_component(&config.platform)
    );
    let data = sub2api_get(agent, base_url, admin_api_key, &path)?;
    let groups = data.as_array().ok_or(PollError::RequestFailed)?;

    for group in groups {
        let name = group.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.eq_ignore_ascii_case(group_name) {
            return group
                .get("id")
                .and_then(|v| v.as_i64())
                .map(Some)
                .ok_or(PollError::RequestFailed);
        }
    }

    diagnose::log(format!("Sub2API group not found by name: {group_name}"));
    Err(PollError::RequestFailed)
}

fn fetch_sub2api_account_usage(
    agent: &ureq::Agent,
    base_url: &str,
    admin_api_key: &str,
    account_id: i64,
    config: &Sub2apiConfig,
) -> Result<Sub2apiUsageParts, PollError> {
    let source = if config.force_active_usage {
        "active"
    } else {
        "passive"
    };
    let force = if config.force_active_usage {
        "&force=true"
    } else {
        ""
    };
    let path = format!("/api/v1/admin/accounts/{account_id}/usage?source={source}{force}");
    let data = sub2api_get(agent, base_url, admin_api_key, &path)?;

    Ok(Sub2apiUsageParts {
        session: parse_sub2api_usage_section(data.get("five_hour")),
        weekly: parse_sub2api_usage_section(data.get("seven_day")),
        session_stats: parse_sub2api_window_stats(data.get("five_hour")),
        weekly_stats: parse_sub2api_window_stats(data.get("seven_day")),
    })
}

fn fetch_sub2api_account_usages_parallel(
    accounts: Vec<Sub2apiAccountSummary>,
    base_url: &str,
    admin_api_key: &str,
    config: Sub2apiConfig,
) -> Vec<(Sub2apiAccountSummary, Result<Sub2apiUsageParts, PollError>)> {
    const MAX_PARALLEL_USAGE_REQUESTS: usize = 8;

    let mut results = Vec::with_capacity(accounts.len());
    let mut handles = Vec::new();
    for account in accounts {
        let base_url = base_url.to_string();
        let admin_api_key = admin_api_key.to_string();
        let config = config.clone();
        handles.push(std::thread::spawn(move || {
            let result = build_agent().and_then(|agent| {
                fetch_sub2api_account_usage(&agent, &base_url, &admin_api_key, account.id, &config)
            });
            (account, result)
        }));

        if handles.len() >= MAX_PARALLEL_USAGE_REQUESTS {
            join_sub2api_usage_threads(&mut handles, &mut results);
        }
    }
    join_sub2api_usage_threads(&mut handles, &mut results);
    results
}

fn join_sub2api_usage_threads(
    handles: &mut Vec<
        std::thread::JoinHandle<(Sub2apiAccountSummary, Result<Sub2apiUsageParts, PollError>)>,
    >,
    results: &mut Vec<(Sub2apiAccountSummary, Result<Sub2apiUsageParts, PollError>)>,
) {
    for handle in handles.drain(..) {
        if let Ok(result) = handle.join() {
            results.push(result);
        }
    }
}

fn sub2api_get(
    agent: &ureq::Agent,
    base_url: &str,
    admin_api_key: &str,
    path: &str,
) -> Result<serde_json::Value, PollError> {
    let url = format!("{base_url}{path}");
    let started = Instant::now();
    let response = agent
        .get(&url)
        .set("Accept", "application/json")
        .set("User-Agent", "claude-code-usage-monitor/sub2api")
        .set("x-api-key", admin_api_key)
        .call()
        .map_err(|error| {
            diagnose::log(format!(
                "Sub2API GET {path} failed after {}ms",
                started.elapsed().as_millis()
            ));
            diagnose::log_error("Sub2API request failed", error);
            PollError::RequestFailed
        })?;

    let envelope: Sub2apiEnvelope = response.into_json().map_err(|error| {
        diagnose::log_error("unable to parse Sub2API response", error);
        PollError::RequestFailed
    })?;
    diagnose::log(format!(
        "Sub2API GET {path} completed in {}ms",
        started.elapsed().as_millis()
    ));

    if envelope.code != 0 {
        diagnose::log(format!(
            "Sub2API returned non-zero code {} message={}",
            envelope.code,
            envelope.message.unwrap_or_default()
        ));
        return Err(PollError::RequestFailed);
    }

    envelope.data.ok_or(PollError::RequestFailed)
}

fn sub2api_account_id(item: &serde_json::Value) -> Option<i64> {
    item.get("account")
        .and_then(|account| account.get("id"))
        .and_then(|id| id.as_i64())
        .or_else(|| item.get("id").and_then(|id| id.as_i64()))
}

fn sub2api_account_is_normal_or_rate_limited(item: &serde_json::Value) -> bool {
    let account = item.get("account").unwrap_or(item);
    let status = account
        .get("status")
        .or_else(|| item.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if status != "active" {
        return false;
    }

    if account_bool(account, item, "schedulable") == Some(false) {
        return false;
    }
    if future_field(account, item, "temp_unschedulable_until") {
        return false;
    }
    if future_field(account, item, "overload_until") {
        return false;
    }
    if quota_exceeded(account) || quota_exceeded(item) {
        return false;
    }

    true
}

fn account_bool(account: &serde_json::Value, item: &serde_json::Value, key: &str) -> Option<bool> {
    account
        .get(key)
        .or_else(|| item.get(key))
        .and_then(|v| v.as_bool())
}

fn future_field(account: &serde_json::Value, item: &serde_json::Value, key: &str) -> bool {
    account
        .get(key)
        .or_else(|| item.get(key))
        .and_then(parse_sub2api_reset_time)
        .and_then(|time| time.duration_since(SystemTime::now()).ok())
        .map(|remaining| !remaining.is_zero())
        .unwrap_or(false)
}

fn quota_exceeded(value: &serde_json::Value) -> bool {
    quota_pair_exceeded(value, "quota_used", "quota_limit")
        || quota_pair_exceeded(value, "quota_daily_used", "quota_daily_limit")
        || quota_pair_exceeded(value, "quota_weekly_used", "quota_weekly_limit")
}

fn quota_pair_exceeded(value: &serde_json::Value, used_key: &str, limit_key: &str) -> bool {
    let Some(limit) = json_number(value.get(limit_key)) else {
        return false;
    };
    if limit <= 0.0 {
        return false;
    }
    json_number(value.get(used_key))
        .map(|used| used >= limit)
        .unwrap_or(false)
}

fn sub2api_account_name(item: &serde_json::Value, id: i64) -> String {
    item.get("account")
        .and_then(|account| account.get("name"))
        .and_then(|name| name.as_str())
        .or_else(|| item.get("name").and_then(|name| name.as_str()))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("#{id}"))
}

fn sub2api_account_extra(item: &serde_json::Value) -> Option<&serde_json::Value> {
    item.get("account")
        .and_then(|account| account.get("extra"))
        .or_else(|| item.get("extra"))
}

fn parse_sub2api_extra_usage(extra: &serde_json::Value) -> Option<Sub2apiUsageParts> {
    let usage = Sub2apiUsageParts {
        session: parse_sub2api_extra_section(
            extra,
            "codex_5h_used_percent",
            "codex_5h_reset_at",
            "codex_5h_reset_after_seconds",
        ),
        weekly: parse_sub2api_extra_section(
            extra,
            "codex_7d_used_percent",
            "codex_7d_reset_at",
            "codex_7d_reset_after_seconds",
        ),
        session_stats: None,
        weekly_stats: None,
    };
    if usage.session.is_some() || usage.weekly.is_some() {
        Some(usage)
    } else {
        None
    }
}

fn parse_sub2api_extra_section(
    extra: &serde_json::Value,
    percent_key: &str,
    reset_at_key: &str,
    reset_after_key: &str,
) -> Option<UsageSection> {
    let percentage = json_number(extra.get(percent_key))?;
    let resets_at = extra
        .get(reset_at_key)
        .and_then(parse_sub2api_reset_time)
        .or_else(|| {
            json_number(extra.get(reset_after_key)).and_then(|seconds| {
                if seconds.is_finite() {
                    Some(SystemTime::now() + Duration::from_secs_f64(seconds.max(0.0)))
                } else {
                    None
                }
            })
        });

    Some(UsageSection {
        percentage,
        resets_at,
    })
}

fn json_number(value: Option<&serde_json::Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|number| number as f64))
        .or_else(|| value.as_u64().map(|number| number as f64))
        .or_else(|| value.as_str()?.trim().parse::<f64>().ok())
}

fn parse_sub2api_usage_section(value: Option<&serde_json::Value>) -> Option<UsageSection> {
    let value = value?;
    if value.is_null() {
        return None;
    }

    let percentage = value
        .get("percentage")
        .or_else(|| value.get("utilization"))
        .or_else(|| value.get("used_percent"))
        .and_then(|v| v.as_f64())?;
    let resets_at = value
        .get("resets_at")
        .or_else(|| value.get("reset_at"))
        .or_else(|| value.get("reset_time"))
        .and_then(parse_sub2api_reset_time);

    Some(UsageSection {
        percentage,
        resets_at,
    })
}

fn parse_sub2api_window_stats(value: Option<&serde_json::Value>) -> Option<Sub2apiUsageStats> {
    let stats = value?.get("window_stats")?;
    Some(Sub2apiUsageStats {
        requests: json_number(stats.get("requests")).unwrap_or(0.0).max(0.0) as u64,
        tokens: json_number(stats.get("tokens")).unwrap_or(0.0).max(0.0) as u64,
        account_cost: json_number(stats.get("cost"))
            .or_else(|| json_number(stats.get("account_cost")))
            .or_else(|| json_number(stats.get("standard_cost")))
            .unwrap_or(0.0),
    })
}

fn build_sub2api_account_detail(
    account: &Sub2apiAccountSummary,
    usage: &Sub2apiUsageParts,
) -> AccountUsageDetail {
    let session_stats = usage.session_stats.unwrap_or_default();
    let weekly_stats = usage.weekly_stats.unwrap_or_default();
    AccountUsageDetail {
        id: account.id,
        name: account.name.clone(),
        session: usage.session.clone().unwrap_or_default(),
        weekly: usage.weekly.clone().unwrap_or_default(),
        session_requests: session_stats.requests,
        session_tokens: session_stats.tokens,
        session_cost: session_stats.account_cost,
        weekly_requests: weekly_stats.requests,
        weekly_tokens: weekly_stats.tokens,
        weekly_cost: weekly_stats.account_cost,
    }
}

fn parse_sub2api_reset_time(value: &serde_json::Value) -> Option<SystemTime> {
    if let Some(unix) = value.as_i64() {
        return unix_to_system_time_auto(Some(unix));
    }
    let text = value.as_str()?.trim();
    if let Ok(unix) = text.parse::<i64>() {
        return unix_to_system_time_auto(Some(unix));
    }
    parse_iso8601(Some(text))
}

fn collect_sub2api_usage(
    usage: &Sub2apiUsageParts,
    session_values: &mut Vec<f64>,
    weekly_values: &mut Vec<f64>,
    session_resets: &mut Vec<SystemTime>,
    weekly_resets: &mut Vec<SystemTime>,
) {
    if let Some(section) = usage.session.as_ref() {
        session_values.push(section.percentage);
        if let Some(reset) = section.resets_at {
            session_resets.push(reset);
        }
    }
    if let Some(section) = usage.weekly.as_ref() {
        weekly_values.push(section.percentage);
        if let Some(reset) = section.resets_at {
            weekly_resets.push(reset);
        }
    }
}

fn usage_data_from_accounts<'a>(
    usages: impl Iterator<Item = (&'a Sub2apiAccountSummary, &'a Sub2apiUsageParts)>,
) -> Result<UsageData, PollError> {
    let mut session_values = Vec::new();
    let mut weekly_values = Vec::new();
    let mut session_resets = Vec::new();
    let mut weekly_resets = Vec::new();
    let mut details = Vec::new();

    for (account, usage) in usages {
        collect_sub2api_usage(
            usage,
            &mut session_values,
            &mut weekly_values,
            &mut session_resets,
            &mut weekly_resets,
        );
        details.push(build_sub2api_account_detail(account, usage));
    }

    if session_values.is_empty() && weekly_values.is_empty() {
        return Err(PollError::RequestFailed);
    }

    Ok(UsageData {
        session: UsageSection {
            percentage: average_percent(&session_values),
            resets_at: nearest_future_time(session_resets),
        },
        weekly: UsageSection {
            percentage: average_percent(&weekly_values),
            resets_at: nearest_future_time(weekly_resets),
        },
        account_details: details,
    })
}

fn average_percent(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn nearest_future_time(values: Vec<SystemTime>) -> Option<SystemTime> {
    let now = SystemTime::now();
    values
        .into_iter()
        .filter(|time| time.duration_since(now).is_ok())
        .min_by_key(|time| time.duration_since(now).unwrap_or_default().as_secs())
}

fn url_encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn poll_claude_code() -> Result<UsageData, PollError> {
    let creds = match read_first_credentials() {
        Some(c) => c,
        None => {
            diagnose::log("poll failed: no Claude credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    let creds = refresh_or_fallback(creds)?;

    fetch_usage_with_fallback(&creds.access_token)
}

fn poll_codex() -> Result<UsageData, PollError> {
    let creds = match read_codex_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Codex usage poll failed: no Codex credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    match fetch_codex_usage(&creds.access_token, creds.account_id.as_deref()) {
        Ok(data) => Ok(data),
        Err(PollError::AuthRequired) => {
            cli_refresh_codex_token();
            let refreshed = read_codex_credentials().ok_or(PollError::TokenExpired)?;
            fetch_codex_usage(&refreshed.access_token, refreshed.account_id.as_deref())
        }
        Err(error) => Err(error),
    }
}

fn refresh_or_fallback(mut creds: Credentials) -> Result<Credentials, PollError> {
    loop {
        if !is_token_expired(creds.expires_at) {
            return Ok(creds);
        }

        let source = creds.source.clone();
        cli_refresh_token(&source);

        match read_credentials_from_source(&source) {
            Some(refreshed) if !is_token_expired(refreshed.expires_at) => return Ok(refreshed),
            Some(_) => diagnose::log(format!(
                "credentials from {source:?} still expired after refresh attempt"
            )),
            None => diagnose::log(format!(
                "credentials from {source:?} unavailable after refresh attempt"
            )),
        }

        match read_next_credentials_after(&source) {
            Some(next) => creds = next,
            None => return Err(PollError::TokenExpired),
        }
    }
}

/// Invoke the Claude CLI with a minimal prompt to force its internal
/// OAuth token refresh.
fn cli_refresh_token(source: &CredentialSource) {
    match source {
        CredentialSource::Windows(_) => cli_refresh_windows_token(),
        CredentialSource::Wsl { distro } => cli_refresh_wsl_token(distro),
    }
}

fn cli_refresh_windows_token() {
    let claude_path = resolve_windows_claude_path();
    let is_cmd = claude_path.to_lowercase().ends_with(".cmd");
    diagnose::log(format!(
        "attempting Windows Claude token refresh via {claude_path}"
    ));

    let args: &[&str] = &["-p", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&claude_path).args(args);
        c
    } else {
        let mut c = Command::new(&claude_path);
        c.args(args);
        c
    };
    cmd.env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Claude token refresh", error);
            return;
        }
    };

    // Wait up to 30 seconds — don't block the poll thread forever
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

fn cli_refresh_wsl_token(distro: &str) {
    diagnose::log(format!(
        "attempting WSL Claude token refresh in distro {distro}"
    ));
    let mut cmd = Command::new("wsl.exe");
    cmd.arg("-d")
        .arg(distro)
        .arg("--")
        .arg("bash")
        .arg("-lic")
        .arg("if command -v claude >/dev/null 2>&1; then claude -p .; elif [ -x \"$HOME/.local/bin/claude\" ]; then \"$HOME/.local/bin/claude\" -p .; else exit 127; fi")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn WSL Claude token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
}

fn cli_refresh_codex_token() {
    let codex_path = resolve_windows_codex_path();
    let is_cmd = codex_path.to_lowercase().ends_with(".cmd");
    let is_ps1 = codex_path.to_lowercase().ends_with(".ps1");
    diagnose::log(format!(
        "attempting Windows Codex token refresh via {codex_path}"
    ));

    let args: &[&str] = &["exec", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&codex_path).args(args);
        c
    } else if is_ps1 {
        let mut c = Command::new("powershell.exe");
        c.arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&codex_path)
            .args(args);
        c
    } else {
        let mut c = Command::new(&codex_path);
        c.args(args);
        c
    };
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Codex token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
}

/// Spawn a command and wait up to `timeout` for it to finish.
/// Returns None if the process fails to start or exceeds the deadline.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

fn wait_for_refresh(child: &mut std::process::Child) {
    // Wait up to 30 seconds; don't block the poll thread forever.
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

/// Resolve the full path to the `claude` CLI executable.
fn resolve_windows_claude_path() -> String {
    for name in &["claude.cmd", "claude"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return name.to_string();
        }
    }

    for name in &["claude.cmd", "claude"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "claude.cmd".to_string()
}

fn resolve_windows_codex_path() -> String {
    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return name.to_string();
        }
    }

    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "codex.cmd".to_string()
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    let tls = native_tls::TlsConnector::new().map_err(|_| PollError::RequestFailed)?;
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

pub fn credential_watch_snapshot(mode: CredentialWatchMode) -> CredentialWatchSnapshot {
    let sources = match mode {
        CredentialWatchMode::ActiveSource => read_first_credentials()
            .map(|creds| vec![creds.source])
            .unwrap_or_else(all_known_credential_sources),
        CredentialWatchMode::AllSources => all_known_credential_sources(),
    };

    let mut snapshot: CredentialWatchSnapshot = sources
        .into_iter()
        .filter_map(|source| credential_watch_signature(&source))
        .collect();
    snapshot.sort();
    snapshot.dedup();
    snapshot
}

fn all_known_credential_sources() -> Vec<CredentialSource> {
    let mut sources = Vec::new();
    if let Some(source) = windows_credential_source() {
        sources.push(source);
    }
    for distro in list_wsl_distros() {
        sources.push(CredentialSource::Wsl { distro });
    }
    sources
}

fn windows_credential_source() -> Option<CredentialSource> {
    let home = dirs::home_dir()?;
    Some(CredentialSource::Windows(
        home.join(".claude").join(".credentials.json"),
    ))
}

fn credential_watch_signature(source: &CredentialSource) -> Option<String> {
    match source {
        CredentialSource::Windows(path) => Some(windows_credential_watch_signature(path)),
        CredentialSource::Wsl { distro } => wsl_credential_watch_signature(distro),
    }
}

fn windows_credential_watch_signature(path: &PathBuf) -> String {
    let key = format!("win:{}", path.display());
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!("{key}|present|{}|{modified}", metadata.len())
        }
        Err(_) => format!("{key}|missing"),
    }
}

fn wsl_credential_watch_signature(distro: &str) -> Option<String> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg(
                "if [ -f ~/.claude/.credentials.json ]; then \
                 stat -c 'present|%s|%Y' ~/.claude/.credentials.json; \
                 else echo missing; fi",
            )
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    let state = if output.status.success() {
        decode_wsl_text(&output.stdout).trim().to_string()
    } else {
        format!("status-{}", output.status)
    };

    Some(format!("wsl:{distro}|{state}"))
}

fn fetch_usage_with_fallback(token: &str) -> Result<UsageData, PollError> {
    // Try the dedicated usage endpoint first
    match try_usage_endpoint(token)? {
        Some(data) => {
            // If reset timers are missing, fill them in from the Messages API
            if data.session.resets_at.is_none() || data.weekly.resets_at.is_none() {
                if let Ok(fallback) = fetch_usage_via_messages(token) {
                    let mut merged = data;
                    if merged.session.resets_at.is_none() {
                        merged.session.resets_at = fallback.session.resets_at;
                    }
                    if merged.weekly.resets_at.is_none() {
                        merged.weekly.resets_at = fallback.weekly.resets_at;
                    }
                    return Ok(merged);
                }
            }
            return Ok(data);
        }
        None => {}
    }

    // Fall back to Messages API with rate limit headers
    let result = fetch_usage_via_messages(token);
    if result.is_err() {
        diagnose::log("usage endpoint and Messages API fallback both failed");
    }
    result
}

fn try_usage_endpoint(token: &str) -> Result<Option<UsageData>, PollError> {
    let agent = build_agent()?;

    let resp = match agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "usage endpoint returned auth error status {code}; re-login required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(_) => return Ok(None),
    };

    let response: UsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    let mut data = UsageData::default();

    if let Some(bucket) = &response.five_hour {
        data.session.percentage = bucket.utilization;
        data.session.resets_at = parse_iso8601(bucket.resets_at.as_deref());
    }

    if let Some(bucket) = &response.seven_day {
        data.weekly.percentage = bucket.utilization;
        data.weekly.resets_at = parse_iso8601(bucket.resets_at.as_deref());
    }

    Ok(Some(data))
}

fn fetch_usage_via_messages(token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent()?;

    for model in MODEL_FALLBACK_CHAIN {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "."}]
        });

        let response = match agent
            .post(MESSAGES_URL)
            .set("Authorization", &format!("Bearer {token}"))
            .set("anthropic-version", "2023-06-01")
            .set("anthropic-beta", "oauth-2025-04-20")
            .send_json(&body)
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
                diagnose::log(format!(
                    "messages endpoint returned auth error status {code}; re-login required"
                ));
                return Err(PollError::AuthRequired);
            }
            Err(ureq::Error::Status(_code, resp)) => resp,
            Err(_) => continue,
        };

        let h5 = response.header("anthropic-ratelimit-unified-5h-utilization");
        let h7 = response.header("anthropic-ratelimit-unified-7d-utilization");
        let hs = response.header("anthropic-ratelimit-unified-status");

        if h5.is_some() || h7.is_some() || hs.is_some() {
            return Ok(parse_rate_limit_headers(&response));
        }
    }

    Err(PollError::RequestFailed)
}

fn parse_rate_limit_headers(response: &ureq::Response) -> UsageData {
    let mut data = UsageData::default();

    data.session.percentage =
        get_header_f64(response, "anthropic-ratelimit-unified-5h-utilization") * 100.0;
    data.session.resets_at = unix_to_system_time(get_header_i64(
        response,
        "anthropic-ratelimit-unified-5h-reset",
    ));

    data.weekly.percentage =
        get_header_f64(response, "anthropic-ratelimit-unified-7d-utilization") * 100.0;
    data.weekly.resets_at = unix_to_system_time(get_header_i64(
        response,
        "anthropic-ratelimit-unified-7d-reset",
    ));

    let overall_reset = get_header_i64(response, "anthropic-ratelimit-unified-reset");

    if data.session.percentage == 0.0 && data.weekly.percentage == 0.0 {
        let status = response.header("anthropic-ratelimit-unified-status");
        if status == Some("rejected") {
            let claim = response.header("anthropic-ratelimit-unified-representative-claim");
            match claim {
                Some("five_hour") => data.session.percentage = 100.0,
                Some("seven_day") => data.weekly.percentage = 100.0,
                _ => {}
            }
        }

        if data.session.resets_at.is_none() && overall_reset.is_some() {
            data.session.resets_at = unix_to_system_time(overall_reset);
        }
    }

    data
}

fn fetch_codex_usage(token: &str, account_id: Option<&str>) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let mut request = agent
        .get(CODEX_USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "codex-cli");

    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    let resp = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Codex usage endpoint returned auth error status {code}; refresh required"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(error) => {
            diagnose::log_error("Codex usage endpoint request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let response: CodexUsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Codex usage response", error);
            return Err(PollError::RequestFailed);
        }
    };

    codex_usage_from_response(response).ok_or(PollError::RequestFailed)
}

fn codex_usage_from_response(response: CodexUsageResponse) -> Option<UsageData> {
    let details = *response.rate_limit.flatten()?;
    let mut data = UsageData::default();

    if let Some(window) = details.primary_window.flatten() {
        data.session = codex_section_from_window(&window);
    }

    if let Some(window) = details.secondary_window.flatten() {
        data.weekly = codex_section_from_window(&window);
    }

    Some(data)
}

fn codex_section_from_window(window: &CodexRateLimitWindow) -> UsageSection {
    UsageSection {
        percentage: window.used_percent,
        resets_at: unix_to_system_time(Some(window.reset_at)),
    }
}

fn get_header_f64(response: &ureq::Response, name: &str) -> f64 {
    response
        .header(name)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn get_header_i64(response: &ureq::Response, name: &str) -> Option<i64> {
    response.header(name).and_then(|s| s.parse::<i64>().ok())
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

fn unix_to_system_time_auto(unix: Option<i64>) -> Option<SystemTime> {
    let value = unix?;
    if value < 0 {
        return None;
    }

    // Sub2API-style payloads may use either Unix seconds or milliseconds.
    let secs = if value >= 1_000_000_000_000 {
        value / 1000
    } else {
        value
    };
    unix_to_system_time(Some(secs))
}

struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
    source: CredentialSource,
}

#[derive(Clone, Debug)]
enum CredentialSource {
    Windows(PathBuf),
    Wsl { distro: String },
}

fn read_first_credentials() -> Option<Credentials> {
    if let Some(creds) = read_windows_credentials() {
        return Some(creds);
    }

    for distro in list_wsl_distros() {
        if let Some(creds) = read_wsl_credentials(&distro) {
            return Some(creds);
        }
    }

    None
}

fn read_windows_credentials() -> Option<Credentials> {
    let CredentialSource::Windows(cred_path) = windows_credential_source()? else {
        return None;
    };
    let content = match std::fs::read_to_string(&cred_path) {
        Ok(content) => content,
        Err(error) => {
            if diagnose::is_enabled() {
                diagnose::log_error(
                    &format!(
                        "unable to read Windows credentials at {}",
                        cred_path.display()
                    ),
                    error,
                );
            }
            return None;
        }
    };
    parse_credentials(&content, CredentialSource::Windows(cred_path))
}

fn read_credentials_from_source(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(path) => {
            let content = std::fs::read_to_string(path).ok()?;
            parse_credentials(&content, source.clone())
        }
        CredentialSource::Wsl { distro } => read_wsl_credentials(distro),
    }
}

fn codex_auth_path() -> Option<PathBuf> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        return Some(codex_home.join("auth.json"));
    }

    Some(dirs::home_dir()?.join(".codex").join("auth.json"))
}

fn read_codex_credentials() -> Option<CodexTokenData> {
    let auth_path = codex_auth_path()?;
    let content = match std::fs::read_to_string(&auth_path) {
        Ok(content) => content,
        Err(error) => {
            diagnose::log_error(
                &format!(
                    "unable to read Codex credentials at {}",
                    auth_path.display()
                ),
                error,
            );
            return None;
        }
    };

    let auth: CodexAuthFile = serde_json::from_str(&content).ok()?;
    auth.tokens.filter(|tokens| !tokens.access_token.is_empty())
}

fn read_wsl_credentials(distro: &str) -> Option<Credentials> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("cat ~/.claude/.credentials.json")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    if !output.status.success() {
        diagnose::log(format!(
            "WSL credentials probe failed for distro {distro} with status {}",
            output.status
        ));
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    parse_credentials(
        &content,
        CredentialSource::Wsl {
            distro: distro.to_string(),
        },
    )
}

fn parse_credentials(content: &str, source: CredentialSource) -> Option<Credentials> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    let oauth = json.get("claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())?
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64());

    Some(Credentials {
        access_token,
        expires_at,
        source,
    })
}

fn read_next_credentials_after(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(_) => {
            for distro in list_wsl_distros() {
                if let Some(creds) = read_wsl_credentials(&distro) {
                    return Some(creds);
                }
            }
        }
        CredentialSource::Wsl { distro } => {
            let mut past_current = false;
            for candidate_distro in list_wsl_distros() {
                if !past_current {
                    past_current = candidate_distro == *distro;
                    continue;
                }
                if let Some(creds) = read_wsl_credentials(&candidate_distro) {
                    return Some(creds);
                }
            }
        }
    }

    None
}

fn list_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Some(output) if output.status.success() => output,
        _ => {
            diagnose::log("unable to enumerate WSL distros");
            return Vec::new();
        }
    };

    let stdout = decode_wsl_text(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Some(decoded) = decode_utf16le(bytes) {
        return decoded;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };

    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(String::from_utf16_lossy(&units))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    if units == 0 {
        return false;
    }

    let nul_high_bytes = bytes[..sample_len]
        .chunks_exact(2)
        .filter(|chunk| chunk[1] == 0)
        .count();

    nul_high_bytes * 2 >= units
}

fn is_token_expired(expires_at: Option<i64>) -> bool {
    let Some(exp) = expires_at else { return false };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    now >= exp
}

/// Parse an ISO 8601 timestamp string into a SystemTime.
fn parse_iso8601(s: Option<&str>) -> Option<SystemTime> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }

    let (datetime_part, offset_secs) = split_iso8601_offset(s)?;
    let local_secs = parse_datetime_to_unix(datetime_part).ok()?;
    let utc_secs = local_secs.checked_sub(offset_secs)?;
    unix_to_system_time(Some(utc_secs))
}

fn split_iso8601_offset(s: &str) -> Option<(&str, i64)> {
    if let Some(datetime) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return Some((datetime, 0));
    }

    let time_start = s.find('T').or_else(|| s.find(' '))?;
    let timezone_pos = s[time_start + 1..]
        .rfind(|c| c == '+' || c == '-')
        .map(|pos| time_start + 1 + pos);

    if let Some(pos) = timezone_pos {
        let datetime = &s[..pos];
        let offset = &s[pos..];
        return parse_timezone_offset(offset).map(|secs| (datetime, secs));
    }

    Some((s, 0))
}

fn parse_timezone_offset(offset: &str) -> Option<i64> {
    let sign = match offset.as_bytes().first().copied()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let body = &offset[1..];
    let (hours, minutes) = if let Some((h, m)) = body.split_once(':') {
        (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)
    } else if body.len() == 4 {
        (
            body[..2].parse::<i64>().ok()?,
            body[2..].parse::<i64>().ok()?,
        )
    } else {
        (body.parse::<i64>().ok()?, 0)
    };

    if hours > 23 || minutes > 59 {
        return None;
    }

    Some(sign * (hours * 3600 + minutes * 60))
}

/// Minimal datetime parser — avoids pulling in chrono/time crates.
fn parse_datetime_to_unix(s: &str) -> Result<i64, ()> {
    // Extract date and time parts from "YYYY-MM-DDTHH:MM:SS[.frac]"
    let (date_str, time_str) = s.split_once('T').or_else(|| s.split_once(' ')).ok_or(())?;
    let date_parts: Vec<&str> = date_str.split('-').collect();
    if date_parts.len() != 3 {
        return Err(());
    }

    let year: i64 = date_parts[0].parse().map_err(|_| ())?;
    let month: i64 = date_parts[1].parse().map_err(|_| ())?;
    let day: i64 = date_parts[2].parse().map_err(|_| ())?;

    // Strip fractional seconds
    let time_base = time_str.split('.').next().unwrap_or(time_str);
    let time_parts: Vec<&str> = time_base.split(':').collect();
    if time_parts.len() != 3 {
        return Err(());
    }

    let hour: i64 = time_parts[0].parse().map_err(|_| ())?;
    let min: i64 = time_parts[1].parse().map_err(|_| ())?;
    let sec: i64 = time_parts[2].parse().map_err(|_| ())?;
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day < 1
        || hour > 23
        || min > 59
        || sec > 59
    {
        return Err(());
    }

    // Days from year (using a simplified calculation for dates after 1970)
    let mut days: i64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    let max_day = month_days[month as usize] + if month == 2 && is_leap(year) { 1 } else { 0 };
    if day > max_day {
        return Err(());
    }
    days += day - 1;

    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Format a usage section as "X% · Yh" style text
pub fn format_line(section: &UsageSection, strings: Strings) -> String {
    let pct = format!("{:.0}%", section.percentage);
    let cd = format_countdown(section.resets_at, strings);
    if cd.is_empty() {
        pct
    } else {
        format!("{pct} \u{00b7} {cd}")
    }
}

fn format_countdown(resets_at: Option<SystemTime>, strings: Strings) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return strings.now.to_string(),
    };

    format_countdown_from_secs(remaining.as_secs(), strings)
}

/// Calculate how long until the display text would change
pub fn time_until_display_change(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;
    Some(time_until_display_change_from_secs(remaining.as_secs()))
}

fn format_countdown_from_secs(total_secs: u64, strings: Strings) -> String {
    let _localized_suffixes = (
        strings.day_suffix,
        strings.hour_suffix,
        strings.minute_suffix,
        strings.second_suffix,
    );
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    if total_days >= 1 {
        let hours = (total_secs % 86400) / 3600;
        if hours > 0 {
            format!("{total_days}d {hours}h")
        } else {
            format!("{total_days}d")
        }
    } else if total_hours >= 1 {
        let mins = (total_secs % 3600) / 60;
        if mins > 0 {
            format!("{total_hours}h {mins}m")
        } else {
            format!("{total_hours}h")
        }
    } else if total_mins >= 1 {
        format!("{total_mins}m")
    } else {
        format!("{total_secs}s")
    }
}

fn time_until_display_change_from_secs(total_secs: u64) -> Duration {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    let current_bucket_start = if total_days >= 1 {
        total_days * 86400 + ((total_secs % 86400) / 3600) * 3600
    } else if total_hours >= 1 {
        total_hours * 3600 + ((total_secs % 3600) / 60) * 60
    } else if total_mins >= 1 {
        total_mins * 60
    } else {
        total_secs
    };

    Duration::from_secs(total_secs.saturating_sub(current_bucket_start) + 1)
}

/// Returns true if either section has reached "now" (reset time has passed).
pub fn is_past_reset(data: &UsageData) -> bool {
    let now = SystemTime::now();
    let past = |s: &UsageSection| matches!(s.resets_at, Some(t) if now.duration_since(t).is_ok());
    past(&data.session) || past(&data.weekly)
}

pub fn app_is_past_reset(data: &AppUsageData) -> bool {
    data.claude_code.as_ref().is_some_and(is_past_reset)
        || data.codex.as_ref().is_some_and(is_past_reset)
}
