// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, RequestBuilder, Response, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
  SearchResult, SearchResultMetadata,
};
use crate::request_quota::{RequestQuota, RequestQuotaSnapshot};

const ENDPOINT: &str = "https://api.you.com/mcp?profile=free&tools=you-search";
const PROTOCOL: &str = "2026-07-28";
const LEGACY_PROTOCOL: &str = "2025-06-18";
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;

const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESULTS: usize = 100;
const MAX_SNIPPET_CHARS: usize = 1000;
pub const MAX_DAILY_FREE_REQUESTS: u32 = 100;

/// Keyless You.com MCP free profile only. No API keys, account credits, wallet
/// payments, discovery tools, extraction or research requests are dispatched.
pub struct You {
  client: Client,
  quota: Arc<RequestQuota>,
  requests_per_minute: NonZeroU32,
}

impl You {
  pub fn new(directory: &Path, requests_per_minute: NonZeroU32) -> ProviderResult<Self> {
    let client = Client::builder()
      .no_proxy()
      .https_only(true)
      .redirect(reqwest::redirect::Policy::none())
      .connect_timeout(Duration::from_secs(10))
      .timeout(SEARCH_TIMEOUT)
      .build()
      .map_err(|_| ProviderError::new(ProviderErrorKind::Internal, "Could not build You.com MCP HTTP client."))?;
    Ok(Self { client, quota: RequestQuota::open(directory, "you", MAX_DAILY_FREE_REQUESTS)?, requests_per_minute })
  }

  async fn search_until(&self, request: SearchRequest<'_>, deadline: Instant) -> ProviderResult<SearchResponse> {
    let max_results = result_limit(&request)?;
    let mut session = Session::new(self.client.clone());
    session.prepare().await?;
    // Protocol discovery does not consume a search allowance. Durably charge just
    // before tools/call; late persistence must never dispatch a search.
    self.quota.charge(deadline).await?;
    let result = session
      .call(json!({
        "query": request.query.trim(),
        "count": max_results,
        "extraction": "none"
      }))
      .await?;
    if result.get("isError").is_some_and(|value| value != &Value::Bool(false)) {
      return Err(tool_error(&result));
    }
    let payload = match result.get("structuredContent").filter(|value| !value.is_null()) {
      Some(payload) => payload.clone(),
      None => {
        // The current server mirrors structured JSON in text for MCP clients
        // without structured output support. Do not scrape prose for results.
        let content = result.get("content").and_then(Value::as_array).ok_or_else(invalid_response)?;
        let texts: Vec<&str> = content
          .iter()
          .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
          .filter_map(|item| item.get("text").and_then(Value::as_str))
          .collect();
        serde_json::from_str(&texts.join("\n")).map_err(|_| invalid_response())?
      }
    };
    let payload: SearchPayload = serde_json::from_value(payload).map_err(|_| invalid_response())?;
    let results = normalize_results(payload.results.web.into_iter().chain(payload.results.news), max_results);
    Ok(SearchResponse { results, reported_cost_usd_micros: None })
  }
}

impl SearchProvider for You {
  fn name(&self) -> &'static str {
    "you"
  }

  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    Some(self.requests_per_minute)
  }

  fn request_quota(&self) -> BoxFuture<'_, ProviderResult<Option<RequestQuotaSnapshot>>> {
    Box::pin(async { self.quota.snapshot().await.map(Some) })
  }

  fn clear_quota_cooldown(&self) -> BoxFuture<'_, ProviderResult<()>> {
    Box::pin(self.quota.clear_cooldown())
  }

  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost> {
    result_limit(request)?;
    // No monetary cost. Track the provider's fixed daily allowance as an
    // estimate; upstream quota rejection controls the recovery cooldown.
    Ok(RequestCost::Unmetered)
  }

  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>> {
    Box::pin(async move {
      let deadline = Instant::now() + SEARCH_TIMEOUT;
      let operation = async {
        let result = self.search_until(request, deadline).await;
        if let Err(error) = result {
          if error.kind == ProviderErrorKind::QuotaExhausted {
            let delay = error.retry_after.unwrap_or(super::QUOTA_RETRY_DELAY);
            self.quota.exhaust(delay).await?;
          }
          return Err(error);
        }
        result
      };
      tokio::time::timeout_at(deadline, operation).await.unwrap_or_else(|_| {
        Err(ProviderError::new(ProviderErrorKind::Transient, "You.com search exceeded its 20-second deadline."))
      })
    })
  }
}

