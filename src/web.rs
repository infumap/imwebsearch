// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::HashMap;
use std::fmt::Write;

use bytes::Bytes;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use http_body_util::Full;
use hyper::header::{ALLOW, CACHE_CONTROL, CONTENT_TYPE, ORIGIN};
use hyper::{Method, Request, Response, StatusCode};
use log::warn;
use once_cell::sync::Lazy;
use reqwest::Url;
use serde_json::json;
use tokio::sync::Mutex;

mod status;

pub use status::page as status_page;

use crate::providers::SearchResult;
use crate::routing::{self, SearchTrace};
use crate::{protocol, search, tools};

const PAGE_RESULTS: usize = 10;
const MAX_QUERY_BYTES: usize = 4_096;
const HTML: &str = "text/html; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";

type BrowserSearchResult = Result<(Vec<SearchResult>, search::Source), String>;
type PendingSearch = Shared<BoxFuture<'static, BrowserSearchResult>>;

static PENDING_SEARCHES: Lazy<Mutex<HashMap<String, PendingSearch>>> = Lazy::new(Mutex::default);

pub fn handles_path(path: &str) -> bool {
  matches!(path, "/" | "/search" | "/fetch" | "/search.css" | "/infumap.png" | "/favicon.ico")
}

/// Browser routes share the MCP server's providers, accounting and fetch tool.
pub async fn handle_http<B>(req: &Request<B>) -> Response<Full<Bytes>> {
  let origin_allowed = match req.headers().get(ORIGIN) {
    Some(origin) => origin.to_str().is_ok_and(|origin| protocol::origin_is_allowed(Some(origin))),
    None => true,
  };
  // GET forms and new-tab links may omit Origin. Do not let another website
  // consume provider capacity via browser navigations or embedded resources.
  let cross_site = req.headers().get("sec-fetch-site").is_some_and(|value| value == "cross-site");
  if !origin_allowed || cross_site {
    return response(StatusCode::FORBIDDEN, TEXT, "Cross-site requests are not allowed.");
  }
  if req.method() != Method::GET {
    let mut result = response(StatusCode::METHOD_NOT_ALLOWED, TEXT, "Use GET.");
    result.headers_mut().insert(ALLOW, hyper::header::HeaderValue::from_static("GET"));
    return result;
  }
  match req.uri().path() {
    "/" => response(StatusCode::OK, HTML, page("", "", StatusCode::OK)),
    "/search.css" => response(StatusCode::OK, "text/css; charset=utf-8", include_str!("web/search.css")),
    "/infumap.png" => {
      response(StatusCode::OK, "image/png", Bytes::from_static(include_bytes!("web/assets/circle.png")))
    }
    "/favicon.ico" => {
      response(StatusCode::OK, "image/x-icon", Bytes::from_static(include_bytes!("web/assets/favicon.ico")))
    }
    "/search" => search_page(&parameter(req, "q").unwrap_or_default()).await,
    "/fetch" => fetch_page(req).await,
    _ => response(StatusCode::NOT_FOUND, TEXT, "Not found."),
  }
}

fn parameter<B>(req: &Request<B>, name: &str) -> Option<String> {
  let mut url = Url::parse("http://localhost/").expect("Static URL is valid");
  url.set_query(req.uri().query());
  url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
}

/// Browser navigation can replace a connection with another request for the
/// same query. Share the running search and keep it alive across disconnects
/// so the replacement receives the one-shot provider override's result.
async fn browser_search(query: &str) -> BrowserSearchResult {
  let pending = {
    let mut searches = PENDING_SEARCHES.lock().await;
    if let Some(pending) = searches.get(query) {
      pending.clone()
    } else {
      let owned_query = query.to_owned();
      let pending = async move {
        let mut trace = SearchTrace::default();
        let result = search::search(&owned_query, PAGE_RESULTS, &mut trace).await;
        routing::record(&owned_query, &trace, None);
        result.map_err(|error| error.to_string())
      }
      .boxed()
      .shared();
      searches.insert(query.to_owned(), pending.clone());
      let running = pending.clone();
      let key = query.to_owned();
      tokio::spawn(async move {
        let _ = running.await;
        PENDING_SEARCHES.lock().await.remove(&key);
      });
      pending
    }
  };
  pending.await
}

