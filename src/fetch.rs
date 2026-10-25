// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

mod bootstrap;
mod browser;
mod cache;
mod config;
mod http;
mod isolation;
mod markdown;
pub mod pages;
mod pdf;
mod policy;
mod proxy;
mod relay;
mod supervision;
mod wikipedia;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use log::{error, warn};
use reqwest::Url;
use serde::Serialize;
use tokio::time::{Instant, timeout_at};

use crate::error::InfuResult;
use browser::BrowserRenderer;
use config::FetchConfig;
use http::{DocumentKind, DownloadedDocument, HttpDownloader};
use policy::{assert_public_destination, hosts_differ, normalize_url_scheme};
pub(crate) use relay::RenderError;

pub const DEFAULT_MAX_CHARS: usize = 8_000;
pub const MAX_CHARS_CAP: usize = 100_000;

pub fn run_browser_helper() -> Option<i32> {
  isolation::run_browser_helper()
}

pub async fn setup_browser(update: bool) -> InfuResult<std::path::PathBuf> {
  bootstrap::command(update).await
}

static FETCH_STATE: OnceLock<Result<FetchState, String>> = OnceLock::new();
static NEXT_FETCH_ID: AtomicU64 = AtomicU64::new(1);

struct FetchState {
  config: FetchConfig,
  http: HttpDownloader,
  browser: BrowserRenderer,
  relay: Arc<relay::Relay>,
  pdf: Arc<pdf::PdfService>,
}

impl FetchState {
  fn from_file() -> Result<Self, String> {
    let config = FetchConfig::from_file()?;
    Ok(Self {
      http: HttpDownloader::new(config.clone()),
      browser: BrowserRenderer::new(config.clone()),
      relay: Arc::new(relay::Relay::new(&config)),
      pdf: Arc::new(pdf::PdfService::new(config.clone())),
      config,
    })
  }

  async fn fetch(&self, requested: Url) -> InfuResult<FetchedPage> {
    // A connected relay browser renders every page, and its user may have to
    // complete a challenge first, so it has its own budget.
    let budget =
      if self.relay.connected() { self.config.timeout.max(self.relay.timeout()) } else { self.config.timeout };
    let deadline = Instant::now() + budget;
    timeout_at(deadline, async {
      assert_public_destination(&requested).await?;
      let _source_lock =
        self.pdf.cache.lock("url", &cache::digest(requested.as_str().as_bytes()), deadline).await.unwrap_or(None);
      let cached = self.pdf.cache.lookup(requested.as_str()).await.unwrap_or(None);
      let mut revalidated = None;
      if let Some(cached) = cached {
        let final_url = Url::parse(&cached.source.final_url).map_err(|_| "Invalid cached PDF destination.")?;
        assert_public_destination(&final_url).await?;
        let unchanged = if cached.fresh {
          true
        } else {
          match self.http.download_conditional(&final_url, &cached.source.validators).await {
            Ok(http::DownloadOutcome::Unmodified) => true,
            Ok(http::DownloadOutcome::Document(document)) => {
              revalidated = Some(document);
              false
            }
            Err(_) => false, // A fresh browser context may be required by the site.
          }
        };
        if unchanged {
          if !cached.fresh {
            let _ = self.pdf.cache.publish(&cached.source.content_hash, cached.bytes, &cached.text, deadline).await;
            let _ = self.pdf.cache.remember(cached.source.clone(), deadline).await;
          }
          let document = DownloadedDocument {
            final_url,
            content_type: "application/pdf".into(),
            body: Vec::new(),
            validators: cached.source.validators,
            transport: "pdf cache",
          };
          let requested = requested.clone();
          return tokio::task::spawn_blocking(move || Self::finish(&requested, document, Some(cached.text.markdown)))
            .await
            .map_err(|error| format!("Cached PDF extraction worker failed: {error}"))?;
        }
      }
      // Suffixes and a previously validated PDF are routing hints only.
      let used_direct = revalidated.is_some() || http::is_direct_text_url(&requested);
      let mut document = if let Some(document) = revalidated {
        document
      } else if http::is_direct_text_url(&requested) {
        self.http.download(&requested).await?
      } else {
        self.render(&requested, deadline).await?
      };
      if document.kind() == DocumentKind::Html && used_direct {
        document = self.render(&requested, deadline).await?;
      }
      let native_text = if document.kind() == DocumentKind::Pdf {
        let source = cache::Source {
          url: requested.to_string(),
          final_url: document.final_url.to_string(),
          validators: document.validators.clone(),
          content_hash: String::new(),
          checked_at: 0,
        };
        Some(self.pdf.extract(bytes::Bytes::from(std::mem::take(&mut document.body)), source, deadline).await?.markdown)
      } else {
        None
      };
      // Parsing a bounded but large DOM must not block the async search server
      // or prevent its overall fetch deadline from being observed.
      tokio::task::spawn_blocking(move || Self::finish(&requested, document, native_text))
        .await
        .map_err(|error| format!("Document extraction worker failed: {error}"))?
    })
    .await
    .map_err(|_| "Web fetch exceeded its overall deadline.")?
  }

