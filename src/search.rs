// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

mod merge;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{StreamExt, stream};
use log::{debug, warn};
use once_cell::sync::Lazy;
use serde::Serialize;
use tokio::time::Instant;

use crate::allocation::AllocationStore;
use crate::budget::{BudgetStore, BudgetedProvider};
use crate::config;
use crate::error::InfuResult;
pub use crate::providers::SearchResult;
use crate::providers::brave::{self, Brave};
use crate::providers::brightdata::{self, BrightData};
use crate::providers::context::{self, Context};
use crate::providers::diffbot::{self, Diffbot};
use crate::providers::exa::{self, Exa};
use crate::providers::exa_mcp::ExaMcp;
use crate::providers::firecrawl::{self, Firecrawl};
use crate::providers::firecrawl_mcp::{self, FirecrawlMcp};
use crate::providers::hacker_news::HackerNews;
use crate::providers::parallel::{self, Parallel};
use crate::providers::parallel_mcp::{self, ParallelMcp};
#[cfg(unix)]
use crate::providers::plugin::PluginProvider;
use crate::providers::purili::{self, Purili};
use crate::providers::serpapi::{self, SerpApi};
use crate::providers::tavily::{self, Tavily};
use crate::providers::tavily_mcp::TavilyMcp;
use crate::providers::wikipedia::Wikipedia;
use crate::providers::you::You;
use crate::providers::{SearchProvider, SearchRequest};
use crate::router::{Backend, ProviderStatus, SearchLimits, SearchRouter};
use crate::routing::{self, PoolSelection, ProviderPool, SearchTrace};

pub const DEFAULT_MAX_RESULTS: usize = 5;
pub const MAX_RESULTS_CAP: usize = 30;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);
// The general pool is bounded to 30 seconds by the router. Fallback retrieval
// has its own window inside the overall deadline.
const FALLBACK_TIMEOUT: Duration = Duration::from_secs(15);
const FALLBACK_ATTEMPTS_PER_POOL: usize = 1;
const FALLBACK_CONCURRENCY: usize = 2;
const FALLBACK_RESULTS_PER_POOL: usize = 3;

static NEXT_SEARCH_ID: AtomicU64 = AtomicU64::new(1);

/// Encyclopedia retrieval is a fallback source, with independent pacing and
/// health tracking rather than participating in general-web allocation.
static WIKIPEDIA_ROUTER: Lazy<InfuResult<Option<SearchRouter>>> = Lazy::new(|| {
  let config = config::search_config().map_err(|message| format!("Invalid web-search configuration: {message}"))?;
  let Some(rate) = NonZeroU32::new(config.wikipedia_max_requests_per_minute) else { return Ok(None) };
  let provider =
    Wikipedia::new(&config.wikipedia_language).map_err(|error| format!("Could not initialize Wikipedia: {error}"))?;
  Ok(Some(search_router(vec![Backend::Unmetered {
    provider: Box::new(provider),
    requests_per_minute: Some(rate),
    routing_requests_per_day: 100,
  }])?))
});

static HACKER_NEWS_ROUTER: Lazy<InfuResult<Option<SearchRouter>>> = Lazy::new(|| {
  let config = config::search_config().map_err(|message| format!("Invalid web-search configuration: {message}"))?;
  let Some(rate) = NonZeroU32::new(config.hacker_news_max_requests_per_minute) else { return Ok(None) };
  let provider = HackerNews::new(rate).map_err(|error| format!("Could not initialize Hacker News: {error}"))?;
  Ok(Some(search_router(vec![Backend::Unmetered {
    provider: Box::new(provider),
    requests_per_minute: Some(rate),
    routing_requests_per_day: 100,
  }])?))
});

