// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

mod allocation;
mod budget;
mod config;
mod duration;
mod error;
mod fetch;
mod identity;
mod protocol;
mod providers;
mod request_quota;
mod router;
mod routing;
mod search;
mod status;
mod tools;
mod web;

use std::convert::Infallible;
use std::net::SocketAddr;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use log::{error, info, warn};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use protocol::handle_http;

fn main() {
  // Namespace setup must run before creating any Tokio threads.
  if let Some(code) = fetch::run_browser_helper() {
    std::process::exit(code);
  }
  let options = match Options::parse() {
    Ok(options) => options,
    Err(error) => {
      eprintln!("Invalid command: {error}");
      std::process::exit(1);
    }
  };
  if matches!(options.command, Command::Help) {
    println!("Usage: imwebsearch [--config <path>] [--check-config | --setup-browser | --update-browser]");
    println!("Without --config, use ~/.imwebsearch/config.toml and create it from the embedded example if missing.");
    println!("Every configuration must be named config.toml and reside directly inside its data_dir.");
    return;
  }
  let settings = match config::file::initialize(options.config_path.as_deref()) {
    Ok(settings) => settings,
    Err(error) => {
      eprintln!("Invalid configuration: {error}");
      std::process::exit(1);
    }
  };
  if matches!(options.command, Command::CheckConfig) {
    println!("TOML configuration is valid.");
    return;
  }
  if let Err(error) = settings.prepare_data_dir() {
    eprintln!("Could not initialize storage: {error}");
    std::process::exit(1);
  }
  env_logger::Builder::new()
    // Per-connection lines are only useful when investigating connection
    // reuse; a logging.filter entry such as `reqwest::connect=debug` restores them.
    .filter_module("reqwest::connect", log::LevelFilter::Info)
    .parse_filters(&settings.logging.filter)
    .filter_module("selectors::matching", log::LevelFilter::Off)
    .filter_module("html5ever", log::LevelFilter::Off)
    .init();
  let runtime =
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("Could not start server runtime");
  match options.command {
    Command::SetupBrowser | Command::UpdateBrowser => {
      let result = runtime.block_on(fetch::setup_browser(matches!(options.command, Command::UpdateBrowser)));
      drop(runtime);
      match result {
        Ok(path) => println!("{}", path.display()),
        Err(error) => {
          eprintln!("Browser setup failed: {error}");
          std::process::exit(1);
        }
      }
    }
    Command::Serve => runtime.block_on(run_server(settings)),
    _ => unreachable!("non-runtime commands returned before runtime construction"),
  }
}

#[derive(Clone, Copy)]
enum Command {
  Serve,
  CheckConfig,
  SetupBrowser,
  UpdateBrowser,
  Help,
}

struct Options {
  command: Command,
  config_path: Option<std::path::PathBuf>,
}

impl Options {
  fn parse() -> Result<Self, String> {
    let mut args = std::env::args_os().skip(1);
    let mut command = None;
    let mut path = None;
    while let Some(arg) = args.next() {
      if arg == "--config" && path.is_none() {
        let selected = args.next().ok_or("--config requires a file path.")?;
        if selected.is_empty() || selected.to_string_lossy().starts_with("--") {
          return Err("--config requires a file path.".to_owned());
        }
        path = Some(std::path::PathBuf::from(selected));
        continue;
      }
      if let Some(selected) = arg.to_str().and_then(|value| value.strip_prefix("--config=")) {
        if selected.is_empty() || path.is_some() {
          return Err("Specify one nonempty --config path.".to_owned());
        }
        path = Some(selected.into());
        continue;
      }
      let selected = match arg.to_str() {
        Some("--check-config") => Command::CheckConfig,
        Some("--setup-browser") => Command::SetupBrowser,
        Some("--update-browser") => Command::UpdateBrowser,
        Some("--help" | "-h") => Command::Help,
        _ => return Err("Unknown or duplicate option; use --help for usage.".to_owned()),
      };
      if command.replace(selected).is_some() {
        return Err("Choose only one command.".to_owned());
      }
    }
    Ok(Self { command: command.unwrap_or(Command::Serve), config_path: path })
  }
}

