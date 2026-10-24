// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::net::{IpAddr, SocketAddr};

use reqwest::Url;
use tokio::net::lookup_host;

use crate::error::{InfuError, InfuResult};

const METADATA_HOSTS: &[&str] =
  &["localhost", "ip6-localhost", "metadata", "metadata.google.internal", "metadata.tencentyun.com"];

const SEARCH_ENGINE_HOSTS: &[&str] = &[
  "google.com",
  "bing.com",
  "cn.bing.com",
  "duckduckgo.com",
  "html.duckduckgo.com",
  "lite.duckduckgo.com",
  "yahoo.com",
  "search.yahoo.com",
  "search.brave.com",
  "startpage.com",
  "ecosia.org",
  "baidu.com",
  "yandex.com",
  "yandex.ru",
  "search.aol.com",
  "ask.com",
  "search.ask.com",
];

/// An error for a failed status that names a bot challenge when the response
/// marks one. Cloudflare sets `cf-mitigated: challenge` on every page of a
/// challenged site, so saying so stops a model retrying other pages of it.
pub(super) fn http_status_error<'a>(
  status: u16,
  url: &Url,
  mut headers: impl Iterator<Item = (&'a str, &'a str)>,
) -> InfuError {
  let challenged = headers
    .any(|(name, value)| name.eq_ignore_ascii_case("cf-mitigated") && value.to_ascii_lowercase().contains("challenge"));
  if !challenged {
    return InfuError::from_http_status(status);
  }
  let host = url.host_str().unwrap_or("this site");
  InfuError::from_http_status_with_message(
    status,
    format!(
      "Failed to fetch URL: HTTP {status}, blocked by the site's bot protection (Cloudflare challenge). Other pages \
       on {host} will be blocked too; use search result snippets or another source."
    ),
  )
}

pub(super) fn normalize_url_scheme(url: &str) -> InfuResult<Url> {
  if url.is_empty() {
    return Err("fetch URL cannot be empty".into());
  }
  let with_scheme = if url.starts_with("//") {
    format!("https:{url}")
  } else if url.contains("://") {
    url.to_string()
  } else {
    format!("https://{url}")
  };
  Url::parse(&with_scheme).map_err(|e| format!("invalid URL: {}", e).into())
}

pub(super) async fn assert_public_destination(url: &Url) -> InfuResult<()> {
  document_addresses(url).await.map(|_| ())
}

pub(super) async fn document_addresses(url: &Url) -> InfuResult<Vec<SocketAddr>> {
  if let Some(host) = url.host_str()
    && is_search_engine_host(host)
  {
    return Err(
      format!("Blocked: web_fetch does not fetch search engine URLs ('{host}'). Use web_search for searches.").into(),
    );
  }
  public_addresses(url).await
}

/// The same policy for a page's own subresources and frames. The caller
/// refuses just that request when it fails: a reference to a host that does
/// not resolve (ssl.bbc.co.uk, retired but still used by archived BBC pages)
/// must not fail the whole page.
pub(super) async fn check_subresource(url: &Url, document: bool) -> InfuResult<()> {
  if document { document_addresses(url).await } else { public_addresses(url).await }.map(|_| ())
}

/// Resolve once, validate every address, and connect only to these addresses.
/// A lookup is not retried: the repeated "Failed to resolve host" errors that
/// once suggested transient failures were a retired subresource host.
pub(super) async fn public_addresses(url: &Url) -> InfuResult<Vec<SocketAddr>> {
  match url.scheme() {
    "http" | "https" => {}
    other => return Err(format!("Blocked: unsupported URL scheme '{other}'.").into()),
  }
  if !url.username().is_empty() || url.password().is_some() {
    return Err("Blocked: URL must not include credentials.".into());
  }
  let host = url.host_str().ok_or("Blocked: URL has no host.")?;
  if is_blocked_host(host) {
    return Err(format!("Blocked: refusing to fetch host '{host}'.").into());
  }
  let port = url.port_or_known_default().filter(|port| *port != 0).ok_or("Blocked: invalid destination port.")?;
  let host = host.trim_start_matches('[').trim_end_matches(']');
  if let Ok(ip) = host.parse::<IpAddr>() {
    if !is_global_ip(ip) {
      return Err("Blocked: refusing to fetch a non-public address.".into());
    }
    return Ok(vec![SocketAddr::new(ip, port)]);
  }
  let addrs = lookup_host((host, port)).await.map_err(|error| format!("Failed to resolve host '{host}': {error}"))?;
  let mut addresses = Vec::new();
  for addr in addrs {
    if !is_global_ip(addr.ip()) {
      return Err(format!("Blocked: refusing to fetch non-public address {}.", addr.ip()).into());
    }
    if !addresses.contains(&addr) {
      addresses.push(addr);
    }
  }
  if addresses.is_empty() {
    return Err(format!("Failed to resolve host '{host}': no addresses.").into());
  }
  Ok(addresses)
}

