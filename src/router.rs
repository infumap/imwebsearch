// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use jiff::Timestamp;
use log::{debug, info, warn};
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::allocation::{AllocationStore, AllocationUsage};
use crate::budget::{BudgetBalance, BudgetSnapshot, BudgetedProvider};
use crate::duration::span;
use crate::providers::{
  ProviderError, ProviderErrorKind, ProviderResult, QUOTA_RETRY_DELAY, RequestCost, RequestRateLimit, SearchProvider,
  SearchRequest, SearchResult,
};
use crate::request_quota::{RequestQuota, RequestQuotaSnapshot};

const POOL_DEADLINE: Duration = Duration::from_secs(30);
const ATTEMPT_DEADLINE: Duration = Duration::from_secs(20);
const NO_FORCED_PROVIDER: usize = usize::MAX;
const PERSISTED_COOLDOWN_SLACK: Duration = Duration::from_secs(2);

/// Pools in one execution stage share an absolute deadline and attempt budget.
/// Admission consumes an attempt before any provider accounting or dispatch.
pub struct SearchLimits {
  /// Tags log lines so interleaved concurrent searches can be told apart.
  pub search_id: u64,
  pub deadline: Instant,
  remaining_attempts: AtomicUsize,
  /// Each dispatched attempt's outcome in order, such as `exa_mcp:RateLimited`
  /// or `you:ok(5)`, for the search's summary log line.
  outcomes: std::sync::Mutex<Vec<String>>,
}

impl SearchLimits {
  pub fn new(search_id: u64, deadline: Instant, attempts: usize) -> Self {
    Self { search_id, deadline, remaining_attempts: AtomicUsize::new(attempts), outcomes: Default::default() }
  }

  pub fn outcomes(&self) -> Vec<String> {
    self.outcomes.lock().map(|outcomes| outcomes.clone()).unwrap_or_default()
  }

  pub fn exhausted(&self) -> bool {
    self.remaining_attempts.load(Ordering::Relaxed) == 0
  }

  // Newer toolchains deprecate fetch_update in favour of try_update, which is
  // unavailable at the declared rust-version.
  #[allow(deprecated)]
  fn take_attempt(&self) -> bool {
    self.remaining_attempts.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(1)).is_ok()
  }
}

pub enum Backend {
  /// Local minute capacity supplements any provider-owned rate windows.
  Unmetered {
    provider: Box<dyn SearchProvider>,
    requests_per_minute: Option<NonZeroU32>,
    /// Soft weight for providers without provider-owned daily accounting.
    routing_requests_per_day: u32,
  },
  Metered(BudgetedProvider),
}

impl Backend {
  pub fn name(&self) -> &'static str {
    self.provider().name()
  }

  fn provider(&self) -> &dyn SearchProvider {
    match self {
      Self::Unmetered { provider, .. } => provider.as_ref(),
      Self::Metered(provider) => provider,
    }
  }

  async fn search(
    &self,
    request: SearchRequest<'_>,
    deadline: Instant,
    recovery: bool,
  ) -> ProviderResult<Vec<SearchResult>> {
    if Instant::now() >= deadline {
      return Err(ProviderError::new(ProviderErrorKind::Transient, "Search provider exceeded the request deadline."));
    }
    match self {
      Self::Unmetered { provider, .. } => provider.search(request).await,
      Self::Metered(provider) => provider.search_with_budget(request, Some(deadline), recovery).await,
    }
    .map(|response| response.results)
  }
}

pub struct SearchRouter {
  providers: Vec<ProviderSlot>,
  allocation: Arc<AllocationStore>,
  /// Operator override from the status page: the next selection prefers this
  /// provider index over tier and allocation ranking, then forgets it.
  forced: AtomicUsize,
}

struct ProviderSlot {
  backend: Backend,
  tier: Option<u32>,
  cooldown: Arc<RequestQuota>,
  dispatch: Arc<Mutex<()>>,
  rate_limit: Option<RateLimit>,
  max_concurrent_requests: Option<NonZeroU32>,
  health: Arc<Mutex<Health>>,
  usage_status: Mutex<UsageStatus>,
  /// Operator pause from the status page. The marker file's presence persists
  /// it across restarts.
  paused: AtomicBool,
  pause_marker: PathBuf,
}

#[derive(Clone, Default)]
struct Health {
  disabled: Option<&'static str>,
  cooldown_until: Option<Instant>,
  quota_reset: Option<i64>,
  failures: u32,
  generation: u64,
  in_flight: usize,
  recovering: bool,
  request_starts: VecDeque<Instant>,
  last_rejection: Option<RejectionStatus>,
}

#[derive(Serialize)]
pub struct ProviderStatus {
  pub name: &'static str,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tier: Option<u32>,
  pub enabled: bool,
  pub available: bool,
  pub unavailable_reasons: Vec<String>,
  pub reference_cost_usd_micros: Option<u64>,
  pub funding: Option<&'static str>,
  pub health: Option<HealthStatus>,
  pub budget: Option<BudgetSnapshot>,
  pub usage: Option<UsageStatus>,
  pub rate_limit: Option<RateLimitStatus>,
  pub allocation: Option<AllocationStatus>,
  #[serde(skip_serializing_if = "std::ops::Not::not")]
  pub forced_next: bool,
  #[serde(skip_serializing_if = "std::ops::Not::not")]
  pub paused: bool,
  // Keep the existing status field name for provider-neutral request quotas.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub daily_quota: Option<RequestQuotaSnapshot>,
}

impl ProviderStatus {
  /// Rate pacing is not an unavailable reason: it is reported through
  /// `rate_limit.next_request_in_ms`, but still makes the provider unavailable.
  fn settle_available(&mut self) {
    self.available =
      self.unavailable_reasons.is_empty() && self.rate_limit.as_ref().is_none_or(|limit| limit.next_request_in_ms == 0);
  }

