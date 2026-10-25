// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Stateless request metadata and HTTP binding for MCP 2026-07-28.
//! Keep validation here so initialization-based clients retain their earlier wire format.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hyper::{HeaderMap, StatusCode};
use serde_json::{Value, json};

use super::{
  HEADER_PROTOCOL_VERSION, PROTOCOL_VERSION_2026_07_28, SUPPORTED_VERSIONS, dispatch_method,
  legacy_protocol_version_supported, rpc_result, server_info,
};
use crate::tools::WebTool;

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";

pub(super) fn is_modern(headers: &HeaderMap, message: &Value) -> bool {
  let metadata = message.get("params").and_then(|params| params.get("_meta"));
  if metadata
    .and_then(|metadata| metadata.get(META_VERSION))
    .is_some_and(|version| version.as_str().is_none_or(|version| !legacy_protocol_version_supported(version)))
  {
    return true;
  }
  if let Some(version) = headers.get(HEADER_PROTOCOL_VERSION) {
    return version.to_str().map_or(true, |version| !legacy_protocol_version_supported(version));
  }
  message.get("method").and_then(Value::as_str) == Some("server/discover")
    || headers.contains_key("mcp-method")
    || metadata.is_some_and(|metadata| metadata.get(META_CAPABILITIES).is_some())
}

fn valid_id(id: &Value) -> bool {
  id.is_string() || id.is_i64() || id.is_u64()
}

fn error(id: Option<&Value>, code: i64, message: &str) -> Value {
  let mut payload = json!({ "jsonrpc": "2.0", "error": { "code": code, "message": message } });
  if let Some(id) = id.filter(|id| valid_id(id)) {
    payload["id"] = id.clone();
  }
  payload
}

fn reject(id: Option<&Value>, code: i64, message: &str) -> (StatusCode, Option<Value>) {
  (StatusCode::BAD_REQUEST, Some(error(id, code, message)))
}

/// The standard Mcp-Name header can contain an encoded Unicode name or URI.
/// Decode exactly one header before comparing it with the authoritative body.
fn header_matches(headers: &HeaderMap, name: &str, expected: &str, allow_base64: bool) -> bool {
  let mut values = headers.get_all(name).iter();
  let Some(value) = values.next() else { return false };
  if values.next().is_some() {
    return false;
  }
  let Ok(value) = value.to_str() else { return false };
  if !value.bytes().all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte)) {
    return false;
  }
  if allow_base64 && let Some(encoded) = value.strip_prefix("=?base64?").and_then(|value| value.strip_suffix("?=")) {
    return STANDARD.decode(encoded).is_ok_and(|decoded| decoded == expected.as_bytes());
  }
  value == expected
}

fn validate_metadata(headers: &HeaderMap, method: &str, params: &Value) -> Result<(), (i64, &'static str)> {
  let metadata = params
    .get("_meta")
    .filter(|metadata| metadata.is_object())
    .ok_or((-32602, "Invalid params: _meta must contain the protocol version and client capabilities."))?;
  let version = metadata
    .get(META_VERSION)
    .and_then(Value::as_str)
    .ok_or((-32602, "Invalid params: _meta['io.modelcontextprotocol/protocolVersion'] must be a string."))?;
  if !metadata.get(META_CAPABILITIES).is_some_and(Value::is_object) {
    return Err((-32602, "Invalid params: _meta['io.modelcontextprotocol/clientCapabilities'] must be an object."));
  }
  if let Some(info) = metadata.get(META_CLIENT_INFO)
    && (!info.get("name").is_some_and(Value::is_string) || !info.get("version").is_some_and(Value::is_string))
  {
    return Err((-32602, "Invalid params: clientInfo must contain a string name and version."));
  }
  if !header_matches(headers, HEADER_PROTOCOL_VERSION, version, false) {
    return Err((-32020, "Header mismatch: MCP-Protocol-Version must match the protocol version in _meta."));
  }
  if !header_matches(headers, "mcp-method", method, false) {
    return Err((-32020, "Header mismatch: Mcp-Method must match the request method."));
  }
  let name_field = match method {
    "tools/call" | "prompts/get" => Some("name"),
    "resources/read" => Some("uri"),
    _ => None,
  };
  if let Some(field) = name_field {
    let name = params.get(field).and_then(Value::as_str).ok_or((-32602, "Invalid params: missing name or URI."))?;
    if !header_matches(headers, "mcp-name", name, true) {
      return Err((-32020, "Header mismatch: Mcp-Name must match the request name or URI."));
    }
  }
  Ok(())
}

pub(super) async fn handle_rpc(tools: &[WebTool], headers: &HeaderMap, message: &Value) -> (StatusCode, Option<Value>) {
  let id = message.get("id");
  if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
    || id.is_some_and(|id| !valid_id(id))
    || message.get("result").is_some()
    || message.get("error").is_some()
  {
    return reject(id, -32600, "Invalid Request");
  }
  let Some(method) = message.get("method").and_then(Value::as_str) else {
    return reject(id, -32600, "Invalid Request: missing method.");
  };
  let Some(id) = id else {
    // This revision defines no client notifications for HTTP. Ignore extension notifications
    // without executing request methods such as tools/call as a side effect.
    return (StatusCode::ACCEPTED, None);
  };
  let params = &message["params"];
  if let Err((code, message)) = validate_metadata(headers, method, params) {
    return reject(Some(id), code, message);
  }
  let version = params["_meta"][META_VERSION].as_str().expect("Validated protocol version");
  if version != PROTOCOL_VERSION_2026_07_28 {
    let mut payload = error(Some(id), -32022, "Unsupported protocol version");
    payload["error"]["data"] = json!({ "supported": SUPPORTED_VERSIONS, "requested": version });
    return (StatusCode::BAD_REQUEST, Some(payload));
  }
  if method == "tools/call" {
    let name = params["name"].as_str().expect("Validated tool name");
    if !tools.iter().any(|tool| name == tool.name()) {
      return reject(Some(id), -32602, "Unknown tool for this endpoint.");
    }
    if params.get("arguments").is_some_and(|arguments| !arguments.is_object()) {
      return reject(Some(id), -32602, "Invalid params: arguments must be an object.");
    }
  }
  let result = match method {
    "server/discover" => Ok(json!({ "supportedVersions": SUPPORTED_VERSIONS, "capabilities": { "tools": {} } })),
    // A modern request cannot select the initialization-based lifecycle.
    "initialize" | "notifications/initialized" => Err((-32601, "Method not found".to_owned())),
    _ => dispatch_method(tools, method, params.clone()).await,
  };
  match result {
    Ok(mut result) => {
      result["resultType"] = json!("complete");
      result["_meta"] = json!({ "io.modelcontextprotocol/serverInfo": server_info() });
      (StatusCode::OK, Some(rpc_result(id.clone(), result)))
    }
    Err((code, message)) => {
      let status = if code == -32601 { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
      (status, Some(error(Some(id), code, &message)))
    }
  }
}
