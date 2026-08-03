use crate::models::{AccountUsageDetail, UsageData, UsageSection};

#[derive(Clone, Debug, Default)]
pub struct UsageSummary {
    pub account_count: usize,
    pub session: UsageSummarySection,
    pub weekly: UsageSummarySection,
}

#[derive(Clone, Debug, Default)]
pub struct UsageSummarySection {
    pub usage: UsageSection,
    pub requests: u64,
    pub tokens: u64,
    pub used_cost: f64,
    pub total_quota: Option<f64>,
}

pub fn summarize(usage: &UsageData) -> UsageSummary {
    UsageSummary {
        account_count: usage.account_details.len(),
        session: summarize_section(&usage.account_details, usage.session.clone(), |detail| {
            (
                detail.session_requests,
                detail.session_tokens,
                detail.session_cost,
            )
        }),
        weekly: summarize_section(&usage.account_details, usage.weekly.clone(), |detail| {
            (
                detail.weekly_requests,
                detail.weekly_tokens,
                detail.weekly_cost,
            )
        }),
    }
}

fn summarize_section(
    details: &[AccountUsageDetail],
    usage: UsageSection,
    values: impl Fn(&AccountUsageDetail) -> (u64, u64, f64),
) -> UsageSummarySection {
    let (requests, tokens, used_cost) = details.iter().fold(
        (0_u64, 0_u64, 0.0_f64),
        |(requests, tokens, cost), detail| {
            let (detail_requests, detail_tokens, detail_cost) = values(detail);
            (
                requests.saturating_add(detail_requests),
                tokens.saturating_add(detail_tokens),
                cost + detail_cost.max(0.0),
            )
        },
    );
    let total_quota = estimate_total_quota(used_cost, usage.percentage);

    UsageSummarySection {
        usage,
        requests,
        tokens,
        used_cost,
        total_quota,
    }
}

pub(crate) fn estimate_total_quota(used_cost: f64, percentage: f64) -> Option<f64> {
    if !used_cost.is_finite() || !percentage.is_finite() || used_cost <= 0.0 || percentage <= 0.0 {
        return None;
    }

    Some(used_cost.max(0.0) * 100.0 / percentage.clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_usage_and_estimates_total_quota() {
        let usage = UsageData {
            session: UsageSection {
                percentage: 25.0,
                resets_at: None,
            },
            weekly: UsageSection {
                percentage: 50.0,
                resets_at: None,
            },
            account_details: vec![
                AccountUsageDetail {
                    session_requests: 3,
                    session_tokens: 1_000,
                    session_cost: 2.0,
                    weekly_requests: 7,
                    weekly_tokens: 2_000,
                    weekly_cost: 6.0,
                    ..Default::default()
                },
                AccountUsageDetail {
                    session_requests: 4,
                    session_tokens: 500,
                    session_cost: 3.0,
                    weekly_requests: 8,
                    weekly_tokens: 3_000,
                    weekly_cost: 4.0,
                    ..Default::default()
                },
            ],
        };

        let summary = summarize(&usage);
        assert_eq!(summary.account_count, 2);
        assert_eq!(summary.session.requests, 7);
        assert_eq!(summary.session.tokens, 1_500);
        assert_eq!(summary.session.used_cost, 5.0);
        assert_eq!(summary.session.total_quota, Some(20.0));
        assert_eq!(summary.weekly.total_quota, Some(20.0));
    }

    #[test]
    fn leaves_quota_unknown_when_percentage_is_zero() {
        assert_eq!(estimate_total_quota(12.0, 0.0), None);
    }
}