  pub fn unconfigured(name: &'static str, reason: &str) -> Self {
    Self {
      name,
      tier: None,
      enabled: false,
      available: false,
      unavailable_reasons: vec![reason.to_owned()],
      reference_cost_usd_micros: None,
      funding: None,
      health: None,
      budget: None,
      usage: None,
      rate_limit: None,
      daily_quota: None,
      allocation: None,
      forced_next: false,
      paused: false,
    }
  }
}

struct RateLimit {
  requests_per_minute: u32,
  windows: Vec<RequestRateLimit>,
  minimum_interval: Duration,
}

impl RateLimit {
  fn next_start(&self, health: &Health, now: Instant) -> Instant {
    let mut next = now;
    if let Some(&last) = health.request_starts.back() {
      next = next.max(last + self.minimum_interval);
    }
    for limit in &self.windows {
      // The Nth most recent attempt must expire before another can start.
      if let Some(&start) = health.request_starts.iter().rev().nth(limit.max_requests as usize - 1) {
        next = next.max(start + limit.window);
      }
    }
    next
  }

  fn record_start(&self, health: &mut Health, now: Instant) {
    let retention = self.windows.iter().map(|limit| limit.window).max().unwrap_or_default().max(self.minimum_interval);
    while health.request_starts.front().is_some_and(|&start| now.duration_since(start) >= retention) {
      health.request_starts.pop_front();
    }
    health.request_starts.push_back(now);
  }
}

#[derive(Serialize)]
pub struct RateLimitStatus {
  requests_per_minute: u32,
  requests_in_last_minute: usize,
  next_request_in_ms: u64,
}

#[derive(Serialize)]
pub struct HealthStatus {
  disabled_reason: Option<&'static str>,
  cooldown_remaining_ms: u64,
  quota_resets_at: Option<i64>,
  consecutive_failures: u32,
  in_flight: usize,
  recovery_probe_required: bool,
  last_rejection: Option<RejectionStatus>,
}

/// Bounded metadata from rejections observed by this running process. Kept
/// outside persistent cooldown ledgers so existing ledger formats stay intact.
#[derive(Clone, Serialize)]
struct RejectionStatus {
  observed_at: i64,
  kind: ProviderErrorKind,
  code: Option<String>,
  http_status: Option<u16>,
  retry_after_ms: Option<u64>,
}

#[derive(Clone, Serialize)]
pub struct UsageStatus {
  state: &'static str,
  last_checked_at: Option<i64>,
  last_report: Option<ReportedUsage>,
  error: Option<String>,
}

impl Default for UsageStatus {
  fn default() -> Self {
    Self { state: "not_checked", last_checked_at: None, last_report: None, error: None }
  }
}

#[derive(Clone, Serialize)]
struct ReportedUsage {
  observed_at: i64,
  remaining_usd_micros: Option<u64>,
  allowance_usd_micros: Option<u64>,
  resets_at: Option<i64>,
}

/// Daily targets guide ranking only; neither a score of one nor an overrun
/// makes a provider unavailable. Monetary units retain request-cost differences.
struct AllocationTarget {
  daily_units: f64,
  request_units: u64,
  unit: &'static str,
  source: &'static str,
}

#[derive(Serialize)]
pub struct AllocationStatus {
  window_seconds: u64,
  bucket_seconds: u64,
  unit: &'static str,
  target_source: &'static str,
  daily_target_units: f64,
  reference_requests_per_day: f64,
  consumed_units: u64,
  pressure: f64,
  last_selected_at_ms: Option<i64>,
}

impl AllocationTarget {
  fn status(&self, usage: &AllocationUsage) -> AllocationStatus {
    AllocationStatus {
      window_seconds: 24 * 60 * 60,
      bucket_seconds: 60,
      unit: self.unit,
      target_source: self.source,
      daily_target_units: self.daily_units,
      reference_requests_per_day: self.daily_units / self.request_units as f64,
      consumed_units: usage.consumed_units,
      pressure: usage.consumed_units as f64 / self.daily_units,
      last_selected_at_ms: usage.last_selected_at_ms,
    }
  }
}

struct Candidate {
  index: usize,
  target: AllocationTarget,
  usage: AllocationUsage,
  /// Chosen by the status page's "Use next": dispatch skips local health,
  /// concurrency and pacing checks.
  forced: bool,
}

