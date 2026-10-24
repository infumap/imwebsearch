// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::mcp_http::{Endpoint, Session};
use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, RequestRateLimit, SearchProvider, SearchRequest,
  SearchResponse, SearchResult, SearchResultMetadata,
};
use crate::request_quota::{RequestQuota, RequestQuotaSnapshot};

// Advanced search returns JSON in its text block, unlike web_search_exa's prose.
// Enable only this anonymous search tool; never attach account credentials.
const ENDPOINT: &str = "https://mcp.exa.ai/mcp?tools=web_search_advanced_exa";
const TOOL: &str = "web_search_advanced_exa";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESULTS: usize = 25;
const MAX_SNIPPET_CHARS: usize = 1000;
// Upstream api/mcp.ts defaults to 50/day per public IP and 2 QPS. These can be
// overridden in deployment. A rolling day is a local estimate, not a reset claim.
pub const MAX_DAILY_FREE_REQUESTS: u32 = 50;

/// Anonymous hosted Exa MCP search, independent of the authenticated REST adapter.
/// One search call per attempt, with no retries, pagination or login fallback.
pub struct ExaMcp {
  client: Client,
  quota: Arc<RequestQuota>,
  requests_per_minute: NonZeroU32,
}

impl ExaMcp {
  pub fn new(directory: &Path, requests_per_minute: NonZeroU32) -> ProviderResult<Self> {
    let client = Client::builder()
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(10))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Exa MCP HTTP client."))?;
    Ok(Self { client, quota: RequestQuota::open(directory, "exa_mcp", MAX_DAILY_FREE_REQUESTS)?, requests_per_minute })
  }

  async fn search_until(&self, request: SearchRequest<'_>, deadline: Instant) -> ProviderResult<SearchResponse> {
    let max_results = result_limit(&request)?;
    let mut session = Session::new(self.client.clone(), &MCP_ENDPOINT);
    session.prepare().await?;
    // Charge only the search, durably before dispatch. A cancelled or uncertain
    // tools/call is never replayed, and retains its local charge.
    self.quota.charge(deadline).await?;
    let result = session
      .call(json!({
        "query": request.query.trim(),
        "numResults": max_results,
        "type": "lite",
        "textMaxCharacters": MAX_SNIPPET_CHARS,
        "enableHighlights": true,
        "highlightsMaxCharacters": MAX_SNIPPET_CHARS
      }))
      .await?;
    let payload = match result.get("structuredContent").filter(|value| !value.is_null()) {
      Some(payload) => payload.clone(),
      None => {
        let content = result.get("content").and_then(Value::as_array).ok_or_else(invalid_response)?;
        let texts: Vec<&str> = content
          .iter()
          .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
          .filter_map(|item| item.get("text").and_then(Value::as_str))
          .collect();
        serde_json::from_str(&texts.join("\n")).map_err(|_| invalid_response())?
      }
    };
    let payload: SearchPayload = serde_json::from_value(payload).map_err(|_| invalid_response())?;
    Ok(SearchResponse { results: normalize_results(payload.results, max_results), reported_cost_usd_micros: None })
  }
}

impl SearchProvider for ExaMcp {
  fn name(&self) -> &'static str {
    "exa_mcp"
  }

  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    Some(self.requests_per_minute)
  }

  fn additional_rate_limits(&self) -> Vec<RequestRateLimit> {
    vec![RequestRateLimit { max_requests: 2, window: Duration::from_millis(1_100) }]
  }

  fn request_quota(&self) -> BoxFuture<'_, ProviderResult<Option<RequestQuotaSnapshot>>> {
    Box::pin(async { self.quota.snapshot().await.map(Some) })
  }

  fn clear_quota_cooldown(&self) -> BoxFuture<'_, ProviderResult<()>> {
    Box::pin(self.quota.clear_cooldown())
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
        if let Err(error) = result {
          if error.kind == ProviderErrorKind::QuotaExhausted {
            let delay = error.retry_after.unwrap_or(super::QUOTA_RETRY_DELAY);
            self.quota.exhaust(delay).await?;
          }
          return Err(error);
        }
        result
      };
      tokio::time::timeout_at(deadline, operation).await.unwrap_or_else(|_| {
        Err(ProviderError::new(ProviderErrorKind::Transient, "Exa MCP search exceeded its 20-second deadline."))
      })
    })
  }
}

fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  if request.query.trim().is_empty() || request.max_results == 0 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "Exa MCP search requires a query and a positive result limit.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

#[derive(Deserialize)]
struct SearchPayload {
  results: Vec<ExaResult>,
}

