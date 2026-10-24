// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::HashSet;

use crate::providers::SearchResult;

pub(super) struct MergedResults {
  pub results: Vec<SearchResult>,
  /// Only pools that contributed an emitted result, in first-contribution order.
  pub sources: Vec<&'static str>,
}

/// Interleave arbitrary collections while preserving each collection's ranking.
/// Provenance follows emitted results after URL deduplication and truncation.
pub(super) fn merge(sets: Vec<(&'static str, Vec<SearchResult>)>, max_results: usize) -> MergedResults {
  let mut iterators: Vec<_> = sets.into_iter().map(|(name, results)| (name, results.into_iter())).collect();
  let mut results = Vec::new();
  let mut sources = Vec::new();
  let mut seen_urls = HashSet::new();
  let mut seen_sources = HashSet::new();
  loop {
    let mut advanced = false;
    for (name, iterator) in &mut iterators {
      if results.len() >= max_results {
        break;
      }
      if let Some(result) = iterator.next() {
        advanced = true;
        if seen_urls.insert(result.url.clone()) {
          if seen_sources.insert(*name) {
            sources.push(*name);
          }
          results.push(result);
        }
      }
    }
    if !advanced || results.len() >= max_results {
      break;
    }
  }
  MergedResults { results, sources }
}