  /// Render in the relay browser when one is connected, otherwise in the owned
  /// headless browser. PDF responses retain the selected browser's session and
  /// their bytes go to the native PDF extraction service.
  async fn render(&self, requested: &Url, deadline: Instant) -> InfuResult<DownloadedDocument> {
    if !self.relay.connected() {
      return self.browser.render(requested, deadline).await;
    }
    self.relay.render(requested, deadline, false).await.map_err(Into::into)
  }

  /// The whole extracted text; `pages::chunk` slices it for the response.
  fn finish(requested: &Url, document: DownloadedDocument, native_text: Option<String>) -> InfuResult<FetchedPage> {
    let (text, via) = if let Some(text) = native_text {
      (text, "pdf")
    } else {
      match document.kind() {
        DocumentKind::Pdf => return Err("Missing verified native PDF text.".into()),
        DocumentKind::Html => (markdown::html_to_markdown(&String::from_utf8_lossy(&document.body))?, "html"),
        DocumentKind::Text => (String::from_utf8_lossy(&document.body).trim().to_string(), "text"),
        DocumentKind::Unsupported => {
          return Err("Unsupported web document type; expected HTML, text, JSON or PDF.".into());
        }
      }
    };
    Ok(FetchedPage {
      url: requested.to_string(),
      final_url: document.final_url.to_string(),
      content_type: document.content_type,
      text,
      via,
      transport: document.transport,
    })
  }
}

/// Prepare fetch configuration once, independently of search providers. No browser is launched here.
pub fn initialize() -> InfuResult<()> {
  state().map(|_| ())
}

/// Read local configuration/last-observed state only: no network or browser probes.
pub fn readiness() -> serde_json::Value {
  match FETCH_STATE.get() {
    Some(Ok(state)) => serde_json::json!({
      "browser": state.browser.status(),
      "pdf": { "service": state.pdf.status(), "policy": "native_only", "policy_version": pdf::POLICY_VERSION,
        "ocr_allowed": false, "cache_enabled": state.config.cache_dir.is_some(),
        "refresh_requested": state.config.pdf_refresh, "cache_ttl_secs": state.config.cache_ttl.as_secs(),
        "cache_artifact_budget_bytes": config::PDF_CACHE_MAX_BYTES },
      "timeout_secs": state.config.timeout.as_secs(),
      "max_document_bytes": config::MAX_DOCUMENT_BYTES,
      "max_rendered_bytes": config::MAX_RENDERED_BYTES,
      "max_pdf_bytes": config::MAX_PDF_BYTES,
      "max_pdf_response_bytes": config::MAX_PDF_RESPONSE_BYTES,
      "html_dumps_enabled": state.config.html_dump_dir.is_some(),
      "browser_relay": state.relay.status(),
    }),
    _ => serde_json::json!({ "available": false, "reason": "not_initialized" }),
  }
}

/// Register a relay browser: a WebSocket upgrade on `/browser-relay`.
pub fn accept_relay<B>(req: hyper::Request<B>) -> hyper::Response<http_body_util::Full<bytes::Bytes>> {
  match state() {
    Ok(state) => state.relay.accept(req),
    Err(_) => hyper::Response::builder()
      .status(hyper::StatusCode::SERVICE_UNAVAILABLE)
      .body(http_body_util::Full::new(bytes::Bytes::from_static(b"Web fetch is unavailable.")))
      .expect("Static response is valid"),
  }
}

/// Whether a relay browser may connect, for search plugins to know up front.
pub fn relay_configured() -> bool {
  FETCH_STATE.get().is_some_and(|state| state.as_ref().is_ok_and(|state| state.relay.enabled()))
}

/// Render an http or https page in the relay browser for a search plugin, never
/// in the owned headless browser. Search engine pages are allowed. Returns the final URL and the captured HTML:
/// the page's DOM with links made absolute and consent banners removed.
pub(crate) async fn render_in_relay(requested: &Url, deadline: Instant) -> Result<(Url, String), RenderError> {
  let Some(Ok(state)) = FETCH_STATE.get() else { return Err(RenderError::Unavailable) };
  if !state.relay.connected() {
    return Err(RenderError::Unavailable);
  }
  policy::assert_public_address(requested).await?;
  let document = state.relay.render(requested, deadline, true).await?;
  if document.kind() != DocumentKind::Html {
    return Err("The rendered page is not an HTML document.".into());
  }
  let html = String::from_utf8(document.body).map_err(|_| "The rendered page is not valid UTF-8.")?;
  Ok((document.final_url, html))
}

