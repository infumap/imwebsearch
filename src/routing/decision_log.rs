// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! One line per search recording fallback routing.
//! Records contain queries and result titles;
//! owner-only permissions do not replace operator-managed retention/rotation.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use jiff::Timestamp;
use log::warn;
use serde_json::{Value, json};

use super::SearchTrace;
use crate::config;

const FILE_NAME: &str = "search-routing.jsonl";
const MAX_LABEL_CHARS: usize = 64;
const MAX_MODEL_CHARS: usize = 200;

/// Serializes appends so concurrent calls never interleave within a line.
static APPEND: Mutex<()> = Mutex::new(());

/// Append one record without delaying the search, with optional local diagnostic labels.
pub fn record(query: &str, trace: &SearchTrace, context: Option<&Value>) {
  let Some(directory) = config::search_config().ok().and_then(|config| config.state_dir.clone()) else { return };
  let field = |name: &str| context.and_then(|context| context.get(name));
  let line = json!({
    "at": Timestamp::now().as_second(),
    "mode": field("mode").map(|mode| bounded(mode, MAX_LABEL_CHARS)),
    "model": field("model").map(|model| bounded(model, MAX_MODEL_CHARS)),
    "call": field("number").and_then(Value::as_u64),
    "query": query,
    "route": trace,
  })
  .to_string();
  tokio::task::spawn_blocking(move || {
    if let Err(error) = append(&directory, &line) {
      warn!("Could not record web_search routing: {error}");
    }
  });
}

fn bounded(value: &Value, max_chars: usize) -> Value {
  match value.as_str() {
    Some(text) if text.chars().count() <= max_chars => Value::String(text.to_owned()),
    _ => Value::String("<invalid>".to_owned()),
  }
}

fn append(directory: &Path, line: &str) -> std::io::Result<()> {
  let _guard = APPEND.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let mut builder = DirBuilder::new();
  builder.recursive(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
  }
  builder.create(directory)?;
  let mut options = OpenOptions::new();
  options.append(true).create(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  let mut file = options.open(directory.join(FILE_NAME))?;
  file.write_all(format!("{line}\n").as_bytes())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bounds_labels() {
    assert_eq!(bounded(&json!("chat"), MAX_LABEL_CHARS), json!("chat"));
    assert_eq!(bounded(&json!(3), MAX_LABEL_CHARS), json!("<invalid>"));
    assert_eq!(bounded(&json!("x".repeat(MAX_LABEL_CHARS + 1)), MAX_LABEL_CHARS), json!("<invalid>"));
  }
}
