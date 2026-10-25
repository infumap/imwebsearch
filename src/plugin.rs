// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Search-provider plugin contract shared by configuration and discovery.
//! See PLUGINS.md.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use log::{error, info, warn};
use serde_json::Value;

use crate::config::file::{PluginConfig, PluginProviderConfig, ProviderTier};

/// Plugin providers must not reuse these names.
pub const BUILT_IN_PROVIDERS: [&str; 17] = [
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

/// Bounds on provider metadata, whether declared by the plugin or configured by
/// the operator. Counts must also be positive. The request quota ledger holds
/// at most 10,000 requests.
pub const MAX_REQUESTS_PER_MINUTE: u32 = 1_000;
pub const MAX_CONCURRENT: u32 = 64;
pub const MAX_MIN_INTERVAL_MS: u64 = 3_600_000;
pub const MAX_DAILY_QUOTA: u32 = 10_000;
pub const MAX_ROUTING_REQUESTS_PER_DAY: u32 = 1_000_000;

const MAX_PROVIDERS: usize = 32;
const CONTRACT_VERSION: u64 = 1;
const METADATA_KEY: &str = "imwebsearch/provider";
const DISCOVERY_DEADLINE: Duration = Duration::from_secs(10);
const DEFAULT_ROUTING_REQUESTS_PER_DAY: u32 = 100;

static PROVIDERS: OnceLock<Vec<ProviderSpec>> = OnceLock::new();

/// Plugin-declared metadata after operator overrides.
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
pub fn check_provider_name(name: &str) -> Result<(), &'static str> {
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
#[expect(dead_code, reason = "Plugin providers are registered with the search router next.")]
pub fn providers() -> &'static [ProviderSpec] {
  PROVIDERS.get().map_or(&[], Vec::as_slice)
}

/// Run once at startup, before search providers are registered. Failure only
/// leaves plugin providers out; built-in search is unaffected.
pub async fn discover(plugin: &PluginConfig) {
  let Some(command) = &plugin.command else { return };
  let providers = match tokio::time::timeout(DISCOVERY_DEADLINE, declared_tools(command, plugin)).await {
    Ok(Ok(tools)) => providers_from(&tools, &plugin.providers),
    Ok(Err(message)) => {
      error!("Search plugin discovery failed: {message} No plugin providers are registered.");
      Vec::new()
    }
    Err(_) => {
      error!(
        "Search plugin discovery exceeded {}s. No plugin providers are registered.",
        DISCOVERY_DEADLINE.as_secs()
      );
      Vec::new()
    }
  };
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
  if PROVIDERS.set(providers).is_err() {
    warn!("Search plugin discovery ran more than once; keeping the first result.");
  }
}

