// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::time::Duration;

use reqwest::Response;
use serde_json::Value;

use super::ProviderError;

const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_DETAIL_CHARS: usize = 300;

/// A bounded error body. Never retain or expose the raw body: keep its JSON
/// only when complete, and otherwise a short sanitized summary for debug logs.
pub(crate) struct ErrorBody {
  pub json: Option<Value>,
  pub detail: Option<String>,
}

impl ErrorBody {
  /// `complete` is false when the body was cut at the size limit.
  pub(crate) fn parse(body: &[u8], complete: bool) -> Self {
    let json: Option<Value> = if complete { serde_json::from_slice(body).ok() } else { None };
    if json.is_some() {
      return Self { json, detail: None };
    }
    // Rejection pages from CDNs and bot filters are HTML, not provider JSON.
    let text = String::from_utf8_lossy(body);
    let detail = if text.trim_start().starts_with('<') {
      Some(super::html_fingerprint::fingerprint(&text))
    } else {
      detail(&text)
    };
    Self { json, detail }
  }
}

pub(crate) async fn read_error_body(response: &mut Response) -> ErrorBody {
  let mut body = Vec::new();
  let mut complete = true;
  while let Ok(Some(chunk)) = response.chunk().await {
    let room = MAX_ERROR_BYTES - body.len();
    if chunk.len() > room {
      body.extend_from_slice(&chunk[..room]);
      complete = false;
      break;
    }
    body.extend_from_slice(&chunk);
  }
  ErrorBody::parse(&body, complete)
}

pub(crate) async fn response_error(error: ProviderError, response: &mut Response) -> ProviderError {
  let error =
    error.with_http_status(response.status().as_u16()).with_retry_after(super::retry_after(response.headers()));
  let body = read_error_body(response).await;
  match &body.json {
    Some(payload) => error.with_error_payload(payload),
    None => error,
  }
  .with_upstream_detail(body.detail)
}

pub(super) fn apply(error: ProviderError, payload: &Value) -> ProviderError {
  apply_inner(error, payload, 0)
}

fn apply_inner(mut error: ProviderError, payload: &Value, depth: usize) -> ProviderError {
  if depth >= 4 {
    return error;
  }
  // Prefer a specific structured/text error over a generic JSON-RPC code.
  for field in ["structuredContent", "error", "data", "detail"] {
    if let Some(value) = payload.get(field).filter(|value| value.is_object()) {
      error = apply_inner(error, value, depth + 1);
    }
  }
  if let Some(content) = payload.get("content").and_then(Value::as_array) {
    for item in content.iter().filter(|item| item["type"] == "text") {
      if let Some(text) = item["text"].as_str().filter(|text| text.len() <= MAX_ERROR_BYTES) {
        if let Ok(value) = serde_json::from_str::<Value>(text) {
          error = apply_inner(error, &value, depth + 1);
        } else {
          if error.upstream_detail.is_none() {
            error.upstream_detail = detail(text);
          }
          for line in text.lines() {
            let line = line.trim().to_ascii_lowercase();
            if let Some(raw) = line.strip_prefix("retry after:").or_else(|| line.strip_prefix("retry-after:")) {
              error =
                error.with_retry_after(seconds(&Value::String(raw.trim().trim_end_matches('s').trim().to_owned())));
            }
            if error.rejection_code.is_none() {
              error.rejection_code = line.strip_prefix("code:").and_then(|code| safe_code(code.trim()));
            }
          }
        }
      }
    }
  }
  if error.upstream_detail.is_none() {
    error.upstream_detail = ["message", "error", "detail", "error_description"]
      .into_iter()
      .find_map(|field| payload.get(field).and_then(Value::as_str).and_then(detail));
  }
  if error.rejection_code.is_none() {
    for field in ["code", "error_code", "errorCode"] {
      if let Some(value) = payload.get(field) {
        let code = match value {
          Value::String(value) => safe_code(value),
          Value::Number(value) if value.is_i64() || value.is_u64() => Some(value.to_string()),
          _ => None,
        };
        if code.is_some() {
          error.rejection_code = code;
          break;
        }
      }
    }
  }
  for field in ["retry_after_seconds", "retryAfterSeconds", "retry_after", "retryAfter"] {
    error = error.with_retry_after(payload.get(field).and_then(seconds));
  }
  if let Some(actions) = payload.get("next_actions").and_then(Value::as_array) {
    for action in actions.iter().take(16) {
      // Read only timing metadata; never execute upstream continuation actions.
      error = error.with_retry_after(action.get("after_seconds").and_then(seconds));
    }
  }
  if error.http_status.is_none() {
    for field in ["status", "statusCode", "status_code"] {
      if let Some(status) = payload.get(field).and_then(Value::as_u64).filter(|status| (400..=599).contains(status)) {
        error.http_status = Some(status as u16);
        break;
      }
    }
  }
  error
}

/// Collapse whitespace and control characters so one log line stays one line.
fn detail(text: &str) -> Option<String> {
  let words: Vec<&str> = text.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect();
  (!words.is_empty()).then(|| redact(&words.join(" ")).chars().take(MAX_DETAIL_CHARS).collect())
}

/// Error bodies can echo API keys, session IDs or signed tokens. Mask any run
/// of 24 or more token characters that mixes letters and digits.
pub(super) fn redact(text: &str) -> String {
  let mut output = String::with_capacity(text.len());
  let mut token = String::new();
  let flush = |token: &mut String, output: &mut String| {
    let secret = token.chars().count() >= 24
      && token.chars().any(|c| c.is_ascii_digit())
      && token.chars().any(|c| c.is_ascii_alphabetic());
    output.push_str(if secret { "[redacted]" } else { token });
    token.clear();
  };
  for c in text.chars() {
    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
      token.push(c);
    } else {
      flush(&mut token, &mut output);
      output.push(c);
    }
  }
  flush(&mut token, &mut output);
  output
}

fn safe_code(code: &str) -> Option<String> {
  (!code.is_empty()
    && code.len() <= 96
    && code.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte)))
  .then(|| code.to_owned())
}

fn seconds(value: &Value) -> Option<Duration> {
  if let Some(seconds) = value.as_u64().or_else(|| value.as_str()?.trim().parse().ok()) {
    return Some(Duration::from_secs(seconds));
  }
  let seconds = value.as_f64().or_else(|| value.as_str()?.trim().parse().ok())?;
  Duration::try_from_secs_f64(seconds).ok()
}