enum Availability {
  Ready,
  Paused(&'static str),
  Blocked(&'static str),
}

impl Candidate {
  fn pressure(&self) -> f64 {
    self.usage.consumed_units as f64 / self.target.daily_units
  }

  fn preferred_to(&self, other: &Self) -> bool {
    // Compare existing usage, not usage after this request: unused providers
    // participate even at low volume. Never-used entries sort before used ones.
    self
      .pressure()
      .total_cmp(&other.pressure())
      .then_with(|| self.usage.last_sequence.cmp(&other.usage.last_sequence))
      .is_lt()
  }
}

impl SearchRouter {
  /// Registration order only; this does not inspect quota or contact providers.
  pub fn provider_names(&self) -> Vec<&'static str> {
    self.providers.iter().map(|slot| slot.backend.provider().name()).collect()
  }

  pub fn new(
    backends: Vec<(Option<u32>, Backend)>,
    directory: &Path,
    allocation: Arc<AllocationStore>,
  ) -> ProviderResult<Self> {
    Ok(Self {
      allocation,
      forced: AtomicUsize::new(NO_FORCED_PROVIDER),
      providers: backends
        .into_iter()
        .map(|(tier, backend)| {
          let cooldown = RequestQuota::cooldown(directory, backend.provider().name())?;
          let requests_per_minute = match &backend {
            Backend::Unmetered { requests_per_minute, .. } => *requests_per_minute,
            Backend::Metered(provider) => provider.requests_per_minute(),
          };
          let mut windows = backend.provider().additional_rate_limits();
          if windows.iter().any(|limit| limit.max_requests == 0 || limit.window.is_zero()) {
            return Err(internal("Provider rate windows must have positive capacity and duration."));
          }
          if let Some(rate) = requests_per_minute {
            windows.push(RequestRateLimit { max_requests: rate.get(), window: Duration::from_secs(60) });
          }
          let minimum_interval = backend.provider().minimum_request_interval();
          let rate_limit = (!windows.is_empty() || !minimum_interval.is_zero()).then_some(RateLimit {
            requests_per_minute: requests_per_minute.map_or(0, NonZeroU32::get),
            windows,
            minimum_interval,
          });
          let max_concurrent_requests = backend.provider().max_concurrent_requests();
          let pause_marker = directory.join(format!("{}-paused", backend.provider().name()));
          let paused = pause_marker.try_exists().map_err(|_| internal("Could not read provider pause marker."))?;
          Ok(ProviderSlot {
            backend,
            tier,
            cooldown,
            dispatch: Arc::default(),
            rate_limit,
            max_concurrent_requests,
            health: Arc::default(),
            usage_status: Mutex::default(),
            paused: AtomicBool::new(paused),
            pause_marker,
          })
        })
        .collect::<ProviderResult<_>>()?,
    })
  }

  /// Inspect eligibility for a reference request without selecting a provider,
  /// updating health, persisting a rollover or contacting an external service.
  pub async fn status(&self, request: &SearchRequest<'_>) -> Vec<ProviderStatus> {
    let usage = self.allocation.snapshot().await;
    let mut providers = Vec::with_capacity(self.providers.len());
    let forced = self.forced.load(Ordering::Relaxed);
    for (index, slot) in self.providers.iter().enumerate() {
      let mut status = slot.status(request).await;
      status.forced_next = index == forced;
      match &usage {
        Ok(usage) => {
          if let Ok(cost) = slot.backend.provider().estimate_cost(request) {
            match slot.allocation_target(cost, status.daily_quota.as_ref(), status.budget.as_ref().map(|b| &b.balance))
            {
              Ok(target) => {
                status.allocation = Some(target.status(usage.get(status.name).unwrap_or(&AllocationUsage::default())))
              }
              Err(error) => status.unavailable_reasons.push(error.to_string()),
            }
          }
        }
        Err(error) => status.unavailable_reasons.push(error.to_string()),
      }
      status.settle_available();
      providers.push(status);
    }
    providers
  }

  pub async fn log_persisted_cooldowns(&self) {
    for slot in &self.providers {
      slot.log_persisted_cooldown().await;
    }
  }

  /// The provider the next selection prefers, if one is forced.
  pub fn forced_next(&self) -> Option<&'static str> {
    self.providers.get(self.forced.load(Ordering::Relaxed)).map(|slot| slot.backend.name())
  }

  /// Send the next selection to `name` whatever its state; see `select`.
  /// Returns false if this router does not hold that provider.
  pub fn force_next(&self, name: &str) -> bool {
    let Some(index) = self.providers.iter().position(|slot| slot.backend.name() == name) else { return false };
    self.forced.store(index, Ordering::Relaxed);
    info!("{name} will be tried first for the next search");
    true
  }

