// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! MCP lifecycle and JSON-RPC reply handling shared by the HTTP and stdio
//! transports.

use serde_json::{Value, json};

use super::{ProviderError, ProviderErrorKind, ProviderResult};

pub(super) const PROTOCOL: &str = "2025-06-18";

pub(super) fn initialize_params() -> Value {
  json!({
    "protocolVersion": PROTOCOL,
    "capabilities": {},
    "clientInfo": { "name": crate::identity::NAME, "version": env!("CARGO_PKG_VERSION") }
  })
}

/// Accept only protocol versions whose tool semantics this client implements.
pub(super) fn negotiated_protocol(initialize_result: &Value) -> ProviderResult<&'static str> {
  let protocol = match initialize_result.get("protocolVersion").and_then(Value::as_str) {
    Some("2025-03-26") => "2025-03-26",
    Some("2025-06-18") => "2025-06-18",
    Some("2025-11-25") => "2025-11-25",
    _ => return Err(unexpected("MCP negotiated an unsupported protocol version.")),
  };
  if !initialize_result.pointer("/capabilities/tools").is_some_and(Value::is_object) {
    return Err(unexpected("MCP did not advertise tool support."));
  }
  Ok(protocol)
}

/// None means the message is not the reply to `id`. `tool_error` classifies
/// JSON-RPC error objects beyond invalid requests.
pub(super) fn matching_reply(
  message: Value,
  id: u64,
  tool_error: fn(&Value) -> Option<ProviderError>,
) -> ProviderResult<Option<Value>> {
  if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
    return Err(unexpected("MCP returned an invalid JSON-RPC envelope."));
  }
  if message.get("id").and_then(Value::as_u64) != Some(id) {
    return Ok(None);
  }
  if let Some(error) = message.get("error") {
    if matches!(error.get("code").and_then(Value::as_i64), Some(-32602..=-32600)) {
      return Err(
        ProviderError::new(ProviderErrorKind::InvalidRequest, "MCP rejected the request parameters.")
          .with_error_payload(error),
      );
    }
    return Err(
      tool_error(error).unwrap_or_else(|| unexpected("MCP reported a protocol error.")).with_error_payload(error),
    );
  }
  message
    .get("result")
    .filter(|result| result.is_object())
    .cloned()
    .map(Some)
    .ok_or_else(|| unexpected("MCP reply omitted its result."))
}

pub(super) fn unexpected(message: &'static str) -> ProviderError {
  ProviderError::new(ProviderErrorKind::UnexpectedResponse, message)
}
