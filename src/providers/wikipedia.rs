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

use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
  SearchResult, SearchResultMetadata,
};

// Bound latency for this last-resort source independently of general web search.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_RESULTS: usize = 100;
pub const MAX_QUERY_CHARS: usize = 300;
const MAX_SNIPPET_CHARS: usize = 1000;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Full-text page search through one wiki's MediaWiki REST API. Keyless and
/// unmetered; serves only the fallback route, never general web search.
pub struct Wikipedia {
  client: Client,
  search_url: Url,
  article_base: Url,
}

impl Wikipedia {
  /// `language` is a Wikipedia subdomain such as `en`, validated by configuration.
  pub fn new(language: &str) -> ProviderResult<Self> {
    let host = format!("https://{language}.wikipedia.org");
    let invalid = || ProviderError::new(ProviderErrorKind::Internal, "Invalid Wikipedia language.");
    let search_url = Url::parse(&format!("{host}/w/rest.php/v1/search/page")).map_err(|_| invalid())?;
    let article_base = Url::parse(&format!("{host}/wiki/")).map_err(|_| invalid())?;
    let client = Client::builder()
      .user_agent(crate::identity::USER_AGENT)
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(5))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build Wikipedia HTTP client."))?;
    Ok(Self { client, search_url, article_base })
  }
}

impl SearchProvider for Wikipedia {
  fn name(&self) -> &'static str {
    "wikipedia"
  }

  fn max_concurrent_requests(&self) -> Option<NonZeroU32> {
    // https://www.mediawiki.org/wiki/Wikimedia_APIs/Rate_limits
    NonZeroU32::new(3)
  }

  fn additional_rate_limits(&self) -> Vec<super::RequestRateLimit> {
    // Additional local ceiling; the operator's minute limit can be lower.
    // https://www.mediawiki.org/wiki/Wikimedia_APIs/Rate_limits
    vec![super::RequestRateLimit { max_requests: 200, window: Duration::from_secs(60) }]
  }

  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost> {
    result_limit(request)?;
    Ok(RequestCost::Unmetered)
  }

  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>> {
    Box::pin(async move {
      let num_results = result_limit(&request)?;
      let mut response = self
        .client
        .get(self.search_url.clone())
        .header(ACCEPT, "application/json")
        .query(&[("q", request.query.trim().to_owned()), ("limit", num_results.to_string())])
        .send()
        .await
        .map_err(|error| {
          let message =
            if error.is_timeout() { "Wikipedia search request timed out." } else { "Wikipedia search request failed." };
          ProviderError::new(ProviderErrorKind::Transient, message)
        })?;

      let status = response.status();
      if !status.is_success() {
        let kind = match status.as_u16() {
          400 | 413 | 414 | 422 => ProviderErrorKind::InvalidRequest,
          402 => ProviderErrorKind::QuotaExhausted,
          403 => ProviderErrorKind::Blocked,
          429 => ProviderErrorKind::RateLimited,
          408 | 500..=599 => ProviderErrorKind::Transient,
          _ => ProviderErrorKind::UnexpectedResponse,
        };
        let retry_after = crate::providers::retry_after(response.headers());
        return Err(
          super::response_error(
            ProviderError::new(kind, format!("Wikipedia search returned HTTP {status}.")).with_retry_after(retry_after),
            &mut response,
          )
          .await,
        );
      }

      let body = read_body(&mut response).await?;
      let response: WikipediaSearchResponse = serde_json::from_slice(&body).map_err(|_| {
        ProviderError::new(ProviderErrorKind::UnexpectedResponse, "Wikipedia search returned an invalid response.")
      })?;
      Ok(SearchResponse {
        results: normalize_results(response.pages, &self.article_base, num_results),
        reported_cost_usd_micros: None,
      })
    })
  }
}

fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  let query = request.query.trim();
  if query.is_empty() || request.max_results == 0 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "Wikipedia search requires a query and a positive result limit.",
    ));
  }
  if query.chars().count() > MAX_QUERY_CHARS {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "Wikipedia search queries must not exceed 300 characters.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

async fn read_body(response: &mut reqwest::Response) -> ProviderResult<Vec<u8>> {
  let mut body = Vec::new();
  while let Some(chunk) = response
    .chunk()
    .await
    .map_err(|_| ProviderError::new(ProviderErrorKind::Transient, "Could not read Wikipedia search response."))?
  {
    if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
      return Err(ProviderError::new(
        ProviderErrorKind::UnexpectedResponse,
        "Wikipedia search response exceeded the size limit.",
      ));
    }
    body.extend_from_slice(&chunk);
  }
  Ok(body)
}