  /// Exclude `name` from selection until resumed, across restarts. Unlike a
  /// cooldown, nothing but the operator lifts it. Returns false if this router
  /// does not hold that provider.
  pub async fn set_paused(&self, name: &str, paused: bool) -> ProviderResult<bool> {
    let Some(index) = self.providers.iter().position(|slot| slot.backend.name() == name) else { return Ok(false) };
    let slot = &self.providers[index];
    let marker = slot.pause_marker.clone();
    let persisted = tokio::task::spawn_blocking(move || {
      if paused {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
          use std::os::unix::fs::OpenOptionsExt;
          options.mode(0o600);
        }
        options.open(&marker).map(drop)
      } else {
        std::fs::remove_file(&marker).or_else(|error| match error.kind() {
          std::io::ErrorKind::NotFound => Ok(()),
          _ => Err(error),
        })
      }
    })
    .await;
    if !matches!(persisted, Ok(Ok(()))) {
      return Err(internal("Could not update provider pause marker."));
    }
    slot.paused.store(paused, Ordering::Relaxed);
    if paused {
      let _ = self.forced.compare_exchange(index, NO_FORCED_PROVIDER, Ordering::Relaxed, Ordering::Relaxed);
    }
    info!("{name} {}", if paused { "paused from the status page" } else { "resumed from the status page" });
    Ok(true)
  }

  /// Forget local and persisted cooldowns for `name`, as a success would.
  /// Disabled providers stay disabled. Returns false if this router does not
  /// hold that provider.
  pub async fn clear_cooldown(&self, name: &str) -> ProviderResult<bool> {
    let Some(slot) = self.providers.iter().find(|slot| slot.backend.name() == name) else { return Ok(false) };
    slot.clear_cooldown().await?;
    info!("Cleared {name} cooldown");
    Ok(true)
  }

  /// Forget recent request starts for `name`, so local rate pacing allows a
  /// request now. Returns false if this router does not hold that provider.
  pub async fn clear_pacing(&self, name: &str) -> bool {
    let Some(slot) = self.providers.iter().find(|slot| slot.backend.name() == name) else { return false };
    slot.health.lock().await.request_starts.clear();
    info!("Cleared {name} pacing");
    true
  }

  pub async fn search(&self, request: SearchRequest<'_>, limits: &SearchLimits) -> ProviderResult<Vec<SearchResult>> {
    self.search_with_provider(request, limits).await.map(|(results, _)| results)
  }

  /// Return the provider that supplied the results, rather than one that was
  /// merely attempted. Attribution is local to this request, including retries.
  pub async fn search_with_provider(
    &self,
    request: SearchRequest<'_>,
    limits: &SearchLimits,
  ) -> ProviderResult<(Vec<SearchResult>, Option<&'static str>)> {
    let deadline = limits.deadline.min(Instant::now() + POOL_DEADLINE);
    let mut provider = None;
    // Cover selection, lock/worker waits, reservations and settlement, as well
    // as provider HTTP. Dropping this future never waits on synchronous cleanup.
    match tokio::time::timeout_at(deadline, self.search_until(request, deadline, limits, &mut provider)).await {
      Ok(result) if Instant::now() < deadline => result.map(|results| (results, provider)),
      _ => Err(ProviderError::new(ProviderErrorKind::Transient, "Search pool reached its deadline.")),
    }
  }

  async fn search_until(
    &self,
    request: SearchRequest<'_>,
    deadline: Instant,
    limits: &SearchLimits,
    provider: &mut Option<&'static str>,
  ) -> ProviderResult<Vec<SearchResult>> {
    if self.providers.is_empty() {
      return Err(internal(
        "No search providers enabled. Set a general-web provider's API key or a positive max_requests_per_minute for providers.you, providers.exa_mcp, providers.tavily_mcp, providers.firecrawl_mcp, providers.parallel_mcp, or providers.purili.",
      ));
    }
    let mut attempted = vec![false; self.providers.len()];
    let mut reasons = vec![None; self.providers.len()];
    let mut attempts = 0;
    let mut saw_empty_results = false;
    while !limits.exhausted() && Instant::now() < deadline {
      // Include in-flight allocations in the next caller's ranking. Transfer
      // this gate to the persistence worker before releasing it for HTTP.
      let selection = Arc::clone(&self.allocation.selection).lock_owned().await;
      let usage = self.allocation.snapshot().await?;
      let Some(candidate) = self.select(limits.search_id, &request, &mut attempted, &mut reasons, usage).await else {
        break;
      };
      if Instant::now() >= deadline {
        break;
      }
      let slot = &self.providers[candidate.index];
      // Eligibility can change while other requests are selecting a provider.
      let flight = match slot.acquire(deadline, limits, candidate.forced).await {
        Some(flight) => flight,
        None => {
          attempted[candidate.index] = true;
          reasons[candidate.index] = Some("provider became unavailable".to_owned());
          continue;
        }
      };
      attempted[candidate.index] = true;
      attempts += 1;
      let name = slot.backend.provider().name();
      if let Err(error) = self.allocation.record(name, candidate.target.request_units, deadline, selection).await {
        let result = Err(error);
        flight.finish(&result).await;
        return result;
      }
      let tier = slot.tier.map_or_else(|| "untiered".to_owned(), |tier| format!("tier {tier}"));
      debug!(
        "[search {}] Searching with {tier} provider '{name}' at allocation pressure {:.4}",
        limits.search_id,
        candidate.pressure()
      );
      let started = Instant::now();
      let attempt_deadline = deadline.min(Instant::now() + ATTEMPT_DEADLINE);
      let result = match tokio::time::timeout_at(attempt_deadline, slot.search(request, attempt_deadline)).await {
        Ok(result) if Instant::now() < attempt_deadline => result,
        _ => Err(ProviderError::new(ProviderErrorKind::Transient, "Search provider exceeded the request deadline.")),
      };
      let probe = matches!(result, Ok((_, true)));
      let result = result.map(|(results, _)| results);
      let earlier_failures = flight.recovering.then_some(flight.failures);
      flight.finish(&result).await;
      let outcome = match &result {
        Ok(results) => format!("{name}:ok({})", results.len()),
        Err(error) => format!("{name}:{:?}", error.kind),
      };
      if let Ok(mut outcomes) = limits.outcomes.lock() {
        outcomes.push(outcome);
      }
      if result.is_ok() {
        if probe {
          info!("[search {}] {name} recovered: a request succeeded after its persisted cooldown", limits.search_id);
        } else if let Some(failures) = earlier_failures {
          info!("[search {}] {name} recovered after {failures} consecutive failure(s)", limits.search_id);
        }
      }
      debug!(
        "[search {}] Search provider attempt completed: provider={}, attempt={}, elapsed_ms={}, results={:?}, error_kind={:?}",
        limits.search_id,
        slot.backend.provider().name(),
        attempts,
        started.elapsed().as_millis(),
        result.as_ref().ok().map(Vec::len),
        result.as_ref().err().map(|error| error.kind)
      );
      match result {
        // Empty results are successful for health/accounting, but this query
        // should try the next eligible provider before returning to the model.
        Ok(results) if results.is_empty() => {
          saw_empty_results = true;
          debug!(
            "[search {}] {} returned no results; trying another eligible provider",
            limits.search_id,
            slot.backend.provider().name()
          );
        }
        Ok(results) => {
          *provider = Some(name);
          return Ok(results);
        }
        Err(error) => {
          let search_id = limits.search_id;
          warn!("[search {search_id}] {} search failed ({:?}): {error}", slot.backend.provider().name(), error.kind);
          if let Some(detail) = &error.upstream_detail {
            debug!("[search {search_id}] {} upstream error detail: {detail:?}", slot.backend.provider().name());
          }
          slot.log_cooldown(search_id, &error).await;
          reasons[candidate.index] = Some(error.to_string());
        }
      }
    }
    // Preserve a completed empty search if no fallback produced results. If
    // every attempt failed, retain the normal unavailability error instead.
    // The outer timeout still enforces the overall deadline for either outcome.
    if saw_empty_results {
      return Ok(Vec::new());
    }
    let details = self
      .providers
      .iter()
      .zip(reasons)
      .filter_map(|(slot, reason)| reason.map(|reason| format!("{}: {reason}", slot.backend.provider().name())))
      .collect::<Vec<_>>()
      .join("; ");
    let message = if Instant::now() >= deadline {
      "Search pool reached its deadline"
    } else if limits.exhausted() {
      "Web search reached its attempt limit"
    } else {
      "No eligible search providers remain"
    };
    Err(ProviderError::new(ProviderErrorKind::Transient, format!("{message}. {details}")))
  }

  async fn select(
    &self,
    search_id: u64,
    request: &SearchRequest<'_>,
    attempted: &mut [bool],
    reasons: &mut [Option<String>],
    mut usage: BTreeMap<String, AllocationUsage>,
  ) -> Option<Candidate> {
    // Only a request's first selection may consume the override, so a search
    // already falling back does not take it from the next request.
    let forced = if attempted.contains(&true) {
      NO_FORCED_PROVIDER
    } else {
      self.forced.swap(NO_FORCED_PROVIDER, Ordering::Relaxed)
    };
    // The operator asked for this provider whatever its state: skip pause,
    // cooldown, pacing, tier and allocation ranking. Monetary budgets still
    // apply, so a provider without free credit falls back to normal selection.
    if let Some(slot) = self.providers.get(forced) {
      let name = slot.backend.name();
      let recent = usage.remove(slot.backend.provider().name()).unwrap_or_default();
      let candidate = match slot.clear_cooldown().await {
        Ok(()) => slot.candidate(forced, request, recent).await,
        Err(error) => Err(error),
      };
      match candidate {
        Ok(Some(candidate)) => return Some(Candidate { forced: true, ..candidate }),
        Ok(None) => {
          info!("[search {search_id}] {name} was forced but has no quota or free credit available; selecting normally")
        }
        Err(error) => {
          info!("[search {search_id}] {name} was forced but could not be used ({error}); selecting normally")
        }
      }
    }
    let mut active_tier = None;
    let mut candidates = Vec::new();
    for (index, slot) in self.providers.iter().enumerate() {
      // Check attempted providers too: an empty result or a request-specific
      // failure must not open the next tier while this provider is still ready.
      let tier = slot.tier.unwrap_or(1);
      match slot.availability().await {
        Availability::Ready => (),
        Availability::Paused(reason) => {
          reasons[index] = Some(reason.to_owned());
          continue;
        }
        Availability::Blocked(reason) => {
          active_tier = Some(active_tier.map_or(tier, |current: u32| current.min(tier)));
          reasons[index] = Some(reason.to_owned());
          continue;
        }
      }
      let recent = usage.remove(slot.backend.provider().name()).unwrap_or_default();
      match slot.candidate(index, request, recent).await {
        Ok(Some(candidate)) => {
          active_tier = Some(active_tier.map_or(tier, |current| current.min(tier)));
          if !attempted[index] {
            candidates.push(candidate);
          }
        }
        Ok(None) => {
          reasons[index] = Some("provider quota or account period is unavailable".to_owned());
        }
        Err(error) => {
          if !attempted[index] {
            slot.record_failure(&error).await;
            slot.log_cooldown(search_id, &error).await;
          }
          attempted[index] = true;
          reasons[index] = Some(error.to_string());
          if !matches!(slot.availability().await, Availability::Paused(_)) {
            active_tier = Some(active_tier.map_or(tier, |current| current.min(tier)));
          }
        }
      }
    }
    candidates
      .into_iter()
      .filter(|candidate| Some(self.providers[candidate.index].tier.unwrap_or(1)) == active_tier)
      .reduce(|best, candidate| if candidate.preferred_to(&best) { candidate } else { best })
  }

  pub async fn refresh_usage(&self) {
    for slot in &self.providers {
      let result = tokio::time::timeout(Duration::from_secs(20), slot.backend.provider().usage()).await;
      let checked_at = Timestamp::now().as_second();
      let update = match result {
        Ok(Ok(Some(snapshot))) => UsageStatus {
          state: "reported",
          last_checked_at: Some(checked_at),
          last_report: Some(ReportedUsage {
            observed_at: checked_at,
            remaining_usd_micros: snapshot.remaining_usd_micros,
            allowance_usd_micros: snapshot.allowance_usd_micros,
            resets_at: snapshot.resets_at.and_then(|time| Timestamp::try_from(time).ok()).map(|time| time.as_second()),
          }),
          error: None,
        },
        Ok(Ok(None)) => {
          UsageStatus { state: "unsupported", last_checked_at: Some(checked_at), last_report: None, error: None }
        }
        Ok(Err(error)) => {
          warn!("Could not reconcile {} usage: {error}", slot.backend.provider().name());
          UsageStatus {
            state: "error",
            last_checked_at: Some(checked_at),
            last_report: None,
            error: Some(format!("Usage refresh failed ({:?}).", error.kind)),
          }
        }
        Err(_) => {
          warn!("{} usage refresh timed out", slot.backend.provider().name());
          UsageStatus {
            state: "error",
            last_checked_at: Some(checked_at),
            last_report: None,
            error: Some("Usage refresh timed out.".to_owned()),
          }
        }
      };
      let mut status = slot.usage_status.lock().await;
      // A failed refresh must not erase the last successful observation.
      let last_report = update.last_report.clone().or_else(|| status.last_report.clone());
      *status = UsageStatus { last_report, ..update };
    }
  }
}

