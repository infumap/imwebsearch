// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::num::NonZeroU32;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use super::mcp_http::{Endpoint, Session};
use super::tavily::{TavilySearchResult, normalize_results};
use super::{
  ProviderError, ProviderErrorKind, ProviderResult, QUOTA_RETRY_DELAY, RequestCost, SearchProvider, SearchRequest,
  SearchResponse,
};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
// Soft routing weight only: upstream daily capacity is undocumented.
pub const ASSUMED_REQUESTS_PER_DAY: u32 = 100;
const MAX_RESULTS: usize = 20;
// https://docs.tavily.com/documentation/keyless (reviewed 2026-10-23).
// The header selects free keyless access; credentials would take precedence.
const ENDPOINT: Endpoint = Endpoint {
  name: "Tavily MCP",
  url: "https://mcp.tavily.com/mcp/",
  tool: "tavily_search",
  headers: &[("X-Tavily-Access-Mode", "keyless")],
  tool_error,
  http_error,
};

/// Independent, opt-in keyless search. Upstream publishes neither the numeric
/// cap nor its reset boundary. The router supplies a soft allocation weight,
/// independently of account credits and remote quota cooldowns.
/// The shared router persists rate/quota cooldowns under the tavily_mcp name.
pub struct TavilyMcp {
  client: Client,
  requests_per_minute: NonZeroU32,
}

impl TavilyMcp {
  pub fn new(requests_per_minute: NonZeroU32) -> ProviderResult<Self> {
    let client = Client::builder()
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(10))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Tavily MCP HTTP client."))?;
    Ok(Self { client, requests_per_minute })
  }

  async fn search_once(&self, request: SearchRequest<'_>) -> ProviderResult<SearchResponse> {
    let max_results = result_limit(&request)?;
    let mut session = Session::new(self.client.clone(), &ENDPOINT);
    session.prepare().await?;
    let result = session
      .call(json!({
        "query": request.query.trim(),
        "search_depth": "basic",
        "topic": "general",
        // Official MCP schema has a minimum of five. Trim locally afterward.
        "max_results": max_results.max(5),
        "include_images": false,
        "include_image_descriptions": false,
        "include_raw_content": false,
        "include_favicon": false
      }))
      .await?;
    let results = if let Some(payload) = result.get("structuredContent").filter(|value| !value.is_null()) {
      serde_json::from_value::<SearchPayload>(payload.clone()).map_err(|_| invalid_response())?.results
    } else {
      let text = text_content(&result).ok_or_else(invalid_response)?;
      match serde_json::from_str::<SearchPayload>(&text) {
        Ok(payload) => payload.results,
        Err(_) => formatted_results(&text)?,
      }
    };
    Ok(SearchResponse { results: normalize_results(results, max_results), reported_cost_usd_micros: None })
  }
}

impl SearchProvider for TavilyMcp {
  fn name(&self) -> &'static str {
    "tavily_mcp"
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
      // One attempt, including initialization. Never replay a cancelled call or
      // fall back to credentials, OAuth, payments or upstream continuation tasks.
      tokio::time::timeout(SEARCH_TIMEOUT, self.search_once(request)).await.unwrap_or_else(|_| {
        Err(ProviderError::new(ProviderErrorKind::Transient, "Tavily MCP search exceeded its 20-second deadline."))
      })
    })
  }
}

fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  if request.query.trim().is_empty() || request.max_results == 0 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "Tavily MCP search requires a query and a positive result limit.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

