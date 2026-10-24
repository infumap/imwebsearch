// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::header::ACCEPT;
use reqwest::{Client, Url};
use serde::Deserialize;

use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
  SearchResult,
};

// https://puri.li/developer/documentation (reviewed 2026-10-23).
const SEARCH_URL: &str = "https://puri.li/api/search";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESULTS: usize = 30;
const MAX_SNIPPET_CHARS: usize = 1000;
// Soft allocation weight only; not a published daily quota.
pub const ASSUMED_REQUESTS_PER_DAY: u32 = 100;

pub struct Purili {
  client: Client,
  requests_per_minute: NonZeroU32,
}

impl Purili {
  pub fn new(requests_per_minute: NonZeroU32) -> ProviderResult<Self> {
    let client = Client::builder()
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(10))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Purili HTTP client."))?;
    Ok(Self { client, requests_per_minute })
  }
}

impl SearchProvider for Purili {
  fn name(&self) -> &'static str {
    "purili"
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
      let limit = result_limit(&request)?;
      // One page, with query correction disabled to preserve the original query.
      // The documented API has no result-count parameter; trim locally.
      let mut response = self
        .client
        .get(SEARCH_URL)
        .header(ACCEPT, "application/json")
        .query(&[("q", request.query.trim()), ("page", "1"), ("exact", "1")])
        .send()
        .await
        .map_err(|error| {
          ProviderError::new(
            ProviderErrorKind::Transient,
            if error.is_timeout() { "Purili search request timed out." } else { "Purili search request failed." },
          )
        })?;
      let status = response.status();
      if !status.is_success() {
        let kind = match status.as_u16() {
          400 | 413 | 414 | 422 => ProviderErrorKind::InvalidRequest,
          // This API has no credentials to repair.
          401 | 403 => ProviderErrorKind::Blocked,
          402 => ProviderErrorKind::QuotaExhausted,
          429 => ProviderErrorKind::RateLimited,
          408 | 500..=599 => ProviderErrorKind::Transient,
          _ => ProviderErrorKind::UnexpectedResponse,
        };
        return Err(
          super::response_error(
            ProviderError::new(kind, format!("Purili search returned HTTP {status}.")),
            &mut response,
          )
          .await,
        );
      }
      let mut body = Vec::new();
      while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProviderError::new(ProviderErrorKind::Transient, "Could not read Purili search response."))?
      {
        if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
          return Err(ProviderError::new(
            ProviderErrorKind::UnexpectedResponse,
            "Purili search response exceeded the size limit.",
          ));
        }
        body.extend_from_slice(&chunk);
      }
      // Require a results array: an error envelope is not a successful empty search.
      let response: PuriliResponse = serde_json::from_slice(&body).map_err(|_| {
        ProviderError::new(ProviderErrorKind::UnexpectedResponse, "Purili search returned an invalid response.")
      })?;
      Ok(SearchResponse { results: normalize_results(response.results, limit), reported_cost_usd_micros: None })
    })
  }
}

fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  if request.query.trim().is_empty() || request.max_results == 0 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "Purili search requires a query and a positive result limit.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

#[derive(Deserialize)]
struct PuriliResponse {
  results: Vec<PuriliResult>,
}

#[derive(Deserialize)]
struct PuriliResult {
  url: String,
  title: Option<String>,
  description: Option<String>,
}

fn normalize_results(results: Vec<PuriliResult>, limit: usize) -> Vec<SearchResult> {
  let mut seen = HashSet::new();
  results
    .into_iter()
    .filter_map(|result| {
      let url = Url::parse(result.url.trim()).ok()?;
      if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
      {
        return None;
      }
      let url = url.to_string();
      if !seen.insert(url.clone()) {
        return None;
      }
      let title = result.title.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
      let snippet = result.description.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
      Some(SearchResult {
        title: if title.is_empty() { url.clone() } else { title },
        url,
        snippet: snippet.chars().take(MAX_SNIPPET_CHARS).collect(),
        metadata: None,
      })
    })
    .take(limit)
    .collect()
}