static ROUTER: Lazy<InfuResult<SearchRouter>> = Lazy::new(|| {
  let config = config::search_config().map_err(|message| format!("Invalid web-search configuration: {message}"))?;
  // One store owns the directory lock; metered providers share it while keeping
  // independent provider/account entries in the existing ledger.
  let store = if config.exa.is_some()
    || config.tavily.is_some()
    || config.diffbot.is_some()
    || config.firecrawl.is_some()
    || config.brave.is_some()
    || config.parallel.is_some()
    || config.brightdata.is_some()
    || config.serpapi.is_some()
    || config.context.is_some()
  {
    let directory = config.state_dir.as_ref().ok_or("Budget state directory is not configured.")?;
    Some(BudgetStore::open(directory).map_err(|error| format!("Could not initialize search budgets: {error}"))?)
  } else {
    None
  };
  let mut providers = Vec::new();
  if let Some(rate) = NonZeroU32::new(config.you_max_requests_per_minute) {
    let directory = config.state_dir.as_ref().ok_or("Quota state directory is not configured.")?;
    let provider = You::new(directory, rate).map_err(|error| format!("Could not initialize You.com: {error}"))?;
    providers.push(Backend::Unmetered {
      provider: Box::new(provider),
      requests_per_minute: Some(rate),
      routing_requests_per_day: crate::providers::you::MAX_DAILY_FREE_REQUESTS,
    });
  }
  if let Some(rate) = NonZeroU32::new(config.exa_mcp_max_requests_per_minute) {
    let directory = config.state_dir.as_ref().ok_or("Quota state directory is not configured.")?;
    let provider = ExaMcp::new(directory, rate).map_err(|error| format!("Could not initialize Exa MCP: {error}"))?;
    providers.push(Backend::Unmetered {
      provider: Box::new(provider),
      requests_per_minute: Some(rate),
      routing_requests_per_day: crate::providers::exa_mcp::MAX_DAILY_FREE_REQUESTS,
    });
  }
  if let Some(rate) = NonZeroU32::new(config.tavily_mcp_max_requests_per_minute) {
    let provider = TavilyMcp::new(rate).map_err(|error| format!("Could not initialize Tavily MCP: {error}"))?;
    providers.push(Backend::Unmetered {
      provider: Box::new(provider),
      requests_per_minute: Some(rate),
      routing_requests_per_day: crate::providers::tavily_mcp::ASSUMED_REQUESTS_PER_DAY,
    });
  }
  if let Some(rate) = NonZeroU32::new(config.firecrawl_mcp_max_requests_per_minute) {
    let provider = FirecrawlMcp::new(rate).map_err(|error| format!("Could not initialize Firecrawl MCP: {error}"))?;
    providers.push(Backend::Unmetered {
      provider: Box::new(provider),
      requests_per_minute: Some(rate),
      routing_requests_per_day: firecrawl_mcp::ASSUMED_REQUESTS_PER_DAY,
    });
  }
  if let Some(rate) = NonZeroU32::new(config.parallel_mcp_max_requests_per_minute) {
    let provider = ParallelMcp::new(rate).map_err(|error| format!("Could not initialize Parallel MCP: {error}"))?;
    providers.push(Backend::Unmetered {
      provider: Box::new(provider),
      requests_per_minute: Some(rate),
      routing_requests_per_day: parallel_mcp::ASSUMED_REQUESTS_PER_DAY,
    });
  }
  if let Some(exa) = &config.exa {
    let provider = Exa::new(&exa.api_key)
      .and_then(|adapter| budgeted_provider(adapter, exa::FREE_ALLOWANCE_CENTS, store.as_ref()))
      .map_err(|error| format!("Could not initialize Exa: {error}"))?;
    providers.push(Backend::Metered(provider));
  }
  if let Some(tavily) = &config.tavily {
    let provider = Tavily::new(&tavily.api_key)
      .and_then(|adapter| budgeted_provider(adapter, tavily::FREE_ALLOWANCE_CENTS, store.as_ref()))
      .map_err(|error| format!("Could not initialize Tavily: {error}"))?;
    providers.push(Backend::Metered(provider));
  }
  if let Some(diffbot) = &config.diffbot {
    let provider = Diffbot::new(&diffbot.api_key)
      .and_then(|adapter| budgeted_provider(adapter, diffbot::FREE_ALLOWANCE_CENTS, store.as_ref()))
      .map_err(|error| format!("Could not initialize Diffbot: {error}"))?;
    providers.push(Backend::Metered(provider));
  }
  if let Some(firecrawl) = &config.firecrawl {
    let provider = Firecrawl::new(&firecrawl.api_key)
      .and_then(|adapter| budgeted_provider(adapter, firecrawl::FREE_ALLOWANCE_CENTS, store.as_ref()))
      .map_err(|error| format!("Could not initialize Firecrawl: {error}"))?;
    providers.push(Backend::Metered(provider));
  }
  if let Some(brave) = &config.brave {
    let provider = Brave::new(&brave.api_key)
      .and_then(|adapter| budgeted_provider(adapter, brave::FREE_ALLOWANCE_CENTS, store.as_ref()))
      .map_err(|error| format!("Could not initialize Brave: {error}"))?;
    providers.push(Backend::Metered(provider));
  }
  if let Some(parallel) = &config.parallel {
    let provider = Parallel::new(&parallel.api_key)
      .and_then(|adapter| budgeted_provider(adapter, parallel::FREE_ALLOWANCE_CENTS, store.as_ref()))
      .map_err(|error| format!("Could not initialize Parallel: {error}"))?;
    providers.push(Backend::Metered(provider));
  }
  if let Some(brightdata) = &config.brightdata {
    let provider = BrightData::new(&brightdata.api_key)
      .and_then(|adapter| budgeted_provider(adapter, brightdata::FREE_ALLOWANCE_CENTS, store.as_ref()))
      .map_err(|error| format!("Could not initialize Bright Data: {error}"))?;
    providers.push(Backend::Metered(provider));
  }
  if let Some(serpapi) = &config.serpapi {
    let provider = SerpApi::new(&serpapi.api_key)
      .and_then(|adapter| budgeted_provider(adapter, serpapi::FREE_ALLOWANCE_CENTS, store.as_ref()))
      .map_err(|error| format!("Could not initialize SerpApi: {error}"))?;
    providers.push(Backend::Metered(provider));
  }
  if let Some(context) = &config.context {
    let provider = Context::new(&context.api_key)
      .and_then(|adapter| budgeted_provider(adapter, context::FREE_ALLOWANCE_CENTS, store.as_ref()))
      .map_err(|error| format!("Could not initialize Context.dev: {error}"))?;
    providers.push(Backend::Metered(provider));
  }
  if let Some(rate) = NonZeroU32::new(config.purili_max_requests_per_minute) {
    let provider = Purili::new(rate).map_err(|error| format!("Could not initialize Purili: {error}"))?;
    providers.push(Backend::Unmetered {
      provider: Box::new(provider),
      requests_per_minute: Some(rate),
      routing_requests_per_day: purili::ASSUMED_REQUESTS_PER_DAY,
    });
  }
  #[cfg(unix)]
  if let Some(processes) = crate::plugin::processes() {
    let plugin = &config::file::current()?.plugin;
    let directory = config.state_dir.as_ref().ok_or("Quota state directory is not configured.")?;
    for spec in crate::plugin::providers() {
      // A plugin provider must never prevent built-in search from starting.
      match PluginProvider::new(spec, plugin, Arc::clone(processes), directory) {
        Ok(provider) => providers.push(Backend::Unmetered {
          provider: Box::new(provider),
          requests_per_minute: Some(spec.requests_per_minute),
          routing_requests_per_day: spec.routing_requests_per_day,
        }),
        Err(error) => warn!("Search plugin provider {} skipped: could not initialize: {error}", spec.name),
      }
    }
  }
  search_router(providers)
});

