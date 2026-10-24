// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! The browser relay: a desktop browser running the extension in `extension/`
//! dials in over a WebSocket. While it is connected it renders every page in
//! place of the owned headless browser, and its user completes any
//! verification challenge, which bot protection such as Cloudflare often shows
//! headless browsers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_tungstenite::WebSocketStream;
use async_tungstenite::tokio::TokioAdapter;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::tungstenite::handshake::derive_accept_key;
use async_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use async_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::Full;
use hyper::header::{
  CONNECTION, CONTENT_TYPE, ORIGIN, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use jiff::Timestamp;
use reqwest::Url;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::time::{Instant, timeout, timeout_at};

use crate::duration::span;
use crate::error::{InfuError, InfuResult};

use super::config::FetchConfig;
use super::http::{DownloadedDocument, Validators, is_pdf};
use super::policy::assert_public_destination;

/// Bumped when the message format changes; the extension must match.
const PROTOCOL_VERSION: u64 = 2;
const PDF_CHUNK_BYTES: usize = 64 * 1024;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// The extension pings every 20 seconds to keep its service worker alive.
const IDLE_TIMEOUT: Duration = Duration::from_secs(70);
/// Tabs the extension renders at once.
const CONCURRENT_JOBS: usize = 2;

pub(super) struct Relay {
  token: Option<String>,
  timeout: Duration,
  max_rendered_bytes: usize,
  /// Zero when native PDF extraction is not configured.
  pdf_limit: usize,
  html_dump_dir: Option<std::path::PathBuf>,
  connection: StdMutex<Option<Connection>>,
  slots: Semaphore,
  next_id: AtomicU64,
  /// When the last browser left, so a reconnection can report the gap.
  disconnected_at: StdMutex<Option<Timestamp>>,
}

#[derive(Clone)]
struct Connection {
  generation: u64,
  outgoing: mpsc::UnboundedSender<Message>,
  pending: Arc<StdMutex<HashMap<u64, PendingReply>>>,
  connected_at: i64,
  agent: String,
}

struct PendingReply {
  sender: oneshot::Sender<Reply>,
  pdf_body: Vec<u8>,
}

impl PendingReply {
  fn append_pdf(&mut self, data: &str, limit: usize) -> InfuResult<()> {
    if data.len() > PDF_CHUNK_BYTES.div_ceil(3) * 4 {
      return Err("Registered browser returned an oversized PDF chunk.".into());
    }
    let bytes = BASE64_STANDARD.decode(data).map_err(|_| "Registered browser returned an invalid PDF chunk.")?;
    if bytes.len() > PDF_CHUNK_BYTES || bytes.len() > limit.saturating_sub(self.pdf_body.len()) {
      return Err("Registered browser PDF exceeds its byte limit.".into());
    }
    self.pdf_body.extend_from_slice(&bytes);
    Ok(())
  }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Incoming {
  Hello {
    token: String,
    version: u64,
    #[serde(default)]
    agent: String,
  },
  Ping,
  Waiting {
    id: u64,
  },
  PdfChunk {
    id: u64,
    data: String,
  },
  Result {
    id: u64,
    #[serde(flatten)]
    reply: Reply,
  },
}

#[derive(Default, Deserialize)]
struct Reply {
  #[serde(default)]
  url: Option<String>,
  #[serde(default)]
  content_type: Option<String>,
  #[serde(default)]
  body: Option<String>,
  #[serde(default)]
  raw: Option<String>,
  /// The final main-frame HTTP status when it was an error.
  #[serde(default)]
  status: Option<u16>,
  #[serde(default)]
  error: Option<String>,
  /// The page is still showing a verification challenge, left open for its owner.
  #[serde(default)]
  verification_pending: bool,
  /// PDF bytes arrive in bounded chunks before this final reply.
  #[serde(default)]
  pdf: bool,
  #[serde(default)]
  pdf_size: Option<usize>,
  #[serde(skip)]
  pdf_body: Vec<u8>,
  #[serde(default)]
  etag: Option<String>,
  #[serde(default)]
  last_modified: Option<String>,
}

impl Relay {
  pub fn new(config: &FetchConfig) -> Self {
    Self {
      token: config.relay_token.clone(),
      timeout: config.relay_timeout,
      max_rendered_bytes: config.max_rendered_bytes,
      pdf_limit: if config.gpu_tools_url.is_some() { config.max_pdf_bytes } else { 0 },
      html_dump_dir: config.html_dump_dir.clone(),
      connection: StdMutex::new(None),
      slots: Semaphore::new(CONCURRENT_JOBS),
      next_id: AtomicU64::new(1),
      disconnected_at: StdMutex::new(None),
    }
  }

  pub fn enabled(&self) -> bool {
    self.token.is_some()
  }

  /// The budget of a fetch rendered in the relay browser.
  pub fn timeout(&self) -> Duration {
    self.timeout
  }

  pub fn connected(&self) -> bool {
    self.connection.lock().expect("Relay lock poisoned").is_some()
  }

  pub fn status(&self) -> serde_json::Value {
    let connection = self.connection.lock().expect("Relay lock poisoned").clone();
    json!({
      "enabled": self.enabled(),
      "connected": connection.is_some(),
      "connected_since": connection.as_ref().map(|connection| connection.connected_at),
      "agent": connection.as_ref().map(|connection| connection.agent.clone()),
      "busy": self.slots.available_permits() == 0,
      "timeout_secs": self.timeout.as_secs(),
    })
  }

  pub fn shutdown(&self) {
    self.slots.close();
    if let Some(connection) = self.connection.lock().expect("Relay lock poisoned").take() {
      let _ = connection.outgoing.send(close(CloseCode::Away, "Server is shutting down."));
    }
  }

  /// Render a page in the registered browser.
  pub async fn render(&self, requested: &Url, deadline: Instant) -> InfuResult<DownloadedDocument> {
    let _permit = timeout_at(deadline, self.slots.acquire())
      .await
      .map_err(|_| "Browser relay deadline expired while queued.")?
      .map_err(|_| "Browser relay is shutting down.")?;
    let connection =
      self.connection.lock().expect("Relay lock poisoned").clone().ok_or("No browser is connected to the relay.")?;
    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = oneshot::channel();
    connection.pending.lock().expect("Relay lock poisoned").insert(id, PendingReply { sender, pdf_body: Vec::new() });
    let job = PendingJob { id, connection: &connection, finished: false };
    // Leave the extension a moment to send its answer before our deadline.
    let budget = deadline.saturating_duration_since(Instant::now()).saturating_sub(Duration::from_secs(1));
    let message = json!({
      "type": "fetch",
      "id": id,
      "url": requested.as_str(),
      "timeout_ms": budget.as_millis() as u64,
      "limit": self.max_rendered_bytes,
      "pdf_limit": self.pdf_limit,
      "dump": self.html_dump_dir.is_some(),
    });
    connection.outgoing.send(Message::text(message.to_string())).map_err(|_| "Browser relay disconnected.")?;
    let reply = timeout_at(deadline, receiver)
      .await
      .map_err(|_| "The registered browser did not return the page before the deadline.")?
      .map_err(|_| "The registered browser disconnected during the fetch.")?;
    job.finish();
    if reply.verification_pending {
      return Err(
        "The site is showing a human verification challenge in the registered browser. Its tab has been left open \
         for the user; once they complete it, retry this fetch."
          .into(),
      );
    }
    if let Some(status) = reply.status {
      return Err(InfuError::from_http_status(status));
    }
    if let Some(error) = reply.error {
      return Err(format!("Registered browser: {error}").into());
    }
    let final_url = Url::parse(reply.url.as_deref().ok_or("Registered browser returned no final URL.")?)
      .map_err(|_| "Registered browser returned an invalid final URL.")?;
    assert_public_destination(&final_url).await?;
    let (body, content_type) = if reply.pdf {
      if self.pdf_limit == 0 {
        return Err(super::pdf::UNSUPPORTED.into());
      }
      if reply.pdf_size != Some(reply.pdf_body.len()) || !is_pdf(&reply.pdf_body) {
        return Err("Registered browser returned an incomplete or invalid PDF.".into());
      }
      (reply.pdf_body, "application/pdf".to_owned())
    } else {
      if !reply.pdf_body.is_empty() {
        return Err("Registered browser returned PDF chunks without a PDF result.".into());
      }
      let body = reply.body.ok_or("Registered browser returned no document.")?;
      if body.len() > self.max_rendered_bytes {
        return Err("Rendered document exceeds its byte or complexity limit.".into());
      }
      if let (Some(directory), Some(raw)) = (self.html_dump_dir.clone(), reply.raw) {
        super::browser::dump_html(directory, raw);
      }
      (body.into_bytes(), reply.content_type.unwrap_or_else(|| "text/html".to_owned()))
    };
    Ok(DownloadedDocument {
      final_url,
      content_type,
      body,
      validators: Validators {
        etag: reply.etag.filter(|value| value.len() <= 1024),
        last_modified: reply.last_modified.filter(|value| value.len() <= 1024),
      },
      transport: "relay",
    })
  }

  /// Answer a WebSocket upgrade on `/browser-relay`.
  pub fn accept<B>(self: &Arc<Self>, mut req: Request<B>) -> Response<Full<Bytes>> {
    let Some(token) = self.token.clone() else {
      return plain(StatusCode::NOT_FOUND, "The browser relay is disabled; set browser_relay.token.");
    };
    // Browsers attach the extension's own origin. Web pages may open
    // WebSockets to any host, so refuse them even before the token check.
    let origin = req.headers().get(ORIGIN).and_then(|value| value.to_str().ok()).unwrap_or("");
    if !origin.is_empty() && !origin.starts_with("chrome-extension://") && !origin.starts_with("moz-extension://") {
      return plain(StatusCode::FORBIDDEN, "Only the relay browser extension may connect.");
    }
    let header = |name| req.headers().get(name).and_then(|value: &hyper::header::HeaderValue| value.to_str().ok());
    let upgrade = header(UPGRADE).is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let connection =
      header(CONNECTION).is_some_and(|value| value.split(',').any(|part| part.trim().eq_ignore_ascii_case("upgrade")));
    let version = header(SEC_WEBSOCKET_VERSION) == Some("13");
    let key = header(SEC_WEBSOCKET_KEY).map(str::to_owned);
    let (true, true, true, Some(key)) = (req.method() == Method::GET, upgrade && connection, version, key) else {
      return plain(StatusCode::BAD_REQUEST, "Expected a WebSocket upgrade.");
    };
    let upgrading = hyper::upgrade::on(&mut req);
    let relay = self.clone();
    tokio::spawn(async move {
      match upgrading.await {
        Ok(upgraded) => relay.serve(TokioAdapter::new(TokioIo::new(upgraded)), token).await,
        Err(error) => log::warn!("Browser relay upgrade failed: {error}"),
      }
    });
    Response::builder()
      .status(StatusCode::SWITCHING_PROTOCOLS)
      .header(UPGRADE, "websocket")
      .header(CONNECTION, "Upgrade")
      .header(SEC_WEBSOCKET_ACCEPT, derive_accept_key(key.as_bytes()))
      .body(Full::new(Bytes::new()))
      .expect("Static response headers are valid")
  }

  async fn serve<S>(self: Arc<Self>, stream: S, token: String)
  where
    S: futures_util::AsyncRead + futures_util::AsyncWrite + Unpin + Send + 'static,
  {
    // Allow a whole rendered document plus its optional raw dump and JSON escaping.
    let limit = self.max_rendered_bytes.saturating_mul(if self.html_dump_dir.is_some() { 4 } else { 2 }) + (1 << 20);
    let config = WebSocketConfig::default().max_message_size(Some(limit)).max_frame_size(Some(limit));
    let socket = WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
    let (mut sink, mut stream) = socket.split();
    let agent = match timeout(HELLO_TIMEOUT, stream.next()).await {
      Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<Incoming>(&text) {
        Ok(Incoming::Hello { token: offered, version, agent }) => {
          if !constant_time_eq(offered.as_bytes(), token.as_bytes()) {
            log::warn!("Browser relay connection refused: wrong token.");
            let _ = sink.send(close(CloseCode::Library(4001), "Wrong relay token.")).await;
            return;
          }
          if version != PROTOCOL_VERSION {
            log::warn!("Browser relay connection refused: extension protocol {version}, server {PROTOCOL_VERSION}.");
            let _ = sink.send(close(CloseCode::Library(4002), "Extension and server versions differ.")).await;
            return;
          }
          agent.chars().take(256).collect::<String>()
        }
        _ => return,
      },
      _ => return,
    };
    let (outgoing, mut queue) = mpsc::unbounded_channel();
    let generation = self.next_id.fetch_add(1, Ordering::Relaxed);
    let connection = Connection {
      generation,
      outgoing: outgoing.clone(),
      pending: Arc::default(),
      connected_at: Timestamp::now().as_second(),
      agent: agent.clone(),
    };
    let pending = connection.pending.clone();
    // A newer registration replaces the old one, such as after an extension reload.
    if let Some(old) = self.connection.lock().expect("Relay lock poisoned").replace(connection) {
      let _ = old.outgoing.send(close(CloseCode::Policy, "Another browser registered with the relay."));
    }
    let disconnected_at = self.disconnected_at.lock().expect("Relay lock poisoned").take();
    match disconnected_at {
      Some(at) => log::info!("Browser relay connected after {} disconnected: {agent}", since(at)),
      None => log::info!("Browser relay connected: {agent}"),
    }
    let connected_at = Timestamp::now();
    let _ = outgoing.send(Message::text(json!({ "type": "ready" }).to_string()));
    let writer = tokio::spawn(async move {
      while let Some(message) = queue.recv().await {
        let closing = matches!(message, Message::Close(_));
        if sink.send(message).await.is_err() || closing {
          break;
        }
      }
      let _ = sink.close(None).await;
    });
    let mut last_message = Timestamp::now();
    loop {
      let message = match timeout(IDLE_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(message))) => {
          last_message = Timestamp::now();
          message
        }
        Ok(Some(Err(error))) => {
          log::warn!("Browser relay connection failed: {error}");
          break;
        }
        Ok(None) => break,
        Err(_) => {
          // The idle timer is monotonic and stops while this host sleeps; a
          // longer wall-clock gap means the server, not the browser, was paused.
          let silent = since(last_message);
          let cause = if Timestamp::now().duration_since(last_message).as_secs() > IDLE_TIMEOUT.as_secs() as i64 + 5 {
            "this server was probably suspended"
          } else {
            "the browser stopped sending its 20-second pings"
          };
          log::info!("Browser relay idle: no message for {silent}; {cause}. Closing it.");
          break;
        }
      };
      let text = match message {
        Message::Text(text) => text,
        Message::Close(_) => break,
        _ => continue,
      };
      match serde_json::from_str::<Incoming>(&text) {
        Ok(Incoming::Ping) => {
          let _ = outgoing.send(Message::text(json!({ "type": "pong" }).to_string()));
        }
        Ok(Incoming::Waiting { id }) => {
          log::info!("Browser relay job {id} is waiting for the user to complete a verification challenge.");
        }
        Ok(Incoming::PdfChunk { id, data }) => {
          let mut jobs = pending.lock().expect("Relay lock poisoned");
          if let Some(job) = jobs.get_mut(&id)
            && let Err(error) = job.append_pdf(&data, self.pdf_limit)
          {
            let job = jobs.remove(&id).expect("Pending relay job disappeared");
            let _ = job.sender.send(Reply { error: Some(error.to_string()), ..Reply::default() });
            let _ = outgoing.send(Message::text(json!({ "type": "cancel", "id": id }).to_string()));
          }
        }
        Ok(Incoming::Result { id, mut reply }) => {
          if let Some(job) = pending.lock().expect("Relay lock poisoned").remove(&id) {
            reply.pdf_body = job.pdf_body;
            let _ = job.sender.send(reply);
          }
        }
        Ok(Incoming::Hello { .. }) => {}
        Err(error) => log::warn!("Invalid browser relay message: {error}"),
      }
    }
    {
      let mut current = self.connection.lock().expect("Relay lock poisoned");
      if current.as_ref().is_some_and(|connection| connection.generation == generation) {
        *current = None;
      }
    }
    // Dropping the senders fails every job still waiting on this browser.
    pending.lock().expect("Relay lock poisoned").clear();
    drop(outgoing);
    let _ = writer.await;
    *self.disconnected_at.lock().expect("Relay lock poisoned") = Some(Timestamp::now());
    log::info!("Browser relay disconnected after {} connected: {agent}", since(connected_at));
  }
}

