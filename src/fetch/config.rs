// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::path::PathBuf;
use std::time::Duration;

pub(super) const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_RENDERED_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_PDF_BYTES: usize = 128 * 1024 * 1024;
pub(super) const MAX_PDF_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub(super) const PDF_CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct FetchConfig {
  pub cache_dir: Option<PathBuf>,
  pub cache_ttl: Duration,
  pub pdf_refresh: bool,
  pub gpu_tools_url: Option<reqwest::Url>,
  pub browser_path: Option<PathBuf>,
  pub temporary_dir: PathBuf,
  pub timeout: Duration,
  pub html_dump_dir: Option<PathBuf>,
  pub relay_token: Option<String>,
  pub relay_timeout: Duration,
}

impl FetchConfig {
  pub fn from_file() -> Result<Self, String> {
    let config = crate::config::file::current().map_err(str::to_owned)?;
    let fetch = &config.fetch;
    Ok(Self {
      cache_dir: fetch.pdf_cache.enabled.then(|| config.pdf_cache_dir()),
      cache_ttl: Duration::from_secs(fetch.pdf_cache.ttl_secs),
      pdf_refresh: fetch.pdf_cache.refresh,
      gpu_tools_url: fetch
        .gpu_tools_url
        .as_deref()
        .map(reqwest::Url::parse)
        .transpose()
        .map_err(|_| "Invalid validated fetch.gpu_tools_url.".to_owned())?,
      browser_path: config.browser.path.clone(),
      temporary_dir: config.temporary_dir(),
      timeout: Duration::from_secs(fetch.timeout_secs),
      html_dump_dir: fetch.html_dumps_enabled.then(|| config.html_dump_dir()),
      relay_token: Some(config.browser_relay.token.clone()).filter(|token| !token.is_empty()),
      relay_timeout: Duration::from_secs(config.browser_relay.timeout_secs),
    })
  }
}
