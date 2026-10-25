// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

pub mod brave;
pub mod brightdata;
pub mod context;
pub mod diffbot;
mod error_metadata;
pub mod exa;
pub mod exa_mcp;
pub mod firecrawl;
pub mod firecrawl_mcp;
pub mod hacker_news;
mod html_fingerprint;
mod mcp;
mod mcp_http;
#[cfg(unix)]
mod mcp_stdio;
pub mod parallel;
pub mod parallel_mcp;
pub mod purili;
pub mod reset;
pub mod serpapi;
pub mod tavily;
pub mod tavily_mcp;
pub mod wikipedia;
pub mod you;

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use futures_util::future::BoxFuture;
use serde::Serialize;

pub(super) use error_metadata::{ErrorBody, read_error_body, response_error};

pub type ProviderResult<T> = Result<T, ProviderError>;

/// Fallback for rate/quota rejections without an explicit upstream retry hint.
pub const QUOTA_RETRY_DELAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Retry-After supports both delta seconds and an HTTP date. Malformed values
/// leave the caller's backoff policy in charge; past dates never go negative.
/// Describe a provider-reported error by its kind. This message can reach the
/// model, so it never repeats upstream text; that goes to `upstream_detail`.
pub(super) fn reported_error(source: &str, kind: ProviderErrorKind) -> String {
  match kind {
    ProviderErrorKind::QuotaExhausted => format!("{source} reported its quota is exhausted."),
    ProviderErrorKind::RateLimited => format!("{source} reported rate limiting."),
    ProviderErrorKind::Blocked => format!("{source} rejected access."),
    ProviderErrorKind::InvalidCredentials => format!("{source} rejected the credentials."),
    ProviderErrorKind::Transient => format!("{source} reported a temporary failure."),
    ProviderErrorKind::InvalidRequest => format!("{source} rejected the request."),
    _ => format!("{source} reported a tool or protocol error."),
  }
}

pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
  let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();
  if let Ok(seconds) = value.parse::<u64>() {
    return Some(Duration::from_secs(seconds));
  }
  let until = jiff::fmt::rfc2822::parse(value).ok()?.timestamp();
  let seconds = until.as_second().saturating_sub(jiff::Timestamp::now().as_second()).max(0) as u64;
  Some(Duration::from_secs(seconds))
}

pub struct SearchResponse {
  pub results: Vec<SearchResult>,
  /// Provider-reported request estimate, not an account balance or final bill.
  pub reported_cost_usd_micros: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchResult {
  pub title: String,
  pub url: String,
  pub snippet: String,
  // Retain provider metadata internally without changing the MCP result shape.
  #[serde(skip)]
  #[expect(dead_code, reason = "Optional metadata is retained for subsequent response formatting work.")]
  pub metadata: Option<SearchResultMetadata>,
}

#[expect(dead_code, reason = "Optional metadata is retained for subsequent response formatting work.")]
#[derive(Clone, Debug)]
pub struct SearchResultMetadata {
  pub published_date: Option<String>,
  pub author: Option<String>,
  pub excerpts: Vec<String>,
  /// A short description of the page's subject, such as Wikipedia's.
  pub description: Option<String>,
}

/// Input is trimmed and result limits are normalized by the search layer.
#[derive(Clone, Copy, Debug)]
pub struct SearchRequest<'a> {
  pub query: &'a str,
  pub max_results: usize,
}

/// Monetary cost is independent of rate limits or available monthly quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestCost {
  Unmetered,
  UsdMicros(u64),
}

/// Provider-reported allowance for a billing period, normalized to millionths
/// of a US dollar. Unknown values remain None, rather than implying capacity.
#[derive(Clone, Debug)]
pub struct UsageSnapshot {
  pub remaining_usd_micros: Option<u64>,
  pub allowance_usd_micros: Option<u64>,
  pub resets_at: Option<SystemTime>,
  /// Start of the upstream period, when the provider exposes both boundaries.
  pub period_start: Option<SystemTime>,
}