#[derive(Deserialize)]
struct WikipediaSearchResponse {
  pages: Vec<WikipediaPage>,
}

#[derive(Deserialize)]
struct WikipediaPage {
  key: String,
  title: String,
  excerpt: Option<String>,
  description: Option<String>,
}

fn normalize_results(pages: Vec<WikipediaPage>, article_base: &Url, max_results: usize) -> Vec<SearchResult> {
  let mut seen_urls = HashSet::new();
  pages
    .into_iter()
    .filter_map(|page| {
      let url = article_url(article_base, &page.key)?;
      if !seen_urls.insert(url.clone()) {
        return None;
      }
      let title = collapse_whitespace(&page.title);
      // The short description identifies the subject; the excerpt shows where
      // the query matched, which is often outside the lead section.
      let description = collapse_whitespace(page.description.as_deref().unwrap_or_default());
      let excerpt = collapse_whitespace(&html_text(page.excerpt.as_deref().unwrap_or_default()));
      let snippet = match (description.is_empty(), excerpt.is_empty()) {
        (false, false) => format!("{}. {excerpt}", description.trim_end_matches('.')),
        (false, true) => description.clone(),
        _ => excerpt.clone(),
      };
      Some(SearchResult {
        title: if title.is_empty() { url.clone() } else { title },
        url,
        snippet: snippet.chars().take(MAX_SNIPPET_CHARS).collect(),
        metadata: Some(SearchResultMetadata {
          published_date: None,
          author: None,
          excerpts: if excerpt.is_empty() { Vec::new() } else { vec![excerpt] },
          description: (!description.is_empty()).then_some(description),
        }),
      })
    })
    .take(max_results)
    .collect()
}

/// Keep `/` readable in titles such as `AC/DC`; `%` is literal in page keys.
fn article_url(article_base: &Url, key: &str) -> Option<String> {
  let key = key.trim();
  if key.is_empty() {
    return None;
  }
  let mut url = article_base.clone();
  url.set_path(&format!("{}{}", article_base.path(), key.replace('%', "%25")));
  Some(url.to_string())
}

/// Excerpts are HTML with `searchmatch` spans and entities.
fn html_text(html: &str) -> String {
  Html::parse_fragment(html).root_element().text().collect()
}

fn collapse_whitespace(text: &str) -> String {
  text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
  use super::*;

  fn base() -> Url {
    Url::parse("https://en.wikipedia.org/wiki/").unwrap()
  }

  #[test]
  fn builds_article_urls() {
    assert_eq!(article_url(&base(), "AC/DC").as_deref(), Some("https://en.wikipedia.org/wiki/AC/DC"));
    assert_eq!(article_url(&base(), "100%_(song)").as_deref(), Some("https://en.wikipedia.org/wiki/100%25_(song)"));
    assert_eq!(article_url(&base(), "What?_#1").as_deref(), Some("https://en.wikipedia.org/wiki/What%3F_%231"));
    assert_eq!(article_url(&base(), " "), None);
  }

  #[test]
  fn normalizes_pages() {
    let response: WikipediaSearchResponse = serde_json::from_str(
      r#"{"pages":[
        {"id":1,"key":"Rocket_Festival","title":"Rocket Festival",
         "excerpt":"The <span class=\"searchmatch\">Rocket</span> Festival &quot;Bun Bang Fai&quot;",
         "description":"Traditional festival of Laos and Thailand"},
        {"id":1,"key":"Rocket_Festival","title":"Duplicate","excerpt":null,"description":null},
        {"id":2,"key":"Phi_Ta_Khon","title":"Phi Ta Khon","excerpt":"","description":null}
      ]}"#,
    )
    .unwrap();
    let results = normalize_results(response.pages, &base(), 5);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].url, "https://en.wikipedia.org/wiki/Rocket_Festival");
    assert_eq!(results[0].snippet, "Traditional festival of Laos and Thailand. The Rocket Festival \"Bun Bang Fai\"");
    assert_eq!(results[1].snippet, "");
  }
}
