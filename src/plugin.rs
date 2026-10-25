// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Search-provider plugin contract shared by configuration and discovery.
//! See PLUGINS.md.

/// Plugin providers must not reuse these names.
pub const BUILT_IN_PROVIDERS: [&str; 17] = [
  "you",
  "exa",
  "exa_mcp",
  "tavily",
  "tavily_mcp",
  "firecrawl",
  "firecrawl_mcp",
  "parallel",
  "parallel_mcp",
  "brave",
  "brightdata",
  "diffbot",
  "serpapi",
  "context",
  "purili",
  "hacker_news",
  "wikipedia",
];

/// Bounds on provider metadata, whether declared by the plugin or configured by
/// the operator. Counts must also be positive. The request quota ledger holds
/// at most 10,000 requests.
pub const MAX_REQUESTS_PER_MINUTE: u32 = 1_000;
pub const MAX_CONCURRENT: u32 = 64;
pub const MAX_MIN_INTERVAL_MS: u64 = 3_600_000;
pub const MAX_DAILY_QUOTA: u32 = 10_000;
pub const MAX_ROUTING_REQUESTS_PER_DAY: u32 = 1_000_000;

/// Provider names key persistent cooldown and quota ledgers, so they must be
/// filename-safe and distinct from every built-in provider.
pub fn check_provider_name(name: &str) -> Result<(), &'static str> {
  if !(1..=32).contains(&name.len())
    || !name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
  {
    return Err("must be 1 to 32 characters of a-z, 0-9 and _");
  }
  if BUILT_IN_PROVIDERS.contains(&name) {
    return Err("must not be a built-in provider name");
  }
  Ok(())
}