fn result_limit(request: &SearchRequest<'_>) -> ProviderResult<usize> {
  let query = request.query.trim();
  if query.is_empty() || request.max_results == 0 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "You.com search requires a query and a positive result limit.",
    ));
  }
  // The hosted tool documents short, single-intent queries. Reject instead of
  // silently changing a query or dispatching multiple searches against quota.
  if query.encode_utf16().count() > 400 || query.split_whitespace().count() > 50 {
    return Err(ProviderError::new(
      ProviderErrorKind::InvalidRequest,
      "You.com queries must fit 400 UTF-16 code units and 50 words.",
    ));
  }
  Ok(request.max_results.min(MAX_RESULTS))
}

#[derive(Deserialize)]
struct SearchPayload {
  results: SearchResults,
}

#[derive(Deserialize)]
struct SearchResults {
  web: Vec<YouResult>,
  #[serde(default)]
  news: Vec<YouResult>,
}

#[derive(Deserialize)]
struct YouResult {
  url: String,
  title: Option<String>,
  description: Option<String>,
  snippets: Option<Vec<String>>,
  page_age: Option<String>,
}

fn normalize_results(results: impl Iterator<Item = YouResult>, limit: usize) -> Vec<SearchResult> {
  let mut seen_urls = HashSet::new();
  results
    .filter_map(|result| {
      let url = Url::parse(result.url.trim()).ok()?;
      if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
      }
      let url = url.to_string();
      if !seen_urls.insert(url.clone()) {
        return None;
      }
      let title = result.title.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
      let title = if title.is_empty() { url.clone() } else { title };
      let excerpts: Vec<String> =
        result.snippets.unwrap_or_default().into_iter().filter(|snippet| !snippet.trim().is_empty()).collect();
      let text = if excerpts.is_empty() { result.description.unwrap_or_default() } else { excerpts.join(" ") };
      let snippet = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_SNIPPET_CHARS).collect();
      Some(SearchResult {
        title,
        url,
        snippet,
        metadata: Some(SearchResultMetadata {
          published_date: result.page_age,
          author: None,
          excerpts,
          description: None,
        }),
      })
    })
    .take(limit)
    .collect()
}

fn invalid_response() -> ProviderError {
  ProviderError::new(ProviderErrorKind::UnexpectedResponse, "You.com search returned an invalid result payload.")
}

/// One search's MCP transport. Modern requests are stateless; legacy sessions
/// have independent cleanup. An interrupted search is never replayed.
struct Session {
  client: Client,
  id: Option<HeaderValue>,
  protocol: &'static str,
  pending: Option<u64>,
}

impl Session {
  fn new(client: Client) -> Self {
    Self { client, id: None, protocol: PROTOCOL, pending: None }
  }

