// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Single-file configuration shared by every server and browser command.
//! Configuration types deliberately omit Debug and Serialize to protect credentials.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use once_cell::sync::OnceCell;
use reqwest::header::HeaderValue;
use serde::Deserialize;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const EXAMPLE_CONFIG: &str = include_str!("../../config.example.toml");
static CONFIG: OnceCell<FileConfig> = OnceCell::new();

/// Locate the per-user configuration, creating it from the embedded example
/// only when absent. Publish a complete file without replacing an existing one.
fn ensure_default_config() -> Result<PathBuf, String> {
  let home = std::env::home_dir()
    .filter(|home| home.is_absolute())
    .ok_or("Could not determine an absolute home directory; use --config <path>.")?;
  let directory = home.join(".imwebsearch");
  let path = directory.join("config.toml");
  match std::fs::symlink_metadata(&path) {
    Ok(_) => return Ok(path),
    Err(error) if error.kind() == ErrorKind::NotFound => {}
    Err(_) => return Err("Could not inspect ~/.imwebsearch/config.toml.".to_owned()),
  }
  let mut builder = std::fs::DirBuilder::new();
  builder.recursive(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
  }
  builder.create(&directory).map_err(|_| "Could not create ~/.imwebsearch directory.".to_owned())?;
  // NamedTempFile uses owner-only permissions on Unix. Keep those permissions
  // when publishing, so API keys can be added to the generated file later.
  let mut file = tempfile::Builder::new()
    .prefix(".config-")
    .tempfile_in(&directory)
    .map_err(|_| "Could not create temporary configuration file in ~/.imwebsearch.".to_owned())?;
  file.write_all(EXAMPLE_CONFIG.as_bytes()).map_err(|_| "Could not write default configuration.".to_owned())?;
  match file.persist_noclobber(&path) {
    Ok(_) => {}
    // Another process may have created the configuration after our inspection.
    Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {}
    Err(_) => return Err("Could not create ~/.imwebsearch/config.toml.".to_owned()),
  }
  Ok(path)
}

/// Select the file once at startup. Only an omitted path creates a missing
/// default configuration; explicit paths must already exist.
pub fn initialize(path: Option<&Path>) -> Result<&'static FileConfig, String> {
  CONFIG.get_or_try_init(|| match path {
    Some(path) => FileConfig::load(path),
    None => FileConfig::load(&ensure_default_config()?),
  })
}

pub fn current() -> Result<&'static FileConfig, &'static str> {
  CONFIG.get().ok_or("TOML configuration has not been initialized.")
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
  pub data_dir: PathBuf,
  pub server: ServerConfig,
  pub logging: LoggingConfig,
  pub providers: ProvidersConfig,
  pub fallback_providers: FallbackProvidersConfig,
  pub plugin: PluginConfig,
  pub browser: BrowserConfig,
  pub browser_relay: BrowserRelayConfig,
  pub fetch: FetchSettings,
}

impl Default for FileConfig {
  fn default() -> Self {
    Self {
      data_dir: ".".into(),
      server: ServerConfig::default(),
      logging: LoggingConfig::default(),
      providers: ProvidersConfig::default(),
      fallback_providers: FallbackProvidersConfig::default(),
      plugin: PluginConfig::default(),
      browser: BrowserConfig::default(),
      browser_relay: BrowserRelayConfig::default(),
      fetch: FetchSettings::default(),
    }
  }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
  pub host: IpAddr,
  pub port: u16,
}

impl Default for ServerConfig {
  fn default() -> Self {
    Self { host: IpAddr::V4(Ipv4Addr::LOCALHOST), port: 8791 }
  }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
  pub filter: String,
}

impl Default for LoggingConfig {
  fn default() -> Self {
    Self { filter: "info".to_owned() }
  }
}

/// Tiers are positive integers. Keyless providers default to tier one;
/// API-key providers use tier two, or tier three when a credit card or phone number is required.
/// Purili defaults to tier four.
#[derive(Clone, Copy, Deserialize)]
#[serde(transparent)]
pub struct ProviderTier(NonZeroU32);

impl Default for ProviderTier {
  fn default() -> Self {
    Self(NonZeroU32::MIN)
  }
}