impl ProviderSlot {
  async fn status(&self, request: &SearchRequest<'_>) -> ProviderStatus {
    let mut status = ProviderStatus {
      tier: self.tier,
      enabled: true,
      unavailable_reasons: Vec::new(),
      ..ProviderStatus::unconfigured(self.backend.provider().name(), "")
    };
    // Expiry cleanup must only change this copy during inspection. Release the
    // health lock before waiting for any budget operation.
    let mut health = self.health.lock().await.clone();
    status.paused = self.paused.load(Ordering::Relaxed);
    if status.paused {
      status.unavailable_reasons.push("Paused from the status page.".to_owned());
    }
    if let Some(reason) = self.blocked(&mut health) {
      status.unavailable_reasons.push(reason.to_owned());
    }
    match self.cooldown.snapshot().await {
      Ok(quota) => {
        health.recovering |= quota.recovery_probe_required;
        if let Some(until) = quota.remote_exhausted_until_ms {
          health.quota_reset = Some((until / 1_000).saturating_add(i64::from(until % 1_000 != 0)));
          status.unavailable_reasons.push("Provider quota is cooling down.".to_owned());
        }
        if let Some(error) = quota.persistence_error {
          status.unavailable_reasons.push(error.to_owned());
        }
      }
      Err(error) => status.unavailable_reasons.push(error.to_string()),
    }
    status.rate_limit = self.rate_limit.as_ref().map(|limit| {
      let now = Instant::now();
      RateLimitStatus {
        requests_per_minute: limit.requests_per_minute,
        requests_in_last_minute: health
          .request_starts
          .iter()
          .filter(|&&start| now.duration_since(start) < Duration::from_secs(60))
          .count(),
        next_request_in_ms: u64::try_from(
          limit.next_start(&health, now).saturating_duration_since(now).as_nanos().div_ceil(1_000_000),
        )
        .unwrap_or(u64::MAX),
      }
    });
    status.health = Some(HealthStatus {
      disabled_reason: health.disabled,
      cooldown_remaining_ms: health.cooldown_until.map_or(0, |until| {
        u64::try_from(until.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(u64::MAX)
      }),
      quota_resets_at: health.quota_reset.filter(|reset| *reset > Timestamp::now().as_second()),
      consecutive_failures: health.failures,
      in_flight: health.in_flight,
      recovery_probe_required: health.recovering,
      last_rejection: health.last_rejection,
    });
    status.usage = Some(self.usage_status.lock().await.clone());
    match self.backend.provider().request_quota().await {
      Ok(Some(quota)) => {
        if let Some(error) = quota.persistence_error {
          status.unavailable_reasons.push(error.to_owned());
        } else if !quota.available() {
          status.unavailable_reasons.push("Daily free request quota is unavailable.".to_owned());
        }
        status.daily_quota = Some(quota);
      }
      Ok(None) => (),
      Err(error) => status.unavailable_reasons.push(error.to_string()),
    }
    if let Backend::Metered(provider) = &self.backend {
      match provider.snapshot().await {
        Ok(snapshot) => {
          if let Some(error) = snapshot.persistence_error {
            status.unavailable_reasons.push(error.to_owned());
          }
          status.budget = Some(snapshot);
        }
        Err(error) => status.unavailable_reasons.push(error.to_string()),
      }
    }
    match self.backend.provider().estimate_cost(request) {
      Ok(RequestCost::Unmetered) => status.funding = Some("unmetered"),
      Ok(RequestCost::UsdMicros(cost)) => {
        status.reference_cost_usd_micros = Some(cost);
        if let Some(snapshot) = &status.budget {
          let balance = &snapshot.balance;
          if balance.dispatch_allowed {
            status.funding = Some("free");
          } else {
            status.unavailable_reasons.push("Provider account period is not available.".to_owned());
          }
        } else if status.unavailable_reasons.is_empty() {
          status.unavailable_reasons.push("Metered search provider has no budget accounting.".to_owned());
        }
      }
      Err(error) => status.unavailable_reasons.push(error.to_string()),
    }
    status.settle_available();
    status
  }

  async fn availability(&self) -> Availability {
    // Paused, not Blocked: a paused provider must not hold its tier open.
    if self.paused.load(Ordering::Relaxed) {
      return Availability::Paused("paused from the status page");
    }
    match self.cooldown.snapshot().await {
      Ok(quota) if quota.persistence_error.is_some() => {
        return Availability::Blocked("provider cooldown ledger is unavailable");
      }
      Ok(quota) if !quota.available() => return Availability::Paused("provider quota is cooling down"),
      Err(_) => return Availability::Blocked("provider cooldown ledger is unavailable"),
      Ok(_) => (),
    }
    let mut health = self.health.lock().await;
    match self.unavailable(&mut health) {
      None => Availability::Ready,
      Some(reason) => {
        let now = Instant::now();
        let paused = health.cooldown_until.is_some_and(|until| until > now)
          || health.quota_reset.is_some_and(|reset| reset > Timestamp::now().as_second())
          || self.rate_limit.as_ref().is_some_and(|limit| limit.next_start(&health, now) > now);
        if paused { Availability::Paused(reason) } else { Availability::Blocked(reason) }
      }
    }
  }

  /// On success, also reports whether this request was the recovery probe that
  /// ended a persisted cooldown.
  async fn search(&self, request: SearchRequest<'_>, deadline: Instant) -> ProviderResult<(Vec<SearchResult>, bool)> {
    // Serialize cooldown admission and persistence, never the HTTP request.
    // Recovery leases remain durable and exclusive across concurrent callers.
    let guard = Arc::clone(&self.dispatch).lock_owned().await;
    let lease = self.cooldown.admit(deadline).await?;
    drop(guard);
    let result = self.backend.search(request, deadline, lease.is_some()).await;
    let delay = result.as_ref().err().and_then(|error| {
      // Persist any block an adapter marks as lasting at least a day.
      let persistent_block =
        error.kind == ProviderErrorKind::Blocked && error.retry_after.is_some_and(|delay| delay >= QUOTA_RETRY_DELAY);
      (persistent_block || matches!(error.kind, ProviderErrorKind::QuotaExhausted | ProviderErrorKind::RateLimited))
        .then_some(error.retry_after.unwrap_or(QUOTA_RETRY_DELAY))
    });
    let guard = Arc::clone(&self.dispatch).lock_owned().await;
    self.cooldown.finish_attempt(lease, delay, result.is_ok(), guard).await?;
    result.map(|results| (results, lease.is_some()))
  }

  /// Report the cooldown in force after a failure: the later of this process's
  /// backoff and the persisted ledger, which can differ in length.
  async fn log_cooldown(&self, search_id: u64, error: &ProviderError) {
    let name = self.backend.name();
    let (disabled, local) = {
      let health = self.health.lock().await;
      let now = Instant::now();
      let quota = health.quota_reset.map(|reset| seconds_until(reset, Timestamp::now().as_second()));
      (health.disabled, health.cooldown_until.map(|until| until.saturating_duration_since(now)).max(quota))
    };
    if let Some(reason) = disabled {
      warn!("[search {search_id}] {name} disabled: {reason}");
      return;
    }
    let persisted = match self.cooldown.snapshot().await {
      Ok(snapshot) => snapshot.remote_exhausted_until_ms.map(milliseconds_until),
      Err(_) => None,
    };
    let delay = local.unwrap_or_default().max(persisted.unwrap_or_default());
    if delay.is_zero() {
      return;
    }
    // The ledger is written just before the in-memory state, which is also
    // rounded up to whole seconds, so the same cooldown reads slightly shorter.
    let scope = if persisted.is_some_and(|persisted| persisted + PERSISTED_COOLDOWN_SLACK >= delay) {
      "persisted"
    } else {
      "this process only"
    };
    let hint =
      error.retry_after.map_or_else(|| "no retry hint".to_owned(), |hint| format!("retry hint {}", span(hint)));
    let message = format!(
      "[search {search_id}] {name} cooling down {} until {} ({scope}; {hint})",
      span(delay),
      Timestamp::from_second(Timestamp::now().as_second().saturating_add_unsigned(delay.as_secs())).unwrap_or_default()
    );
    // Short backoffs follow most transient failures; only long pauses change routing.
    if persisted.is_some() || delay >= Duration::from_secs(60) {
      info!("{message}");
    } else {
      debug!("{message}");
    }
  }

  /// Persisted cooldowns survive restarts and silently exclude a provider.
  async fn log_persisted_cooldown(&self) {
    if self.paused.load(Ordering::Relaxed) {
      info!("{} is paused (persisted from an earlier run); resume it from the status page", self.backend.name());
    }
    if let Ok(snapshot) = self.cooldown.snapshot().await
      && let Some(until) = snapshot.remote_exhausted_until_ms
    {
      let delay = milliseconds_until(until);
      if !delay.is_zero() {
        info!(
          "{} cooling down {} until {} (persisted from an earlier run)",
          self.backend.name(),
          span(delay),
          Timestamp::from_second(until / 1000).unwrap_or_default()
        );
      }
    }
  }

  /// Reasons other than local rate pacing, which status reports separately.
  fn blocked(&self, health: &mut Health) -> Option<&'static str> {
    health.unavailable().or_else(|| {
      self
        .max_concurrent_requests
        .is_some_and(|limit| health.in_flight >= limit.get() as usize)
        .then_some("provider concurrency limit reached")
    })
  }

