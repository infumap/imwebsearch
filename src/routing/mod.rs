// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Retrieve fallback sources without query rewriting or result judging.

mod decision_log;
mod route;

use std::collections::BTreeMap;

use log::{info, warn};
use serde::Serialize;
use tokio::time::Instant;

pub use self::decision_log::record;
pub use self::route::{PoolSelection, ProviderPool};
use crate::providers::{ProviderResult, SearchRequest, SearchResult};
use crate::router::SearchLimits;

/// Routing decisions for enabled fallback sources in one search.
#[derive(Default, Serialize)]
pub struct SearchTrace {
  /// Process-local search number for log correlation; assigned by the search.
  #[serde(skip)]
  pub id: u64,
  /// Provider attempt outcomes across every pool, for the summary log line.
  #[serde(skip)]
  pub attempts: Vec<String>,
  pub probes: BTreeMap<&'static str, RouteTrace>,
}

#[derive(Serialize)]
pub struct RouteTrace {
  status: &'static str,
  titles: Vec<String>,
  probe_ms: u64,
}

/// Retrieve one fallback source, retaining its ranking and independent limits.
pub async fn probe_source(
  pool: &ProviderPool<'_>,
  request: SearchRequest<'_>,
  limits: &SearchLimits,
) -> (ProviderResult<Vec<SearchResult>>, RouteTrace) {
  let started = Instant::now();
  let mut outcome = pool.router.search(request, limits).await;
  if let Ok(results) = &mut outcome {
    results.truncate(request.max_results);
  }
  let trace = RouteTrace {
    status: match &outcome {
      Ok(results) if results.is_empty() => "empty",
      Ok(_) => "served",
      Err(_) => "failed",
    },
    titles: outcome
      .as_ref()
      .map(|results| results.iter().map(|result| result.title.clone()).collect())
      .unwrap_or_default(),
    probe_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
  };
  if let Err(error) = &outcome {
    warn!("[search {}] Fallback source {} failed: {error}", limits.search_id, pool.name);
  }
  info!(
    "[search {}] Fallback source {}: status={}, elapsed_ms={}, query={:?}",
    limits.search_id, pool.name, trace.status, trace.probe_ms, request.query
  );
  (outcome, trace)
}
