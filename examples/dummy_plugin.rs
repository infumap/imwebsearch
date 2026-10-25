// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Minimal search-provider plugin implementing PLUGINS.md, contract version 1.
//! Declares two providers that each return one canned result. Set
//! DUMMY_PLUGIN_ERROR to an error kind to return that error instead, with an
//! optional DUMMY_PLUGIN_RETRY_AFTER_SECS.
//!
//! Build with `cargo build --release --example dummy_plugin`.

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

const PROTOCOLS: [&str; 3] = ["2025-03-26", "2025-06-18", "2025-11-25"];
const PROVIDERS: [&str; 2] = ["dummy_alpha", "dummy_beta"];

fn main() -> io::Result<()> {
  let mut stdout = io::stdout().lock();
  for line in io::stdin().lock().lines() {
    let line = line?;
    if line.trim().is_empty() {
      continue;
    }
    let reply = match serde_json::from_str::<Value>(&line) {
      Ok(message) => handle(&message),
      Err(_) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error." } })),
    };
    if let Some(reply) = reply {
      writeln!(stdout, "{reply}")?;
      stdout.flush()?;
    }
  }
  // imwebsearch closes stdin after the reply; exit promptly.
  Ok(())
}

/// Notifications and responses carry no request to answer.
fn handle(message: &Value) -> Option<Value> {
  let id = message.get("id")?.clone();
  let method = message.get("method").and_then(Value::as_str)?;
  let params = message.get("params").unwrap_or(&Value::Null);
  let result = match method {
    "initialize" => Ok(initialize(params)),
    "ping" => Ok(json!({})),
    "tools/list" => Ok(json!({ "tools": PROVIDERS.map(tool) })),
    "tools/call" => call(params),
    _ => Err((-32601, "Method not found.")),
  };
  Some(match result {
    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
  })
}

fn initialize(params: &Value) -> Value {
  let requested = params.get("protocolVersion").and_then(Value::as_str);
  json!({
    "protocolVersion": requested.filter(|version| PROTOCOLS.contains(version)).unwrap_or("2025-06-18"),
    "capabilities": { "tools": {} },
    "serverInfo": { "name": "dummy_plugin", "version": env!("CARGO_PKG_VERSION") }
  })
}

fn tool(name: &str) -> Value {
  json!({
    "name": name,
    "description": "Dummy search provider returning one canned result.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "query": { "type": "string" },
        "max_results": { "type": "integer", "minimum": 1, "maximum": 30 }
      },
      "required": ["query", "max_results"]
    },
    "_meta": {
      // Below every built-in tier, so canned results never displace real ones.
      "imwebsearch/provider": { "version": 1, "tier": 5, "requests_per_minute": 60 }
    }
  })
}

fn call(params: &Value) -> Result<Value, (i64, &'static str)> {
  let name = params
    .get("name")
    .and_then(Value::as_str)
    .filter(|name| PROVIDERS.contains(name))
    .ok_or((-32602, "Unknown tool."))?;
  let arguments = params.get("arguments").unwrap_or(&Value::Null);
  let query = arguments
    .get("query")
    .and_then(Value::as_str)
    .map(str::trim)
    .filter(|query| !query.is_empty())
    .ok_or((-32602, "query must be a non-empty string."))?;
  let max_results = arguments
    .get("max_results")
    .and_then(Value::as_u64)
    .filter(|count| (1..=30).contains(count))
    .ok_or((-32602, "max_results must be an integer from 1 to 30."))?;
  eprintln!("dummy_plugin: {name} query={query:?} max_results={max_results}");

  if let Ok(kind) = std::env::var("DUMMY_PLUGIN_ERROR") {
    let mut error = json!({ "kind": kind, "code": "simulated" });
    if let Some(seconds) =
      std::env::var("DUMMY_PLUGIN_RETRY_AFTER_SECS").ok().and_then(|value| value.parse::<u64>().ok())
    {
      error["retry_after_secs"] = json!(seconds);
    }
    return Ok(json!({
      "isError": true,
      "content": [{ "type": "text", "text": format!("Simulated {kind} error.") }],
      "structuredContent": error
    }));
  }

  let results = json!({
    "results": [{
      "title": format!("Dummy result from {name}"),
      "url": format!("https://example.com/{name}"),
      "snippet": format!("Canned result for query {query:?}.")
    }]
  });
  Ok(json!({ "content": [{ "type": "text", "text": results.to_string() }], "structuredContent": results }))
}
