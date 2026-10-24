// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Wikipedia articles are read through the wiki's API as plain text rather
//! than rendered in the browser. Long articles are megabytes of HTML (Barack
//! Obama: 2.76 MB) but under 100 KB of text, and searches now favour
//! Wikipedia, so these are the pages models most often fetch. The extract
//! drops the infobox, tables and links, which matters less to a model reading
//! facts than clean prose does. Anything this path cannot serve falls back to
//! the ordinary browser fetch.
//!
//! Requests go only to `<language>.wikipedia.org`, matched by a fixed pattern,
//! so the browser's destination policy is not needed for them.

use std::time::Duration;

use reqwest::header::ACCEPT;
use reqwest::{Client, Url};
use serde::Deserialize;

use super::FetchedPage;

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Pages outside the article namespace have no useful extract.
const NAMESPACES: &[&str] = &[
  "special",
  "file",
  "image",
  "media",
  "user",
  "wikipedia",
  "wp",
  "project",
  "mediawiki",
  "template",
  "help",
  "category",
  "portal",
  "draft",
  "module",
  "timedtext",
  "book",
  "education program",
  "gadget",
  "topic",
];

pub(super) struct Article {
  language: String,
  title: String,
}

/// An article URL such as `https://en.wikipedia.org/wiki/Barack_Obama` or its
/// mobile form. URLs with a query (`?action=raw`, `?oldid=`) ask for
/// something specific and are fetched as given.
pub(super) fn article(url: &Url) -> Option<Article> {
  if !matches!(url.scheme(), "http" | "https") || url.query().is_some() {
    return None;
  }
  let host = url.host_str()?.to_ascii_lowercase();
  let subdomain = host.strip_suffix(".wikipedia.org")?;
  let language = subdomain.strip_suffix(".m").unwrap_or(subdomain);
  let valid_language = (2..=12).contains(&language.len())
    && language.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'-')
    && !language.starts_with('-')
    && !language.ends_with('-');
  if !valid_language {
    return None;
  }
  let encoded = url.path().strip_prefix("/wiki/")?;
  let title = percent_decode(encoded)?.replace('_', " ").trim().to_owned();
  if title.is_empty() || in_other_namespace(&title) {
    return None;
  }
  Some(Article { language: language.to_owned(), title })
}

/// Why the extract was not used. Either way the caller falls back to an
/// ordinary fetch, which hides the failure unless it is logged distinctly.
pub(super) enum ExtractError {
  /// Wikimedia refused this client outright, such as for a missing
  /// User-Agent: a server configuration problem, not a transient failure.
  Rejected(String),
  Failed(String),
}

impl From<String> for ExtractError {
  fn from(message: String) -> Self {
    Self::Failed(message)
  }
}

/// `Ok(None)` when the page is missing or has no text, so the caller fetches
/// the URL normally and reports whatever the site returns.
pub(super) async fn fetch(article: &Article, requested: &Url) -> Result<Option<FetchedPage>, ExtractError> {
  let mut api = Url::parse(&format!("https://{}.wikipedia.org/w/api.php", article.language))
    .map_err(|_| "Invalid Wikipedia API URL.".to_owned())?;
  api
    .query_pairs_mut()
    .append_pair("action", "query")
    .append_pair("prop", "extracts|info")
    .append_pair("explaintext", "1")
    .append_pair("exsectionformat", "wiki")
    .append_pair("inprop", "url")
    .append_pair("redirects", "1")
    .append_pair("format", "json")
    .append_pair("formatversion", "2")
    .append_pair("titles", &article.title);
  let client = Client::builder()
    .user_agent(crate::identity::USER_AGENT)
    .no_proxy()
    .https_only(true)
    .redirect(reqwest::redirect::Policy::none())
    .timeout(TIMEOUT)
    .build()
    .map_err(|_| "Could not build Wikipedia HTTP client.".to_owned())?;
  let mut response = client
    .get(api)
    .header(ACCEPT, "application/json")
    .send()
    .await
    .map_err(|error| if error.is_timeout() { "request timed out".to_owned() } else { "request failed".to_owned() })?;
  if !response.status().is_success() {
    let status = response.status().as_u16();
    if status == 403 {
      return Err(ExtractError::Rejected(format!("HTTP {status}")));
    }
    return Err(format!("HTTP {status}").into());
  }
  let mut body = Vec::new();
  while let Some(chunk) = response.chunk().await.map_err(|_| "could not read response".to_owned())? {
    if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
      return Err("response exceeded the size limit".to_owned().into());
    }
    body.extend_from_slice(&chunk);
  }
  let response: ApiResponse = serde_json::from_slice(&body).map_err(|_| "invalid response".to_owned())?;
  let Some(page) = response.query.and_then(|query| query.pages.into_iter().next()) else { return Ok(None) };
  let extract = page.extract.unwrap_or_default();
  if page.missing || extract.trim().is_empty() {
    return Ok(None);
  }
  Ok(Some(FetchedPage {
    url: requested.to_string(),
    final_url: page.fullurl.unwrap_or_else(|| requested.to_string()),
    content_type: "text/markdown".to_owned(),
    text: to_markdown(&page.title, &extract),
    via: "text",
    transport: "wikipedia api",
  }))
}

