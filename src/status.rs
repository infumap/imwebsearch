// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{ALLOW, CACHE_CONTROL, CONTENT_TYPE, HOST, LOCATION, ORIGIN};
use hyper::{Method, Request, Response, StatusCode};
use jiff::Timestamp;
use log::warn;
use reqwest::Url;
use serde_json::json;

use crate::{fetch, plugin, protocol, search, web};

/// Operator actions posted from the status page.
pub fn handles_action(path: &str) -> bool {
  matches!(path, "/web/status/clear-cooldown" | "/web/status/force-next")
}

/// Operator endpoint: no provider calls, reservations, ledger writes or keys,
/// except the explicit cooldown and force-next actions.
pub async fn handle_http<B>(req: &Request<B>) -> Response<Full<Bytes>> {
  let path = req.uri().path().trim_end_matches('/');
  let html = path.starts_with("/web/status");
  let origin_allowed = match req.headers().get(ORIGIN) {
    Some(origin) => origin.to_str().is_ok_and(|origin| protocol::origin_is_allowed(Some(origin))),
    None => true,
  } || (handles_action(path) && same_origin(req));
  let cross_site = req.headers().get("sec-fetch-site").is_some_and(|value| value == "cross-site");
  if !origin_allowed || cross_site {
    return response(StatusCode::FORBIDDEN, json!({ "error": "Cross-site requests are not allowed." }), html);
  }
  if handles_action(path) {
    return action(req, path).await;
  }
  if req.method() != Method::GET {
    let mut response = response(StatusCode::METHOD_NOT_ALLOWED, json!({ "error": "Use GET." }), html);
    response.headers_mut().insert(ALLOW, hyper::header::HeaderValue::from_static("GET"));
    return response;
  }
  match tokio::try_join!(search::provider_status(), search::probe_provider_status()) {
    Ok((providers, probe_providers)) => {
      let configured = providers.iter().chain(&probe_providers).any(|provider| provider.enabled);
      let errors: Vec<&str> = if configured { vec![] } else { vec!["No search providers enabled."] };
      response(
        StatusCode::OK,
        json!({
          "generated_at": Timestamp::now().as_second(),
          "reference_request": { "num_results": search::DEFAULT_MAX_RESULTS },
          "search_available": providers.iter().chain(&probe_providers).any(|provider| provider.available),
          "configuration_errors": errors,
          "providers": providers,
          "probe_providers": probe_providers,
          "plugin": plugin::readiness(),
          "fetch": fetch::readiness(),
        }),
        html,
      )
    }
    Err(error) => {
      warn!("Provider status unavailable: {error}");
      response(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({
          "error": "Provider status unavailable; inspect server startup diagnostics.",
          "fetch": fetch::readiness()
        }),
        html,
      )
    }
  }
}

/// The status page may be opened by LAN address or hostname, since the server
/// can listen on all interfaces. Its own form posts then carry that origin
/// rather than a loopback one. Only the actions accept it: unlike the loopback
/// check, this does not stop DNS rebinding, so it is limited to these
/// low-impact operator controls.
fn same_origin<B>(req: &Request<B>) -> bool {
  let (Some(origin), Some(host)) = (req.headers().get(ORIGIN), req.headers().get(HOST)) else { return false };
  let (Ok(origin), Ok(host)) = (origin.to_str(), host.to_str()) else { return false };
  let Ok(origin) = Url::parse(origin) else { return false };
  let Ok(target) = Url::parse(&format!("{}://{host}", origin.scheme())) else { return false };
  matches!(origin.scheme(), "http" | "https")
    && origin.host_str().is_some()
    && origin.host_str() == target.host_str()
    && origin.port_or_known_default() == target.port_or_known_default()
}

/// State changes require a same-origin POST; GET-only checks above treat a
/// missing Origin as allowed, which a form POST from this page never omits.
async fn action<B>(req: &Request<B>, path: &str) -> Response<Full<Bytes>> {
  if req.method() != Method::POST {
    let mut response = response(StatusCode::METHOD_NOT_ALLOWED, json!({ "error": "Use POST." }), true);
    response.headers_mut().insert(ALLOW, hyper::header::HeaderValue::from_static("POST"));
    return response;
  }
  if req.headers().get(ORIGIN).is_none() {
    return response(StatusCode::FORBIDDEN, json!({ "error": "Status actions require an Origin header." }), true);
  }
  let mut url = Url::parse("http://localhost/").expect("Static URL is valid");
  url.set_query(req.uri().query());
  let Some(provider) = url.query_pairs().find(|(key, _)| key == "provider").map(|(_, value)| value.into_owned()) else {
    return response(StatusCode::BAD_REQUEST, json!({ "error": "Missing provider." }), true);
  };
  let result = if path == "/web/status/clear-cooldown" {
    search::clear_provider_cooldown(&provider).await
  } else {
    search::force_next_provider(&provider).await
  };
  match result {
    Ok(true) => {
      let mut response = Response::new(Full::new(Bytes::new()));
      *response.status_mut() = StatusCode::SEE_OTHER;
      response.headers_mut().insert(LOCATION, hyper::header::HeaderValue::from_static("/web/status"));
      response.headers_mut().insert(CACHE_CONTROL, hyper::header::HeaderValue::from_static("no-store"));
      response
    }
    Ok(false) => response(StatusCode::NOT_FOUND, json!({ "error": "Provider is not enabled." }), true),
    Err(error) => {
      warn!("Status action {path} failed for {provider}: {error}");
      response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": error.to_string() }), true)
    }
  }
}

fn response(status: StatusCode, body: serde_json::Value, html: bool) -> Response<Full<Bytes>> {
  if html {
    return web::status_page(status, &body);
  }
  let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
  *response.status_mut() = status;
  response.headers_mut().insert(CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/json"));
  response.headers_mut().insert(CACHE_CONTROL, hyper::header::HeaderValue::from_static("no-store"));
  response
}