/// Construct each source pool once, keeping router health independent.
static POOLS: Lazy<InfuResult<Vec<ProviderPool<'static>>>> = Lazy::new(|| {
  let mut pools = Vec::new();
  for (name, configured) in [("wikipedia", &*WIKIPEDIA_ROUTER), ("hacker_news", &*HACKER_NEWS_ROUTER)] {
    match configured {
      Ok(Some(router)) => pools.push(ProviderPool::fallback(name, router)),
      Ok(None) => {}
      Err(error) => return Err(error.to_string().into()),
    }
  }
  pools.push(ProviderPool::general_web("web", configured_router()?));
  Ok(pools)
});

static ALLOCATION_STORE: Lazy<InfuResult<Arc<AllocationStore>>> = Lazy::new(|| {
  let config = config::search_config().map_err(|message| format!("Invalid web-search configuration: {message}"))?;
  let directory = config.state_dir.as_ref().ok_or("Routing allocation directory is not configured.")?;
  AllocationStore::open(directory).map_err(|error| format!("Could not initialize routing allocation: {error}").into())
});

fn search_router(backends: Vec<Backend>) -> InfuResult<SearchRouter> {
  let config = config::search_config().map_err(|message| format!("Invalid web-search configuration: {message}"))?;
  let directory = config.state_dir.as_ref().ok_or("Provider cooldown state directory is not configured.")?;
  let allocation = ALLOCATION_STORE.as_ref().map_err(|error| error.to_string())?;
  let backends = backends.into_iter().map(|backend| (provider_tier(config, backend.name()), backend)).collect();
  SearchRouter::new(backends, directory, Arc::clone(allocation))
    .map_err(|error| format!("Could not initialize provider cooldowns: {error}").into())
}

