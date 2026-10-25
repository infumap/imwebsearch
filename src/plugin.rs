// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Search-provider plugin contract shared by configuration and discovery.
//! See PLUGINS.md.

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use log::{error, info, warn};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::config::file::PluginConfig;

/// Plugin providers must not reuse these names.
const BUILT_IN_PROVIDERS: [&str; 17] = [
  "you",
  "exa",
  "exa_mcp",
  "tavily",
  "tavily_mcp",
  "firecrawl",
  "firecrawl_mcp",
  "parallel",
  "parallel_mcp",
  "brave",
  "brightdata",
  "diffbot",
  "serpapi",
  "context",
  "purili",
  "hacker_news",
  "wikipedia",
];

/// Bounds on declared provider metadata. Counts must also be positive. The
/// request quota ledger holds at most 10,000 requests.
const MAX_REQUESTS_PER_MINUTE: u32 = 1_000;
const MAX_CONCURRENT: u32 = 64;
const MAX_MIN_INTERVAL_MS: u64 = 3_600_000;
const MAX_DAILY_QUOTA: u32 = 10_000;
const MAX_ROUTING_REQUESTS_PER_DAY: u32 = 1_000_000;

const MAX_PROVIDERS: usize = 32;
const CONTRACT_VERSION: u64 = 1;
const METADATA_KEY: &str = "imwebsearch/provider";
const DISCOVERY_DEADLINE: Duration = Duration::from_secs(10);
const DEFAULT_ROUTING_REQUESTS_PER_DAY: u32 = 100;

static DISCOVERY: OnceLock<Discovery> = OnceLock::new();

/// Set once at startup when a plugin is configured, whatever the outcome.
struct Discovery {
  providers: Vec<ProviderSpec>,
  error: Option<String>,
  /// Shared by every provider of the plugin.
  processes: Arc<Semaphore>,
  max_processes: u32,
}

/// Provider metadata as the plugin declares it.
pub struct ProviderSpec {
  pub name: &'static str,
  pub tier: u32,
  pub requests_per_minute: NonZeroU32,
  pub max_concurrent: Option<NonZeroU32>,
  pub min_interval: Duration,
  pub daily_quota: Option<u32>,
  pub routing_requests_per_day: u32,
}

/// Provider names key persistent cooldown and quota ledgers, so they must be
/// filename-safe and distinct from every built-in provider.
fn check_provider_name(name: &str) -> Result<(), &'static str> {
  if !(1..=32).contains(&name.len())
    || !name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
  {
    return Err("must be 1 to 32 characters of a-z, 0-9 and _");
  }
  if BUILT_IN_PROVIDERS.contains(&name) {
    return Err("must not be a built-in provider name");
  }
  Ok(())
}

/// Empty unless discovery succeeded.
pub fn providers() -> &'static [ProviderSpec] {
  DISCOVERY.get().map_or(&[], |discovery| discovery.providers.as_slice())
}

pub fn tier(name: &str) -> Option<u32> {
  providers().iter().find(|provider| provider.name == name).map(|provider| provider.tier)
}

pub fn processes() -> Option<&'static Arc<Semaphore>> {
  DISCOVERY.get().map(|discovery| &discovery.processes)
}

/// Local state only; never contacts the plugin.
pub fn readiness() -> Value {
  let Some(discovery) = DISCOVERY.get() else { return json!({ "configured": false }) };
  let names: Vec<&str> = discovery.providers.iter().map(|provider| provider.name).collect();
  json!({
    "configured": true,
    "state": if discovery.error.is_some() { "failed" } else { "ready" },
    "error": discovery.error,
    "providers": names,
    "max_processes": discovery.max_processes,
    "running_processes": (discovery.max_processes as usize).saturating_sub(discovery.processes.available_permits()),
  })
}

/// Run once at startup, before search providers are registered. Failure only
/// leaves plugin providers out; built-in search is unaffected.
pub async fn discover(plugin: &PluginConfig) {
  let Some(command) = &plugin.command else { return };
  let (providers, failure) = match tokio::time::timeout(DISCOVERY_DEADLINE, declared_tools(command, plugin)).await {
    Ok(Ok(tools)) => (providers_from(&tools), None),
    Ok(Err(message)) => (Vec::new(), Some(message)),
    Err(_) => (Vec::new(), Some(format!("Discovery exceeded {}s.", DISCOVERY_DEADLINE.as_secs()))),
  };
  if let Some(message) = &failure {
    error!("Search plugin discovery failed: {message} No plugin providers are registered.");
  }
  for provider in &providers {
    info!(
      "Search plugin provider {}: tier={}, requests_per_minute={}, max_concurrent={}, min_interval_ms={}, \
       daily_quota={}, routing_requests_per_day={}",
      provider.name,
      provider.tier,
      provider.requests_per_minute,
      provider.max_concurrent.map_or("none".to_owned(), |limit| limit.to_string()),
      provider.min_interval.as_millis(),
      provider.daily_quota.map_or("none".to_owned(), |quota| quota.to_string()),
      provider.routing_requests_per_day
    );
  }
  let discovery = Discovery {
    providers,
    error: failure,
    processes: Arc::new(Semaphore::new(plugin.max_processes as usize)),
    max_processes: plugin.max_processes,
  };
  if DISCOVERY.set(discovery).is_err() {
    warn!("Search plugin discovery ran more than once; keeping the first result.");
  }
}

