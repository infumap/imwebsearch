// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! One provider declared by the search plugin. Each attempt runs its own plugin
//! process; see PLUGINS.md for the contract.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::Url;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::time::Instant;

use super::mcp_stdio::Session;
use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
  SearchResult,
};
use crate::config::file::PluginConfig;
use crate::plugin::ProviderSpec;
use crate::request_quota::{RequestQuota, RequestQuotaSnapshot};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESULTS: usize = 30;
const MAX_TITLE_CHARS: usize = 300;
const MAX_SNIPPET_CHARS: usize = 1000;

pub struct PluginProvider {
  spec: &'static ProviderSpec,
  plugin: &'static PluginConfig,
  /// Shared by every provider of the plugin.
  processes: Arc<Semaphore>,
  quota: Option<Arc<RequestQuota>>,
}

impl PluginProvider {
  pub fn new(
    spec: &'static ProviderSpec,
    plugin: &'static PluginConfig,
    processes: Arc<Semaphore>,
    directory: &Path,
  ) -> ProviderResult<Self> {
    let quota = spec.daily_quota.map(|allowance| RequestQuota::open(directory, spec.name, allowance)).transpose()?;
    Ok(Self { spec, plugin, processes, quota })
  }

  async fn search_until(&self, request: SearchRequest<'_>, deadline: Instant) -> ProviderResult<SearchResponse> {
    let max_results = result_limit(&request)?;
    let program = self.plugin.command.as_deref().ok_or_else(|| {
      ProviderError::new(ProviderErrorKind::Internal, "Search plugin provider has no configured command.")
    })?;
    let _process = match tokio::time::timeout_at(deadline, self.processes.acquire()).await {
      Ok(Ok(permit)) => permit,
      _ => return Err(ProviderError::new(ProviderErrorKind::Transient, "Search plugin process limit was reached.")),
    };
    let mut session =
      Session::start(self.spec.name, program, &self.plugin.args, &self.plugin.env, Some(deadline)).await?;
    // Charge only the search, durably before dispatch. A cancelled or uncertain
    // call is never replayed, and retains its local charge.
    if let Some(quota) = &self.quota {
      quota.charge(deadline).await?;
    }
    let result = session
      .request(
        "tools/call",
        json!({
          "name": self.spec.name,
          "arguments": { "query": request.query.trim(), "max_results": max_results }
        }),
      )
      .await;
    session.finish();
    search_response(self.spec.name, &result?, max_results)
  }
}

impl SearchProvider for PluginProvider {
  fn name(&self) -> &'static str {
    self.spec.name
  }

  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    Some(self.spec.requests_per_minute)
  }

  fn max_concurrent_requests(&self) -> Option<NonZeroU32> {
    self.spec.max_concurrent
  }

  fn minimum_request_interval(&self) -> Duration {
    self.spec.min_interval
  }

  fn request_quota(&self) -> BoxFuture<'_, ProviderResult<Option<RequestQuotaSnapshot>>> {
    Box::pin(async {
      match &self.quota {
        Some(quota) => quota.snapshot().await.map(Some),
        None => Ok(None),
      }
    })
  }

  fn clear_quota_cooldown(&self) -> BoxFuture<'_, ProviderResult<()>> {
    Box::pin(async {
      match &self.quota {
        Some(quota) => quota.clear_cooldown().await,
        None => Ok(()),
      }
    })
  }

  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost> {
    result_limit(request)?;
    Ok(RequestCost::Unmetered)
  }

  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>> {
    Box::pin(async move {
      let deadline = Instant::now() + SEARCH_TIMEOUT;
      let operation = async {
        let result = self.search_until(request, deadline).await;
        if let (Err(error), Some(quota)) = (&result, &self.quota)
          && error.kind == ProviderErrorKind::QuotaExhausted
        {
          quota.exhaust(error.retry_after.unwrap_or(super::QUOTA_RETRY_DELAY)).await?;
        }
        result
      };
      tokio::time::timeout_at(deadline, operation).await.unwrap_or_else(|_| {
        Err(ProviderError::new(ProviderErrorKind::Transient, "Search plugin exceeded its 20-second deadline."))
      })
    })
  }
}

fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  if request.query.trim().is_empty() || request.max_results == 0 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "Search plugin requires a query and a positive result limit.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