fn budgeted_provider(
  adapter: impl SearchProvider + 'static,
  free_allowance_cents: u64,
  store: Option<&Arc<BudgetStore>>,
) -> crate::providers::ProviderResult<BudgetedProvider> {
  let store = store.ok_or_else(|| {
    crate::providers::ProviderError::new(
      crate::providers::ProviderErrorKind::Internal,
      "Budget state directory is not configured.",
    )
  })?;
  let account = store.account(adapter.name(), free_allowance_cents)?;
  BudgetedProvider::new(adapter, account)
}

/// Prepare configured clients and validate credentials locally, without issuing
/// any provider requests.
pub fn initialize_providers() -> InfuResult<()> {
  for pool in configured_pools()? {
    log::info!(
      "Search pool registration: pool={}, selection={:?}, providers={:?}",
      pool.name,
      pool.selection,
      pool.router.provider_names()
    );
  }
  log::info!(
    "Search execution limits: overall_ms={}, fallback_ms={}, fallback_attempts_per_pool={}, fallback_concurrency={}",
    SEARCH_TIMEOUT.as_millis(),
    FALLBACK_TIMEOUT.as_millis(),
    FALLBACK_ATTEMPTS_PER_POOL,
    FALLBACK_CONCURRENCY
  );
  Ok(())
}

pub async fn provider_status() -> InfuResult<Vec<ProviderStatus>> {
  let config = config::search_config().map_err(str::to_owned)?;
  let request = SearchRequest { query: "status", max_results: DEFAULT_MAX_RESULTS };
  let mut providers = configured_router()?.status(&request).await;
  if config.purili_max_requests_per_minute == 0 {
    providers.push(ProviderStatus::unconfigured(
      "purili",
      "Purili is disabled; set providers.purili.max_requests_per_minute to a positive integer to enable.",
    ));
  }
  if config.exa_mcp_max_requests_per_minute == 0 {
    providers.push(ProviderStatus::unconfigured(
      "exa_mcp",
      "Exa MCP is disabled; set providers.exa_mcp.max_requests_per_minute to a positive integer to enable.",
    ));
  }
  if config.tavily_mcp_max_requests_per_minute == 0 {
    providers.push(ProviderStatus::unconfigured(
      "tavily_mcp",
      "Tavily MCP is disabled; set providers.tavily_mcp.max_requests_per_minute to a positive integer to enable.",
    ));
  }
  if config.firecrawl_mcp_max_requests_per_minute == 0 {
    providers.push(ProviderStatus::unconfigured(
      "firecrawl_mcp",
      "Firecrawl MCP is disabled; set providers.firecrawl_mcp.max_requests_per_minute to a positive integer to enable.",
    ));
  }
  if config.parallel_mcp_max_requests_per_minute == 0 {
    providers.push(ProviderStatus::unconfigured(
      "parallel_mcp",
      "Parallel MCP is disabled; set providers.parallel_mcp.max_requests_per_minute to a positive integer to enable.",
    ));
  }
  if config.parallel.is_none() {
    providers.push(ProviderStatus::unconfigured("parallel", "providers.parallel.api_key is not configured."));
  }
  if config.brightdata.is_none() {
    providers.push(ProviderStatus::unconfigured("brightdata", "providers.brightdata.api_key is not configured."));
  }
  if config.serpapi.is_none() {
    providers.push(ProviderStatus::unconfigured("serpapi", "providers.serpapi.api_key is not configured."));
  }
  if config.context.is_none() {
    providers.push(ProviderStatus::unconfigured("context", "providers.context.api_key is not configured."));
  }
  if config.exa.is_none() {
    providers.push(ProviderStatus::unconfigured("exa", "providers.exa.api_key is not configured."));
  }
  if config.tavily.is_none() {
    providers.push(ProviderStatus::unconfigured("tavily", "providers.tavily.api_key is not configured."));
  }
  if config.diffbot.is_none() {
    providers.push(ProviderStatus::unconfigured("diffbot", "providers.diffbot.api_key is not configured."));
  }
  if config.firecrawl.is_none() {
    providers.push(ProviderStatus::unconfigured("firecrawl", "providers.firecrawl.api_key is not configured."));
  }
  if config.brave.is_none() {
    providers.push(ProviderStatus::unconfigured("brave", "providers.brave.api_key is not configured."));
  }
  if config.you_max_requests_per_minute == 0 {
    providers.push(ProviderStatus::unconfigured(
      "you",
      "You.com is disabled; set providers.you.max_requests_per_minute to a positive integer to enable.",
    ));
  }
  for provider in &mut providers {
    provider.tier = provider_tier(config, provider.name);
  }
  Ok(providers)
}

