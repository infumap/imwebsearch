// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use futures_util::future::BoxFuture;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use reqwest::{Client, Response, Url};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
  SearchResult,
};

const SEARCH_URL: &str = "https://api.context.dev/v1/web/search";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESULTS: usize = 30;
const MAX_SNIPPET_CHARS: usize = 1000;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

// https://www.context.dev/pricing and https://docs.context.dev/account/credits
// (documentation reviewed 2026-10-23). Normalize the recurring 1,000 free
// credits at Developer's $2.20/1,000 overage rate for the shared ledger.
// These are scheduling units, not the account's monetary balance.
const CREDIT_COST_USD_MICROS: u64 = 2200;
pub const FREE_ALLOWANCE_CENTS: u64 = 1000 * CREDIT_COST_USD_MICROS / 10_000;

/// One authenticated web search per attempt, without retries, page scraping,
/// highlights, query fan-out or pagination. Free credits are shared by the
/// organization; the budget wrapper accounts only for this service's searches.
pub struct Context {
  client: Client,
  api_key: HeaderValue,
}

impl Context {
  pub fn new(api_key: &str) -> ProviderResult<Self> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
      return Err(ProviderError::new(
        ProviderErrorKind::InvalidCredentials,
        "providers.context.api_key must not be empty.",
      ));
    }
    let mut api_key = HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|_| {
      ProviderError::new(
        ProviderErrorKind::InvalidCredentials,
        "providers.context.api_key is not a valid HTTP header value.",
      )
    })?;
    api_key.set_sensitive(true);
    let client = Client::builder()
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(10))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Context.dev HTTP client."))?;
    Ok(Self { client, api_key })
  }
}

impl SearchProvider for Context {
  fn name(&self) -> &'static str {
    "context"
  }

  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    // Conservative local pacing, not a published upstream per-minute limit.
    NonZeroU32::new(5)
  }

  fn max_concurrent_requests(&self) -> Option<NonZeroU32> {
    // https://www.context.dev/pricing: Free permits two concurrent requests.
    NonZeroU32::new(2)
  }

  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost> {
    let limit = result_limit(request)?;
    Ok(RequestCost::UsdMicros(upstream_result_limit(limit).div_ceil(10) as u64 * CREDIT_COST_USD_MICROS))
  }

  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>> {
    Box::pin(async move {
      let max_results = result_limit(&request)?;
      let mut response = self
        .client
        .post(SEARCH_URL)
        .header(AUTHORIZATION, self.api_key.clone())
        .header(ACCEPT, "application/json")
        .json(&json!({
          "query": request.query.trim(),
          "numResults": upstream_result_limit(max_results),
          "queryFanout": false,
          "markdownOptions": { "enabled": false },
          "highlightsOptions": { "enabled": false },
          "descriptionMaxCharacters": MAX_SNIPPET_CHARS
        }))
        .send()
        .await
        .map_err(|error| {
          let message = if error.is_timeout() {
            "Context.dev search request timed out."
          } else {
            "Context.dev search request failed."
          };
          ProviderError::new(ProviderErrorKind::Transient, message)
        })?;

      let status = response.status();
      if !status.is_success() {
        let retry_after = super::retry_after(response.headers());
        let body = super::read_error_body(&mut response).await;
        return Err(
          response_error(status.as_u16(), body.json.as_ref())
            .with_retry_after(retry_after)
            .with_upstream_detail(body.detail),
        );
      }
      let header_credits = response
        .headers()
        .get("x-credits-used")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
      let body = read_body(&mut response).await?;
      let payload: SearchPayload = serde_json::from_slice(&body).map_err(|_| invalid_response())?;
      let reported_cost_usd_micros = payload
        .key_metadata
        .and_then(|metadata| metadata.credits_consumed)
        .or(header_credits)
        .and_then(|credits| credits.checked_mul(CREDIT_COST_USD_MICROS));
      let mut seen = HashSet::new();
      let results = payload
        .results
        .into_iter()
        .filter_map(|entry| {
          let url = Url::parse(entry.url.trim()).ok()?;
          if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || !seen.insert(url.to_string()) {
            return None;
          }
          let url = url.to_string();
          let title = entry.title.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
          let title = if title.is_empty() { url.clone() } else { title };
          let snippet = entry.description.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
          Some(SearchResult { title, url, snippet: snippet.chars().take(MAX_SNIPPET_CHARS).collect(), metadata: None })
        })
        .take(max_results)
        .collect();
      Ok(SearchResponse { results, reported_cost_usd_micros })
    })
  }
}

fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  let query = request.query.trim();
  if query.is_empty() || request.max_results == 0 || query.encode_utf16().count() > 500 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "Context.dev search requires a query of at most 500 UTF-16 code units and a positive result limit.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

fn upstream_result_limit(limit: usize) -> usize {
  // The reference permits 1–100; the overview says 10–100. Use their common
  // range and trim locally. Both five and ten results reserve one credit.
  limit.max(10)
}

fn response_error(status: u16, details: Option<&Value>) -> ProviderError {
  let code = details.and_then(|value| value.get("error_code")).and_then(Value::as_str);
  // Context.dev documents USAGE_EXCEEDED with HTTP 401, separately from
  // invalid credentials. Never disable a valid key merely for depleted quota.
  let kind = match (status, code) {
    (_, Some("USAGE_EXCEEDED")) | (402, _) => ProviderErrorKind::QuotaExhausted,
    (400 | 413 | 414 | 422, _) => ProviderErrorKind::InvalidRequest,
    (401, _) => ProviderErrorKind::InvalidCredentials,
    (403, _) => ProviderErrorKind::Blocked,
    (429, _) => ProviderErrorKind::RateLimited,
    (408 | 500..=599, _) => ProviderErrorKind::Transient,
    _ => ProviderErrorKind::UnexpectedResponse,
  };
  let mut error = ProviderError::new(kind, "Context.dev search reported an upstream error.").with_http_status(status);
  if let Some(details) = details {
    error = error.with_error_payload(details);
    if let Some(data) = details.get("error_data") {
      error = error.with_error_payload(data);
      if kind == ProviderErrorKind::QuotaExhausted {
        let refill = data
          .get("next_refill_at")
          .and_then(Value::as_str)
          .and_then(|value| value.parse::<jiff::Timestamp>().ok())
          .map(|time| SystemTime::from(time).duration_since(SystemTime::now()).unwrap_or(Duration::ZERO));
        error = error.with_retry_after(refill);
      }
    }
  }
  // Retain only documented codes; arbitrary upstream strings can echo secrets.
  error.rejection_code = code
    .filter(|code| {
      matches!(
        *code,
        "USAGE_EXCEEDED" | "RATE_LIMITED" | "INPUT_VALIDATION_ERROR" | "REQUEST_TIMEOUT" | "INTERNAL_ERROR"
      )
    })
    .map(str::to_owned);
  let charged = details.and_then(|value| value.pointer("/error_data/credits_charged")).and_then(Value::as_u64);
  // Credits documentation excludes validation, auth/permission, timeout,
  // throttling and server errors from billing. Unknown failures stay reserved.
  if charged == Some(0)
    || (charged.is_none() && matches!(status, 400 | 401 | 402 | 403 | 408 | 413 | 414 | 422 | 429 | 500..=599))
  {
    error = error.without_charge();
  }
  error
}

async fn read_body(response: &mut Response) -> ProviderResult<Vec<u8>> {
  let mut body = Vec::new();
  while let Some(chunk) = response
    .chunk()
    .await
    .map_err(|_| ProviderError::new(ProviderErrorKind::Transient, "Could not read Context.dev search response."))?
  {
    if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
      return Err(invalid_response());
    }
    body.extend_from_slice(&chunk);
  }
  Ok(body)
}

#[derive(Deserialize)]
struct SearchPayload {
  results: Vec<WebResult>,
  key_metadata: Option<KeyMetadata>,
}

#[derive(Deserialize)]
struct KeyMetadata {
  credits_consumed: Option<u64>,
}

#[derive(Deserialize)]
struct WebResult {
  url: String,
  title: Option<String>,
  description: Option<String>,
}

fn invalid_response() -> ProviderError {
  ProviderError::new(ProviderErrorKind::UnexpectedResponse, "Context.dev search returned an invalid result payload.")
}