  async fn prepare(&mut self) -> ProviderResult<()> {
    // The modern protocol is stateless: initialize and initialized are not
    // part of its lifecycle. Discover before charging or sending tools/call.
    let response = self
      .request("server/discover")
      .json(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": { "_meta": metadata(PROTOCOL) }
      }))
      .send()
      .await
      .map_err(request_error)?;
    let retry_after = super::retry_after(response.headers());
    match check_response(response, "server/discover", self.protocol).await {
      Ok(response) => match read_reply(response, 1).await.map_err(|error| error.with_retry_after(retry_after)) {
        Ok(result) => {
          let versions = result
            .get("supportedVersions")
            .and_then(Value::as_array)
            .ok_or_else(|| unexpected("You.com MCP discovery omitted supported protocol versions."))?;
          if versions.iter().any(|version| version.as_str() == Some(PROTOCOL)) {
            return Ok(());
          }
          self.protocol = legacy_protocol(versions)
            .ok_or_else(|| unexpected("You.com MCP discovery advertised no supported protocol version."))?;
        }
        // A legacy server can return method-not-found as a JSON-RPC error
        // inside HTTP 200. This preflight never dispatches a search.
        Err(error) if error.kind == ProviderErrorKind::InvalidRequest => self.protocol = LEGACY_PROTOCOL,
        Err(error) => return Err(error),
      },
      Err(failure) if failure.legacy_fallback => self.protocol = LEGACY_PROTOCOL,
      Err(failure) => return Err(failure.error),
    }
    self.initialize_legacy().await
  }

  async fn initialize_legacy(&mut self) -> ProviderResult<()> {
    let result = self
      .rpc(
        1,
        "initialize",
        json!({
          "protocolVersion": self.protocol,
          "capabilities": {},
          "clientInfo": { "name": crate::identity::NAME, "version": env!("CARGO_PKG_VERSION") }
        }),
      )
      .await?;
    self.protocol = match result.get("protocolVersion").and_then(Value::as_str) {
      Some("2025-03-26") => "2025-03-26",
      Some("2025-06-18") => "2025-06-18",
      Some("2025-11-25") => "2025-11-25",
      Some("2026-07-28") => "2026-07-28",
      _ => return Err(unexpected("You.com MCP negotiated an unsupported protocol version.")),
    };
    if self.protocol == PROTOCOL {
      self.id = None;
      return Ok(());
    }
    if !result.pointer("/capabilities/tools").is_some_and(Value::is_object) {
      return Err(unexpected("You.com MCP did not advertise tool support."));
    }
    let response = self
      .request("notifications/initialized")
      .json(&json!({
        "jsonrpc": "2.0", "method": "notifications/initialized", "params": {}
      }))
      .send()
      .await
      .map_err(request_error)?;
    check_response(response, "notifications/initialized", self.protocol).await.map_err(|failure| failure.error)?;
    Ok(())
  }

  async fn call(&mut self, arguments: Value) -> ProviderResult<Value> {
    self.pending = Some(2);
    let result = self
      .rpc(
        2,
        "tools/call",
        json!({
          "name": "you-search", "arguments": arguments,
          "_meta": metadata(self.protocol)
        }),
      )
      .await;
    // Errors can be uncertain about execution. Legacy sessions need an explicit
    // cancellation notification; modern cancellation closes the response stream.
    // The persistent local charge remains in either case.
    if result.is_ok() {
      self.pending = None;
    }
    result
  }

  fn request(&self, method: &'static str) -> RequestBuilder {
    let mut request = self
      .client
      .post(ENDPOINT)
      .header(ACCEPT, "application/json, text/event-stream")
      .header("mcp-protocol-version", self.protocol)
      .header("mcp-method", method)
      .header("x-allowed-tools", "you-search");
    if method == "tools/call" {
      request = request.header("mcp-name", "you-search");
    }
    if let Some(id) = &self.id {
      request = request.header("mcp-session-id", id.clone());
    }
    request
  }

  async fn rpc(&mut self, id: u64, method: &'static str, params: Value) -> ProviderResult<Value> {
    let response = self
      .request(method)
      .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
      .send()
      .await
      .map_err(request_error)?;
    let response = check_response(response, method, self.protocol).await.map_err(|failure| failure.error)?;
    let retry_after = super::retry_after(response.headers());
    if method == "initialize"
      && let Some(value) = response.headers().get("mcp-session-id")
    {
      if value.as_bytes().is_empty()
        || value.as_bytes().len() > 1024
        || !value.as_bytes().iter().all(|&byte| (0x21..=0x7e).contains(&byte))
      {
        return Err(unexpected("You.com MCP returned an invalid session identifier."));
      }
      let mut value = value.clone();
      value.set_sensitive(true);
      self.id = Some(value);
    }
    let result = read_reply(response, id).await.map_err(|error| error.with_retry_after(retry_after))?;
    if method == "tools/call" && result.get("isError").is_some_and(|value| value != &Value::Bool(false)) {
      return Err(tool_error(&result).with_retry_after(retry_after));
    }
    Ok(result)
  }
}

impl Drop for Session {
  fn drop(&mut self) {
    // Modern Streamable HTTP cancellation closes the response stream; it has
    // neither protocol sessions nor a notifications/cancelled HTTP request.
    if self.protocol == PROTOCOL {
      return;
    }
    if self.id.is_none() && self.pending.is_none() {
      return;
    }
    let client = self.client.clone();
    let id = self.id.clone();
    let protocol = self.protocol;
    let pending = self.pending;
    tokio::spawn(async move {
      let headers = |request: RequestBuilder| {
        let request =
          request.header("mcp-protocol-version", protocol).header(ACCEPT, "application/json, text/event-stream");
        if let Some(id) = &id { request.header("mcp-session-id", id.clone()) } else { request }
      };
      if let Some(request_id) = pending {
        let _ = headers(client.post(ENDPOINT))
          .timeout(Duration::from_secs(2))
          .json(&json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": request_id }
          }))
          .send()
          .await;
      }
      if id.is_some() {
        let _ = headers(client.delete(ENDPOINT)).timeout(Duration::from_secs(2)).send().await;
      }
    });
  }
}