/// Results with unusable URLs are dropped rather than failing the search.
fn search_response(name: &str, result: &Value, max_results: usize) -> ProviderResult<SearchResponse> {
  if result.get("isError").is_some_and(|value| value != &Value::Bool(false)) {
    return Err(tool_error(name, result));
  }
  let entries = result.pointer("/structuredContent/results").and_then(Value::as_array).ok_or_else(|| {
    ProviderError::new(ProviderErrorKind::UnexpectedResponse, "Search plugin returned no results array.")
  })?;
  let mut seen_urls = HashSet::new();
  let results = entries
    .iter()
    .filter_map(|entry| {
      let url = Url::parse(entry.get("url")?.as_str()?.trim()).ok()?;
      if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
      }
      let url = url.to_string();
      if !seen_urls.insert(url.clone()) {
        return None;
      }
      let text = |field: &str, limit: usize| -> String {
        let value = entry.get(field).and_then(Value::as_str).unwrap_or_default();
        value.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(limit).collect()
      };
      let title = text("title", MAX_TITLE_CHARS);
      let title = if title.is_empty() { url.clone() } else { title };
      Some(SearchResult { title, url, snippet: text("snippet", MAX_SNIPPET_CHARS), metadata: None })
    })
    .take(max_results)
    .collect();
  Ok(SearchResponse { results, reported_cost_usd_micros: None })
}