#[derive(Deserialize)]
struct SearchPayload {
  results: Vec<TavilySearchResult>,
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

/// Official format-results.ts emits a Detailed Results section with Title,
/// optional ID, URL and multiline Content fields. Reject other prose, including
/// keyless continuation notices, rather than treating it as a successful empty.
fn formatted_results(text: &str) -> ProviderResult<Vec<TavilySearchResult>> {
  let text = text.replace("\r\n", "\n");
  let mut lines = text.lines().peekable();
  if !lines.by_ref().any(|line| line == "Detailed Results:") {
    return Err(invalid_response());
  }
  let mut results = Vec::new();
  let mut current: Option<TavilySearchResult> = None;
  let mut content = false;
  let mut separator = true;
  while let Some(line) = lines.next() {
    let mut following = lines.clone();
    let mut next = following.next();
    if next.is_some_and(|line| line.starts_with("ID: ")) {
      next = following.next();
    }
    let title_header = line.starts_with("Title: ") && next.is_some_and(|line| line.starts_with("URL: "));
    let url_header =
      line.starts_with("URL: ") && next.is_none_or(|line| line.is_empty() || line.starts_with("Content: "));
    if separator && (title_header || url_header) {
      if let Some(result) = current.take() {
        if result.url.is_empty() {
          return Err(invalid_response());
        }
        results.push(result);
      }
      current = Some(TavilySearchResult { title: None, url: String::new(), content: None, published_date: None });
      content = false;
    }
    if let Some(result) = &mut current {
      if !content {
        if let Some(title) = line.strip_prefix("Title: ") {
          result.title = Some(title.to_owned());
        } else if let Some(url) = line.strip_prefix("URL: ") {
          result.url = url.to_owned();
        } else if let Some(text) = line.strip_prefix("Content: ") {
          result.content = Some(text.to_owned());
          content = true;
        } else if !line.is_empty() && !line.starts_with("ID: ") {
          return Err(invalid_response());
        }
      } else {
        let text = result.content.get_or_insert_default();
        text.push('\n');
        text.push_str(line);
      }
    } else if !line.is_empty() {
      return Err(invalid_response());
    }
    separator = line.is_empty();
  }
  if let Some(result) = current {
    if result.url.is_empty() {
      return Err(invalid_response());
    }
    results.push(result);
  }
  Ok(results)
}

fn tool_error(value: &Value) -> Option<ProviderError> {
  let flagged = value.get("isError").is_some_and(|value| value != &Value::Bool(false));
  let payload = value.get("structuredContent").filter(|value| !value.is_null()).unwrap_or(value);
  // Successful result snippets may themselves discuss rate limits or errors.
  if !flagged && payload.get("results").is_some_and(Value::is_array) {
    return None;
  }
  let text = text_content(value);
  if !flagged
    && let Some(text) = &text
    && (serde_json::from_str::<SearchPayload>(text).is_ok() || formatted_results(text).is_ok())
  {
    return None;
  }
  // Some MCP deployments wrap JSON errors in a text block, others return prose
  // without isError. Inspect only this bounded non-result envelope.
  let parsed = text.as_deref().and_then(|text| serde_json::from_str::<Value>(text).ok());
  let error = parsed.as_ref().unwrap_or(payload);
  let mut messages = Vec::new();
  for field in ["/message", "/code", "/error/message", "/error/code", "/detail/message", "/detail/error_code"] {
    if let Some(message) = error.pointer(field).and_then(Value::as_str) {
      messages.push(message);
    }
  }
  if let Some(text) = &text {
    messages.push(text);
  }
  let text = messages.join("\n").to_ascii_lowercase().replace(['_', '-'], " ");
  let kind = if text.contains("quota")
    || text.contains("credit")
    || text.contains("usage limit")
    || text.contains("daily limit")
  {
    ProviderErrorKind::QuotaExhausted
  } else if text.contains("rate limit")
    || text.contains("too many requests")
    || (text.contains("keyless") && (text.contains("limit") || text.contains("cap")))
  {
    ProviderErrorKind::RateLimited
  } else if text.contains("api key") || text.contains("unauthorized") || text.contains("authentication") {
    ProviderErrorKind::InvalidCredentials
  } else if text.contains("forbidden") || text.contains("blocked") {
    ProviderErrorKind::Blocked
  } else if text.contains("invalid parameter") || text.contains("invalid query") {
    ProviderErrorKind::InvalidRequest
  } else if flagged || error.get("error").is_some() || error.get("code").is_some() {
    ProviderErrorKind::UnexpectedResponse
  } else {
    return None;
  };
  let retry_after = error
    .pointer("/error/retry_after_seconds")
    .or_else(|| error.get("retry_after_seconds"))
    .and_then(Value::as_u64)
    .or_else(|| {
      text.lines().find_map(|line| line.strip_prefix("retry after: ")?.strip_suffix('s')?.trim().parse().ok())
    })
    .map(Duration::from_secs);
  let build = |kind| {
    ProviderError::new(kind, super::reported_error("Tavily MCP", kind))
      .with_error_payload(error)
      .with_retry_after(retry_after)
  };
  let reported = build(kind);
  // The monthly keyless cap arrives with a short retry hint (387 s observed)
  // that is not its reset: honoring it would re-probe every few minutes for the
  // rest of the month. Probe at most daily instead; the longer delay wins.
  let monthly_cap = reported.rejection_code.as_deref().is_some_and(|code| code.starts_with("monthly_cap_reached"))
    || text.contains("monthly keyless");
  if monthly_cap {
    let reported =
      if kind == ProviderErrorKind::QuotaExhausted { reported } else { build(ProviderErrorKind::QuotaExhausted) };
    return Some(reported.with_retry_after(Some(QUOTA_RETRY_DELAY)));
  }
  Some(reported)
}

fn http_error(status: u16, details: Option<&Value>) -> ProviderErrorKind {
  match status {
    400 | 413 | 414 | 422 => ProviderErrorKind::InvalidRequest,
    401 => ProviderErrorKind::InvalidCredentials,
    402 | 432 | 433 => ProviderErrorKind::QuotaExhausted,
    403 => ProviderErrorKind::Blocked,
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
  ProviderError::new(ProviderErrorKind::UnexpectedResponse, "Tavily MCP search returned an invalid result payload.")
}