/// Plugin tiers are known only after discovery, so they are not in configuration.
fn provider_tier(config: &config::SearchConfig, name: &str) -> Option<u32> {
  config.provider_tiers.get(name).copied().or_else(|| crate::plugin::tier(name))
}

/// Fallback sources, reported separately from the general pool.
pub async fn probe_provider_status() -> InfuResult<Vec<ProviderStatus>> {
  let request = SearchRequest { query: "status", max_results: DEFAULT_MAX_RESULTS };
  let mut statuses = Vec::new();
  for pool in configured_pools()?.iter().filter(|pool| pool.selection == PoolSelection::Fallback) {
    statuses.extend(pool.router.status(&request).await);
  }
  let config = config::search_config().map_err(str::to_owned)?;
  if config.wikipedia_max_requests_per_minute == 0 {
    statuses.push(ProviderStatus::unconfigured(
      "wikipedia",
      "Wikipedia is disabled; set fallback_providers.wikipedia.max_requests_per_minute to a positive integer to enable.",
    ));
  }
  if config.hacker_news_max_requests_per_minute == 0 {
    statuses.push(ProviderStatus::unconfigured(
      "hacker_news",
      "Hacker News is disabled; set fallback_providers.hacker_news.max_requests_per_minute to a positive integer to enable.",
    ));
  }
  Ok(statuses)
}

/// Report cooldowns persisted by an earlier run, which exclude providers silently.
pub async fn log_persisted_cooldowns() {
  let Ok(pools) = configured_pools() else { return };
  for pool in pools {
    pool.router.log_persisted_cooldowns().await;
  }
}

/// Poll optional usage reporting without adding another request to every search.
/// Firecrawl reports account periods here; other current adapters return None.
pub async fn refresh_provider_usage() {
  let Ok(router) = configured_router() else { return };
  let mut interval = tokio::time::interval(Duration::from_secs(300));
  interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
  loop {
    interval.tick().await;
    router.refresh_usage().await;
  }
}

/// Clear `name`'s cooldown in whichever pool holds it. Returns false for an
/// unknown or disabled provider.
pub async fn clear_provider_cooldown(name: &str) -> InfuResult<bool> {
  let mut found = false;
  for pool in configured_pools()? {
    found |= pool.router.clear_cooldown(name).await.map_err(|error| error.to_string())?;
  }
  Ok(found)
}

