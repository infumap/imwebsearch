// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::BTreeMap;
use std::path::PathBuf;

use once_cell::sync::Lazy;

pub mod file;

static CONFIG: Lazy<Result<SearchConfig, String>> = Lazy::new(SearchConfig::from_file);

pub struct SearchConfig {
  pub provider_tiers: BTreeMap<&'static str, u32>,
  pub exa: Option<MeteredConfig>,
  pub tavily: Option<MeteredConfig>,
  pub diffbot: Option<MeteredConfig>,
  pub firecrawl: Option<MeteredConfig>,
  pub brave: Option<MeteredConfig>,
  pub parallel: Option<MeteredConfig>,
  pub brightdata: Option<MeteredConfig>,
  pub serpapi: Option<MeteredConfig>,
  pub context: Option<MeteredConfig>,
  pub you_max_requests_per_minute: u32,
  pub exa_mcp_max_requests_per_minute: u32,
  pub tavily_mcp_max_requests_per_minute: u32,
  pub firecrawl_mcp_max_requests_per_minute: u32,
  pub parallel_mcp_max_requests_per_minute: u32,
  pub purili_max_requests_per_minute: u32,
  pub hacker_news_max_requests_per_minute: u32,
  /// Last-resort encyclopedia search.
  pub wikipedia_max_requests_per_minute: u32,
  pub wikipedia_language: String,
  pub state_dir: Option<PathBuf>,
}

// Deliberately does not implement Debug: configuration contains credentials.
pub struct MeteredConfig {
  pub api_key: String,
}

/// Runtime search view of the one validated TOML configuration.
pub fn search_config() -> Result<&'static SearchConfig, &'static str> {
  CONFIG.as_ref().map_err(String::as_str)
}

impl SearchConfig {
  /// Registration order for startup reporting. Runtime selection balances
  /// recent usage against soft daily targets within the first eligible tier.
  pub fn provider_preference(&self) -> Vec<&'static str> {
    let mut providers = Vec::new();
    if self.you_max_requests_per_minute > 0 {
      providers.push("you");
    }
    if self.exa_mcp_max_requests_per_minute > 0 {
      providers.push("exa_mcp");
    }
    if self.tavily_mcp_max_requests_per_minute > 0 {
      providers.push("tavily_mcp");
    }
    if self.firecrawl_mcp_max_requests_per_minute > 0 {
      providers.push("firecrawl_mcp");
    }
    if self.parallel_mcp_max_requests_per_minute > 0 {
      providers.push("parallel_mcp");
    }
    if self.exa.as_ref().is_some_and(|exa| !exa.api_key.is_empty()) {
      providers.push("exa");
    }
    if self.tavily.as_ref().is_some_and(|tavily| !tavily.api_key.is_empty()) {
      providers.push("tavily");
    }
    if self.diffbot.is_some() {
      providers.push("diffbot");
    }
    if self.firecrawl.is_some() {
      providers.push("firecrawl");
    }
    if self.brave.is_some() {
      providers.push("brave");
    }
    if self.parallel.is_some() {
      providers.push("parallel");
    }
    if self.brightdata.is_some() {
      providers.push("brightdata");
    }
    if self.serpapi.is_some() {
      providers.push("serpapi");
    }
    if self.context.is_some() {
      providers.push("context");
    }
    if self.purili_max_requests_per_minute > 0 {
      providers.push("purili");
    }
    providers
  }

  fn from_file() -> Result<Self, String> {
    let config = file::current().map_err(str::to_owned)?;
    let providers = &config.providers;
    let fallback_providers = &config.fallback_providers;
    Ok(Self {
      provider_tiers: BTreeMap::from([
        ("you", providers.you.tier.get()),
        ("exa_mcp", providers.exa_mcp.tier.get()),
        ("tavily_mcp", providers.tavily_mcp.tier.get()),
        ("firecrawl_mcp", providers.firecrawl_mcp.tier.get()),
        ("parallel_mcp", providers.parallel_mcp.tier.get()),
        ("exa", providers.exa.tier.get()),
        ("tavily", providers.tavily.tier.get()),
        ("diffbot", providers.diffbot.tier.get()),
        ("firecrawl", providers.firecrawl.tier.get()),
        ("brave", providers.brave.tier.get()),
        ("parallel", providers.parallel.tier.get()),
        ("brightdata", providers.brightdata.tier.get()),
        ("serpapi", providers.serpapi.tier.get()),
        ("context", providers.context.tier.get()),
        ("purili", providers.purili.tier.get()),
      ]),
      exa: metered(&providers.exa.api_key),
      tavily: metered(&providers.tavily.api_key),
      diffbot: metered(&providers.diffbot.api_key),
      firecrawl: metered(&providers.firecrawl.api_key),
      brave: metered(&providers.brave.api_key),
      parallel: metered(&providers.parallel.api_key),
      brightdata: metered(&providers.brightdata.api_key),
      serpapi: metered(&providers.serpapi.api_key),
      context: metered(&providers.context.api_key),
      you_max_requests_per_minute: providers.you.max_requests_per_minute,
      exa_mcp_max_requests_per_minute: providers.exa_mcp.max_requests_per_minute,
      tavily_mcp_max_requests_per_minute: providers.tavily_mcp.max_requests_per_minute,
      firecrawl_mcp_max_requests_per_minute: providers.firecrawl_mcp.max_requests_per_minute,
      parallel_mcp_max_requests_per_minute: providers.parallel_mcp.max_requests_per_minute,
      purili_max_requests_per_minute: providers.purili.max_requests_per_minute,
      hacker_news_max_requests_per_minute: fallback_providers.hacker_news.max_requests_per_minute,
      wikipedia_max_requests_per_minute: fallback_providers.wikipedia.max_requests_per_minute,
      wikipedia_language: fallback_providers.wikipedia.language.clone(),
      state_dir: Some(config.state_dir()),
    })
  }
}

fn metered(api_key: &str) -> Option<MeteredConfig> {
  (!api_key.is_empty()).then(|| MeteredConfig { api_key: api_key.to_owned() })
}