#[cfg(unix)]
async fn declared_tools(command: &Path, plugin: &PluginConfig) -> Result<Vec<Value>, String> {
  use crate::providers::mcp_stdio::Session;

  check_executable(command)?;
  let mut session = Session::start("discovery", command, &plugin.args, &plugin.env).await.map_err(describe)?;
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

/// Whoever can modify the executable controls code the server runs.
#[cfg(unix)]
fn check_executable(path: &Path) -> Result<(), String> {
  use std::os::unix::fs::MetadataExt;

  let metadata =
    std::fs::metadata(path).map_err(|_| format!("plugin.command {path:?} does not exist or is not accessible."))?;
  if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
    return Err(format!("plugin.command {path:?} must be an executable regular file."));
  }
  if metadata.mode() & 0o022 != 0 {
    return Err(format!("plugin.command {path:?} must not be writable by group or others."));
  }
  // SAFETY: getuid has no preconditions and cannot fail.
  let uid = unsafe { libc::getuid() };
  if metadata.uid() != 0 && metadata.uid() != uid {
    return Err(format!("plugin.command {path:?} must be owned by root or the server's user."));
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
fn providers_from(tools: &[Value], overrides: &BTreeMap<String, PluginProviderConfig>) -> Vec<ProviderSpec> {
  let defaults = PluginProviderConfig::default();
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
    let config = overrides.get(name).unwrap_or(&defaults);
    if !config.enabled {
      info!("Search plugin provider {name} is disabled by configuration.");
      continue;
    }
    if providers.len() == MAX_PROVIDERS {
      warn!("Search plugin provider {name} skipped: at most {MAX_PROVIDERS} providers are registered.");
      continue;
    }
    match spec(name, metadata, config) {
      Ok(provider) => providers.push(provider),
      Err(reason) => warn!("Search plugin provider {name} skipped: {reason}."),
    }
  }
  for name in overrides.keys().filter(|name| !declared.contains(name.as_str())) {
    warn!("plugin.providers.{name} matches no provider the plugin declares.");
  }
  providers
}

/// Declared values are validated even where the operator overrides them.
fn spec(name: &str, metadata: &Value, config: &PluginProviderConfig) -> Result<ProviderSpec, String> {
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
  // Configuration validation applied the same bounds to overrides.
  let configured = |value: Option<u32>| value.and_then(NonZeroU32::new);
  let daily_quota = configured(config.daily_quota).or(daily_quota);
  Ok(ProviderSpec {
    // Registered once at startup and referenced for the life of the process.
    name: Box::leak(name.to_owned().into_boxed_str()),
    tier: config.tier.map_or(tier.get(), ProviderTier::get),
    requests_per_minute: configured(config.requests_per_minute).unwrap_or(requests_per_minute),
    max_concurrent: configured(config.max_concurrent).or(max_concurrent),
    min_interval: Duration::from_millis(config.min_interval_ms.unwrap_or(min_interval_ms)),
    daily_quota: daily_quota.map(NonZeroU32::get),
    routing_requests_per_day: configured(config.routing_requests_per_day)
      .or(routing_requests_per_day)
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

  fn overrides(source: &str) -> BTreeMap<String, PluginProviderConfig> {
    toml::from_str(source).unwrap()
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
    let providers = providers_from(
      &[tool("acme_a", minimal), tool("acme_b", full), json!({ "name": "general_tool", "inputSchema": {} })],
      &BTreeMap::new(),
    );
    assert_eq!(names(&providers), ["acme_a", "acme_b"]);
    let (a, b) = (&providers[0], &providers[1]);
    assert_eq!((a.tier, a.requests_per_minute.get(), a.max_concurrent, a.daily_quota), (2, 10, None, None));
    assert_eq!((a.min_interval, a.routing_requests_per_day), (Duration::ZERO, DEFAULT_ROUTING_REQUESTS_PER_DAY));
    assert_eq!((b.tier, b.max_concurrent.map(NonZeroU32::get), b.min_interval), (3, Some(2), Duration::from_millis(1500)));
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
    let providers = providers_from(
      &[
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
      ],
      &BTreeMap::new(),
    );
    assert_eq!(names(&providers), ["ok"]);
  }

  #[test]
  fn overrides_replace_declared_metadata() {
    let metadata = json!({ "version": 1, "tier": 1, "requests_per_minute": 10, "daily_quota": 50 });
    let providers = providers_from(
      &[tool("acme_a", metadata.clone()), tool("acme_b", metadata)],
      &overrides(
        "[acme_a]\ntier = 4\nrequests_per_minute = 2\nmax_concurrent = 1\nmin_interval_ms = 10\ndaily_quota = 20\n\
         [acme_b]\nenabled = false\n[missing]\ntier = 2\n",
      ),
    );
    assert_eq!(names(&providers), ["acme_a"]);
    let provider = &providers[0];
    assert_eq!((provider.tier, provider.requests_per_minute.get()), (4, 2));
    assert_eq!((provider.max_concurrent.map(NonZeroU32::get), provider.min_interval), (Some(1), Duration::from_millis(10)));
    assert_eq!((provider.daily_quota, provider.routing_requests_per_day), (Some(20), 20));
  }

  #[test]
  fn registers_at_most_the_provider_limit() {
    let metadata = json!({ "version": 1, "tier": 1, "requests_per_minute": 1 });
    let tools: Vec<_> = (0..MAX_PROVIDERS + 2).map(|index| tool(&format!("p{index}"), metadata.clone())).collect();
    assert_eq!(providers_from(&tools, &overrides("[p0]\nenabled = false\n")).len(), MAX_PROVIDERS);
  }

  #[cfg(unix)]
  #[test]
  fn executable_must_be_protected() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("plugin");
    std::fs::write(&path, "#!/bin/sh\n").unwrap();
    for (mode, allowed) in [(0o755, true), (0o700, true), (0o644, false), (0o775, false), (0o757, false)] {
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
    assert_eq!(names(&providers_from(&tools, &BTreeMap::new())), ["acme"]);
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
