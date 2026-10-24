// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Provider rules, never operator configuration. Keep verification guides in sync.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BudgetResetPolicy {
  /// First of each month; UTC is our accounting convention. The providers
  /// document the calendar day but do not specify a timezone in these sources.
  CalendarMonthUtc,
  /// Replenish only after observing a new upstream billing period.
  ReportedPeriod,
  /// No established calendar boundary. Quota rejections and successful daily
  /// recovery probes govern availability instead of a guessed reset date.
  Unknown,
}

pub fn budget_policy(provider: &str) -> BudgetResetPolicy {
  match provider {
    // https://exa.ai/docs/admin/pricing
    // https://help.tavily.com/articles/3116968635-when-do-my-monthly-api-credits-reset
    // https://github.com/brightdata/brightdata-mcp#pricing-and-free-tier
    "exa" | "tavily" | "brightdata" => BudgetResetPolicy::CalendarMonthUtc,
    // https://docs.firecrawl.dev/api-reference/endpoint/credit-usage
    "firecrawl" => BudgetResetPolicy::ReportedPeriod,
    // https://serpapi.com/account-api exposes an account renewal date, but
    // this adapter does not fetch it and the exact reset timezone is unknown.
    "serpapi" => BudgetResetPolicy::Unknown,
    // Context.dev refills Free accounts on an account-specific grant-anchor
    // day, not a shared calendar boundary. No account period is fetched here.
    // https://docs.context.dev/account/credits
    "context" => BudgetResetPolicy::Unknown,
    // Diffbot documents billing periods, not their exact boundaries. Brave's
    // rate-limit windows do not establish renewal of its free monetary credits.
    // Parallel documents recurring monthly credits without a reset boundary.
    // These recurring allowances recover through the shared quota cooldown,
    // not an invented calendar reset.
    _ => BudgetResetPolicy::Unknown,
  }
}