#[cfg(unix)]
async fn declared_tools(command: &Path, plugin: &PluginConfig) -> Result<Vec<Value>, String> {
  use crate::providers::mcp_stdio::Session;

  check_executable(command)?;
  let mut session = Session::start("discovery", command, &plugin.args, &plugin.env, None).await.map_err(describe)?;
  let result = session.request("tools/list", serde_json::json!({})).await;
  session.finish();
  match result.map_err(describe)?.get_mut("tools").map(Value::take) {
    Some(Value::Array(tools)) => Ok(tools),
    _ => Err("tools/list returned no tools array.".to_owned()),
  }
}

#[cfg(not(unix))]
async fn declared_tools(_command: &Path, _plugin: &PluginConfig) -> Result<Vec<Value>, String> {
  Err("Search plugins require a Unix platform.".to_owned())
}

/// Reports a clearer error than a failed spawn would.
#[cfg(unix)]
fn check_executable(path: &Path) -> Result<(), String> {
  use std::os::unix::fs::PermissionsExt;

  let metadata =
    std::fs::metadata(path).map_err(|_| format!("plugin.command {path:?} does not exist or is not accessible."))?;
  if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
    return Err(format!("plugin.command {path:?} must be an executable regular file."));
  }
  Ok(())
}

#[cfg(unix)]
fn describe(error: crate::providers::ProviderError) -> String {
  match &error.upstream_detail {
    Some(detail) => format!("{error} ({detail:?})"),
    None => error.to_string(),
  }
}

/// Tools without provider metadata are ignored. Invalid declarations are
/// logged and skipped, so the remaining providers still register.
fn providers_from(tools: &[Value]) -> Vec<ProviderSpec> {
  let mut providers = Vec::new();
  let mut declared = BTreeSet::new();
  for tool in tools {
    let Some(metadata) = tool.get("_meta").and_then(|meta| meta.get(METADATA_KEY)) else { continue };
    let Some(name) = tool.get("name").and_then(Value::as_str) else {
      warn!("Search plugin declared a provider without a name; skipped.");
      continue;
    };
    if let Err(reason) = check_provider_name(name) {
      warn!("Search plugin provider {name:?} skipped: its name {reason}.");
      continue;
    }
    if !declared.insert(name) {
      warn!("Search plugin provider {name} is declared more than once; repeated declarations skipped.");
      continue;
    }
    if providers.len() == MAX_PROVIDERS {
      warn!("Search plugin provider {name} skipped: at most {MAX_PROVIDERS} providers are registered.");
      continue;
    }
    match spec(name, metadata) {
      Ok(provider) => providers.push(provider),
      Err(reason) => warn!("Search plugin provider {name} skipped: {reason}."),
    }
  }
  providers
}

fn spec(name: &str, metadata: &Value) -> Result<ProviderSpec, String> {
  if metadata.get("version").and_then(Value::as_u64) != Some(CONTRACT_VERSION) {
    return Err(format!("version must be {CONTRACT_VERSION}"));
  }
  let tier = count(metadata, "tier", u32::MAX)?.ok_or("tier is required")?;
  let requests_per_minute =
    count(metadata, "requests_per_minute", MAX_REQUESTS_PER_MINUTE)?.ok_or("requests_per_minute is required")?;
  let max_concurrent = count(metadata, "max_concurrent", MAX_CONCURRENT)?;
  let daily_quota = count(metadata, "daily_quota", MAX_DAILY_QUOTA)?;
  let routing_requests_per_day = count(metadata, "routing_requests_per_day", MAX_ROUTING_REQUESTS_PER_DAY)?;
  let min_interval_ms = match metadata.get("min_interval_ms") {
    None | Some(Value::Null) => 0,
    Some(value) => value
      .as_u64()
      .filter(|&interval| interval <= MAX_MIN_INTERVAL_MS)
      .ok_or_else(|| format!("min_interval_ms must be an integer from 0 to {MAX_MIN_INTERVAL_MS}"))?,
  };
  Ok(ProviderSpec {
    // Registered once at startup and referenced for the life of the process.
    name: Box::leak(name.to_owned().into_boxed_str()),
    tier: tier.get(),
    requests_per_minute,
    max_concurrent,
    min_interval: Duration::from_millis(min_interval_ms),
    daily_quota: daily_quota.map(NonZeroU32::get),
    routing_requests_per_day: routing_requests_per_day
      .or(daily_quota)
      .map_or(DEFAULT_ROUTING_REQUESTS_PER_DAY, NonZeroU32::get),
  })
}

