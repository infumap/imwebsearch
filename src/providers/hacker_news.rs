// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::header::ACCEPT;
use reqwest::{Client, Url};
use scraper::Html;
use serde::Deserialize;
use tokio::time::Instant;

use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
  SearchResult, SearchResultMetadata,
};

const SEARCH_URL: &str = "https://hn.algolia.com/api/v1/search";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_RESULTS: usize = 30;
const MAX_SNIPPET_CHARS: usize = 1000;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Public Algolia HN search. One story-search request supplies article links
/// and discussion entry points without credentials, pagination or enrichment.
pub struct HackerNews {
  client: Client,
  requests_per_minute: NonZeroU32,
}

impl HackerNews {
  pub fn new(requests_per_minute: NonZeroU32) -> ProviderResult<Self> {
    let client = Client::builder()
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(5))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Hacker News HTTP client."))?;
    Ok(Self { client, requests_per_minute })
  }

  async fn search_stories(&self, request: SearchRequest<'_>) -> ProviderResult<SearchResponse> {
    let limit = result_limit(&request)?;
    // Search HN's existing index. No linked articles or comment trees are fetched.
    // Endpoint and parameters: https://hn.algolia.com/api
    let mut response = self
      .client
      .get(SEARCH_URL)
      .header(ACCEPT, "application/json")
      .query(&[
        ("query", request.query.trim().to_owned()),
        ("tags", "story".to_owned()),
        ("hitsPerPage", limit.to_string()),
        ("page", "0".to_owned()),
      ])
      .send()
      .await
      .map_err(|error| {
        ProviderError::new(
          ProviderErrorKind::Transient,
          if error.is_timeout() {
            "Hacker News search request timed out."
          } else {
            "Hacker News search request failed."
          },
        )
      })?;
    let status = response.status();
    if !status.is_success() {
      let kind = match status.as_u16() {
        400 | 413 | 414 | 422 => ProviderErrorKind::InvalidRequest,
        401 | 403 => ProviderErrorKind::Blocked,
        429 => ProviderErrorKind::RateLimited,
        408 | 500..=599 => ProviderErrorKind::Transient,
        _ => ProviderErrorKind::UnexpectedResponse,
      };
      let retry_after = crate::providers::retry_after(response.headers());
      return Err(
        super::response_error(
          ProviderError::new(kind, format!("Hacker News search returned HTTP {status}.")).with_retry_after(retry_after),
          &mut response,
        )
        .await,
      );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
      .chunk()
      .await
      .map_err(|_| ProviderError::new(ProviderErrorKind::Transient, "Could not read Hacker News search response."))?
    {
      if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
        return Err(ProviderError::new(
          ProviderErrorKind::UnexpectedResponse,
          "Hacker News search response exceeded the size limit.",
        ));
      }
      body.extend_from_slice(&chunk);
    }
    let response: HnResponse = serde_json::from_slice(&body).map_err(|_| {
      ProviderError::new(ProviderErrorKind::UnexpectedResponse, "Hacker News search returned an invalid response.")
    })?;
    Ok(SearchResponse { results: normalize_results(response.hits, limit), reported_cost_usd_micros: None })
  }
}

impl SearchProvider for HackerNews {
  fn name(&self) -> &'static str {
    "hacker_news"
  }

  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    Some(self.requests_per_minute)
  }

  fn additional_rate_limits(&self) -> Vec<super::RequestRateLimit> {
    // Shared-IP hourly ceiling recorded in hacker_news.verify.md. Enforce it
    // independently if the operator raises the default local minute limit.
    vec![super::RequestRateLimit { max_requests: 10_000, window: Duration::from_secs(60 * 60) }]
  }

  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost> {
    result_limit(request)?;
    Ok(RequestCost::Unmetered)
  }

  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>> {
    Box::pin(async move {
      let deadline = Instant::now() + SEARCH_TIMEOUT;
      match tokio::time::timeout_at(deadline, self.search_stories(request)).await {
        Ok(result) if Instant::now() < deadline => result,
        _ => Err(ProviderError::new(ProviderErrorKind::Transient, "Hacker News search reached its 8-second deadline.")),
      }
    })
  }
}

fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  if request.query.trim().is_empty() || request.max_results == 0 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "Hacker News search requires a query and a positive result limit.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

#[derive(Deserialize)]
struct HnResponse {
  hits: Vec<HnStory>,
}

#[derive(Deserialize)]
struct HnStory {
  #[serde(rename = "objectID")]
  id: String,
  title: Option<String>,
  url: Option<String>,
  story_text: Option<String>,
  created_at: Option<String>,
  author: Option<String>,
  points: Option<i64>,
  num_comments: Option<u64>,
}

fn normalize_results(stories: Vec<HnStory>, limit: usize) -> Vec<SearchResult> {
  let mut seen = HashSet::new();
  stories
    .into_iter()
    .filter_map(|story| {
      // Never interpolate arbitrary upstream strings into a discussion URL.
      let id = story.id.parse::<u64>().ok().filter(|&id| id > 0)?;
      let discussion = format!("https://news.ycombinator.com/item?id={id}");
      let article = story.url.as_deref().and_then(article_url);
      let title = plain_text(story.title.as_deref().unwrap_or_default());
      let text: String =
        plain_text(story.story_text.as_deref().unwrap_or_default()).chars().take(MAX_SNIPPET_CHARS).collect();
      if article.is_none() && title.is_empty() && text.is_empty() {
        return None;
      }
      let url = article.unwrap_or_else(|| discussion.clone());
      if !seen.insert(url.clone()) {
        return None;
      }
      let mut parts = vec![format!("Hacker News discussion: {discussion}")];
      if let Some(points) = story.points {
        parts.push(format!("{points} points"));
      }
      if let Some(comments) = story.num_comments {
        parts.push(format!("{comments} comments"));
      }
      if let Some(created_at) = &story.created_at {
        parts.push(format!("Submitted {created_at}"));
      }
      if !text.is_empty() {
        parts.push(text.clone());
      }
      let snippet = parts.join(" | ").chars().take(MAX_SNIPPET_CHARS).collect();
      Some(SearchResult {
        title: if title.is_empty() { url.clone() } else { title },
        url,
        snippet,
        metadata: Some(SearchResultMetadata {
          published_date: story.created_at,
          author: story.author,
          excerpts: if text.is_empty() { Vec::new() } else { vec![text] },
          description: None,
        }),
      })
    })
    .take(limit)
    .collect()
}

fn article_url(value: &str) -> Option<String> {
  let url = Url::parse(value.trim()).ok()?;
  (matches!(url.scheme(), "http" | "https")
    && url.host_str().is_some()
    && url.username().is_empty()
    && url.password().is_none())
  .then(|| url.to_string())
}

fn plain_text(html: &str) -> String {
  Html::parse_fragment(html)
    .root_element()
    .text()
    .collect::<Vec<_>>()
    .join(" ")
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
}