pub async fn shutdown() {
  if let Some(Ok(state)) = FETCH_STATE.get() {
    state.relay.shutdown();
    state.browser.shutdown().await;
    state.pdf.shutdown().await;
  }
}

fn state() -> InfuResult<&'static FetchState> {
  FETCH_STATE.get_or_init(FetchState::from_file).as_ref().map_err(|error| error.clone().into())
}

/// A page's whole extracted text, as cached; responses are chunks of it.
#[derive(Clone, Debug, Serialize)]
pub struct FetchedPage {
  pub url: String,
  pub final_url: String,
  pub content_type: String,
  pub text: String,
  /// How the text was read, for the server log: `html`, `text` or `pdf`.
  /// Not part of the model-facing response.
  pub via: &'static str,
  /// What retrieved it, for the server log: `http`, `headless`, `relay`,
  /// `pdf cache` or `wikipedia api`.
  pub transport: &'static str,
}

/// Explicit model-facing fields. `truncated` means more text follows this
/// chunk, at `nextOffset`; `outline` comes with the first chunk of such a page.
#[derive(Serialize)]
struct CompactFetchedPage<'a> {
  url: &'a str,
  #[serde(rename = "finalUrl")]
  final_url: &'a str,
  #[serde(rename = "hostChanged", skip_serializing_if = "is_false")]
  host_changed: bool,
  truncated: bool,
  #[serde(skip_serializing_if = "is_zero")]
  offset: usize,
  #[serde(rename = "totalChars")]
  total_chars: usize,
  #[serde(rename = "nextOffset", skip_serializing_if = "Option::is_none")]
  next_offset: Option<usize>,
  #[serde(skip_serializing_if = "Option::is_none")]
  outline: Option<&'a [pages::Heading]>,
  text: &'a str,
}

/// The response for one chunk of a page.
pub fn page_chunk_json(page: &FetchedPage, chunk: &pages::Chunk) -> InfuResult<String> {
  serde_json::to_string(&CompactFetchedPage {
    url: &page.url,
    final_url: &page.final_url,
    host_changed: hosts_differ(&page.url, &page.final_url),
    truncated: chunk.next_offset.is_some(),
    offset: chunk.offset,
    total_chars: chunk.total_chars,
    next_offset: chunk.next_offset,
    outline: chunk.outline.as_deref(),
    text: &chunk.text,
  })
  .map_err(|error| format!("Could not serialize fetched page: {error}").into())
}

/// Process-local number tagging one fetch's log lines, like `[search N]`.
pub fn next_fetch_id() -> u64 {
  NEXT_FETCH_ID.fetch_add(1, Ordering::Relaxed)
}

/// Fetch a page's whole text, or reuse it if fetched in the last ten minutes,
/// so reading a long page in chunks renders it once. Returns whether the
/// page came from the cache.
pub async fn fetch_url(url: &str, fetch_id: u64) -> InfuResult<(Arc<FetchedPage>, bool)> {
  let url = normalize_url_scheme(url.trim())?;
  if let Some(page) = pages::cached(url.as_str()) {
    return Ok((page, true));
  }
  let page = Arc::new(fetch_uncached(&url, fetch_id).await?);
  pages::remember(url.to_string(), page.clone());
  Ok((page, false))
}

async fn fetch_uncached(url: &Url, fetch_id: u64) -> InfuResult<FetchedPage> {
  if let Some(article) = wikipedia::article(url) {
    match wikipedia::fetch(&article, url).await {
      Ok(Some(page)) => return Ok(page),
      Ok(None) => {}
      Err(wikipedia::ExtractError::Rejected(error)) => error!(
        "[fetch {fetch_id}] Wikipedia API rejected this server ({error}); fetching the page instead. Every \
         Wikipedia fetch will take this slower path until the request is fixed."
      ),
      Err(wikipedia::ExtractError::Failed(error)) => {
        warn!("[fetch {fetch_id}] Wikipedia extract failed; fetching the page: {error}")
      }
    }
  }
  state()?.fetch(url.clone()).await
}

fn is_zero(value: &usize) -> bool {
  *value == 0
}

fn is_false(value: &bool) -> bool {
  !*value
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn blocks_loopback_fetch() {
    let err = fetch_url("http://127.0.0.1/", 0).await.unwrap_err();
    assert!(err.to_string().contains("Blocked"), "{}", err);
  }

  #[tokio::test]
  async fn blocks_localhost_by_name() {
    let err = fetch_url("http://localhost/", 0).await.unwrap_err();
    assert!(err.to_string().contains("Blocked"), "{}", err);
  }

  #[tokio::test]
  async fn blocks_link_local_metadata_host() {
    let err = fetch_url("http://169.254.169.254/", 0).await.unwrap_err();
    assert!(err.to_string().contains("Blocked"), "{}", err);
  }
}
