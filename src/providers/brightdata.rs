// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::header::HeaderValue;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};

use super::mcp_http::{Endpoint, Session};
use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
  SearchResult,
};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESULTS: usize = 30;
const MAX_SNIPPET_CHARS: usize = 1000;
// https://brightdata.com/pricing/mcp-server (reviewed 2026-10-23).
// Normalize the 5,000-request monthly allowance into existing budget units.
// These are local scheduling estimates, not an upstream monetary balance.
const REQUEST_COST_USD_MICROS: u64 = 1500;
pub const FREE_ALLOWANCE_CENTS: u64 = 5000 * REQUEST_COST_USD_MICROS / 10_000;
const ENDPOINT: Endpoint = Endpoint {
  name: "Bright Data",
  url: "https://mcp.brightdata.com/mcp",
  tool: "search_engine",
  headers: &[],
  tool_error,
  http_error,
};

/// Authenticated hosted MCP search. One Google search per attempt, without
/// pagination or retries. Other tools/users share the upstream free allowance;
/// the shared budget wrapper only accounts for this service's attempts.
pub struct BrightData {
  client: Client,
  // Contains credentials: never derive Debug or include this URL in errors.
  url: Url,
}

impl BrightData {
  pub fn new(api_key: &str) -> ProviderResult<Self> {
    let api_key = api_key.trim();
    if api_key.is_empty() || HeaderValue::from_str(api_key).is_err() {
      return Err(ProviderError::new(
        ProviderErrorKind::InvalidCredentials,
        "providers.brightdata.api_key must be a nonempty valid HTTP header value.",
      ));
    }
    let mut url = Url::parse(ENDPOINT.url)
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Bright Data endpoint."))?;
    // Encode the token as a single query value; never concatenate credentials.
    url.query_pairs_mut().append_pair("token", api_key).append_pair("tools", ENDPOINT.tool);
    let client = Client::builder()
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(10))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Bright Data HTTP client."))?;
    Ok(Self { client, url })
  }

  async fn search_once(&self, request: SearchRequest<'_>) -> ProviderResult<SearchResponse> {
    let max_results = result_limit(&request)?;
    let mut session = Session::new(self.client.clone(), &ENDPOINT).with_url(self.url.clone());
    session.prepare().await?;
    // Google returns JSON; Bing and Yandex return Markdown. The tool has no
    // count argument. Apply the caller's result cap after normalization.
    let result = session.call(json!({ "query": request.query.trim(), "engine": "google" })).await?;
    let payload: SearchPayload =
      serde_json::from_value(result_payload(&result).ok_or_else(invalid_response)?).map_err(|_| invalid_response())?;
    let mut seen = HashSet::new();
    let results = payload
      .organic
      .into_iter()
      .filter_map(|entry| {
        let url = Url::parse(entry.link.trim()).ok()?;
        let title = entry.title.trim();
        if !matches!(url.scheme(), "http" | "https")
          || url.host_str().is_none()
          || title.is_empty()
          || !seen.insert(url.to_string())
        {
          return None;
        }
        Some(SearchResult {
          title: title.to_owned(),
          url: url.to_string(),
          snippet: entry.description.trim().chars().take(MAX_SNIPPET_CHARS).collect(),
          metadata: None,
        })
      })
      .take(max_results)
      .collect();
    Ok(SearchResponse { results, reported_cost_usd_micros: None })
  }
}

impl SearchProvider for BrightData {
  fn name(&self) -> &'static str {
    "brightdata"
  }

  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    // Conservative local pacing, not a documented upstream rate limit.
    NonZeroU32::new(5)
  }

  fn max_concurrent_requests(&self) -> Option<NonZeroU32> {
    // Local policy, independent of the monthly free allowance.
    Some(NonZeroU32::MIN)
  }

  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost> {
    result_limit(request)?;
    Ok(RequestCost::UsdMicros(REQUEST_COST_USD_MICROS))
  }

  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>> {
    Box::pin(async move {
      tokio::time::timeout(SEARCH_TIMEOUT, self.search_once(request))
        .await
        .unwrap_or_else(|_| {
          Err(ProviderError::new(ProviderErrorKind::Transient, "Bright Data search exceeded its 20-second deadline."))
        })
        .map_err(|mut error| {
          // Even a syntactically safe upstream rejection code can echo a token.
          // Retain the classification, HTTP status and retry timing only.
          error.rejection_code = None;
          error
        })
    })
  }
}

fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  if request.query.trim().is_empty() || request.max_results == 0 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "Bright Data search requires a query and a positive result limit.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

#[derive(Deserialize)]
struct SearchPayload {
  organic: Vec<OrganicResult>,
}

#[derive(Deserialize)]
struct OrganicResult {
  title: String,
  link: String,
  #[serde(default)]
  description: String,
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

fn result_payload(value: &Value) -> Option<Value> {
  if let Some(payload) = value.get("structuredContent").filter(|value| !value.is_null()) {
    Some(payload.clone())
  } else {
    let text = text_content(value)?;
    if let Ok(payload) = serde_json::from_str(&text) {
      return Some(payload);
    }
    // The official server wraps external results in a notice and nonce-bound
    // markers. Parse only the JSON between matching outer markers; never
    // search arbitrary prose or snippets for a JSON-looking substring.
    let (notice, rest) = text.trim().split_once('\n')?;
    if !notice.starts_with("SECURITY NOTICE: ") {
      return None;
    }
    let (begin, body) = rest.split_once('\n')?;
    let nonce = begin.strip_prefix("=====UNTRUSTED_")?.strip_suffix("_BEGIN=====")?;
    if nonce.len() != 32 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
      return None;
    }
    let end = format!("\n=====UNTRUSTED_{nonce}_END=====");
    serde_json::from_str(body.strip_suffix(&end)?).ok()
  }
}

fn tool_error(value: &Value) -> Option<ProviderError> {
  let flagged = value.get("isError").is_some_and(|value| value != &Value::Bool(false));
  let parsed = result_payload(value);
  let payload = parsed.as_ref().unwrap_or(value);
  // Search snippets can themselves mention quota or authentication failures.
  if !flagged && payload.get("organic").is_some_and(Value::is_array) {
    return None;
  }
  let mut messages = Vec::new();
  for field in ["/message", "/code", "/error", "/error/message", "/error/code", "/detail"] {
    if let Some(message) = payload.pointer(field).and_then(Value::as_str) {
      messages.push(message.to_owned());
    }
  }
  if parsed.is_none()
    && let Some(text) = text_content(value)
  {
    messages.push(text);
  }
  let message = messages.join("\n").to_ascii_lowercase().replace(['_', '-'], " ");
  let status = payload.get("status").or_else(|| payload.get("status_code")).and_then(Value::as_u64);
  let kind = if message.contains("quota")
    || message.contains("credit")
    || message.contains("balance")
    || message.contains("funds")
    || message.contains("monthly limit")
    || status == Some(402)
  {
    ProviderErrorKind::QuotaExhausted
  } else if message.contains("rate limit") || message.contains("too many requests") || status == Some(429) {
    ProviderErrorKind::RateLimited
  } else if message.contains("unauthorized")
    || message.contains("authentication")
    || message.contains("invalid token")
    || message.contains("invalid api")
    || status == Some(401)
  {
    ProviderErrorKind::InvalidCredentials
  } else if message.contains("forbidden") || message.contains("blocked") || status == Some(403) {
    ProviderErrorKind::Blocked
  } else if message.contains("timeout")
    || message.contains("timed out")
    || status.is_some_and(|status| status == 408 || (500..=599).contains(&status))
  {
    ProviderErrorKind::Transient
  } else if message.contains("invalid parameter")
    || message.contains("invalid query")
    || matches!(status, Some(400 | 413 | 414 | 422))
  {
    ProviderErrorKind::InvalidRequest
  } else if flagged || payload.get("error").is_some_and(|error| !error.is_null()) || payload.get("code").is_some() {
    ProviderErrorKind::UnexpectedResponse
  } else {
    return None;
  };
  Some(ProviderError::new(kind, super::reported_error("Bright Data", kind)).with_error_payload(payload))
}

fn http_error(status: u16, details: Option<&Value>) -> ProviderErrorKind {
  match status {
    400 | 413 | 414 | 422 => ProviderErrorKind::InvalidRequest,
    401 => ProviderErrorKind::InvalidCredentials,
    402 => ProviderErrorKind::QuotaExhausted,
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
  ProviderError::new(ProviderErrorKind::UnexpectedResponse, "Bright Data search returned an invalid result payload.")
}
