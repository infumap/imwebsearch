// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::num::NonZeroU32;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use super::firecrawl::{FirecrawlSearchResult, normalize_results, result_limit};
use super::mcp_http::{Endpoint, Session};
use super::{
  ProviderError, ProviderErrorKind, ProviderResult, QUOTA_RETRY_DELAY, RequestCost, SearchProvider, SearchRequest,
  SearchResponse,
};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
// Soft routing weight, not the unpublished per-IP request or credit cap.
pub const ASSUMED_REQUESTS_PER_DAY: u32 = 100;
// https://docs.firecrawl.dev/mcp-server/keyless (reviewed 2026-10-23).
const ENDPOINT: Endpoint = Endpoint {
  name: "Firecrawl MCP",
  url: "https://mcp.firecrawl.dev/v2/mcp",
  tool: "firecrawl_search",
  headers: &[("X-Firecrawl-Origin", crate::identity::NAME)],
  tool_error,
  http_error,
};

/// Keyless search only, with independent allocation and cooldown state. Never
/// attach the REST provider's credentials or execute returned follow-up tools.
pub struct FirecrawlMcp {
  client: Client,
  requests_per_minute: NonZeroU32,
}

impl FirecrawlMcp {
  pub fn new(requests_per_minute: NonZeroU32) -> ProviderResult<Self> {
    let client = Client::builder()
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(10))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Firecrawl MCP HTTP client."))?;
    Ok(Self { client, requests_per_minute })
  }

  async fn search_once(&self, request: SearchRequest<'_>) -> ProviderResult<SearchResponse> {
    let max_results = result_limit(&request)?;
    let mut session = Session::new(self.client.clone(), &ENDPOINT);
    session.prepare().await?;
    // Omit scrapeOptions to avoid fetching each result's full page. Explicitly
    // disable domain tools and highlights; use ordinary web results only.
    let result = session
      .call(json!({
        "query": request.query.trim(),
        "limit": max_results,
        "sources": ["web"],
        "domainTools": false,
        "highlights": false
      }))
      .await?;
    let payload = if let Some(payload) = result.get("structuredContent").filter(|value| !value.is_null()) {
      payload.clone()
    } else {
      serde_json::from_str(&text_content(&result).ok_or_else(invalid_response)?).map_err(|_| invalid_response())?
    };
    let payload: SearchPayload = serde_json::from_value(payload).map_err(|_| invalid_response())?;
    if !payload.success {
      return Err(invalid_response());
    }
    Ok(SearchResponse {
      results: normalize_results(payload.data.web, max_results),
      // Keyless capacity is separate from authenticated account credits.
      reported_cost_usd_micros: None,
    })
  }
}

impl SearchProvider for FirecrawlMcp {
  fn name(&self) -> &'static str {
    "firecrawl_mcp"
  }

  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    Some(self.requests_per_minute)
  }

  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost> {
    result_limit(request)?;
    Ok(RequestCost::Unmetered)
  }

  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>> {
    Box::pin(async move {
      // Exactly one tools/call per attempt, with no retry or credential fallback.
      tokio::time::timeout(SEARCH_TIMEOUT, self.search_once(request)).await.unwrap_or_else(|_| {
        Err(ProviderError::new(ProviderErrorKind::Transient, "Firecrawl MCP search exceeded its 20-second deadline."))
      })
    })
  }
}

#[derive(Deserialize)]
struct SearchPayload {
  success: bool,
  data: SearchData,
}

#[derive(Deserialize)]
struct SearchData {
  web: Vec<FirecrawlSearchResult>,
}

fn text_content(value: &Value) -> Option<String> {
  let texts: Vec<&str> = value
    .get("content")?
    .as_array()?
    .iter()
    .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
    .filter_map(|item| item.get("text").and_then(Value::as_str))
    .collect();
  (!texts.is_empty()).then(|| texts.join("\n"))
}

