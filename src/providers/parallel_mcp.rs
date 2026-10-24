// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::num::NonZeroU32;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::Client;
use serde_json::{Value, json};

use super::mcp_http::{Endpoint, Session};
use super::parallel::{self, ParallelSearchResponse, normalize_results, result_limit};
use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
// Soft allocation weight, not a published upstream allowance.
pub const ASSUMED_REQUESTS_PER_DAY: u32 = 100;
// https://docs.parallel.ai/integrations/mcp/search-mcp (reviewed 2026-10-23)
const ENDPOINT: Endpoint = Endpoint {
  name: "Parallel MCP",
  url: "https://search.parallel.ai/mcp",
  tool: "web_search",
  headers: &[],
  tool_error,
  http_error,
};

/// Anonymous search with its own pacing, allocation and persistent cooldown.
/// Never attach credentials from the independent REST provider.
pub struct ParallelMcp {
  client: Client,
  requests_per_minute: NonZeroU32,
}

impl ParallelMcp {
  pub fn new(requests_per_minute: NonZeroU32) -> ProviderResult<Self> {
    let client = Client::builder()
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(10))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Parallel MCP HTTP client."))?;
    Ok(Self { client, requests_per_minute })
  }

  async fn search_once(&self, request: SearchRequest<'_>) -> ProviderResult<SearchResponse> {
    let max_results = result_limit(&request)?;
    let mut session = Session::new(self.client.clone(), &ENDPOINT);
    session.prepare().await?;
    // Anonymous mode/count overrides are ignored upstream. Request only the
    // documented per-call inputs and apply the caller's result cap locally.
    // Omit optional conversation/model identity: SearchRequest carries neither.
    let result =
      session.call(json!({ "objective": request.query.trim(), "search_queries": [request.query.trim()] })).await?;
    let payload = if let Some(payload) = result.get("structuredContent").filter(|value| !value.is_null()) {
      serde_json::from_value::<ParallelSearchResponse>(payload.clone()).map_err(|_| invalid_response())?
    } else {
      serde_json::from_str::<ParallelSearchResponse>(&text_content(&result).ok_or_else(invalid_response)?)
        .map_err(|_| invalid_response())?
    };
    Ok(SearchResponse { results: normalize_results(payload.results, max_results), reported_cost_usd_micros: None })
  }
}

impl SearchProvider for ParallelMcp {
  fn name(&self) -> &'static str {
    "parallel_mcp"
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
      tokio::time::timeout(SEARCH_TIMEOUT, self.search_once(request)).await.unwrap_or_else(|_| {
        Err(ProviderError::new(ProviderErrorKind::Transient, "Parallel MCP search exceeded its 20-second deadline."))
      })
    })
  }
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
  // Successful excerpts can discuss errors, quota, or authentication. Never
  // classify those as errors merely because they contain matching keywords.
  if !flagged
    && (payload.get("results").is_some_and(Value::is_array)
      || parsed.as_ref().is_some_and(|value| value.get("results").is_some_and(Value::is_array)))
  {
    return None;
  }
  let error = if value.get("structuredContent").is_some_and(Value::is_object) {
    payload
  } else {
    parsed.as_ref().unwrap_or(payload)
  };
  let mut messages = Vec::new();
  for field in ["/code", "/message", "/error", "/error/code", "/error/message", "/detail"] {
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
  let status = error.get("status").or_else(|| error.get("status_code")).and_then(Value::as_u64);
  let kind = if message.contains("quota")
    || message.contains("credit")
    || message.contains("daily limit")
    || message.contains("usage limit")
    || status == Some(402)
  {
    ProviderErrorKind::QuotaExhausted
  } else if message.contains("rate limit") || message.contains("too many requests") || status == Some(429) {
    ProviderErrorKind::RateLimited
  } else if message.contains("unauthorized")
    || message.contains("api key")
    || message.contains("authentication")
    || message.contains("forbidden")
    || message.contains("blocked")
    || matches!(status, Some(401 | 403))
  {
    // Anonymous access rejection can recover; there is no credential to fix.
    ProviderErrorKind::Blocked
  } else if message.contains("timed out")
    || message.contains("timeout")
    || message.contains("temporarily unavailable")
    || status.is_some_and(|status| status == 408 || (500..=599).contains(&status))
  {
    ProviderErrorKind::Transient
  } else if message.contains("invalid parameter")
    || message.contains("invalid query")
    || matches!(status, Some(400 | 413 | 414 | 422))
  {
    ProviderErrorKind::InvalidRequest
  } else if flagged || error.get("error").is_some_and(|error| !error.is_null()) || error.get("code").is_some() {
    ProviderErrorKind::UnexpectedResponse
  } else {
    return None;
  };
  let retry_after = error
    .get("retry_after_seconds")
    .or_else(|| error.pointer("/error/retry_after_seconds"))
    .and_then(Value::as_u64)
    .map(Duration::from_secs);
  Some(
    ProviderError::new(kind, super::reported_error("Parallel MCP", kind))
      .with_error_payload(error)
      .with_retry_after(retry_after),
  )
}

fn http_error(status: u16, details: Option<&Value>) -> ProviderErrorKind {
  match status {
    401 | 403 => ProviderErrorKind::Blocked,
    429 => details
      .and_then(tool_error)
      .map(|error| error.kind)
      .filter(|kind| matches!(kind, ProviderErrorKind::QuotaExhausted | ProviderErrorKind::RateLimited))
      .unwrap_or(ProviderErrorKind::RateLimited),
    _ => parallel::http_error(status),
  }
}

fn invalid_response() -> ProviderError {
  ProviderError::new(ProviderErrorKind::UnexpectedResponse, "Parallel MCP search returned an invalid result payload.")
}