  fn unavailable(&self, health: &mut Health) -> Option<&'static str> {
    self.blocked(health).or_else(|| {
      let now = Instant::now();
      self
        .rate_limit
        .as_ref()
        .filter(|limit| limit.next_start(health, now) > now)
        .map(|_| "local request rate limit reached")
    })
  }

  fn allocation_target(
    &self,
    cost: RequestCost,
    quota: Option<&RequestQuotaSnapshot>,
    balance: Option<&BudgetBalance>,
  ) -> ProviderResult<AllocationTarget> {
    match cost {
      RequestCost::Unmetered => {
        let Backend::Unmetered { routing_requests_per_day, .. } = &self.backend else {
          return Err(internal("Unmetered provider has no routing allocation target."));
        };
        let daily = quota.map_or(*routing_requests_per_day, |quota| quota.free_allowance_requests);
        if daily == 0 {
          return Err(internal("Routing allocation target must be positive."));
        }
        Ok(AllocationTarget {
          daily_units: daily as f64,
          request_units: 1,
          unit: "requests",
          source: if quota.is_some() { "provider_daily_estimate" } else { "assumed" },
        })
      }
      RequestCost::UsdMicros(cost) => {
        let balance = balance.ok_or_else(|| internal("Metered search provider has no budget accounting."))?;
        if cost == 0 {
          return Err(internal("Metered routing allocation requires a positive request cost."));
        }
        Ok(AllocationTarget {
          // A zero reported allowance is still a soft estimate. Preserve a
          // positive denominator so upstream acceptance remains authoritative.
          daily_units: balance.free_allowance_usd_micros.max(1) as f64 / 30.0,
          request_units: cost,
          unit: "usd_micros",
          source: "monthly_allowance_divided_by_30",
        })
      }
    }
  }

  async fn candidate(
    &self,
    index: usize,
    request: &SearchRequest<'_>,
    usage: AllocationUsage,
  ) -> ProviderResult<Option<Candidate>> {
    let quota = self.backend.provider().request_quota().await?;
    if let Some(error) = quota.as_ref().and_then(|quota| quota.persistence_error) {
      return Err(internal(error));
    }
    if quota.as_ref().is_some_and(|quota| !quota.available()) {
      return Ok(None);
    }
    let cost = self.backend.provider().estimate_cost(request)?;
    let balance = match &self.backend {
      Backend::Metered(provider) => {
        let balance = provider.balance().await?;
        if !balance.dispatch_allowed {
          return Ok(None);
        }
        Some(balance)
      }
      Backend::Unmetered { .. } => None,
    };
    let target = self.allocation_target(cost, quota.as_ref(), balance.as_ref())?;
    Ok(Some(Candidate { index, target, usage, forced: false }))
  }

  async fn acquire(&self, deadline: Instant, limits: &SearchLimits, forced: bool) -> Option<Flight> {
    let mut health = self.health.lock().await;
    if !forced && self.unavailable(&mut health).is_some() {
      return None;
    }
    let in_flight = health.in_flight.checked_add(1)?;
    // Check the shared budget while holding health, before consuming a pacing
    // slot or creating a flight. Skipping a pool must not count as a failure.
    if Instant::now() >= deadline || !limits.take_attempt() {
      return None;
    }
    if let Some(limit) = &self.rate_limit {
      // Capacity reservations and health checks share a lock, so parallel
      // callers cannot oversubscribe any rolling window. Keep failed attempts.
      limit.record_start(&mut health, Instant::now());
    }
    health.in_flight = in_flight;
    Some(Flight {
      health: Arc::clone(&self.health),
      generation: health.generation,
      recovering: health.recovering,
      failures: health.failures,
      finished: false,
    })
  }

  /// Forget local and persisted cooldowns, as a success would. Disabled
  /// providers stay disabled.
  async fn clear_cooldown(&self) -> ProviderResult<()> {
    self.cooldown.clear_cooldown().await?;
    self.backend.provider().clear_quota_cooldown().await?;
    let mut health = self.health.lock().await;
    health.failures = 0;
    health.recovering = false;
    health.cooldown_until = None;
    health.quota_reset = None;
    Ok(())
  }

  async fn record_failure(&self, error: &ProviderError) {
    self.health.lock().await.fail(error);
  }
}