/// Object-safe interface so the router can hold different provider adapters.
pub trait SearchProvider: Send + Sync {
  fn name(&self) -> &'static str;

  /// Optional rolling-minute attempt limit. Unused capacity allows concurrent
  /// requests; monthly accounting and concurrency limits are independent.
  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    None
  }

  /// Additional rolling windows, enforced alongside the local minute limit.
  fn additional_rate_limits(&self) -> Vec<RequestRateLimit> {
    Vec::new()
  }

  /// Independent concurrency ceiling from provider guidance or local policy.
  fn max_concurrent_requests(&self) -> Option<NonZeroU32> {
    None
  }

  /// A genuine short-term spacing restriction, separate from minute capacity.
  fn minimum_request_interval(&self) -> Duration {
    Duration::ZERO
  }

  /// Local, persistent request allowance, separate from monetary accounting.
  /// Snapshot inspection must not write state or contact the provider.
  /// The adapter must charge its quota immediately before the search dispatch;
  /// router inspection alone cannot reserve capacity across concurrent calls.
  fn request_quota(&self) -> BoxFuture<'_, ProviderResult<Option<crate::request_quota::RequestQuotaSnapshot>>> {
    Box::pin(async { Ok(None) })
  }

  /// Operator override: clear a cooldown recorded in `request_quota`.
  fn clear_quota_cooldown(&self) -> BoxFuture<'_, ProviderResult<()>> {
    Box::pin(async { Ok(()) })
  }

  /// Local estimate only; this must not issue a request to the provider.
  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost>;

  /// Keep execution cooperative: move blocking I/O and lock waits off runtime
  /// threads so request deadlines can cancel this future promptly.
  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>>;

  /// None means usage reporting is unsupported. Never invent a full allowance.
  fn usage(&self) -> BoxFuture<'_, ProviderResult<Option<UsageSnapshot>>> {
    Box::pin(async { Ok(None) })
  }
}

#[derive(Clone, Copy)]
pub struct RequestRateLimit {
  pub max_requests: u32,
  pub window: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
  RateLimited,
  QuotaExhausted,
  InsufficientBudget,
  InvalidCredentials,
  Transient,
  InvalidRequest,
  Blocked,
  UnexpectedResponse,
  Internal,
}

#[derive(Debug)]
pub struct ProviderError {
  pub kind: ProviderErrorKind,
  pub retry_after: Option<Duration>,
  pub rejection_code: Option<String>,
  pub http_status: Option<u16>,
  /// Sanitized excerpt of upstream error text for operator logs. Never part of
  /// the Display message, which can reach the model.
  pub upstream_detail: Option<String>,
  /// Only set when the provider explicitly guarantees this attempt was not billed.
  pub not_charged: bool,
  message: String,
}

impl ProviderError {
  pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
    Self {
      kind,
      retry_after: None,
      rejection_code: None,
      http_status: None,
      upstream_detail: None,
      not_charged: false,
      message: message.into(),
    }
  }

  pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
    // Multiple explicit signals may arrive in HTTP headers and an error body.
    // Preserve the longest, including a valid zero, without adding a fallback.
    self.retry_after = self.retry_after.into_iter().chain(retry_after).max();
    self
  }

  pub fn with_error_payload(self, payload: &serde_json::Value) -> Self {
    error_metadata::apply(self, payload)
  }

  pub fn with_http_status(mut self, status: u16) -> Self {
    self.http_status = Some(status);
    self
  }

  /// Keep an earlier, more specific detail if one is already set.
  pub fn with_upstream_detail(mut self, detail: Option<String>) -> Self {
    if self.upstream_detail.is_none() {
      self.upstream_detail = detail;
    }
    self
  }

  pub fn without_charge(mut self) -> Self {
    self.not_charged = true;
    self
  }
}

impl Display for ProviderError {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.message)?;
    if let Some(code) = &self.rejection_code {
      write!(f, " [code: {code}]")?;
    }
    if let Some(delay) = self.retry_after {
      write!(f, " [retry_after: {}s]", delay.as_secs_f64())?;
    }
    Ok(())
  }
}

impl Error for ProviderError {}
