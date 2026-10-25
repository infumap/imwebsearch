// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, RequestBuilder, Response, Url};
use serde_json::{Value, json};

use super::mcp::{PROTOCOL, initialize_params, matching_reply, negotiated_protocol, unexpected};
use super::{ProviderError, ProviderErrorKind, ProviderResult};

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;

pub(super) struct Endpoint {
  pub name: &'static str,
  pub url: &'static str,
  pub tool: &'static str,
  pub headers: &'static [(&'static str, &'static str)],
  pub tool_error: fn(&Value) -> Option<ProviderError>,
  pub http_error: fn(u16, Option<&Value>) -> ProviderErrorKind,
}

/// Each attempt has its own negotiated session and bounded cleanup. No shared
/// session can be expired or cancelled by another concurrent search.
pub(super) struct Session {
  endpoint: &'static Endpoint,
  url: Option<Url>,
  client: Client,
  id: Option<HeaderValue>,
  protocol: &'static str,
  pending: Option<u64>,
}

impl Session {
  pub(super) fn new(client: Client, endpoint: &'static Endpoint) -> Self {
    Self { endpoint, url: None, client, id: None, protocol: PROTOCOL, pending: None }
  }

  /// Authenticated endpoints may carry a token in their query. Keep the URL
  /// private and reuse it for initialization, calls, cancellation and cleanup.
  pub(super) fn with_url(mut self, url: Url) -> Self {
    self.url = Some(url);
    self
  }

  pub(super) async fn prepare(&mut self) -> ProviderResult<()> {
    let result = self.rpc(1, "initialize", initialize_params()).await?;
    self.protocol = negotiated_protocol(&result)?;
    let response = self
      .request()
      .json(&json!({
        "jsonrpc": "2.0", "method": "notifications/initialized"
      }))
      .send()
      .await
      .map_err(request_error)?;
    self.check_response(response).await?;
    Ok(())
  }

  pub(super) async fn call(&mut self, arguments: Value) -> ProviderResult<Value> {
    self.pending = Some(2);
    let result = self.rpc(2, "tools/call", json!({ "name": self.endpoint.tool, "arguments": arguments })).await;
    if result.is_ok() {
      self.pending = None;
    }
    result
  }

  fn request(&self) -> RequestBuilder {
    let mut request = self
      .client
      .post(self.url.as_ref().map_or(self.endpoint.url, Url::as_str))
      .header(ACCEPT, "application/json, text/event-stream")
      .header("mcp-protocol-version", self.protocol);
    for &(name, value) in self.endpoint.headers {
      request = request.header(name, value);
    }
    if let Some(id) = &self.id {
      request = request.header("mcp-session-id", id.clone());
    }
    request
  }

  async fn rpc(&mut self, id: u64, method: &'static str, params: Value) -> ProviderResult<Value> {
    let response = self
      .request()
      .json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
      .send()
      .await
      .map_err(request_error)?;
    let response = self.check_response(response).await?;
    let retry_after = super::retry_after(response.headers());
    if method == "initialize"
      && let Some(value) = response.headers().get("mcp-session-id")
    {
      if value.as_bytes().is_empty()
        || value.as_bytes().len() > 1024
        || !value.as_bytes().iter().all(|&byte| (0x21..=0x7e).contains(&byte))
      {
        return Err(unexpected("MCP returned an invalid session identifier."));
      }
      let mut value = value.clone();
      value.set_sensitive(true);
      self.id = Some(value);
    }
    let result = read_reply(response, id, self.endpoint).await.map_err(|error| error.with_retry_after(retry_after))?;
    if method == "tools/call"
      && let Some(error) = (self.endpoint.tool_error)(&result)
    {
      return Err(error.with_error_payload(&result).with_retry_after(retry_after));
    }
    Ok(result)
  }
}

impl Drop for Session {
  fn drop(&mut self) {
    if self.id.is_none() && self.pending.is_none() {
      return;
    }
    let endpoint = self.endpoint;
    let url = self.url.clone();
    let client = self.client.clone();
    let id = self.id.clone();
    let protocol = self.protocol;
    let pending = self.pending;
    tokio::spawn(async move {
      let url = url.as_ref().map_or(endpoint.url, Url::as_str);
      let headers = |request: RequestBuilder| {
        let mut request =
          request.header("mcp-protocol-version", protocol).header(ACCEPT, "application/json, text/event-stream");
        for &(name, value) in endpoint.headers {
          request = request.header(name, value);
        }
        if let Some(id) = &id { request.header("mcp-session-id", id.clone()) } else { request }
      };
      if let Some(request_id) = pending {
        let _ = headers(client.post(url))
          .timeout(Duration::from_secs(2))
          .json(&json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": request_id }
          }))
          .send()
          .await;
      }
      if id.is_some() {
        let _ = headers(client.delete(url)).timeout(Duration::from_secs(2)).send().await;
      }
    });
  }
}

async fn read_reply(mut response: Response, id: u64, endpoint: &Endpoint) -> ProviderResult<Value> {
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
    return Err(unexpected("MCP returned an unsupported response type."));
  }
  let mut buffer = Vec::new();
  let mut received = 0usize;
  while let Some(chunk) = response.chunk().await.map_err(request_error)? {
    received = received
      .checked_add(chunk.len())
      .filter(|&total| total <= MAX_RESPONSE_BYTES)
      .ok_or_else(|| unexpected("MCP response exceeded the size limit."))?;
    buffer.extend_from_slice(&chunk);
    if streaming {
      while let Some(event) = take_event(&mut buffer) {
        let event = std::str::from_utf8(&event).map_err(|_| unexpected("MCP returned invalid UTF-8."))?;
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
        let message: Value = serde_json::from_str(&data).map_err(|_| unexpected("MCP returned invalid event JSON."))?;
        if let Some(result) = matching_reply(message, id, endpoint.tool_error)? {
          return Ok(result);
        }
      }
    }
  }
  if !streaming {
    let message: Value = serde_json::from_slice(&buffer).map_err(|_| unexpected("MCP returned invalid JSON."))?;
    if let Some(result) = matching_reply(message, id, endpoint.tool_error)? {
      return Ok(result);
    }
  }
  Err(unexpected("MCP response ended without a matching JSON-RPC reply."))
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

impl Session {
  async fn check_response(&self, mut response: Response) -> ProviderResult<Response> {
    let status = response.status();
    if status.is_success() {
      return Ok(response);
    }
    let retry_after = super::retry_after(response.headers());
    let mut body = Vec::new();
    let mut complete = true;
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
      if chunk.len() > MAX_ERROR_BYTES - body.len() {
        complete = false;
        break;
      }
      body.extend_from_slice(&chunk);
    }
    let super::ErrorBody { json: details, detail } = super::ErrorBody::parse(&body, complete);
    let kind = (self.endpoint.http_error)(status.as_u16(), details.as_ref());
    let mut error = ProviderError::new(kind, format!("{} returned HTTP {status}.", self.endpoint.name))
      .with_http_status(status.as_u16())
      .with_retry_after(retry_after)
      .with_upstream_detail(detail);
    if let Some(details) = details {
      let detail_retry = (self.endpoint.tool_error)(&details).and_then(|error| error.retry_after);
      error = error.with_error_payload(&details).with_retry_after(detail_retry);
    }
    Err(error)
  }
}

fn request_error(error: reqwest::Error) -> ProviderError {
  ProviderError::new(
    ProviderErrorKind::Transient,
    if error.is_timeout() { "MCP request timed out." } else { "MCP request failed." },
  )
}