#[derive(Deserialize)]
struct ExaResult {
  url: String,
  title: Option<String>,
  text: Option<String>,
  highlights: Option<Vec<String>>,
  #[serde(rename = "publishedDate")]
  published_date: Option<String>,
  author: Option<String>,
}

fn normalize_results(results: Vec<ExaResult>, limit: usize) -> Vec<SearchResult> {
  let mut seen_urls = HashSet::new();
  results
    .into_iter()
    .filter_map(|result| {
      let url = Url::parse(result.url.trim()).ok()?;
      if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
      }
      let url = url.to_string();
      if !seen_urls.insert(url.clone()) {
        return None;
      }
      let title = result.title.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
      let title = if title.is_empty() { url.clone() } else { title };
      let excerpts: Vec<String> =
        result.highlights.unwrap_or_default().into_iter().filter(|excerpt| !excerpt.trim().is_empty()).collect();
      let text = if excerpts.is_empty() { result.text.unwrap_or_default() } else { excerpts.join(" ") };
      let snippet = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_SNIPPET_CHARS).collect();
      Some(SearchResult {
        title,
        url,
        snippet,
        metadata: Some(SearchResultMetadata {
          published_date: result.published_date,
          author: result.author,
          excerpts,
          description: None,
        }),
      })
    })
    .take(limit)
    .collect()
}

fn invalid_response() -> ProviderError {
  unexpected("Exa MCP search returned an invalid result payload.")
}

fn tool_error(value: &Value) -> ProviderError {
  // Inspect only bounded error envelopes, never include raw upstream text in
  // diagnostics. Exa hides which free rate dimension was hit; unknown 429s
  // therefore retain the router's full-day fallback instead of rapid probes.
  let mut messages = Vec::new();
  if let Some(message) = value.get("message").and_then(Value::as_str) {
    messages.push(message);
  }
  if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
    messages.push(message);
  }
  if let Some(content) = value.get("content").and_then(Value::as_array) {
    messages.extend(
      content
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str)),
    );
  }
  let text = messages.join("\n").to_ascii_lowercase().replace(['_', '-'], " ");
  let kind = if text.contains("error (402)")
    || text.contains("quota exhausted")
    || text.contains("quota exceeded")
    || text.contains("insufficient credit")
    || text.contains("daily limit")
  {
    ProviderErrorKind::QuotaExhausted
  } else if value.pointer("/_meta/ai.exa~1rateLimited") == Some(&Value::Bool(true))
    || text.contains("error (429)")
    || text.contains("rate limit")
    || text.contains("too many requests")
  {
    ProviderErrorKind::RateLimited
  } else if text.contains("error (401)") {
    ProviderErrorKind::InvalidCredentials
  } else if text.contains("error (403)") {
    ProviderErrorKind::Blocked
  } else if text.contains("error (400)") || text.contains("error (422)") {
    ProviderErrorKind::InvalidRequest
  } else {
    ProviderErrorKind::UnexpectedResponse
  };
  ProviderError::new(kind, super::reported_error("Exa MCP", kind)).with_error_payload(value)
}

const MCP_ENDPOINT: Endpoint =
  Endpoint { name: "Exa MCP", url: ENDPOINT, tool: TOOL, headers: &[], tool_error: result_error, http_error };

fn result_error(value: &Value) -> Option<ProviderError> {
  (value.get("isError").is_some_and(|value| value != &Value::Bool(false))
    || value.pointer("/_meta/ai.exa~1rateLimited") == Some(&Value::Bool(true))
    || value.get("code").is_some()
    || value.get("error").is_some()
    || value.get("message").is_some())
  .then(|| tool_error(value))
}

fn http_error(status: u16, details: Option<&Value>) -> ProviderErrorKind {
  match status {
    400 | 413 | 414 | 422 => ProviderErrorKind::InvalidRequest,
    401 => ProviderErrorKind::InvalidCredentials,
    402 => ProviderErrorKind::QuotaExhausted,
    403 => ProviderErrorKind::Blocked,
    404 | 408 | 500..=599 => ProviderErrorKind::Transient,
    429 if details.is_some_and(|value| tool_error(value).kind == ProviderErrorKind::QuotaExhausted) => {
      ProviderErrorKind::QuotaExhausted
    }
    429 => ProviderErrorKind::RateLimited,
    _ => ProviderErrorKind::UnexpectedResponse,
  }
}

fn unexpected(message: &'static str) -> ProviderError {
  ProviderError::new(ProviderErrorKind::UnexpectedResponse, message)
}
