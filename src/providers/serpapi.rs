// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::header::{ACCEPT, HeaderValue};
use reqwest::{Client, Response, Url};
use serde::Deserialize;
use serde_json::Value;

use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
  SearchResult, SearchResultMetadata,
};

const SEARCH_URL: &str = "https://serpapi.com/search.json";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESULTS: usize = 30;
const MAX_SNIPPET_CHARS: usize = 1000;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

// https://serpapi.com/ (documentation reviewed 2026-10-23).
// Normalize the free plan's 250 monthly searches using Starter's $25/1,000
// rate for the existing ledger. These are scheduling units, not an account
// balance or a claim that searches on the free plan have a monetary charge.
const REQUEST_COST_USD_MICROS: u64 = 25_000;
pub const FREE_ALLOWANCE_CENTS: u64 = 250 * REQUEST_COST_USD_MICROS / 10_000;
const MAX_REQUESTS_PER_HOUR: u32 = 50;

/// One synchronous Google result page per attempt, without retries or archive
/// polling. The shared budget wrapper reserves the estimated search allowance.
pub struct SerpApi {
  client: Client,
  // Contains the private key: never derive Debug or expose request URLs.
  url: Url,
}

impl SerpApi {
  pub fn new(api_key: &str) -> ProviderResult<Self> {
    let api_key = api_key.trim();
    if api_key.is_empty() || HeaderValue::from_str(api_key).is_err() {
      return Err(ProviderError::new(
        ProviderErrorKind::InvalidCredentials,
        "providers.serpapi.api_key must be a nonempty valid HTTP header value.",
      ));
    }
    let mut url = Url::parse(SEARCH_URL)
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build SerpApi endpoint."))?;
    url.query_pairs_mut().append_pair("api_key", api_key);
    let client = Client::builder()
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(10))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build SerpApi HTTP client."))?;
    Ok(Self { client, url })
  }
}

impl SearchProvider for SerpApi {
  fn name(&self) -> &'static str {
    "serpapi"
  }

  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    // Any minute is also bounded by the free account's hourly throughput.
    NonZeroU32::new(MAX_REQUESTS_PER_HOUR)
  }

  fn additional_rate_limits(&self) -> Vec<super::RequestRateLimit> {
    // The free plan publishes 50/hour, not a per-second spacing requirement.
    // Preserve 10% timing headroom, as for other upstream rolling windows.
    vec![super::RequestRateLimit { max_requests: MAX_REQUESTS_PER_HOUR, window: Duration::from_secs(3960) }]
  }

  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost> {
    result_limit(request)?;
    Ok(RequestCost::UsdMicros(REQUEST_COST_USD_MICROS))
  }

  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>> {
    Box::pin(async move {
      let max_results = result_limit(&request)?;
      let mut response = self
        .client
        .get(self.url.clone())
        .header(ACCEPT, "application/json")
        .query(&[("engine", "google"), ("q", request.query.trim()), ("async", "false")])
        .send()
        .await
        .map_err(|error| {
          // reqwest errors include the URL, which contains the private key.
          let message =
            if error.is_timeout() { "SerpApi search request timed out." } else { "SerpApi search request failed." };
          ProviderError::new(ProviderErrorKind::Transient, message)
        })?;

      let status = response.status();
      let retry_after = super::retry_after(response.headers());
      if !status.is_success() {
        let body = super::read_error_body(&mut response).await;
        return Err(
          response_error(status.as_u16(), body.json.as_ref())
            .with_retry_after(retry_after)
            .with_upstream_detail(body.detail)
            .without_charge(),
        );
      }

      let body = read_body(&mut response).await?;
      let payload: Value = serde_json::from_slice(&body).map_err(|_| invalid_response())?;
      match payload.pointer("/search_metadata/status").and_then(Value::as_str) {
        Some("Success") => {}
        Some("Error") => {
          // An explicitly failed search is not billed. Successful empty results
          // can also carry an `error` string; status takes precedence over it.
          return Err(response_error(status.as_u16(), Some(&payload)).with_retry_after(retry_after).without_charge());
        }
        // Never poll or replay a queued/processing response against the quota.
        _ => return Err(invalid_response().with_retry_after(retry_after)),
      }
      let payload: SearchPayload = serde_json::from_value(payload).map_err(|_| invalid_response())?;
      Ok(SearchResponse {
        results: normalize_results(payload.organic_results.unwrap_or_default(), max_results),
        // Cached searches are free, but no documented cache-hit signal is used.
        // Retain the conservative reservation for successful responses.
        reported_cost_usd_micros: None,
      })
    })
  }
}

fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  if request.query.trim().is_empty() || request.max_results == 0 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "SerpApi search requires a query and a positive result limit.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

fn response_error(status: u16, details: Option<&Value>) -> ProviderError {
  let exhausted = details
    .and_then(|value| value.get("error"))
    .and_then(Value::as_str)
    .is_some_and(|message| message.to_ascii_lowercase().contains("your account has run out of searches"));
  let kind = match status {
    400 | 413 | 414 | 422 => ProviderErrorKind::InvalidRequest,
    401 => ProviderErrorKind::InvalidCredentials,
    402 => ProviderErrorKind::QuotaExhausted,
    403 => ProviderErrorKind::Blocked,
    200..=299 | 429 if exhausted => ProviderErrorKind::QuotaExhausted,
    429 => ProviderErrorKind::RateLimited,
    200..=299 | 408 | 500..=599 => ProviderErrorKind::Transient,
    _ => ProviderErrorKind::UnexpectedResponse,
  };
  let mut error = ProviderError::new(kind, "SerpApi search reported an upstream error.").with_http_status(status);
  if let Some(details) = details {
    error = error.with_error_payload(details);
    // Even a syntactically safe machine code could echo a credential.
    error.rejection_code = None;
  }
  error
}

async fn read_body(response: &mut Response) -> ProviderResult<Vec<u8>> {
  let mut body = Vec::new();
  while let Some(chunk) = response
    .chunk()
    .await
    .map_err(|_| ProviderError::new(ProviderErrorKind::Transient, "Could not read SerpApi search response."))?
  {
    if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
      return Err(ProviderError::new(
        ProviderErrorKind::UnexpectedResponse,
        "SerpApi search response exceeded the size limit.",
      ));
    }
    body.extend_from_slice(&chunk);
  }
  Ok(body)
}

#[derive(Deserialize)]
struct SearchPayload {
  organic_results: Option<Vec<OrganicResult>>,
}

#[derive(Deserialize)]
struct OrganicResult {
  link: String,
  title: Option<String>,
  snippet: Option<String>,
  date: Option<String>,
}

fn normalize_results(results: Vec<OrganicResult>, max_results: usize) -> Vec<SearchResult> {
  let mut seen = HashSet::new();
  results
    .into_iter()
    .filter_map(|entry| {
      let url = Url::parse(entry.link.trim()).ok()?;
      if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() || !seen.insert(url.to_string()) {
        return None;
      }
      let url = url.to_string();
      let title = entry.title.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
      let title = if title.is_empty() { url.clone() } else { title };
      let snippet = entry.snippet.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
      let snippet = snippet.chars().take(MAX_SNIPPET_CHARS).collect();
      Some(SearchResult {
        title,
        url,
        snippet,
        metadata: Some(SearchResultMetadata {
          published_date: entry.date,
          author: None,
          excerpts: Vec::new(),
          description: None,
        }),
      })
    })
    .take(max_results)
    .collect()
}

fn invalid_response() -> ProviderError {
  ProviderError::new(ProviderErrorKind::UnexpectedResponse, "SerpApi search returned an invalid result payload.")
}