fn count(metadata: &Value, field: &str, max: u32) -> Result<Option<NonZeroU32>, String> {
  match metadata.get(field) {
    None | Some(Value::Null) => Ok(None),
    Some(value) => value
      .as_u64()
      .and_then(|value| u32::try_from(value).ok())
      .filter(|&value| value <= max)
      .and_then(NonZeroU32::new)
      .map(Some)
      .ok_or_else(|| format!("{field} must be an integer from 1 to {max}")),
  }
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn tool(name: &str, metadata: Value) -> Value {
    json!({ "name": name, "inputSchema": { "type": "object" }, "_meta": { METADATA_KEY: metadata } })
  }

  fn names(providers: &[ProviderSpec]) -> Vec<&str> {
    providers.iter().map(|provider| provider.name).collect()
  }

  #[test]
  fn declared_metadata_and_defaults() {
    let minimal = json!({ "version": 1, "tier": 2, "requests_per_minute": 10 });
    let full = json!({
      "version": 1, "tier": 3, "requests_per_minute": 5, "max_concurrent": 2, "min_interval_ms": 1500,
      "daily_quota": 200, "unknown": true
    });
    let providers = providers_from(&[
      tool("acme_a", minimal),
      tool("acme_b", full),
      json!({ "name": "general_tool", "inputSchema": {} }),
    ]);
    assert_eq!(names(&providers), ["acme_a", "acme_b"]);
    let (a, b) = (&providers[0], &providers[1]);
    assert_eq!((a.tier, a.requests_per_minute.get(), a.max_concurrent, a.daily_quota), (2, 10, None, None));
    assert_eq!((a.min_interval, a.routing_requests_per_day), (Duration::ZERO, DEFAULT_ROUTING_REQUESTS_PER_DAY));
    assert_eq!(
      (b.tier, b.max_concurrent.map(NonZeroU32::get), b.min_interval),
      (3, Some(2), Duration::from_millis(1500))
    );
    // The routing target follows the daily quota unless declared.
    assert_eq!((b.daily_quota, b.routing_requests_per_day), (Some(200), 200));
  }

  #[test]
  fn invalid_declarations_are_skipped() {
    let valid = json!({ "version": 1, "tier": 1, "requests_per_minute": 1 });
    let with = |field: &str, value: Value| {
      let mut metadata = valid.clone();
      metadata[field] = value;
      metadata
    };
    let mut without_tier = valid.clone();
    without_tier.as_object_mut().unwrap().remove("tier");
    let providers = providers_from(&[
      tool("ok", valid.clone()),
      tool("ok", valid.clone()),
      tool("exa", valid.clone()),
      tool("Bad", valid.clone()),
      json!({ "_meta": { METADATA_KEY: valid.clone() } }),
      tool("no_tier", without_tier),
      tool("version_two", with("version", json!(2))),
      tool("zero_rate", with("requests_per_minute", json!(0))),
      tool("float_rate", with("requests_per_minute", json!(1.5))),
      tool("big_quota", with("daily_quota", json!(10_001))),
      tool("long_interval", with("min_interval_ms", json!(3_600_001))),
    ]);
    assert_eq!(names(&providers), ["ok"]);
  }

  #[test]
  fn registers_at_most_the_provider_limit() {
    let metadata = json!({ "version": 1, "tier": 1, "requests_per_minute": 1 });
    let tools: Vec<_> = (0..MAX_PROVIDERS + 2).map(|index| tool(&format!("p{index}"), metadata.clone())).collect();
    assert_eq!(providers_from(&tools).len(), MAX_PROVIDERS);
  }

  #[cfg(unix)]
  #[test]
  fn command_must_be_executable_file() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("plugin");
    std::fs::write(&path, "#!/bin/sh\n").unwrap();
    for (mode, allowed) in [(0o755, true), (0o700, true), (0o777, true), (0o644, false)] {
      std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
      assert_eq!(check_executable(&path).is_ok(), allowed, "mode {mode:o}");
    }
    assert!(check_executable(directory.path()).is_err());
    assert!(check_executable(&directory.path().join("missing")).is_err());
  }

  #[cfg(unix)]
  fn shell_plugin(script: &str) -> PluginConfig {
    PluginConfig {
      command: Some("/bin/sh".into()),
      args: vec!["-c".to_owned(), script.to_owned()],
      ..PluginConfig::default()
    }
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn lists_tools_from_plugin() {
    let plugin = shell_plugin(
      "read -r _; printf '%s\\n' \
       '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}}}}'; \
       read -r _; read -r _; printf '%s\\n' \
       '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"acme\",\"_meta\":{\"imwebsearch/provider\":{\"version\":1,\"tier\":2,\"requests_per_minute\":3}}}]}}'",
    );
    let tools = declared_tools(Path::new("/bin/sh"), &plugin).await.unwrap();
    assert_eq!(names(&providers_from(&tools)), ["acme"]);
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn reports_plugin_failures() {
    for script in [
      "exit 1",
      "read -r _; printf '%s\\n' \
       '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}}}}'; \
       read -r _; read -r _; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}'",
    ] {
      assert!(declared_tools(Path::new("/bin/sh"), &shell_plugin(script)).await.is_err(), "{script}");
    }
  }
}
