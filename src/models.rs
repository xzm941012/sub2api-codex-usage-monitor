use std::time::SystemTime;

#[derive(Clone, Debug, Default)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    pub session: UsageSection,
    pub weekly: UsageSection,
    pub account_details: Vec<AccountUsageDetail>,
}

#[derive(Clone, Debug, Default)]
pub struct AppUsageData {
    pub claude_code: Option<UsageData>,
    pub codex: Option<UsageData>,
}

#[derive(Clone, Debug, Default)]
pub struct AccountUsageDetail {
    pub id: i64,
    pub name: String,
    pub session: UsageSection,
    pub weekly: UsageSection,
    pub session_requests: u64,
    pub session_tokens: u64,
    pub session_cost: f64,
    pub weekly_requests: u64,
    pub weekly_tokens: u64,
    pub weekly_cost: f64,
}