async fn run_server(settings: &'static config::file::FileConfig) {
  if let Err(error) = fetch::initialize() {
    eprintln!("Could not initialize web fetch: {error}");
    std::process::exit(1);
  }
  if let Some(url) = &settings.fetch.gpu_tools_url {
    info!("PDF service configured: {url} (native-only extraction; capabilities not yet checked).");
  } else {
    info!("PDF service not configured; set fetch.gpu_tools_url to enable native PDF extraction.");
  }

  let config = match config::search_config() {
    Ok(config) => config,
    Err(message) => {
      eprintln!("Invalid web-search configuration: {message}");
      std::process::exit(1);
    }
  };
  if let Err(error) = search::initialize_providers() {
    eprintln!("Could not initialize search providers: {error}");
    std::process::exit(1);
  }
  let providers = config.provider_preference();
  if providers.is_empty() {
    warn!("No general search providers configured; web_search can only serve fallback source pools.");
  } else {
    info!("Configured search providers (registration order): {}", providers.join(", "));
  }
  info!("Searches try general web first, then retrieve fallback sources in parallel if no web results are available.");
  if config.wikipedia_max_requests_per_minute > 0 {
    info!(
      "Fallback searches use {}.wikipedia.org: {} attempts per rolling minute, at most three requests in flight",
      config.wikipedia_language, config.wikipedia_max_requests_per_minute
    );
  }
  if config.hacker_news_max_requests_per_minute > 0 {
    info!(
      "Fallback searches use Hacker News through Algolia: {} attempts per rolling minute",
      config.hacker_news_max_requests_per_minute
    );
  }
  if config.exa.is_some() {
    info!(
      "Exa Instant: local caps 2/rolling second, 20/rolling minute; ~2500 searches/month"
    );
  }
  if config.tavily.is_some() {
    info!("Tavily Basic: local cap 20/rolling minute; 1000 searches/month ($0.008/search)");
  }
  if config.diffbot.is_some() {
    info!(
      "Diffbot local limit: {} attempts per rolling {} seconds",
      providers::diffbot::MAX_REQUESTS_PER_MINUTE,
      providers::diffbot::RATE_WINDOW.as_secs()
    );
  }
  if config.firecrawl.is_some() {
    info!(
      "Firecrawl local limits: {} attempts per rolling {} seconds; up to 500 searches/month",
      providers::firecrawl::MAX_REQUESTS_PER_MINUTE,
      providers::firecrawl::RATE_WINDOW.as_secs()
    );
  }
  if config.brave.is_some() {
    info!(
      "Brave search local limits: 3000 attempts per rolling minute, at most 50 per rolling 1.1 seconds, concurrent requests allowed"
    );
  }
  if config.brightdata.is_some() {
    info!("Bright Data MCP local limit: 5 attempts per rolling minute, at most one request in flight");
  }
  if config.serpapi.is_some() {
    info!(
      "SerpApi local limits: 50 attempts per rolling minute, at most 50 per rolling 66 minutes, concurrent requests allowed"
    );
  }
  if config.context.is_some() {
    info!("Context.dev local limit: 5 attempts per rolling minute, at most two requests in flight");
  }
  if config.you_max_requests_per_minute > 0 {
    info!(
      "You.com free configuration: {} requests per rolling 24 hours, {} attempts per rolling minute",
      providers::you::MAX_DAILY_FREE_REQUESTS,
      config.you_max_requests_per_minute
    );
  }
  if config.exa_mcp_max_requests_per_minute > 0 {
    info!(
      "Exa MCP anonymous configuration: local estimate {} requests per rolling 24 hours, {} attempts per rolling minute, at most two per rolling 1.1 seconds",
      providers::exa_mcp::MAX_DAILY_FREE_REQUESTS,
      config.exa_mcp_max_requests_per_minute
    );
  }
  if config.tavily_mcp_max_requests_per_minute > 0 {
    info!(
      "Tavily MCP keyless configuration: {} attempts per rolling minute; upstream allowance and reset timing unknown",
      config.tavily_mcp_max_requests_per_minute
    );
  }
  if config.firecrawl_mcp_max_requests_per_minute > 0 {
    info!(
      "Firecrawl keyless MCP: {} attempts per rolling minute, soft target {} requests/day; independent of REST credits",
      config.firecrawl_mcp_max_requests_per_minute,
      providers::firecrawl_mcp::ASSUMED_REQUESTS_PER_DAY
    );
  }
  if config.parallel_mcp_max_requests_per_minute > 0 {
    info!(
      "Parallel keyless MCP: {} attempts per rolling minute, soft target {} requests/day; upstream allowance and reset timing unknown",
      config.parallel_mcp_max_requests_per_minute,
      providers::parallel_mcp::ASSUMED_REQUESTS_PER_DAY
    );
  }
  if config.parallel.is_some() {
    info!(
      "Parallel Fast search: 600 attempts per rolling minute, $5 recurring monthly credit estimate; reset boundary unknown"
    );
  }
  if config.purili_max_requests_per_minute > 0 {
    info!(
      "Purili keyless search: {} attempts per rolling minute, soft target {} requests/day; no fixed upstream quota published",
      config.purili_max_requests_per_minute,
      providers::purili::ASSUMED_REQUESTS_PER_DAY
    );
  }
  let addr = SocketAddr::new(settings.server.host, settings.server.port);

  let listener = match TcpListener::bind(addr).await {
    Ok(listener) => listener,
    Err(e) => {
      eprintln!("Could not bind {addr}: {e}");
      std::process::exit(1);
    }
  };
  let addr = listener.local_addr().expect("Bound listener has a local address");
  info!("Web search interface: http://{addr}/");
  info!("Combined MCP endpoint: http://{addr}/mcp");
  info!("Web search MCP endpoint: http://{addr}/mcp/search");
  info!("Web fetch MCP endpoint: http://{addr}/mcp/fetch");
  info!("Provider status: http://{addr}/web/status (page), http://{addr}/status (JSON)");
  if settings.browser_relay.token.is_empty() {
    info!("Browser relay disabled; set browser_relay.token to enable it.");
  } else {
    info!("Browser relay endpoint: ws://{addr}/browser-relay");
  }
  search::log_persisted_cooldowns().await;
  let usage_refresh = tokio::spawn(search::refresh_provider_usage());
  let mut connections = JoinSet::new();
  let shutdown = shutdown_signal();
  tokio::pin!(shutdown);

  loop {
    let accepted = tokio::select! {
      biased;
      _ = &mut shutdown => break,
      _ = connections.join_next(), if !connections.is_empty() => continue,
      accepted = listener.accept() => accepted,
    };
    let (stream, _) = match accepted {
      Ok(conn) => conn,
      Err(e) => {
        error!("accept failed: {e}");
        continue;
      }
    };
    let io = TokioIo::new(stream);
    connections.spawn(async move {
      if let Err(err) = http1::Builder::new()
        .serve_connection(
          io,
          service_fn(move |req| async move {
            let response = if req.uri().path() == "/browser-relay" {
              fetch::accept_relay(req)
            } else if matches!(req.uri().path().trim_end_matches('/'), "/status" | "/web/status")
              || status::handles_action(req.uri().path())
            {
              status::handle_http(&req).await
            } else if web::handles_path(req.uri().path()) {
              web::handle_http(req).await
            } else {
              handle_http(req).await
            };
            Ok::<_, Infallible>(response)
          }),
        )
        .with_upgrades()
        .await
      {
        error!("Error serving connection: {err}");
      }
    });
  }
  drop(listener);
  usage_refresh.abort();
  // Dropping HTTP handlers cancels pending fetch receivers. Browser workers
  // retain ownership/permits until their network and process cleanup finishes.
  connections.abort_all();
  while connections.join_next().await.is_some() {}
  fetch::shutdown().await;
  info!("Web-search server stopped; owned browser cleanup completed.");
}

async fn shutdown_signal() {
  #[cfg(unix)]
  {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).expect("Could not register SIGTERM handler");
    let mut interrupt = signal(SignalKind::interrupt()).expect("Could not register SIGINT handler");
    tokio::select! {
      _ = terminate.recv() => {},
      _ = interrupt.recv() => {},
    }
  }
  #[cfg(not(unix))]
  let _ = tokio::signal::ctrl_c().await;
}
