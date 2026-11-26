use std::time::Duration;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const WINDOW_CHOICES: [UsageWindow; 5] = [
    UsageWindow::TrailingDays(7),
    UsageWindow::TrailingDays(30),
    UsageWindow::TrailingDays(90),
    UsageWindow::ThisMonth,
    UsageWindow::LastMonth,
];

pub const MONTHLY_WINDOW: UsageWindow = UsageWindow::Months(12);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum UsageWindow {
    TrailingDays(u32),
    Months(u32),
    ThisMonth,
    LastMonth,
}

impl UsageWindow {
    pub fn bounds(self, today: NaiveDate) -> (NaiveDate, NaiveDate) {
        match self {
            UsageWindow::TrailingDays(days) => (
                today - chrono::Days::new(u64::from(days.saturating_sub(1))),
                today,
            ),
            UsageWindow::Months(months) => (
                first_of_month(today)
                    .checked_sub_months(chrono::Months::new(months.saturating_sub(1)))
                    .unwrap_or_else(|| first_of_month(today)),
                today,
            ),
            UsageWindow::ThisMonth => (first_of_month(today), today),
            UsageWindow::LastMonth => {
                let this_first = first_of_month(today);
                (
                    this_first
                        .checked_sub_months(chrono::Months::new(1))
                        .unwrap_or(this_first),
                    this_first.pred_opt().unwrap_or(today),
                )
            }
        }
    }
}

pub fn first_of_month(day: NaiveDate) -> NaiveDate {
    day.with_day(1).unwrap_or(day)
}

pub fn enumerate_days(since_day: NaiveDate, until_day: NaiveDate) -> Vec<NaiveDate> {
    let mut days = Vec::new();
    let mut cursor = since_day;
    while cursor <= until_day {
        days.push(cursor);
        cursor = cursor + chrono::Days::new(1);
    }
    days
}

pub fn enumerate_months(since_day: NaiveDate, until_day: NaiveDate) -> Vec<NaiveDate> {
    let mut months = Vec::new();
    let mut cursor = first_of_month(since_day);
    let last = first_of_month(until_day);
    while cursor <= last {
        months.push(cursor);
        let Some(next) = cursor.checked_add_months(chrono::Months::new(1)) else {
            break;
        };
        cursor = next;
    }
    months
}

pub fn days_in_month(first_day: NaiveDate) -> u32 {
    first_day
        .checked_add_months(chrono::Months::new(1))
        .and_then(|next| next.pred_opt())
        .map(|last| last.day())
        .unwrap_or(31)
}

use chrono::Datelike as _;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum UsageProvider {
    Claude,
    Codex,
    Antigravity,
}

impl UsageProvider {
    pub const ALL: [UsageProvider; 3] = [
        UsageProvider::Claude,
        UsageProvider::Codex,
        UsageProvider::Antigravity,
    ];

