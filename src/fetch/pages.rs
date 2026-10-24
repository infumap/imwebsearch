// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Long pages are read in chunks. Extracted text is cached briefly so that a
//! follow-up fetch with `offset` does not render the page again, and a
//! truncated first chunk carries an outline of the page's headings with their
//! offsets, so a model can jump to a section instead of reading in order.
//! The text itself is always the page's own: nothing here summarizes it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::FetchedPage;
use crate::error::InfuResult;

/// Long enough for a model to page through a document within a task.
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Extracted text only; a long Wikipedia article is under 400 KB.
const CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_OUTLINE_ENTRIES: usize = 50;
const MAX_HEADING_CHARS: usize = 120;

struct Entry {
  key: String,
  page: Arc<FetchedPage>,
  stored: Instant,
}

static CACHE: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());

/// A cached page fetched within the last ten minutes. The fetch that stored
/// it applied the destination policy.
pub(super) fn cached(key: &str) -> Option<Arc<FetchedPage>> {
  let mut cache = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  cache.retain(|entry| entry.stored.elapsed() < CACHE_TTL);
  cache.iter().find(|entry| entry.key == key).map(|entry| entry.page.clone())
}

/// Store a page, evicting the oldest entries beyond the byte budget.
pub(super) fn remember(key: String, page: Arc<FetchedPage>) {
  if page.text.len() > CACHE_MAX_BYTES {
    return;
  }
  let mut cache = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  cache.retain(|entry| entry.key != key && entry.stored.elapsed() < CACHE_TTL);
  cache.push_back(Entry { key, page, stored: Instant::now() });
  let mut total: usize = cache.iter().map(|entry| entry.page.text.len()).sum();
  while total > CACHE_MAX_BYTES
    && let Some(oldest) = cache.pop_front()
  {
    total -= oldest.page.text.len();
  }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Heading {
  pub offset: usize,
  pub level: usize,
  pub heading: String,
}

/// One chunk of a page's text, in characters.
pub struct Chunk {
  pub text: String,
  pub offset: usize,
  pub total_chars: usize,
  pub next_offset: Option<usize>,
  pub outline: Option<Vec<Heading>>,
}

pub fn chunk(text: &str, offset: usize, max_chars: usize) -> InfuResult<Chunk> {
  if text.trim().is_empty() {
    return Ok(Chunk {
      text: "(page returned no readable text)".to_owned(),
      offset: 0,
      total_chars: 0,
      next_offset: None,
      outline: None,
    });
  }
  let total_chars = text.chars().count();
  if offset >= total_chars {
    return Err(format!("offset {offset} is beyond the end of the page ({total_chars} characters).").into());
  }
  let start = byte_index(text, offset);
  let end = byte_index(text, offset + max_chars);
  let mut body = text[start..end].to_owned();
  let next_offset = (end < text.len()).then_some(offset + max_chars);
  if let Some(next) = next_offset {
    // In the text too, for models that read only the text.
    body
      .push_str(&format!("\n\n... (truncated: {total_chars} chars total; fetch again with offset {next} to continue)"));
  }
  // The outline helps choose where to read next, so it comes with the first
  // chunk of a page that does not fit, not with every chunk.
  let outline = (offset == 0 && next_offset.is_some()).then(|| outline(text)).filter(|outline| !outline.is_empty());
  Ok(Chunk { text: body, offset, total_chars, next_offset, outline })
}

/// Byte position of a character offset, clamped to the end.
fn byte_index(text: &str, chars: usize) -> usize {
  text.char_indices().nth(chars).map_or(text.len(), |(index, _)| index)
}

/// Markdown headings outside code fences, with the character offset where
/// each line starts. Long outlines keep the top two levels, then the first 50.
fn outline(text: &str) -> Vec<Heading> {
  let mut headings = Vec::new();
  let mut fence = false;
  let mut offset = 0;
  for line in text.split_inclusive('\n') {
    let trimmed = line.trim();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
      fence = !fence;
    } else if !fence {
      let level = trimmed.bytes().take_while(|&byte| byte == b'#').count();
      if (1..=6).contains(&level) && trimmed[level..].starts_with(' ') {
        let heading = plain_heading(&trimmed[level..]);
        if !heading.is_empty() {
          headings.push(Heading { offset, level, heading });
        }
      }
    }
    offset += line.chars().count();
  }
  if headings.len() > MAX_OUTLINE_ENTRIES {
    headings.retain(|heading| heading.level <= 2);
    headings.truncate(MAX_OUTLINE_ENTRIES);
  }
  headings
}

/// Heading text without Markdown link or image syntax: `[Text](url)` → `Text`.
fn plain_heading(markdown: &str) -> String {
  let mut plain = String::new();
  let mut rest = markdown.trim();
  while let Some(open) = rest.find('[') {
    let Some(close) = rest[open..].find("](").map(|index| open + index) else { break };
    let Some(end) = rest[close..].find(')').map(|index| close + index) else { break };
    plain.push_str(rest[..open].trim_end_matches('!'));
    plain.push_str(&rest[open + 1..close]);
    rest = &rest[end + 1..];
  }
  plain.push_str(rest);
  let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
  plain.chars().take(MAX_HEADING_CHARS).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  const PAGE: &str = "# Barack Obama\n\nLead text here.\n\n## Early life\n\nBorn in Honolulu.\n\n```\n# not a heading\n```\n\n### [Education](/wiki/Education)\n\nColumbia and Harvard.\n";

  #[test]
  fn returns_a_short_page_whole() {
    let whole = chunk(PAGE, 0, 10_000).unwrap();
    assert_eq!(whole.text, PAGE);
    assert_eq!((whole.next_offset, whole.outline), (None, None));
  }

  #[test]
  fn continues_from_offsets_with_an_outline_on_the_first_chunk() {
    let first = chunk(PAGE, 0, 20).unwrap();
    assert!(first.text.starts_with("# Barack Obama\n\nLead"), "{}", first.text);
    assert!(first.text.contains("fetch again with offset 20"), "{}", first.text);
    assert_eq!(first.next_offset, Some(20));
    let outline = first.outline.unwrap();
    let headings: Vec<_> = outline.iter().map(|heading| (heading.level, heading.heading.as_str())).collect();
    assert_eq!(headings, [(1, "Barack Obama"), (2, "Early life"), (3, "Education")]);
    let early = &outline[1];
    let section = chunk(PAGE, early.offset, 13).unwrap();
    assert!(section.text.starts_with("## Early life"), "{}", section.text);
    assert_eq!(section.outline, None);
    let total = PAGE.chars().count();
    let last = chunk(PAGE, total - 5, 100).unwrap();
    assert_eq!((last.next_offset, last.total_chars), (None, total));
    assert!(chunk(PAGE, total, 100).is_err());
  }

  #[test]
  fn counts_characters_not_bytes() {
    let text = "ééé ünïcødé text that continues";
    let first = chunk(text, 0, 3).unwrap();
    assert!(first.text.starts_with("ééé\n\n..."), "{}", first.text);
    assert!(chunk(text, 3, 4).unwrap().text.starts_with(" ünï"));
  }

  #[test]
  fn caches_pages_briefly() {
    let page = Arc::new(FetchedPage {
      url: "https://example.com/a".into(),
      final_url: "https://example.com/a".into(),
      content_type: "text/html".into(),
      text: "cached text".into(),
      via: "html",
      transport: "headless",
    });
    remember("https://example.com/a".into(), page);
    assert_eq!(cached("https://example.com/a").unwrap().text, "cached text");
    assert!(cached("https://example.com/b").is_none());
  }
}
