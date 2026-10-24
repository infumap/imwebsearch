// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::header::{ACCEPT, HeaderValue};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::json;

use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
  SearchResult, SearchResultMetadata,
};

const SEARCH_URL: &str = "https://api.parallel.ai/v1/search";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
// Stay within Fast mode's ten included results; extra results cost more.
const MAX_RESULTS: usize = 10;
const EXCERPT_MAX_CHARS: usize = 1000;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
// Fast search: $1/1,000 requests. Recurring $5/month excludes signup grants.
// https://docs.parallel.ai/getting-started/pricing
// https://parallel.ai/pricing (reviewed 2026-10-23)
const COST_USD_MICROS: u64 = 1000;
pub const FREE_ALLOWANCE_CENTS: u64 = 500;

pub struct Parallel {
  client: Client,
  api_key: HeaderValue,
}

impl Parallel {
  pub fn new(api_key: &str) -> ProviderResult<Self> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
      return Err(ProviderError::new(
        ProviderErrorKind::InvalidCredentials,
        "providers.parallel.api_key must not be empty.",
      ));
    }
    let mut api_key = HeaderValue::from_str(api_key).map_err(|_| {
      ProviderError::new(
        ProviderErrorKind::InvalidCredentials,
        "providers.parallel.api_key is not a valid HTTP header value.",
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
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Parallel HTTP client."))?;
    Ok(Self { client, api_key })
  }
}

impl SearchProvider for Parallel {
  fn name(&self) -> &'static str {
    "parallel"
  }

  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    // Default Search API limit; independent of free monetary credits.
    // https://docs.parallel.ai/getting-started/rate-limits
    NonZeroU32::new(600)
  }

  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost> {
    result_limit(request)?;
    Ok(RequestCost::UsdMicros(COST_USD_MICROS))
  }

  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>> {
    Box::pin(async move {
      let max_results = result_limit(&request)?;
      let mut response = self
        .client
        .post(SEARCH_URL)
        .header("x-api-key", self.api_key.clone())
        .header(ACCEPT, "application/json")
        .json(&json!({
          "objective": request.query.trim(),
          "search_queries": [request.query.trim()],
          "mode": "fast",
          "max_chars_total": max_results * EXCERPT_MAX_CHARS,
          "advanced_settings": {
            "max_results": max_results,
            "excerpt_settings": { "max_chars_per_result": EXCERPT_MAX_CHARS }
          }
        }))
        .send()
        .await
        .map_err(|error| {
          ProviderError::new(
            ProviderErrorKind::Transient,
            if error.is_timeout() { "Parallel search request timed out." } else { "Parallel search request failed." },
          )
        })?;
      let status = response.status();
      if !status.is_success() {
        // 429 can mean either rate limiting or exhausted quota. Preserve the
        // shared conservative cooldown when no Retry-After hint is supplied.
        return Err(
          super::response_error(
            ProviderError::new(http_error(status.as_u16()), format!("Parallel search returned HTTP {status}."))
              .with_retry_after(super::retry_after(response.headers())),
            &mut response,
          )
          .await,
        );
      }
      let mut body = Vec::new();
      while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProviderError::new(ProviderErrorKind::Transient, "Could not read Parallel search response."))?
      {
        if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
          return Err(ProviderError::new(
            ProviderErrorKind::UnexpectedResponse,
            "Parallel search response exceeded the size limit.",
          ));
        }
        body.extend_from_slice(&chunk);
      }
      let response: ParallelSearchResponse = serde_json::from_slice(&body).map_err(|_| {
        ProviderError::new(ProviderErrorKind::UnexpectedResponse, "Parallel search returned an invalid response.")
      })?;
      Ok(SearchResponse {
        results: normalize_results(response.results, max_results),
        // Usage SKUs are not a monetary balance or a reported dollar cost.
        reported_cost_usd_micros: None,
      })
    })
  }
}

pub(super) fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  if request.query.trim().is_empty() || request.max_results == 0 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "Parallel search requires a query and a positive result limit.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

#[derive(Deserialize)]
pub(super) struct ParallelSearchResponse {
  pub results: Vec<ParallelSearchResult>,
}

#[derive(Deserialize)]
pub(super) struct ParallelSearchResult {
  title: Option<String>,
  url: String,
  publish_date: Option<String>,
  excerpts: Vec<String>,
}

pub(super) fn normalize_results(results: Vec<ParallelSearchResult>, max_results: usize) -> Vec<SearchResult> {
  let mut seen = HashSet::new();
  results
    .into_iter()
    .filter_map(|result| {
      let url = Url::parse(result.url.trim()).ok()?;
      if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
      }
      let url = url.to_string();
      if !seen.insert(url.clone()) {
        return None;
      }
      let title = result.title.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
      let title = if title.is_empty() { url.clone() } else { title };
      let snippet = result.excerpts.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
      Some(SearchResult {
        title,
        url,
        snippet: snippet.chars().take(EXCERPT_MAX_CHARS).collect(),
        metadata: Some(SearchResultMetadata {
          published_date: result.publish_date,
          author: None,
          excerpts: result.excerpts,
          description: None,
        }),
      })
    })
    .take(max_results)
    .collect()
}

pub(super) fn http_error(status: u16) -> ProviderErrorKind {
  match status {
    400 | 413 | 414 | 422 => ProviderErrorKind::InvalidRequest,
    401 => ProviderErrorKind::InvalidCredentials,
    402 => ProviderErrorKind::QuotaExhausted,
    403 => ProviderErrorKind::Blocked,
    429 => ProviderErrorKind::RateLimited,
    404 | 408 | 500..=599 => ProviderErrorKind::Transient,
    _ => ProviderErrorKind::UnexpectedResponse,
  }
}