    pub fn label(self) -> &'static str {
        match self {
            UsageProvider::Claude => "Claude Code",
            UsageProvider::Codex => "Codex",
            UsageProvider::Antigravity => "Antigravity",
        }
    }

    pub fn index(self) -> usize {
        match self {
            UsageProvider::Claude => 0,
            UsageProvider::Codex => 1,
            UsageProvider::Antigravity => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TokenTotals {
    pub uncached_input: u64,
    pub cached_input: u64,
    pub cache_creation: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl TokenTotals {
    pub fn total(&self) -> u64 {
        self.uncached_input + self.cached_input + self.cache_creation + self.output
    }

    pub fn add(&mut self, other: &TokenTotals) {
        self.uncached_input += other.uncached_input;
        self.cached_input += other.cached_input;
        self.cache_creation += other.cache_creation;
        self.output += other.output;
        self.reasoning += other.reasoning;
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum PricingStatus {
    Fresh,
    Cached,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSlice {
    pub provider: UsageProvider,
    pub cost_usd: f64,
    pub total_tokens: u64,
    pub cost_share: f64,
    pub token_share: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ModelSlice {
    pub provider: UsageProvider,
    pub model: String,
    pub cost_usd: f64,
    pub total_tokens: u64,
    pub cost_share: f64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDay {
    pub cost_usd: f64,
    pub total_tokens: u64,
}

/// Decodes a per-provider breakdown indexed by [`UsageProvider::index`].
/// Payloads written before Antigravity joined carry two entries; missing
/// providers decode as zero and entries for providers this build does not
/// know are dropped, so neither an older nor a newer array length is fatal.
fn deserialize_provider_days<'de, D>(deserializer: D) -> Result<[ProviderDay; 3], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let days = Vec::<ProviderDay>::deserialize(deserializer)?;
    let mut by_provider = [ProviderDay::default(); 3];
    for (slot, day) in by_provider.iter_mut().zip(days) {
        *slot = day;
    }
    Ok(by_provider)
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DaySlice {
    pub day: NaiveDate,
    pub cost_usd: f64,
    pub total_tokens: u64,
    #[serde(deserialize_with = "deserialize_provider_days")]
    pub by_provider: [ProviderDay; 3],
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CostQuality {
    pub provider_reported_share: f64,
    pub model_priced_share: f64,
    pub unpriced_share: f64,
    pub cache_savings_usd: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct MonthSlice {
    pub first_day: NaiveDate,
    pub cost_usd: f64,
    pub total_tokens: u64,
    #[serde(deserialize_with = "deserialize_provider_days")]
    pub by_provider: [ProviderDay; 3],
    pub sessions: u64,
    pub active_days: u32,
    pub top_models: Vec<(String, f64)>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSlice {
    pub path: String,
    pub cost_usd: f64,
    pub total_tokens: u64,
    #[serde(deserialize_with = "deserialize_provider_days")]
    pub by_provider: [ProviderDay; 3],
    pub sessions: u64,
    pub cost_share: f64,
    pub last_day: Option<NaiveDate>,
    pub top_models: Vec<(String, f64)>,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistory {
    pub window: UsageWindow,
    pub since_day: NaiveDate,
    pub until_day: NaiveDate,
    pub totals: TokenTotals,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub records: u64,
    pub sessions: u64,
    pub providers: Vec<ProviderSlice>,
    pub models: Vec<ModelSlice>,
    pub daily: Vec<DaySlice>,
    pub months: Vec<MonthSlice>,
    pub projects: Vec<ProjectSlice>,
    pub quality: CostQuality,
    pub pricing: PricingStatus,
    pub scanned_files: usize,
    pub skipped_files: usize,
    pub errors: Vec<String>,
    #[ts(type = "{ secs: number; nanos: number }")]
    pub scan_duration: Duration,
}

impl UsageHistory {
    pub fn day(&self, day: NaiveDate) -> Option<&DaySlice> {
        self.daily
            .binary_search_by_key(&day, |slice| slice.day)
            .ok()
            .map(|index| &self.daily[index])
    }

    pub fn month(&self, first_day: NaiveDate) -> Option<&MonthSlice> {
        self.months
            .binary_search_by_key(&first_day, |slice| slice.first_day)
            .ok()
            .map(|index| &self.months[index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day_json(by_provider: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "day": "2026-01-02",
            "costUsd": 3.0,
            "totalTokens": 30,
            "byProvider": by_provider,
        })
    }

    #[test]
    fn legacy_two_provider_breakdowns_decode_with_zero_antigravity() {
        let legacy = day_json(serde_json::json!([
            { "costUsd": 1.0, "totalTokens": 10 },
            { "costUsd": 2.0, "totalTokens": 20 },
        ]));
        let day: DaySlice = serde_json::from_value(legacy).unwrap();
        assert_eq!(
            day.by_provider[UsageProvider::Claude.index()].total_tokens,
            10
        );
        assert_eq!(
            day.by_provider[UsageProvider::Codex.index()].total_tokens,
            20
        );
        assert_eq!(
            day.by_provider[UsageProvider::Antigravity.index()].total_tokens,
            0
        );

        let month: MonthSlice = serde_json::from_value(serde_json::json!({
            "firstDay": "2026-01-01",
            "costUsd": 1.0,
            "totalTokens": 10,
            "byProvider": [{ "costUsd": 1.0, "totalTokens": 10 }],
            "sessions": 1,
            "activeDays": 1,
            "topModels": [],
        }))
        .unwrap();
        assert_eq!(month.by_provider[0].total_tokens, 10);
        assert_eq!(month.by_provider[1].total_tokens, 0);

        let project: ProjectSlice = serde_json::from_value(serde_json::json!({
            "path": "/tmp/p",
            "costUsd": 0.0,
            "totalTokens": 0,
            "byProvider": [],
            "sessions": 0,
            "costShare": 0.0,
            "lastDay": null,
            "topModels": [],
        }))
        .unwrap();
        assert!(project.by_provider.iter().all(|day| day.total_tokens == 0));
    }

    #[test]
    fn provider_breakdowns_serialize_three_entries_and_tolerate_extra_ones() {
        let current = day_json(serde_json::json!([
            { "costUsd": 1.0, "totalTokens": 10 },
            { "costUsd": 2.0, "totalTokens": 20 },
            { "costUsd": 3.0, "totalTokens": 30 },
            { "costUsd": 4.0, "totalTokens": 40 },
        ]));
        let day: DaySlice = serde_json::from_value(current).unwrap();
        assert_eq!(
            day.by_provider[UsageProvider::Antigravity.index()].total_tokens,
            30
        );
        let json = serde_json::to_value(&day).unwrap();
        assert_eq!(json["byProvider"].as_array().unwrap().len(), 3);
        let round_trip: DaySlice = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip.by_provider[2].total_tokens, 30);
    }
}
