// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use log::{info, warn};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fetch::{self, DEFAULT_MAX_CHARS, MAX_CHARS_CAP};
use crate::routing::{self, SearchTrace};
use crate::search::{self, DEFAULT_MAX_RESULTS, MAX_RESULTS_CAP};

/// Unknown arguments, including the former `numResults` and `kind`, are ignored.
#[derive(Deserialize)]
struct WebSearchArguments {
  query: Option<String>,
  text: Option<String>,
  #[serde(rename = "maxResults")]
  max_results: Option<i64>,
}

#[derive(Deserialize)]
struct FetchPageArguments {
  url: Option<String>,
  #[serde(rename = "maxChars")]
  max_chars: Option<i64>,
  offset: Option<i64>,
}

#[derive(Clone, Copy)]
pub enum WebTool {
  Search,
  Fetch,
}

impl WebTool {
  pub fn name(self) -> &'static str {
    match self {
      Self::Search => "web_search",
      Self::Fetch => "web_fetch",
    }
  }
}

pub fn tools_list(tools: &[WebTool]) -> Value {
  json!({ "tools": tools.iter().copied().map(tool_definition).collect::<Vec<_>>() })
}

fn tool_definition(tool: WebTool) -> Value {
  match tool {
    WebTool::Search => json!({
      "name": tool.name(),
      "title": "Search the web",
      "description": concat!(
        "Search the public web for URLs and snippets. ",
        "Search sequentially so results can guide the next query. ",
        "Match search effort to the request. Stop when results support a useful answer. ",
        "Search further only to resolve material gaps; prefer supported alternatives over repeatedly chasing ",
        "unverified candidates."
      ),
      "inputSchema": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "description": "Focused web search query."
          },
          "maxResults": {
            "type": "integer",
            "minimum": 1,
            "maximum": MAX_RESULTS_CAP,
            "description": format!("Maximum number of search results to return (default: {DEFAULT_MAX_RESULTS}).")
          }
        },
        "required": ["query"],
        "additionalProperties": false
      },
      "annotations": { "readOnlyHint": true }
    }),
    WebTool::Fetch => json!({
      "name": tool.name(),
      "title": "Fetch page",
      "description": concat!(
        "Read page text from an HTTP or HTTPS URL. ",
        "Fetch only sections needed to answer the user's question. ",
        "A truncated response does not mean you need to retrieve the rest. ",
        "A long page returns its first part with nextOffset and an outline of headings with offsets; ",
        "if more evidence is needed, fetch the same URL again with offset to continue or to jump to a section."
      ),
      "inputSchema": {
        "type": "object",
        "properties": {
          "url": {
            "type": "string",
            "description": "HTTP or HTTPS URL to fetch."
          },
          "maxChars": {
            "type": "integer",
            "minimum": 1,
            "maximum": MAX_CHARS_CAP,
            "description": format!("Maximum number of characters of page text to return (default: {DEFAULT_MAX_CHARS}).")
          },
          "offset": {
            "type": "integer",
            "minimum": 0,
            "description": "Character offset to start from: a nextOffset or outline offset from an earlier response."
          }
        },
        "required": ["url"],
        "additionalProperties": false
      },
      "annotations": { "readOnlyHint": true }
    }),
  }
}

fn tool_error_json(message: &str) -> String {
  json!({ "error": message }).to_string()
}

fn tool_result(text: String, is_error: bool) -> Value {
  json!({
    "content": [{ "type": "text", "text": text }],
    "isError": is_error
  })
}

/// Execute a named tool. Failures are returned as MCP `isError` results, not JSON-RPC errors.
pub async fn call_tool(name: &str, arguments: Value) -> Value {
  match name {
    "web_search" => call_web_search(arguments).await,
    "web_fetch" => call_fetch_page(arguments).await,
    other => tool_result(tool_error_json(&format!("Unknown tool '{other}'.")), true),
  }
}

async fn call_web_search(arguments: Value) -> Value {
  let arguments: WebSearchArguments = match serde_json::from_value(arguments) {
    Ok(arguments) => arguments,
    Err(e) => {
      return tool_result(tool_error_json(&format!("Could not parse web_search tool arguments: {}", e)), true);
    }
  };
  let query = arguments.query.or(arguments.text).unwrap_or_default();
  if query.trim().is_empty() {
    return tool_result(tool_error_json("web_search tool argument 'query' is required."), true);
  }
  let num_results =
    arguments.max_results.unwrap_or(DEFAULT_MAX_RESULTS as i64).clamp(1, MAX_RESULTS_CAP as i64) as usize;
  let mut trace = SearchTrace::default();
  let response = search::search_web_json(&query, num_results, &mut trace).await;
  routing::record(query.trim(), &trace, None);
  match response {
    Ok(response) => tool_result(response, false),
    Err(e) => {
      // Provider routing, credentials and budget diagnostics belong in server
      // logs, not in the model's context. Argument errors are handled above.
      warn!("Web search failed: {e}");
      tool_result(tool_error_json("Web search is currently unavailable."), true)
    }
  }
}

async fn call_fetch_page(arguments: Value) -> Value {
  let arguments: FetchPageArguments = match serde_json::from_value(arguments) {
    Ok(arguments) => arguments,
    Err(e) => {
      return tool_result(tool_error_json(&format!("Could not parse web_fetch tool arguments: {}", e)), true);
    }
  };
  let url = arguments.url.unwrap_or_default();
  if url.trim().is_empty() {
    return tool_result(tool_error_json("web_fetch tool argument 'url' is required."), true);
  }
  let max_chars = arguments.max_chars.unwrap_or(DEFAULT_MAX_CHARS as i64).clamp(1, MAX_CHARS_CAP as i64) as usize;
  let offset = arguments.offset.unwrap_or(0).max(0) as usize;
  let started = std::time::Instant::now();
  let fetch_id = fetch::next_fetch_id();
  let fetched = fetch::fetch_url(&url, fetch_id).await;
  let ms = started.elapsed().as_millis();
  match fetched.and_then(|(page, cached)| {
    let chunk = fetch::pages::chunk(&page.text, offset, max_chars)?;
    let end = chunk.next_offset.unwrap_or(chunk.total_chars);
    let transport = if cached { "page cache" } else { page.transport };
    info!(
      "[fetch {fetch_id}] Fetched {} ({} via {transport}, chars {offset}–{end} of {}, {ms} ms)",
      url.trim(),
      page.via,
      chunk.total_chars
    );
    fetch::page_chunk_json(&page, &chunk)
  }) {
    Ok(response) => tool_result(response, false),
    Err(e) => {
      warn!("[fetch {fetch_id}] Fetch failed {} ({ms} ms): {e}", url.trim());
      tool_result(tool_error_json(&e.to_string()), true)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn web_search_requires_query() {
    let result = call_tool("web_search", json!({})).await;
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("query"), "{text}");
  }

  #[tokio::test]
  async fn fetch_page_requires_url() {
    let result = call_tool("fetch_page", json!({})).await;
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("url"), "{text}");
  }

  #[tokio::test]
  async fn unknown_tool_is_error() {
    let result = call_tool("not_a_tool", json!({})).await;
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Unknown tool"), "{text}");
  }

  #[tokio::test]
  async fn fetch_page_blocks_loopback() {
    let result = call_tool("fetch_page", json!({ "url": "http://127.0.0.1/" })).await;
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Blocked"), "{text}");
  }
}