/// Wall-clock time elapsed since `at`, for relay log lines.
fn since(at: Timestamp) -> String {
  span(Duration::from_secs(u64::try_from(Timestamp::now().duration_since(at).as_secs()).unwrap_or(0)))
}

/// Removes an unanswered job and tells the browser to stop it, including when
/// the caller's future is dropped.
struct PendingJob<'a> {
  id: u64,
  connection: &'a Connection,
  finished: bool,
}

impl PendingJob<'_> {
  fn finish(mut self) {
    self.finished = true;
  }
}

impl Drop for PendingJob<'_> {
  fn drop(&mut self) {
    if self.finished {
      return;
    }
    self.connection.pending.lock().expect("Relay lock poisoned").remove(&self.id);
    let _ = self.connection.outgoing.send(Message::text(json!({ "type": "cancel", "id": self.id }).to_string()));
  }
}

fn close(code: CloseCode, reason: &'static str) -> Message {
  Message::Close(Some(CloseFrame { code, reason: reason.into() }))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
  left.len() == right.len() && left.iter().zip(right).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn plain(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
  Response::builder()
    .status(status)
    .header(CONTENT_TYPE, "text/plain; charset=utf-8")
    .body(Full::new(Bytes::from_static(body.as_bytes())))
    .expect("Static response headers are valid")
}