/// The kind selects the router's cooldown. Upstream text stays in operator
/// detail; the message, which can reach the model, is fixed per kind.
fn tool_error(name: &str, result: &Value) -> ProviderError {
  let payload = result.get("structuredContent").unwrap_or(&Value::Null);
  let kind = match payload.get("kind").and_then(Value::as_str) {
    Some("rate_limited") => ProviderErrorKind::RateLimited,
    Some("quota_exhausted") => ProviderErrorKind::QuotaExhausted,
    Some("blocked") => ProviderErrorKind::Blocked,
    Some("transient") => ProviderErrorKind::Transient,
    Some("invalid_credentials") => ProviderErrorKind::InvalidCredentials,
    Some("invalid_request") => ProviderErrorKind::InvalidRequest,
    _ => ProviderErrorKind::UnexpectedResponse,
  };
  let retry_after = payload.get("retry_after_secs").and_then(Value::as_u64).map(Duration::from_secs);
  ProviderError::new(kind, super::reported_error(name, kind)).with_error_payload(result).with_retry_after(retry_after)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn results(entries: Value) -> Vec<SearchResult> {
    search_response("acme", &json!({ "structuredContent": { "results": entries } }), 3).unwrap().results
  }

  #[test]
  fn normalizes_results() {
    let results = results(json!([
      { "title": "  Two\n words ", "url": " https://example.com/a ", "snippet": "a\tb", "extra": 1 },
      { "title": "Duplicate", "url": "https://example.com/a" },
      { "title": "", "url": "https://example.com/b" },
      { "title": "Bad scheme", "url": "ftp://example.com/" },
      { "title": "Relative", "url": "/path" },
      { "title": "Missing url" },
      "not an object",
      { "url": "https://example.com/c" },
      { "url": "https://example.com/beyond-limit" }
    ]));
    let summary: Vec<_> = results.iter().map(|r| (r.title.as_str(), r.url.as_str(), r.snippet.as_str())).collect();
    assert_eq!(
      summary,
      [
        ("Two words", "https://example.com/a", "a b"),
        ("https://example.com/b", "https://example.com/b", ""),
        ("https://example.com/c", "https://example.com/c", "")
      ]
    );
  }

  #[test]
  fn truncates_long_fields() {
    let long = "x".repeat(5000);
    let result = &results(json!([{ "title": long, "url": "https://example.com/", "snippet": long }]))[0];
    assert_eq!((result.title.chars().count(), result.snippet.chars().count()), (MAX_TITLE_CHARS, MAX_SNIPPET_CHARS));
  }

  #[test]
  fn missing_results_is_unexpected() {
    for result in [json!({}), json!({ "structuredContent": { "results": {} } })] {
      let Err(error) = search_response("acme", &result, 3) else { panic!("accepted {result}") };
      assert_eq!(error.kind, ProviderErrorKind::UnexpectedResponse);
    }
  }

  #[test]
  fn maps_reported_errors() {
    let error = |payload: Value| {
      let result = json!({
        "isError": true,
        "content": [{ "type": "text", "text": "Upstream said no." }],
        "structuredContent": payload
      });
      let Err(error) = search_response("acme", &result, 3) else { panic!("accepted {result}") };
      error
    };
    let limited = error(json!({ "kind": "rate_limited", "retry_after_secs": 60, "code": "http_429" }));
    assert_eq!(limited.kind, ProviderErrorKind::RateLimited);
    assert_eq!(limited.retry_after, Some(Duration::from_secs(60)));
    assert_eq!(limited.rejection_code.as_deref(), Some("http_429"));
    assert_eq!(limited.to_string(), "acme reported rate limiting. [code: http_429] [retry_after: 60s]");
    assert_eq!(limited.upstream_detail.as_deref(), Some("Upstream said no."));
    for (kind, expected) in [
      ("quota_exhausted", ProviderErrorKind::QuotaExhausted),
      ("blocked", ProviderErrorKind::Blocked),
      ("transient", ProviderErrorKind::Transient),
      ("invalid_credentials", ProviderErrorKind::InvalidCredentials),
      ("invalid_request", ProviderErrorKind::InvalidRequest),
      ("internal", ProviderErrorKind::UnexpectedResponse),
      ("made_up", ProviderErrorKind::UnexpectedResponse),
    ] {
      assert_eq!(error(json!({ "kind": kind })).kind, expected, "{kind}");
    }
    assert_eq!(error(json!(null)).kind, ProviderErrorKind::UnexpectedResponse);
    assert_eq!(error(json!({ "kind": "blocked" })).retry_after, None);
  }

  fn provider(script: &str, daily_quota: Option<u32>, directory: &Path) -> PluginProvider {
    let spec = Box::leak(Box::new(ProviderSpec {
      name: "acme",
      tier: 1,
      requests_per_minute: NonZeroU32::MIN,
      max_concurrent: None,
      min_interval: Duration::ZERO,
      daily_quota,
      routing_requests_per_day: 100,
    }));
    let plugin = Box::leak(Box::new(PluginConfig {
      command: Some("/bin/sh".into()),
      args: vec!["-c".to_owned(), script.to_owned()],
      ..PluginConfig::default()
    }));
    PluginProvider::new(spec, plugin, Arc::new(Semaphore::new(1)), directory).unwrap()
  }

  /// Reports in its result URL whether the tools/call matched the expected
  /// trimmed query and capped limit. serde_json writes keys in sorted order.
  const CHECKING_PLUGIN: &str = "read -r _; printf '%s\\n' \
    '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}}}}'; \
    read -r _; read -r call; \
    case \"$call\" in \
      *'\"method\":\"tools/call\",\"params\":{\"arguments\":{\"max_results\":30,\"query\":\"rust\"},\"name\":\"acme\"}'*) \
        url=https://example.com/match;; \
      *) url=https://example.com/mismatch;; \
    esac; \
    printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[],\"structuredContent\":{\"results\":[{\"url\":\"%s\"}]}}}\\n' \"$url\"";

  #[tokio::test]
  async fn searches_through_plugin_and_charges_quota() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider(CHECKING_PLUGIN, Some(5), directory.path());
    let response = provider.search(SearchRequest { query: "  rust  ", max_results: 50 }).await.unwrap();
    assert_eq!(response.results[0].url, "https://example.com/match");
    let quota = provider.request_quota().await.unwrap().unwrap();
    assert_eq!((quota.free_allowance_requests, quota.consumed_requests), (5, 1));
  }

  #[tokio::test]
  async fn plugin_failure_before_dispatch_is_not_charged() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider("exit 1", Some(5), directory.path());
    let error = provider.search(SearchRequest { query: "rust", max_results: 5 }).await.err().unwrap();
    assert_eq!(error.kind, ProviderErrorKind::Transient);
    assert_eq!(provider.request_quota().await.unwrap().unwrap().consumed_requests, 0);
  }

  #[tokio::test]
  async fn rejects_empty_query_without_starting_plugin() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider("exit 1", None, directory.path());
    let request = SearchRequest { query: "  ", max_results: 5 };
    assert_eq!(provider.estimate_cost(&request).unwrap_err().kind, ProviderErrorKind::InvalidRequest);
    assert_eq!(provider.search(request).await.err().unwrap().kind, ProviderErrorKind::InvalidRequest);
  }
}