impl ProviderTier {
  pub fn get(self) -> u32 {
    self.0.get()
  }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProvidersConfig {
  pub you: YouConfig,
  pub exa_mcp: ExaMcpConfig,
  pub tavily_mcp: TavilyMcpConfig,
  pub firecrawl_mcp: FirecrawlMcpConfig,
  pub parallel_mcp: ParallelMcpConfig,
  pub exa: MeteredConfig,
  pub tavily: MeteredConfig,
  pub diffbot: MeteredConfig,
  pub firecrawl: MeteredConfig,
  pub brave: TierThreeMeteredConfig,
  pub parallel: TierThreeMeteredConfig,
  pub brightdata: MeteredConfig,
  pub serpapi: TierThreeMeteredConfig,
  pub context: MeteredConfig,
  pub purili: PuriliConfig,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FallbackProvidersConfig {
  pub wikipedia: WikipediaConfig,
  pub hacker_news: HackerNewsConfig,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HackerNewsConfig {
  pub max_requests_per_minute: u32,
}

impl Default for HackerNewsConfig {
  fn default() -> Self {
    Self { max_requests_per_minute: 30 }
  }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct YouConfig {
  pub max_requests_per_minute: u32,
  pub tier: ProviderTier,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExaMcpConfig {
  pub max_requests_per_minute: u32,
  pub tier: ProviderTier,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TavilyMcpConfig {
  pub max_requests_per_minute: u32,
  pub tier: ProviderTier,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FirecrawlMcpConfig {
  pub max_requests_per_minute: u32,
  pub tier: ProviderTier,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParallelMcpConfig {
  pub max_requests_per_minute: u32,
  pub tier: ProviderTier,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MeteredConfig {
  pub api_key: String,
  pub tier: ProviderTier,
}

impl Default for MeteredConfig {
  fn default() -> Self {
    Self { api_key: String::new(), tier: ProviderTier(NonZeroU32::new(2).expect("tier 2 is nonzero")) }
  }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TierThreeMeteredConfig {
  pub api_key: String,
  pub tier: ProviderTier,
}

impl Default for TierThreeMeteredConfig {
  fn default() -> Self {
    Self { api_key: String::new(), tier: ProviderTier(NonZeroU32::new(3).expect("tier 3 is nonzero")) }
  }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PuriliConfig {
  pub max_requests_per_minute: u32,
  pub tier: ProviderTier,
}

impl Default for PuriliConfig {
  fn default() -> Self {
    Self { max_requests_per_minute: 0, tier: ProviderTier(NonZeroU32::new(4).expect("tier 4 is nonzero")) }
  }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WikipediaConfig {
  pub max_requests_per_minute: u32,
  pub language: String,
}

impl Default for WikipediaConfig {
  fn default() -> Self {
    Self { max_requests_per_minute: 30, language: "en".to_owned() }
  }
}

/// One executable adding search providers over MCP stdio; see PLUGINS.md.
/// The executable itself is checked at discovery, so a missing or broken
/// plugin never prevents startup or built-in search.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginConfig {
  /// None disables plugins.
  pub command: Option<PathBuf>,
  pub args: Vec<String>,
  /// The complete plugin environment; nothing is inherited.
  pub env: BTreeMap<String, String>,
  /// Plugin processes running at once, across all of its providers.
  pub max_processes: u32,
}

impl Default for PluginConfig {
  fn default() -> Self {
    Self { command: None, args: Vec::new(), env: BTreeMap::new(), max_processes: 4 }
  }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
  pub path: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserRelayConfig {
  /// Shared secret the relay extension presents; empty disables the relay.
  pub token: String,
  pub timeout_secs: u64,
}

impl Default for BrowserRelayConfig {
  fn default() -> Self {
    Self { token: String::new(), timeout_secs: 55 }
  }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FetchSettings {
  pub timeout_secs: u64,
  pub gpu_tools_url: Option<String>,
  pub html_dumps_enabled: bool,
  pub pdf_cache: PdfCacheConfig,
}

impl Default for FetchSettings {
  fn default() -> Self {
    Self { timeout_secs: 30, gpu_tools_url: None, html_dumps_enabled: false, pdf_cache: PdfCacheConfig::default() }
  }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PdfCacheConfig {
  pub enabled: bool,
  pub ttl_secs: u64,
  pub refresh: bool,
}

impl Default for PdfCacheConfig {
  fn default() -> Self {
    Self { enabled: true, ttl_secs: 86400, refresh: false }
  }
}

impl FileConfig {
  pub fn state_dir(&self) -> PathBuf {
    self.data_dir.join("state")
  }

  pub fn browser_cache_dir(&self) -> PathBuf {
    self.data_dir.join("cache/browsers")
  }

  pub fn pdf_cache_dir(&self) -> PathBuf {
    self.data_dir.join("cache/pdf-native-v1")
  }

  pub fn html_dump_dir(&self) -> PathBuf {
    self.data_dir.join("html-dumps")
  }

  pub fn temporary_dir(&self) -> PathBuf {
    self.data_dir.join("tmp")
  }

  /// Runtime setup is separate from validation, which must create no files.
  pub fn prepare_data_dir(&self) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::DirBuilderExt;
      builder.mode(0o700);
    }
    builder.create(&self.data_dir).map_err(|_| "Could not create data directory.".to_owned())?;
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      std::fs::set_permissions(&self.data_dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| "Could not protect data directory.".to_owned())?;
    }
    Ok(())
  }

  pub(super) fn load(path: &Path) -> Result<Self, String> {
    let path = std::path::absolute(path).map_err(|_| "Could not resolve configuration file path.".to_owned())?;
    if path.file_name() != Some(std::ffi::OsStr::new("config.toml")) {
      return Err("Configuration file must be named config.toml and reside directly inside data_dir.".to_owned());
    }
    let file = File::open(&path).map_err(|error| match error.kind() {
      std::io::ErrorKind::NotFound => "Configuration file does not exist.".to_owned(),
      std::io::ErrorKind::PermissionDenied => "Configuration file is not readable.".to_owned(),
      _ => "Could not open configuration file.".to_owned(),
    })?;
    let metadata = file.metadata().map_err(|_| "Could not inspect configuration file.".to_owned())?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
      return Err("Configuration must be a regular file no larger than 1 MiB.".to_owned());
    }
    let mut source = String::new();
    file
      .take(MAX_CONFIG_BYTES + 1)
      .read_to_string(&mut source)
      .map_err(|_| "Could not read configuration as UTF-8 text.".to_owned())?;
    if source.len() as u64 > MAX_CONFIG_BYTES {
      return Err("Configuration file exceeds 1 MiB.".to_owned());
    }
    let mut config: Self = toml::from_str(&source).map_err(|error| parse_error(&source, error))?;
    // Resolve relative settings against the selected file's directory, including
    // when the file is a symlink. Other runtime paths need not exist yet.
    config.validate(path.parent().ok_or("Configuration file has no parent directory.")?)?;
    Ok(config)
  }

  pub(super) fn validate(&mut self, directory: &Path) -> Result<(), String> {
    if self.logging.filter.trim().is_empty() || env_filter::Builder::new().try_parse(&self.logging.filter).is_err() {
      return Err("logging.filter must be a valid nonempty log filter.".to_owned());
    }
    resolve_path(&mut self.data_dir, directory, "data_dir")?;
    let config_directory =
      directory.canonicalize().map_err(|_| "Could not resolve configuration directory.".to_owned())?;
    let data_directory = self.data_dir.canonicalize().map_err(|_| {
      "data_dir must be the existing directory containing the selected config.toml directly inside it.".to_owned()
    })?;
    if data_directory != config_directory {
      return Err("data_dir must contain the selected config.toml directly inside it.".to_owned());
    }
    self.data_dir = data_directory;
    let providers = &mut self.providers;
    for (name, key) in [
      ("exa", &mut providers.exa.api_key),
      ("tavily", &mut providers.tavily.api_key),
      ("diffbot", &mut providers.diffbot.api_key),
      ("firecrawl", &mut providers.firecrawl.api_key),
      ("brave", &mut providers.brave.api_key),
      ("parallel", &mut providers.parallel.api_key),
      ("brightdata", &mut providers.brightdata.api_key),
      ("serpapi", &mut providers.serpapi.api_key),
      ("context", &mut providers.context.api_key),
    ] {
      validate_api_key(key, name)?;
    }
    let language = &self.fallback_providers.wikipedia.language;
    if !(2..=12).contains(&language.len())
      || !language.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'-')
      || language.starts_with('-')
      || language.ends_with('-')
    {
      return Err("fallback_providers.wikipedia.language must be a Wikipedia subdomain code such as en.".to_owned());
    }
    validate_plugin(&mut self.plugin, directory)?;
    if let Some(path) = &mut self.browser.path {
      resolve_path(path, directory, "browser.path")?;
    }
    let relay = &mut self.browser_relay;
    relay.token = relay.token.trim().to_owned();
    if !relay.token.is_empty() && (relay.token.len() < 32 || !relay.token.bytes().all(|byte| byte.is_ascii_graphic())) {
      return Err("browser_relay.token must be empty or at least 32 printable ASCII characters.".to_owned());
    }
    if !(5..=3600).contains(&relay.timeout_secs) {
      return Err("browser_relay.timeout_secs must be between 5 and 3600.".to_owned());
    }
    let fetch = &mut self.fetch;
    for (field, value, cap) in [
      ("fetch.timeout_secs", fetch.timeout_secs, 3600),
      ("fetch.pdf_cache.ttl_secs", fetch.pdf_cache.ttl_secs, 30 * 86400),
    ] {
      if value == 0 || value > cap {
        return Err(format!("{field} must be between 1 and {cap}."));
      }
    }
    if let Some(raw) = &mut fetch.gpu_tools_url {
      let mut url = http_url(raw, "fetch.gpu_tools_url")?;
      if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("fetch.gpu_tools_url must not contain credentials, a query or a fragment.".to_owned());
      }
      let path = url.path().trim_end_matches('/').to_owned();
      url.set_path(path.strip_suffix("/gpu-tools").unwrap_or(&path));
      *raw = url.to_string();
    }
    Ok(())
  }
}

fn validate_plugin(plugin: &mut PluginConfig, directory: &Path) -> Result<(), String> {
  match &mut plugin.command {
    Some(command) => resolve_path(command, directory, "plugin.command")?,
    None if !plugin.args.is_empty() || !plugin.env.is_empty() => {
      return Err("plugin.args and plugin.env require plugin.command.".to_owned());
    }
    None => {}
  }
  if !(1..=64).contains(&plugin.max_processes) {
    return Err("plugin.max_processes must be between 1 and 64.".to_owned());
  }
  if plugin.args.iter().any(|arg| arg.contains('\0')) {
    return Err("plugin.args must not contain NUL characters.".to_owned());
  }
  // Never echo environment names or values; either may be a credential.
  if plugin.env.iter().any(|(name, value)| name.is_empty() || name.contains(['=', '\0']) || value.contains('\0')) {
    return Err("plugin.env names must be nonempty without = or NUL, and values must not contain NUL.".to_owned());
  }
  Ok(())
}

fn validate_api_key(key: &mut String, provider: &str) -> Result<(), String> {
  *key = key.trim().to_owned();
  if !key.is_empty() && HeaderValue::from_str(key).is_err() {
    return Err(format!("providers.{provider}.api_key must be a valid HTTP header value."));
  }
  Ok(())
}

fn resolve_path(path: &mut PathBuf, directory: &Path, field: &str) -> Result<(), String> {
  if path.as_os_str().as_encoded_bytes().contains(&0) || path.as_os_str().to_string_lossy().trim().is_empty() {
    return Err(format!("{field} must be a nonempty path without NUL characters."));
  }
  if let Ok(relative) = path.strip_prefix("~") {
    let home = std::env::home_dir()
      .filter(|home| home.is_absolute())
      .ok_or_else(|| format!("{field} requires an absolute home directory for ~ expansion."))?;
    *path = home.join(relative);
  } else if path.is_relative() {
    *path = directory.join(&*path);
  }
  Ok(())
}

fn http_url(raw: &mut String, field: &str) -> Result<reqwest::Url, String> {
  *raw = raw.trim().to_owned();
  reqwest::Url::parse(raw)
    .ok()
    .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
    .ok_or_else(|| format!("{field} must be an absolute HTTP(S) URL."))
}

fn parse_error(source: &str, error: toml::de::Error) -> String {
  // TOML/Serde diagnostics can include source lines, secret values and even
  // unknown field names containing secrets. Expose only a safe location.
  let location = error.span().and_then(|span| source.get(..span.start)).map(|prefix| {
    let line = prefix.bytes().filter(|&byte| byte == b'\n').count() + 1;
    let column = prefix.rsplit('\n').next().unwrap_or_default().chars().count() + 1;
    format!(" at line {line}, column {column}")
  });
  format!("Invalid TOML configuration{}; check syntax, setting names and value types.", location.unwrap_or_default())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn validated(source: &str) -> Result<(tempfile::TempDir, FileConfig), String> {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut config: FileConfig = toml::from_str(source).map_err(|error| error.to_string())?;
    config.validate(directory.path())?;
    Ok((directory, config))
  }

  #[test]
  fn example_config_leaves_plugin_disabled() {
    let (_directory, config) = validated(EXAMPLE_CONFIG.replace("~/.imwebsearch", ".").as_str()).unwrap();
    assert!(config.plugin.command.is_none());
    assert_eq!(config.plugin.max_processes, 4);
  }

  #[test]
  fn plugin_command_resolves_against_config_directory() {
    let (directory, config) =
      validated("[plugin]\ncommand = \"bin/plugin\"\nargs = [\"--serve\"]\n[plugin.env]\nKEY = \"a=b\"\n").unwrap();
    assert_eq!(config.plugin.command.as_deref(), Some(directory.path().join("bin/plugin").as_path()));
  }

  #[test]
  fn plugin_settings_require_command() {
    for source in ["[plugin]\nargs = [\"x\"]\n", "[plugin.env]\nKEY = \"x\"\n"] {
      assert!(validated(source).err().unwrap_or_default().contains("require plugin.command"), "{source}");
    }
  }

  #[test]
  fn plugin_rejects_invalid_values() {
    for (source, expected) in [
      ("max_processes = 0", "max_processes"),
      ("max_processes = 65", "max_processes"),
      ("[plugin.env]\n\"\" = \"x\"", "plugin.env"),
      ("[plugin.env]\n\"A=B\" = \"x\"", "plugin.env"),
    ] {
      let source = format!("[plugin]\ncommand = \"/bin/true\"\n{source}\n");
      let error = validated(&source).err().unwrap_or_default();
      assert!(error.contains(expected), "{source}: {error}");
    }
  }
}
