// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone)]
pub(super) struct FetchConfig {
  pub cache_dir: Option<PathBuf>,
  pub cache_max_bytes: u64,
  pub cache_ttl: Duration,
  pub pdf_refresh: bool,
  pub gpu_tools_url: Option<reqwest::Url>,
  pub max_pdf_response_bytes: usize,
  pub browser_path: Option<PathBuf>,
  pub temporary_dir: PathBuf,
  pub timeout: Duration,
  pub max_document_bytes: usize,
  pub max_rendered_bytes: usize,
  pub html_dump_dir: Option<PathBuf>,
  pub max_pdf_bytes: usize,
  pub relay_token: Option<String>,
  pub relay_timeout: Duration,
}

impl FetchConfig {
  pub fn from_file() -> Result<Self, String> {
    let config = crate::config::file::current().map_err(str::to_owned)?;
    let fetch = &config.fetch;
    Ok(Self {
      cache_dir: fetch.pdf_cache.enabled.then(|| config.pdf_cache_dir()),
      cache_max_bytes: fetch.pdf_cache.max_bytes,
      cache_ttl: Duration::from_secs(fetch.pdf_cache.ttl_secs),
      pdf_refresh: fetch.pdf_cache.refresh,
      gpu_tools_url: fetch
        .gpu_tools_url
        .as_deref()
        .map(reqwest::Url::parse)
        .transpose()
        .map_err(|_| "Invalid validated fetch.gpu_tools_url.".to_owned())?,
      max_pdf_response_bytes: fetch.max_pdf_response_bytes as usize,
      browser_path: config.browser.path.clone(),
      temporary_dir: config.temporary_dir(),
      timeout: Duration::from_secs(fetch.timeout_secs),
      max_document_bytes: fetch.max_document_bytes as usize,
      max_rendered_bytes: fetch.max_rendered_bytes as usize,
      html_dump_dir: fetch.html_dumps_enabled.then(|| config.html_dump_dir()),
      max_pdf_bytes: fetch.max_pdf_bytes as usize,
      relay_token: Some(config.browser_relay.token.clone()).filter(|token| !token.is_empty()),
      relay_timeout: Duration::from_secs(config.browser_relay.timeout_secs),
    })
  }
}