/// Try general-web provider `name` first for the next search only, clearing
/// any cooldown so that request is sent. It acts as an early recovery probe: a
/// success restores the provider and a failure starts a fresh cooldown.
/// Returns false for an unknown or disabled provider.
pub async fn force_next_provider(name: &str) -> InfuResult<bool> {
  let router = configured_router()?;
  if !router.clear_cooldown(name).await.map_err(|error| error.to_string())? {
    return Ok(false);
  }
  Ok(router.force_next(name))
}

fn configured_router() -> InfuResult<&'static SearchRouter> {
  ROUTER.as_ref().map_err(|error| error.to_string().into())
}

fn configured_pools() -> InfuResult<&'static [ProviderPool<'static>]> {
  POOLS.as_ref().map(Vec::as_slice).map_err(|error| error.to_string().into())
}

#[derive(Serialize)]
struct CompactWebSearchResponse<'a> {
  query: &'a str,
  /// Pool names that contributed returned results, in first-contribution order.
  sources: Vec<&'static str>,
  /// Warns the caller when results came from the limited fallback sources.
  #[serde(skip_serializing_if = "Option::is_none")]
  note: Option<&'static str>,
  results: Vec<CompactSearchResult<'a>>,
}

/// Provenance for the returned set, independent of which collections were used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
  pools: Vec<&'static str>,
  /// Actual providers for browser attribution; excluded from the MCP response.
  providers: Vec<&'static str>,
  fallback_sources_used: bool,
}

const FALLBACK_NOTE: &str = "Fallback search was used because general web providers returned no results or were unavailable within the search limits. Results are limited to Wikipedia and Hacker News and may not answer the query.";

impl Source {
  pub fn providers(&self) -> &[&'static str] {
    &self.providers
  }

  pub fn note(&self) -> Option<&'static str> {
    self.fallback_sources_used.then_some(FALLBACK_NOTE)
  }
}

/// Explicit model-facing fields: adding internal provider metadata must not
/// implicitly expand the tool response.
#[derive(Serialize)]
struct CompactSearchResult<'a> {
  title: &'a str,
  url: &'a str,
  snippet: &'a str,
}

/// Search with the configured provider and return compact JSON for the model.
pub async fn search_web_json(query: &str, max_results: usize, trace: &mut SearchTrace) -> InfuResult<String> {
  let query = query.trim();
  if query.is_empty() {
    return Err("search query cannot be empty".into());
  }
  let started = Instant::now();
  let response = search(query, max_results, trace).await.and_then(|(results, source)| {
    let response = compact_web_search_json(query, source.clone(), &results)?;
    log::info!(
      "[search {}] Search completed: sources={:?}, providers={:?}, attempts=[{}], results={}, elapsed_ms={}, fallback_sources_used={}, query={query:?}",
      trace.id,
      source.pools,
      source.providers,
      trace.attempts.join(", "),
      results.len(),
      started.elapsed().as_millis(),
      source.fallback_sources_used
    );
    Ok(response)
  });
  if let Err(error) = &response {
    warn!(
      "[search {}] Search failed: attempts=[{}], elapsed_ms={}, query={query:?}, reason={error}",
      trace.id,
      trace.attempts.join(", "),
      started.elapsed().as_millis()
    );
  }
  response
}

pub fn compact_web_search_json(query: &str, source: Source, results: &[SearchResult]) -> InfuResult<String> {
  let results = results
    .iter()
    .map(|result| CompactSearchResult { title: &result.title, url: &result.url, snippet: &result.snippet })
    .collect();
  let note = source.note();
  serde_json::to_string(&CompactWebSearchResponse { query, sources: source.pools, note, results })
    .map_err(|e| format!("Could not serialize web search response: {}", e).into())
}