#[derive(Deserialize)]
struct ApiResponse {
  query: Option<ApiQuery>,
}

#[derive(Deserialize)]
struct ApiQuery {
  #[serde(default)]
  pages: Vec<ApiPage>,
}

#[derive(Deserialize)]
struct ApiPage {
  title: String,
  #[serde(default)]
  missing: bool,
  extract: Option<String>,
  fullurl: Option<String>,
}

/// Title as a heading, `== Section ==` lines as Markdown headings, and
/// sections without text (References, External links and the like, whose
/// content is not prose) dropped.
fn to_markdown(title: &str, extract: &str) -> String {
  let mut blocks: Vec<(Option<usize>, String)> = Vec::new();
  for line in extract.lines() {
    let trimmed = line.trim();
    let level = trimmed.bytes().take_while(|&byte| byte == b'=').count();
    let is_heading = level >= 2 && trimmed.len() > 2 * level && trimmed.ends_with(&"=".repeat(level));
    if is_heading {
      let text = trimmed[level..trimmed.len() - level].trim();
      blocks.push((Some(level), format!("{} {text}", "#".repeat(level))));
    } else if let Some((_, body)) = blocks.last_mut().filter(|(heading, _)| heading.is_none()) {
      body.push('\n');
      body.push_str(line);
    } else {
      blocks.push((None, line.to_owned()));
    }
  }
  let mut kept: Vec<String> = Vec::new();
  for (index, (heading, text)) in blocks.iter().enumerate() {
    match heading {
      // A heading stays if text follows before the next heading at its level or above.
      Some(level) => {
        let has_text = blocks[index + 1..]
          .iter()
          .take_while(|(next, _)| next.is_none_or(|next| next > *level))
          .any(|(next, body)| next.is_none() && !body.trim().is_empty());
        if has_text {
          kept.push(text.clone());
        }
      }
      None if !text.trim().is_empty() => kept.push(text.trim().to_owned()),
      None => {}
    }
  }
  format!("# {title}\n\n{}", kept.join("\n\n"))
}

fn in_other_namespace(title: &str) -> bool {
  let Some((prefix, _)) = title.split_once(':') else { return false };
  let prefix = prefix.trim().to_lowercase();
  prefix == "talk" || prefix.ends_with(" talk") || NAMESPACES.contains(&prefix.as_str())
}

/// Percent-decoding for a URL path segment; invalid UTF-8 is rejected.
fn percent_decode(text: &str) -> Option<String> {
  let bytes = text.as_bytes();
  let mut decoded = Vec::with_capacity(bytes.len());
  let mut index = 0;
  while index < bytes.len() {
    if bytes[index] == b'%' {
      let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
      decoded.push(u8::from_str_radix(hex, 16).ok()?);
      index += 3;
    } else {
      decoded.push(bytes[index]);
      index += 1;
    }
  }
  String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(url: &str) -> Option<(String, String)> {
    article(&Url::parse(url).unwrap()).map(|article| (article.language, article.title))
  }

  #[test]
  fn recognises_article_urls() {
    assert_eq!(parse("https://en.wikipedia.org/wiki/Barack_Obama"), Some(("en".into(), "Barack Obama".into())));
    assert_eq!(
      parse("https://en.m.wikipedia.org/wiki/Barack_Obama#Early_life"),
      Some(("en".into(), "Barack Obama".into()))
    );
    assert_eq!(parse("https://de.wikipedia.org/wiki/G%C3%B6del"), Some(("de".into(), "Gödel".into())));
    assert_eq!(parse("https://en.wikipedia.org/wiki/AC/DC"), Some(("en".into(), "AC/DC".into())));
    assert_eq!(
      parse("https://en.wikipedia.org/wiki/Star_Wars:_Episode_IV"),
      Some(("en".into(), "Star Wars: Episode IV".into()))
    );
  }

  #[test]
  fn leaves_other_urls_to_the_browser() {
    assert_eq!(parse("https://en.wikipedia.org/w/index.php?title=Barack_Obama&action=raw"), None);
    assert_eq!(parse("https://en.wikipedia.org/wiki/Barack_Obama?oldid=1"), None);
    assert_eq!(parse("https://en.wikipedia.org/wiki/Special:Random"), None);
    assert_eq!(parse("https://en.wikipedia.org/wiki/Talk:Barack_Obama"), None);
    assert_eq!(parse("https://en.wikipedia.org/wiki/User_talk:Example"), None);
    assert_eq!(parse("https://en.wikipedia.org/wiki/File:Obama.jpg"), None);
    assert_eq!(parse("https://commons.wikimedia.org/wiki/Main_Page"), None);
    assert_eq!(parse("https://en.wikipedia.org.evil.example/wiki/X"), None);
    assert_eq!(parse("https://wikipedia.org/wiki/X"), None);
  }

  #[test]
  fn converts_headings_and_drops_empty_sections() {
    let extract = "Lead text.\n\n== Early life ==\nBorn in 1961.\n\n=== Education ===\nColumbia.\n\n== See also ==\n\n\n== References ==\n";
    assert_eq!(
      to_markdown("Barack Obama", extract),
      "# Barack Obama\n\nLead text.\n\n## Early life\n\nBorn in 1961.\n\n### Education\n\nColumbia."
    );
  }
}