fn tool_error(value: &Value) -> Option<ProviderError> {
  let flagged = value.get("isError").is_some_and(|value| value != &Value::Bool(false));
  let payload = value.get("structuredContent").filter(|value| !value.is_null()).unwrap_or(value);
  let text = text_content(value);
  let parsed = text.as_deref().and_then(|text| serde_json::from_str::<Value>(text).ok());
  // Never interpret ordinary result snippets as throttle or authentication
  // notices. Inspect only a non-result envelope or an explicitly flagged error.
  if !flagged && (successful_payload(payload) || parsed.as_ref().is_some_and(successful_payload)) {
    return None;
  }
  let error = if value.get("structuredContent").is_some_and(Value::is_object) {
    payload
  } else {
    parsed.as_ref().unwrap_or(payload)
  };
  let mut messages = Vec::new();
  for field in ["/code", "/message", "/error", "/error/code", "/error/message", "/data/code", "/data/message"] {
    if let Some(message) = error.pointer(field).and_then(Value::as_str) {
      messages.push(message);
    }
  }
  if parsed.is_none()
    && let Some(text) = &text
  {
    messages.push(text);
  }
  let message = messages.join("\n").to_ascii_lowercase().replace(['_', '-'], " ");
  let keyless_access_rejected = message.contains("keyless access not available")
    || message.contains("anonymous keyless access is unavailable for this request");
  let status = error.get("status").or_else(|| error.get("statusCode")).and_then(Value::as_u64);
  let kind = if message.contains("quota exhausted")
    || message.contains("daily limit")
    || message.contains("credit limit")
    || message.contains("insufficient credit")
    || message.contains("credits exhausted")
    || status == Some(402)
  {
    ProviderErrorKind::QuotaExhausted
  } else if message.contains("keyless limit reached")
    || message.contains("rate limit")
    || message.contains("too many requests")
    || status == Some(429)
  {
    ProviderErrorKind::RateLimited
  } else if message.contains("keyless eligibility unavailable")
    || message.contains("temporarily unavailable")
    || message.contains("timed out")
    || message.contains("timeout")
    || status.is_some_and(|status| status == 408 || (500..=599).contains(&status))
  {
    ProviderErrorKind::Transient
  } else if keyless_access_rejected
    || message.contains("keyless tool not available")
    || message.contains("credential invalid")
    || message.contains("unauthorized")
    || message.contains("api key")
    || message.contains("forbidden")
    || message.contains("blocked")
    || matches!(status, Some(401 | 403))
  {
    ProviderErrorKind::Blocked
  } else if message.contains("invalid parameter")
    || message.contains("invalid query")
    || matches!(status, Some(400 | 413 | 414 | 422))
  {
    ProviderErrorKind::InvalidRequest
  } else if flagged
    || error.get("success") == Some(&Value::Bool(false))
    || error.get("error").is_some_and(|value| !value.is_null())
    || error.get("code").is_some()
  {
    ProviderErrorKind::UnexpectedResponse
  } else {
    return None;
  };
  let retry_after = [
    "/retry_after_seconds",
    "/error/retry_after_seconds",
    "/data/retry_after_seconds",
    "/next_actions/0/after_seconds",
  ]
  .into_iter()
  .find_map(|path| error.pointer(path).and_then(Value::as_u64))
  .map(Duration::from_secs);
  if kind == ProviderErrorKind::Blocked && keyless_access_rejected {
    // Every observed keyless request has been rejected this way (7 of 7 on
    // 2026-10-24, two networks). Probe at most daily; the router persists a
    // day-long Blocked hint, so restarts do not bring the retries back.
    return Some(
      ProviderError::new(kind, "Firecrawl rejected anonymous keyless access.")
        .with_error_payload(error)
        .with_retry_after(retry_after)
        .with_retry_after(Some(QUOTA_RETRY_DELAY)),
    );
  }
  Some(
    ProviderError::new(kind, super::reported_error("Firecrawl MCP", kind))
      .with_error_payload(error)
      .with_retry_after(retry_after),
  )
}

fn successful_payload(value: &Value) -> bool {
  value.get("success") == Some(&Value::Bool(true))
    && value.pointer("/data/web").is_some_and(Value::is_array)
    && !value.get("error").is_some_and(|error| !error.is_null())
}

fn http_error(status: u16, details: Option<&Value>) -> ProviderErrorKind {
  match status {
    400 | 413 | 414 | 422 => ProviderErrorKind::InvalidRequest,
    // A keyless eligibility rejection can recover; no account key to correct.
    401 | 403 => ProviderErrorKind::Blocked,
    402 => ProviderErrorKind::QuotaExhausted,
    404 | 408 | 500..=599 => ProviderErrorKind::Transient,
    429 => details
      .and_then(tool_error)
      .map(|error| error.kind)
      .filter(|kind| matches!(kind, ProviderErrorKind::RateLimited | ProviderErrorKind::QuotaExhausted))
      .unwrap_or(ProviderErrorKind::RateLimited),
    _ => ProviderErrorKind::UnexpectedResponse,
  }
}

fn invalid_response() -> ProviderError {
  ProviderError::new(ProviderErrorKind::UnexpectedResponse, "Firecrawl MCP search returned an invalid result payload.")
}