/// Try general web providers first. Only when they produce no results, retrieve
/// the top three results from each enabled fallback source in parallel.
pub async fn search(
  query: &str,
  max_results: usize,
  trace: &mut SearchTrace,
) -> InfuResult<(Vec<SearchResult>, Source)> {
  let query = query.trim();
  if query.is_empty() {
    return Err("search query cannot be empty".into());
  }
  let deadline = Instant::now() + SEARCH_TIMEOUT;
  trace.id = NEXT_SEARCH_ID.fetch_add(1, Ordering::Relaxed);
  let search_id = trace.id;
  debug!("[search {search_id}] Search started: max_results={max_results}, query={query:?}");
  let request = SearchRequest { query, max_results: max_results.clamp(1, MAX_RESULTS_CAP) };
  let pools = configured_pools()?;
  let general_pool = pools
    .iter()
    .find(|pool| pool.selection == PoolSelection::GeneralWeb)
    .ok_or("General web route has no provider pool.")?;
  let general_limits = SearchLimits::new(search_id, deadline, general_pool.router.provider_names().len());
  let general = general_pool.router.search_with_provider(request, &general_limits).await;
  trace.attempts.extend(general_limits.outcomes());
  let general_error = match general {
    Ok((results, provider)) if !results.is_empty() => {
      let (results, mut source) = merge_source_results(vec![(general_pool.name, results)], request.max_results, false);
      source.providers = provider.into_iter().collect();
      return Ok((results, source));
    }
    Ok(_) => {
      log::info!("[search {search_id}] General web returned no results; using fallback sources, query={query:?}");
      None
    }
    Err(error) => {
      warn!("[search {search_id}] General web unavailable; using fallback sources: {error}, query={query:?}");
      Some(error)
    }
  };

  let fallback_deadline = (Instant::now() + FALLBACK_TIMEOUT).min(deadline);
  let sources: Vec<_> = pools.iter().filter(|pool| pool.selection == PoolSelection::Fallback).collect();
  if sources.is_empty() {
    return match general_error {
      Some(error) => Err(error.to_string().into()),
      None => Ok(merge_source_results(Vec::new(), request.max_results, false)),
    };
  }
  let fallback_request = SearchRequest { query, max_results: FALLBACK_RESULTS_PER_POOL };
  let probes: Vec<_> = sources
    .iter()
    .enumerate()
    .map(|(index, &pool)| async move {
      let limits = SearchLimits::new(search_id, fallback_deadline, FALLBACK_ATTEMPTS_PER_POOL);
      let probe = routing::probe_source(pool, fallback_request, &limits).await;
      (index, probe, limits.outcomes())
    })
    .collect();
  let mut probes = stream::iter(probes).buffer_unordered(FALLBACK_CONCURRENCY).collect::<Vec<_>>().await;
  // Completion order must not change source ranking or response provenance.
  probes.sort_unstable_by_key(|(index, _, _)| *index);
  let mut completed = general_error.is_none();
  let mut sets = Vec::new();
  for (index, (results, source_trace), outcomes) in probes {
    let pool = sources[index];
    trace.attempts.extend(outcomes);
    trace.probes.insert(pool.name, source_trace);
    if let Ok(results) = results {
      completed = true;
      sets.push((pool.name, results));
    }
  }
  if completed {
    // Fallback always returns up to three per source, independent of the
    // general-web result count selected for chat or deep research.
    Ok(merge_source_results(sets, sources.len() * FALLBACK_RESULTS_PER_POOL, true))
  } else {
    Err(general_error.expect("no search completed").to_string().into())
  }
}

/// Interleave sources without changing their own ranking or the response cap.
fn merge_source_results(
  sets: Vec<(&'static str, Vec<SearchResult>)>,
  max_results: usize,
  fallback_sources_used: bool,
) -> (Vec<SearchResult>, Source) {
  let merged = merge::merge(sets, max_results);
  let source = Source { providers: merged.sources.clone(), pools: merged.sources, fallback_sources_used };
  (merged.results, source)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn only_judged_wikipedia_results_carry_the_note() {
    let shape =
      |source| serde_json::from_str::<serde_json::Value>(&compact_web_search_json("q", source, &[]).unwrap()).unwrap();
    assert_eq!(shape(Source::Web), serde_json::json!({ "query": "q", "results": [] }));
    assert_eq!(shape(Source::Wikipedia)["note"], WIKIPEDIA_NOTE);
    let last_resort = shape(Source::WikipediaLastResort);
    assert_eq!(last_resort["source"], "wikipedia");
    assert!(last_resort.get("note").is_none());
  }
}