async fn read_reply(mut response: Response, id: u64) -> ProviderResult<Value> {
  let content_type = response
    .headers()
    .get(CONTENT_TYPE)
    .and_then(|value| value.to_str().ok())
    .unwrap_or("")
    .split(';')
    .next()
    .unwrap_or("")
    .trim()
    .to_ascii_lowercase();
  let streaming = content_type == "text/event-stream";
  if !streaming && content_type != "application/json" {
    return Err(unexpected("You.com MCP returned an unsupported response type."));
  }
  let mut buffer = Vec::new();
  let mut received = 0usize;
  while let Some(chunk) = response.chunk().await.map_err(request_error)? {
    received = received
      .checked_add(chunk.len())
      .filter(|&total| total <= MAX_RESPONSE_BYTES)
      .ok_or_else(|| unexpected("You.com MCP response exceeded the size limit."))?;
    buffer.extend_from_slice(&chunk);
    if streaming {
      while let Some(event) = take_event(&mut buffer) {
        let event = std::str::from_utf8(&event).map_err(|_| unexpected("You.com MCP returned invalid UTF-8."))?;
        let normalized = event.replace("\r\n", "\n").replace('\r', "\n");
        let data = normalized
          .lines()
          .filter_map(|line| line.strip_prefix("data:"))
          .map(|line| line.strip_prefix(' ').unwrap_or(line))
          .collect::<Vec<_>>()
          .join("\n");
        if data.is_empty() {
          continue;
        }
        let message: Value =
          serde_json::from_str(&data).map_err(|_| unexpected("You.com MCP returned invalid event JSON."))?;
        if let Some(result) = matching_reply(message, id)? {
          return Ok(result);
        }
      }
    }
  }
  if !streaming {
    let message: Value =
      serde_json::from_slice(&buffer).map_err(|_| unexpected("You.com MCP returned invalid JSON."))?;
    if let Some(result) = matching_reply(message, id)? {
      return Ok(result);
    }
  }
  Err(unexpected("You.com MCP response ended without a matching JSON-RPC reply."))
}

fn take_event(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
  let (index, size) = [b"\n\n".as_slice(), b"\r\n\r\n".as_slice(), b"\r\r".as_slice()]
    .into_iter()
    .filter_map(|delimiter| {
      buffer.windows(delimiter.len()).position(|window| window == delimiter).map(|index| (index, delimiter.len()))
    })
    .min_by_key(|&(index, _)| index)?;
  let event = buffer[..index].to_vec();
  buffer.drain(..index + size);
  Some(event)
}

fn matching_reply(message: Value, id: u64) -> ProviderResult<Option<Value>> {
  if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
    return Err(unexpected("You.com MCP returned an invalid JSON-RPC envelope."));
  }
  if message.get("id").and_then(Value::as_u64) != Some(id) {
    return Ok(None);
  }
  if let Some(error) = message.get("error") {
    if matches!(error.get("code").and_then(Value::as_i64), Some(-32602..=-32600)) {
      return Err(
        ProviderError::new(ProviderErrorKind::InvalidRequest, "You.com MCP rejected the request parameters.")
          .with_error_payload(error),
      );
    }
    return Err(tool_error(error));
  }
  message
    .get("result")
    .filter(|result| result.is_object())
    .cloned()
    .map(Some)
    .ok_or_else(|| unexpected("You.com MCP reply omitted its result."))
}

fn tool_error(value: &Value) -> ProviderError {
  // Inspect error envelopes only. Never include raw messages, queries or bodies
  // in diagnostics. Unknown failures stay transiently unavailable, not empty.
  let mut messages = Vec::new();
  if let Some(message) = value.get("message").and_then(Value::as_str) {
    messages.push(message);
  }
  if let Some(message) = value.get("error").and_then(Value::as_str) {
    messages.push(message);
  }
  if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
    messages.push(message);
  }
  if let Some(content) = value.get("content").and_then(Value::as_array) {
    messages.extend(
      content
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str)),
    );
  }
  let text = messages.join("\n").to_ascii_lowercase().replace(['_', '-'], " ");
  let error = if (text.contains("daily") && (text.contains("limit") || text.contains("quota")))
    || text.contains("quota exceeded")
    || text.contains("quota exhausted")
    || text.contains("credit limit")
  {
    ProviderError::new(ProviderErrorKind::QuotaExhausted, "You.com free daily quota was exhausted.")
  } else if text.contains("rate limit") || text.contains("too many requests") {
    ProviderError::new(ProviderErrorKind::RateLimited, "You.com MCP search was rate limited.")
  } else {
    unexpected("You.com MCP search reported a tool or protocol error.")
  };
  error.with_error_payload(value)
}