async fn search_page(query: &str) -> Response<Full<Bytes>> {
  let query = query.trim();
  if query.is_empty() {
    return response(StatusCode::OK, HTML, page("", "", StatusCode::OK));
  }
  if query.len() > MAX_QUERY_BYTES {
    let content = "<p class=notice role=alert>That query is too long. Please use a shorter query.</p>";
    return response(StatusCode::BAD_REQUEST, HTML, page("", content, StatusCode::BAD_REQUEST));
  }
  let started = std::time::Instant::now();
  let (results, source) = match browser_search(query).await {
    Ok(result) => result,
    Err(error) => {
      warn!("Browser search failed: {error}");
      let content = "<p class=notice role=alert>Search is currently unavailable. Please try again shortly.</p>";
      return response(StatusCode::SERVICE_UNAVAILABLE, HTML, page(query, content, StatusCode::SERVICE_UNAVAILABLE));
    }
  };
  let results: Vec<_> = results.into_iter().filter(|result| public_link(&result.url)).take(PAGE_RESULTS).collect();
  let mut content = format!(
    "<div class=results-meta><h1>Search results</h1><span>{} results, {:.2}s",
    results.len(),
    started.elapsed().as_secs_f64()
  );
  if !source.providers().is_empty() {
    content.push_str(", served by: ");
    for (index, provider) in source.providers().iter().enumerate() {
      if index > 0 {
        content.push_str(", ");
      }
      let name = escape(provider);
      if let Some(homepage) = provider_homepage(provider) {
        let _ = write!(content, "<a href=\"{homepage}\" rel=noreferrer>{name}</a>");
      } else {
        content.push_str(&name);
      }
    }
  }
  content.push_str("</span></div>");
  if let Some(note) = source.note() {
    let _ = write!(content, "<p class=notice>{}</p>", escape(note));
  }
  if results.is_empty() {
    content
      .push_str("<div class=empty><h2>No results found.</h2><p>Try different keywords or a broader query.</p></div>");
  } else {
    content.push_str("<ol class=results>");
    for result in results {
      let mut fetch_query = Url::parse("http://localhost/").expect("Static URL is valid");
      fetch_query.query_pairs_mut().append_pair("url", &result.url);
      let url = escape(&result.url);
      let title = if result.title.trim().is_empty() { &url } else { &escape(&result.title) };
      let _ = write!(
        content,
        "<li class=result><div class=result-address>{url}</div>\
         <h2><a href=\"{url}\" rel=noreferrer>{title}</a></h2>\
         <p>{snippet}</p><a class=fetch-link href=\"/fetch?{fetch_query}\" target=_blank rel=\"noopener noreferrer\" \
         aria-label=\"Fetch text for {title} (opens in a new tab)\">Fetch text <span aria-hidden=true>↗</span></a></li>",
        snippet = escape(&result.snippet),
        fetch_query = escape(fetch_query.query().unwrap_or_default()),
      );
    }
    content.push_str("</ol>");
  }
  response(StatusCode::OK, HTML, page(query, &content, StatusCode::OK))
}

async fn fetch_page<B>(req: &Request<B>) -> Response<Full<Bytes>> {
  let url = parameter(req, "url").unwrap_or_default();
  if url.trim().is_empty() {
    return response(StatusCode::BAD_REQUEST, TEXT, "A URL is required.");
  }
  let mut arguments = json!({ "url": url });
  for name in ["maxChars", "offset"] {
    if let Some(value) = parameter(req, name) {
      let Ok(value) = value.parse::<i64>() else {
        return response(StatusCode::BAD_REQUEST, TEXT, format!("{name} must be an integer."));
      };
      arguments[name] = json!(value);
    }
  }
  let result = tools::call_tool("web_fetch", arguments).await;
  let status = if result["isError"].as_bool().unwrap_or(true) { StatusCode::BAD_GATEWAY } else { StatusCode::OK };
  // Return the exact text content seen by MCP clients, including chunk metadata
  // and errors. Never interpret fetched content as HTML in the browser.
  match result["content"][0]["text"].as_str() {
    Some(text) => response(status, TEXT, text.to_owned()),
    None => response(StatusCode::INTERNAL_SERVER_ERROR, TEXT, "Fetch returned no text."),
  }
}

fn provider_homepage(provider: &str) -> Option<&'static str> {
  match provider {
    "you" => Some("https://you.com/"),
    "exa" | "exa_mcp" => Some("https://exa.ai/"),
    "tavily" | "tavily_mcp" => Some("https://www.tavily.com/"),
    "diffbot" => Some("https://www.diffbot.com/"),
    "firecrawl" | "firecrawl_mcp" => Some("https://www.firecrawl.dev/"),
    "brave" => Some("https://brave.com/"),
    "parallel" | "parallel_mcp" => Some("https://parallel.ai/"),
    "brightdata" => Some("https://brightdata.com/"),
    "serpapi" => Some("https://serpapi.com/"),
    "context" => Some("https://www.context.dev/"),
    "purili" => Some("https://puri.li/"),
    "hacker_news" => Some("https://news.ycombinator.com/"),
    "wikipedia" => Some("https://www.wikipedia.org/"),
    _ => None,
  }
}

fn public_link(value: &str) -> bool {
  Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn escape(value: &str) -> String {
  value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn page(query: &str, content: &str, status: StatusCode) -> String {
  let home = query.is_empty() && status == StatusCode::OK;
  let title = if query.is_empty() { "IMWebSearch".to_owned() } else { format!("{query} — IMWebSearch") };
  let content = format!(include_str!("web/index.html"), query = escape(query), content = content);
  document(&title, if home { "home" } else { "searched" }, &content)
}

fn document(title: &str, body_class: &str, content: &str) -> String {
  format!(
    include_str!("web/layout.html"),
    title = escape(title),
    body_class = body_class,
    status_current = if body_class == "status" { " aria-current=page" } else { "" },
    content = content,
  )
}

fn response(status: StatusCode, content_type: &'static str, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
  Response::builder()
    .status(status)
    .header(CONTENT_TYPE, content_type)
    .header(CACHE_CONTROL, "no-store")
    .header("x-content-type-options", "nosniff")
    // Not no-referrer: browsers then send `Origin: null` on the status page's
    // form posts, which the origin check must reject. Other sites still get
    // no referrer, and result links also carry rel=noreferrer.
    .header("referrer-policy", "same-origin")
    .header(
      "content-security-policy",
      "default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
    )
    .body(Full::new(body.into()))
    .expect("Static response headers are valid")
}