fn is_search_engine_host(host: &str) -> bool {
  let host = normalize_host(host);
  let host = host.strip_prefix("www.").unwrap_or(&host);
  if SEARCH_ENGINE_HOSTS.contains(&host) || host.ends_with(".search.yahoo.com") {
    return true;
  }
  // Include Google's country search domains without blocking product hosts
  // such as docs.google.com or unrelated domains containing "google".
  host.strip_prefix("google.").is_some_and(|suffix| {
    let country = suffix.strip_prefix("co.").or_else(|| suffix.strip_prefix("com.")).unwrap_or(suffix);
    country.len() == 2 && country.bytes().all(|byte| byte.is_ascii_lowercase())
  })
}

fn is_blocked_host(host: &str) -> bool {
  let host = host.trim_end_matches('.').to_ascii_lowercase();
  METADATA_HOSTS.iter().any(|blocked| *blocked == host)
    || host.ends_with(".localhost")
    || host.ends_with(".local")
    || host.ends_with(".internal")
    || host.ends_with(".home.arpa")
    || host.ends_with(".invalid")
    || host.ends_with(".test")
    || host.starts_with("169.254.")
    || host == "::ffff:169.254.169.254"
}

fn is_global_ip(ip: IpAddr) -> bool {
  match ip {
    IpAddr::V4(v4) => {
      let o = v4.octets();
      !(v4.is_unspecified()
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_multicast()
        || v4.is_broadcast()
        || o[0] == 0
        || (o[0] == 100 && o[1] & 0b1100_0000 == 64)
        // Azure WireServer uses a public-looking platform virtual IP.
        || o == [168, 63, 129, 16]
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 192 && o[1] == 0 && o[2] == 2)
        || (o[0] == 192 && o[1] == 88 && o[2] == 99)
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || o[0] >= 240)
    }
    IpAddr::V6(v6) => {
      if let Some(v4) = v6.to_ipv4_mapped() {
        return is_global_ip(IpAddr::V4(v4));
      }
      let s = v6.segments();
      // Only ordinary global unicast. Reject protocol/translation/tunnel
      // prefixes that could encode a non-public IPv4 destination, in addition
      // to local, documentation and reserved space. IPv4-mapped IPs above are
      // checked using the IPv4 policy before connecting to that exact address.
      (s[0] & 0xe000) == 0x2000
        && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
        && s[0] != 0x2002
        && !(s[0] == 0x3fff && s[1] < 0x1000)
    }
  }
}

pub(super) fn hosts_differ(requested: &str, final_url: &str) -> bool {
  let requested_host = Url::parse(requested).ok().and_then(|url| url.host_str().map(normalize_host));
  let final_host = Url::parse(final_url).ok().and_then(|url| url.host_str().map(normalize_host));
  match (requested_host, final_host) {
    (Some(requested_host), Some(final_host)) => requested_host != final_host,
    _ => requested != final_url,
  }
}

fn normalize_host(host: &str) -> String {
  host.trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
  use super::*;

  // `.invalid` names are refused by policy before any lookup, so these use a
  // subdomain of example.com, which is public but does not resolve.
  const UNRESOLVABLE: &str = "no-such-host-zz9q.example.com";

  #[tokio::test]
  async fn resolution_errors_name_the_host() {
    let url = Url::parse(&format!("https://{UNRESOLVABLE}/page")).unwrap();
    let error = public_addresses(&url).await.unwrap_err();
    assert!(error.to_string().starts_with(&format!("Failed to resolve host '{UNRESOLVABLE}':")), "{error}");
  }

  #[tokio::test]
  async fn checks_subresources_with_the_page_policy() {
    let url = Url::parse(&format!("https://{UNRESOLVABLE}/tracker.js")).unwrap();
    let error = check_subresource(&url, false).await.unwrap_err();
    assert!(error.to_string().starts_with("Failed to resolve host"), "{error}");
    let search = Url::parse("https://www.google.com/maps/embed").unwrap();
    assert!(check_subresource(&search, true).await.unwrap_err().to_string().contains("search engine"));
  }

  #[test]
  fn names_cloudflare_challenges() {
    let url = Url::parse("https://www.britishmuseum.org/visit").unwrap();
    let challenged =
      http_status_error(403, &url, [("server", "cloudflare"), ("CF-Mitigated", "challenge")].into_iter());
    assert_eq!(challenged.http_status(), Some(403));
    assert!(challenged.to_string().contains("Cloudflare challenge"), "{challenged}");
    assert!(challenged.to_string().contains("www.britishmuseum.org"), "{challenged}");
    let plain = http_status_error(403, &url, [("server", "cloudflare")].into_iter());
    assert_eq!(plain.to_string(), "Failed to fetch URL: HTTP 403");
  }
}