impl Health {
  fn unavailable(&mut self) -> Option<&'static str> {
    if let Some(reason) = self.disabled {
      return Some(reason);
    }
    if self.cooldown_until.is_some_and(|until| until > Instant::now()) {
      return Some("cooling down");
    }
    self.cooldown_until = None;
    if self.quota_reset.is_some_and(|reset| reset > Timestamp::now().as_second()) {
      return Some("quota exhausted until the next recovery attempt");
    }
    self.quota_reset = None;
    if self.recovering && self.in_flight > 0 {
      return Some("waiting for the recovery probe or earlier requests");
    }
    None
  }

  fn fail(&mut self, error: &ProviderError) {
    if error.rejection_code.is_some()
      || error.http_status.is_some()
      || matches!(
        error.kind,
        ProviderErrorKind::QuotaExhausted
          | ProviderErrorKind::RateLimited
          | ProviderErrorKind::Blocked
          | ProviderErrorKind::InvalidCredentials
      )
    {
      self.last_rejection = Some(RejectionStatus {
        observed_at: Timestamp::now().as_second(),
        kind: error.kind,
        code: error.rejection_code.clone(),
        http_status: error.http_status,
        retry_after_ms: error.retry_after.and_then(|delay| u64::try_from(delay.as_nanos().div_ceil(1_000_000)).ok()),
      });
    }
    if matches!(error.kind, ProviderErrorKind::InvalidRequest | ProviderErrorKind::InsufficientBudget) {
      // These are request-specific; another query may fit or be accepted.
      return;
    }
    self.generation = self.generation.wrapping_add(1);
    self.failures = self.failures.saturating_add(1);
    self.recovering = true;
    if self.disabled.is_some() {
      return;
    }
    match error.kind {
      ProviderErrorKind::InvalidCredentials => {
        self.disabled = Some("invalid credentials; restart after correcting configuration");
        return;
      }
      ProviderErrorKind::Internal => {
        self.disabled = Some("internal provider or budget error; restart after repair");
        return;
      }
      ProviderErrorKind::QuotaExhausted => {
        // A reset estimate must not permanently disable a recurring allowance
        // or postpone recovery until an incorrectly inferred billing date.
        let delay = error.retry_after.unwrap_or(QUOTA_RETRY_DELAY);
        let reset = i64::try_from(delay.as_nanos().div_ceil(1_000_000_000))
          .ok()
          .and_then(|delay| Timestamp::now().as_second().checked_add(delay));
        self.quota_reset = reset;
      }
      _ => (),
    }
    let backoff = Duration::from_secs((1u64 << self.failures.min(9)).min(300));
    let default = match error.kind {
      // An ambiguous 429 may be quota exhaustion. Without an upstream retry
      // hint, wait a day rather than repeatedly probing a depleted endpoint.
      ProviderErrorKind::RateLimited | ProviderErrorKind::QuotaExhausted => QUOTA_RETRY_DELAY,
      ProviderErrorKind::Blocked => Duration::from_secs(900),
      ProviderErrorKind::UnexpectedResponse => backoff.max(Duration::from_secs(15)),
      _ => backoff,
    };
    let delay = error.retry_after.unwrap_or(default);
    let Some(until) = Instant::now().checked_add(delay) else {
      self.disabled = Some("provider requested a cooldown beyond the supported clock range");
      return;
    };
    self.cooldown_until = Some(self.cooldown_until.map_or(until, |old| old.max(until)));
  }
}

