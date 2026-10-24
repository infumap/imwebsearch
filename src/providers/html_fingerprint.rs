// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! A one-line summary of an HTML page a scraper could not read, so an operator
//! can tell a changed layout from a challenge or block page in the log.

use once_cell::sync::Lazy;
use scraper::{Html, Node, Selector};

const MAX_TITLE_CHARS: usize = 80;
const MAX_TEXT_CHARS: usize = 160;
const MAX_ATTR_CHARS: usize = 60;
const MAX_FORMS: usize = 3;
const MAX_SCRIPTS: usize = 5;

static TITLE_SELECTOR: Lazy<Selector> = Lazy::new(|| Selector::parse("title").expect("title selector"));
static FORM_SELECTOR: Lazy<Selector> = Lazy::new(|| Selector::parse("form[action]").expect("form selector"));
static SCRIPT_SELECTOR: Lazy<Selector> = Lazy::new(|| Selector::parse("script[src]").expect("script selector"));

/// Size, title, form actions, script file names and the start of the visible
/// text, each bounded. Debug-formatted strings keep it on one line.
pub(crate) fn fingerprint(html: &str) -> String {
  let document = Html::parse_document(html);
  let title =
    document.select(&TITLE_SELECTOR).next().map(|title| bounded(&title.text().collect::<String>(), MAX_TITLE_CHARS));
  let forms: Vec<String> = document
    .select(&FORM_SELECTOR)
    .filter_map(|form| form.value().attr("action"))
    .map(|action| bounded(action, MAX_ATTR_CHARS))
    .take(MAX_FORMS)
    .collect();
  // File names identify challenge scripts; full URLs mostly add query noise.
  let scripts: Vec<String> = document
    .select(&SCRIPT_SELECTOR)
    .filter_map(|script| script.value().attr("src"))
    .map(|src| {
      let path = src.split(['?', '#']).next().unwrap_or(src);
      bounded(path.rsplit('/').find(|segment| !segment.is_empty()).unwrap_or(path), MAX_ATTR_CHARS)
    })
    .take(MAX_SCRIPTS)
    .collect();
  let mut text = String::new();
  for node in document.root_element().descendants() {
    let Node::Text(content) = node.value() else { continue };
    let hidden = node
      .parent()
      .and_then(|parent| parent.value().as_element().map(|element| element.name()))
      .is_some_and(|name| matches!(name, "script" | "style" | "noscript" | "template" | "title"));
    if !hidden {
      text.push(' ');
      text.push_str(content);
      if text.len() > MAX_TEXT_CHARS * 8 {
        break;
      }
    }
  }
  super::error_metadata::redact(&format!(
    "bytes={} title={:?} forms={forms:?} scripts={scripts:?} text={:?}",
    html.len(),
    title.unwrap_or_default(),
    bounded(&text, MAX_TEXT_CHARS)
  ))
}

fn bounded(value: &str, max_chars: usize) -> String {
  value.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max_chars).collect()
}