struct HttpFailure {
  error: ProviderError,
  legacy_fallback: bool,
}

async fn check_response(
  mut response: Response,
  method: &'static str,
  protocol: &'static str,
) -> Result<Response, HttpFailure> {
  let status = response.status();
  if status.is_success() {
    return Ok(response);
  }
  let retry_after = crate::providers::retry_after(response.headers());
  // An unsupported modern protocol or header mismatch is also HTTP 400. Read
  // a bounded error envelope, but expose only numeric codes and known version
  // identifiers, never provider messages, query text, headers or session IDs.
  let mut body = Vec::new();
  let mut complete = true;
  while let Ok(Some(chunk)) = response.chunk().await {
    if chunk.len() > MAX_ERROR_BYTES - body.len() {
      complete = false;
      break;
    }
    body.extend_from_slice(&chunk);
  }
  // The excerpt goes to debug logs only, never into the error message.
  let super::ErrorBody { json: details, detail } = super::ErrorBody::parse(&body, complete);
  let code = details.as_ref().and_then(|details| details.pointer("/error/code")).and_then(Value::as_i64);
  let versions: Vec<&'static str> = details
    .as_ref()
    .and_then(|details| {
      details.pointer("/error/data/supported").or_else(|| details.pointer("/error/data/supportedVersions"))
    })
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|value| known_protocol(value.as_str()?))
    .collect();
  let kind = match status.as_u16() {
    400 if matches!(code, Some(-32022..=-32020)) => ProviderErrorKind::UnexpectedResponse,
    400 | 413 | 414 | 422 => ProviderErrorKind::InvalidRequest,
    401 => ProviderErrorKind::InvalidCredentials,
    402 => ProviderErrorKind::QuotaExhausted,
    403 => ProviderErrorKind::Blocked,
    404 | 408 | 500..=599 => ProviderErrorKind::Transient,
    429 if details.as_ref().is_some_and(|details| tool_error(details).kind == ProviderErrorKind::QuotaExhausted) => {
      ProviderErrorKind::QuotaExhausted
    }
    429 => ProviderErrorKind::RateLimited,
    _ => ProviderErrorKind::UnexpectedResponse,
  };
  let code_detail = code.map(|code| format!("; JSON-RPC code {code}")).unwrap_or_default();
  let version_detail =
    if versions.is_empty() { String::new() } else { format!("; supported versions {}", versions.join(", ")) };
  let mut error = ProviderError::new(
    kind,
    format!("You.com MCP {method} returned HTTP {status} (protocol {protocol}{code_detail}{version_detail})."),
  )
  .with_http_status(status.as_u16())
  .with_retry_after(retry_after)
  .with_upstream_detail(detail);
  if let Some(details) = &details {
    error = error.with_error_payload(details);
  }
  // Only preflight discovery uses this fallback. A recognized modern error
  // must be surfaced, not misinterpreted as permission to change protocols.
  let legacy_fallback = method == "server/discover"
    && match status.as_u16() {
      400 => !matches!(code, Some(-32022..=-32020)),
      404 | 405 => code.is_none() || code == Some(-32601),
      _ => false,
    };
  Err(HttpFailure { error, legacy_fallback })
}

fn metadata(protocol: &'static str) -> Value {
  json!({
    "io.modelcontextprotocol/protocolVersion": protocol,
    "io.modelcontextprotocol/clientInfo": { "name": crate::identity::NAME, "version": env!("CARGO_PKG_VERSION") },
    "io.modelcontextprotocol/clientCapabilities": {}
  })
}

fn known_protocol(value: &str) -> Option<&'static str> {
  match value {
    "2025-03-26" => Some("2025-03-26"),
    "2025-06-18" => Some("2025-06-18"),
    "2025-11-25" => Some("2025-11-25"),
    "2026-07-28" => Some(PROTOCOL),
    _ => None,
  }
}

fn legacy_protocol(versions: &[Value]) -> Option<&'static str> {
  ["2025-11-25", LEGACY_PROTOCOL, "2025-03-26"]
    .into_iter()
    .find(|protocol| versions.iter().any(|version| version.as_str() == Some(*protocol)))
}

fn request_error(error: reqwest::Error) -> ProviderError {
  ProviderError::new(
    ProviderErrorKind::Transient,
    if error.is_timeout() { "You.com MCP request timed out." } else { "You.com MCP request failed." },
  )
}

fn unexpected(message: &'static str) -> ProviderError {
  ProviderError::new(ProviderErrorKind::UnexpectedResponse, message)
}