struct Flight {
  health: Arc<Mutex<Health>>,
  generation: u64,
  /// Health at admission, so a success can report the end of a failure streak.
  recovering: bool,
  failures: u32,
  finished: bool,
}

impl Flight {
  async fn finish(mut self, result: &ProviderResult<Vec<SearchResult>>) {
    let mut health = self.health.lock().await;
    Self::complete(&mut health, self.generation, result.as_ref().err());
    self.finished = true;
  }

  fn complete(health: &mut Health, generation: u64, error: Option<&ProviderError>) {
    health.in_flight = health.in_flight.saturating_sub(1);
    if let Some(error) = error {
      health.fail(error);
    } else if health.generation == generation {
      // A late success must not clear a newer request's cooldown or suspension.
      health.failures = 0;
      health.recovering = false;
      health.cooldown_until = None;
      health.quota_reset = None;
    }
  }
}

impl Drop for Flight {
  fn drop(&mut self) {
    if !self.finished {
      let error = ProviderError::new(ProviderErrorKind::Transient, "Search request was cancelled.");
      if let Ok(mut health) = self.health.try_lock() {
        Self::complete(&mut health, self.generation, Some(&error));
      } else {
        // Timeout cleanup must not wait for another request's health update.
        let health = Arc::clone(&self.health);
        let generation = self.generation;
        tokio::spawn(async move {
          let mut health = health.lock().await;
          Self::complete(&mut health, generation, Some(&error));
        });
      }
    }
  }
}

fn seconds_until(at: i64, now: i64) -> Duration {
  Duration::from_secs(u64::try_from(at.saturating_sub(now)).unwrap_or(0))
}

fn milliseconds_until(at_ms: i64) -> Duration {
  Duration::from_millis(u64::try_from(at_ms.saturating_sub(Timestamp::now().as_millisecond())).unwrap_or(0))
}

fn internal(message: &str) -> ProviderError {
  ProviderError::new(ProviderErrorKind::Internal, message)
}
