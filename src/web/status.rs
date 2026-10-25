// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::fmt::Write;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use serde_json::Value;

use super::{HTML, document, escape, response};

struct Allowance {
  used: f64,
  reserved: f64,
  total: f64,
  remaining: f64,
  credit: bool,
}

impl Allowance {
  fn read(provider: &Value) -> Option<Self> {
    let budget = &provider["budget"];
    if budget.is_object() && budget["persistence_error"].is_null() {
      return Some(Self {
        used: budget["consumed_usd_micros"].as_f64()?,
        reserved: budget["reserved_usd_micros"].as_f64()?,
        total: budget["free_allowance_usd_micros"].as_f64()?,
        remaining: budget["free_remaining_usd_micros"].as_f64()?,
        credit: true,
      });
    }
    let daily = &provider["daily_quota"];
    if daily.is_object() && daily["persistence_error"].is_null() {
      return Some(Self {
        used: daily["consumed_requests"].as_f64()?,
        reserved: 0.0,
        total: daily["free_allowance_requests"].as_f64()?,
        remaining: daily["remaining_requests"].as_f64()?,
        credit: false,
      });
    }
    None
  }

  fn amount(&self, value: f64) -> String {
    if self.credit { format!("${:.4}", value / 1_000_000.0) } else { number(value) }
  }

  fn searches_left(&self, provider: &Value) -> Option<f64> {
    if self.credit {
      let cost = provider["reference_cost_usd_micros"].as_f64().filter(|cost| *cost > 0.0)?;
      Some((self.remaining / cost).floor())
    } else {
      Some(self.remaining)
    }
  }
}

fn number(value: f64) -> String {
  format!("{value:.0}")
}

fn enabled(provider: &Value) -> bool {
  provider["enabled"].as_bool() == Some(true)
}

fn ready(provider: &Value) -> bool {
  provider["available"].as_bool() == Some(true)
    && provider["rate_limit"]["next_request_in_ms"].as_u64().unwrap_or(0) == 0
}

/// Convert monetary allocation units to reference-search equivalents, not
/// actual request counts: providers may charge different result counts.
fn routing_usage(provider: &Value) -> Option<(f64, f64)> {
  let allocation = &provider["allocation"];
  let divisor = match allocation["unit"].as_str()? {
    "requests" => 1.0,
    "usd_micros" => provider["reference_cost_usd_micros"].as_f64().filter(|cost| *cost > 0.0)?,
    _ => return None,
  };
  Some((allocation["consumed_units"].as_f64()? / divisor, allocation["daily_target_units"].as_f64()? / divisor))
}

/// Render an authorized local snapshot without contacting any provider.
pub fn page(status: StatusCode, snapshot: &Value) -> Response<Full<Bytes>> {
  let mut content = String::from("<div class=status-heading><h1>Status</h1><a href=/web/status>Refresh</a></div>");
  if let Some(error) = snapshot["error"].as_str() {
    let _ = write!(content, "<p class=notice role=alert>{}</p>", escape(error));
  }
  if let Some(errors) = snapshot["configuration_errors"].as_array() {
    for error in errors.iter().filter_map(Value::as_str) {
      let _ = write!(content, "<p class=notice role=alert>{}</p>", escape(error));
    }
  }
  let now = snapshot["generated_at"].as_i64().unwrap_or_else(|| jiff::Timestamp::now().as_second());
  if let Some(providers) = snapshot["providers"].as_array() {
    overview(&mut content, providers);
    let results = snapshot["reference_request"]["num_results"].as_u64().unwrap_or(5);
    let _ = write!(
      content,
      "<p class=status-note>Search estimates use a {results}-result request. Allowance windows differ by provider; this is not a combined daily quota. Unknown quotas are excluded from the remaining estimate.</p>"
    );
    provider_table(&mut content, "General web providers", providers, now, false);
  }
  if let Some(providers) = snapshot["probe_providers"].as_array() {
    provider_table(&mut content, "Fallback sources", providers, now, true);
  }
  let plugin = &snapshot["plugin"];
  if plugin["configured"].as_bool() == Some(true) {
    content.push_str("<section class=status-section><h2>Search plugin</h2>");
    let state = match plugin["state"].as_str() {
      Some("ready") => {
        let count = plugin["providers"].as_array().map_or(0, Vec::len);
        format!("Ready ({count} provider{})", if count == 1 { "" } else { "s" })
      }
      _ => "Discovery failed; restart after repairing the plugin".to_owned(),
    };
    let _ = write!(content, "<p>Discovery: <strong>{state}</strong></p>");
    if let Some(error) = plugin["error"].as_str() {
      let _ = write!(content, "<p class=notice role=alert>{}</p>", escape(error));
    }
    if let (Some(running), Some(max)) = (plugin["running_processes"].as_u64(), plugin["max_processes"].as_u64()) {
      let _ = write!(content, "<p>Processes running: <strong>{running} of {max}</strong></p>");
    }
    details(&mut content, "Plugin details", plugin);
    content.push_str("</section>");
  }
  let fetch = &snapshot["fetch"];
  if !fetch.is_null() {
    content.push_str("<section class=status-section><h2>Fetch</h2>");
    if let Some(available) = fetch["browser"]["executable_available"].as_bool() {
      let _ = write!(
        content,
        "<p>Browser executable: <strong>{}</strong></p>",
        if available { "Available" } else { "Unavailable" }
      );
    }
    if let Some(service) = fetch["pdf"]["service"].as_str() {
      let _ = write!(content, "<p>PDF service: <strong>{}</strong></p>", escape(&service.replace('_', " ")));
    }
    let relay = &fetch["browser_relay"];
    if relay["enabled"].as_bool() == Some(true) {
      let state = match relay["agent"].as_str() {
        Some(agent) if relay["connected"].as_bool() == Some(true) => format!("Connected ({})", escape(agent)),
        _ => "No browser connected".to_owned(),
      };
      let _ = write!(content, "<p>Browser relay: <strong>{state}</strong></p>");
    }
    details(&mut content, "Fetch details", fetch);
    content.push_str("</section>");
  }
  if snapshot["providers"].is_array() {
    content.push_str("<details class=status-method><summary>How to read these estimates</summary><p>Credit and daily allowance figures are local estimates, not guaranteed upstream balances. Other clients may use the same account. A soft daily target balances traffic; reaching it does not disable a provider. Daily estimates use rolling 24-hour usage; credit estimates use each provider's current account period.</p><p>24-hour usage includes admitted attempts, including retries and uncertain failures. Monetary usage is converted to search equivalents at the reference request cost. Minute buckets can retain up to one extra minute of usage. Minute pacing counts belong to the current server process. Disabled providers and fallback sources are excluded from general-web totals.</p><p>Refresh reads local state without issuing searches or fetching pages. Retry eligibility is not a confirmed quota reset.</p></details><p class=status-note><a href=/status>View JSON</a></p>");
  }
  response(status, HTML, document("Status — IMWebSearch", "status", &content))
}

fn overview(content: &mut String, providers: &[Value]) {
  let active: Vec<_> = providers.iter().filter(|provider| enabled(provider)).collect();
  let ready_count = active.iter().filter(|provider| ready(provider)).count();
  let mut left = 0.0;
  let mut ready_left = 0.0;
  let mut tracked = 0;
  let mut daily_searches_left = 0.0;
  let mut daily_allowances = 0;
  let mut credit_searches_left = 0.0;
  let mut credit_allowances = 0;
  let mut credit_left = 0.0;
  let mut credit_total = 0.0;
  let mut credit_used = 0.0;
  let mut credit_reserved = 0.0;
  let mut credit_providers = 0;
  let mut usage = 0.0;
  let mut target = 0.0;
  let mut usage_providers = 0;
  for provider in &active {
    if let Some(allowance) = Allowance::read(provider) {
      if let Some(remaining) = allowance.searches_left(provider) {
        tracked += 1;
        left += remaining;
        if allowance.credit {
          credit_allowances += 1;
          credit_searches_left += remaining;
        } else {
          daily_allowances += 1;
          daily_searches_left += remaining;
        }
        if ready(provider) {
          ready_left += remaining;
        }
      }
      if allowance.credit {
        credit_providers += 1;
        credit_left += allowance.remaining;
        credit_total += allowance.total;
        credit_used += allowance.used;
        credit_reserved += allowance.reserved;
      }
    }
    if let Some((used, daily)) = routing_usage(provider) {
      usage += used;
      target += daily;
      usage_providers += 1;
    }
  }
  content.push_str("<div class=status-overview>");
  metric(
    content,
    "General web providers ready now",
    &format!("{ready_count} / {}", active.len()),
    "Enabled providers; excludes current cooldowns and pacing waits.",
  );
  let (estimate_label, timeframe_note) = match (daily_allowances > 0, credit_allowances > 0) {
    (true, false) => ("Estimated searches left (rolling 24h)", String::new()),
    (false, true) => ("Estimated searches left (current credit periods)", "Reset dates vary by provider. ".to_owned()),
    (true, true) => (
      "Estimated searches left (mixed periods)",
      format!(
        "{} in rolling 24h allowances + {} in current credit periods; reset dates vary by provider. ",
        number(daily_searches_left),
        number(credit_searches_left)
      ),
    ),
    (false, false) => ("Estimated searches left", "Allowance timeframes unknown. ".to_owned()),
  };
  metric(
    content,
    estimate_label,
    &if tracked == 0 { "Unknown".to_owned() } else { format!("≈ {}", number(left)) },
    &format!(
      "{timeframe_note}{} from ready providers · {tracked} tracked allowances · {} unknown or unreadable",
      number(ready_left),
      active.len() - tracked
    ),
  );
  metric(
    content,
    "24h usage / daily target",
    &if usage_providers == 0 { "Unknown".to_owned() } else { format!("≈ {} / {}", number(usage), number(target)) },
    &format!("Search equivalents · soft target, not a quota · {usage_providers}/{} providers measured", active.len()),
  );
  if credit_providers > 0 {
    metric(
      content,
      "Estimated free credit left",
      &format!("${:.2}", credit_left / 1_000_000.0),
      &format!(
        "Current credit periods · of ${:.2} · ${:.2} recorded spend · ${:.2} reserved",
        credit_total / 1_000_000.0,
        credit_used / 1_000_000.0,
        credit_reserved / 1_000_000.0
      ),
    );
  }
  content.push_str("</div>");
}

fn metric(content: &mut String, label: &str, value: &str, note: &str) {
  let _ = write!(
    content,
    "<div class=status-metric><div>{}</div><strong>{}</strong><p>{}</p></div>",
    escape(label),
    escape(value),
    escape(note)
  );
}

fn provider_table(content: &mut String, heading: &str, providers: &[Value], now: i64, fallback: bool) {
  let active: Vec<_> = providers.iter().filter(|provider| enabled(provider)).collect();
  let _ = write!(content, "<section class=status-section><h2>{}</h2>", escape(heading));
  if fallback {
    let count = active.iter().filter(|provider| ready(provider)).count();
    let _ = write!(
      content,
      "<p class=status-note>{count} / {} ready. Used when general web search returns no results; excluded from the totals above.</p>",
      active.len()
    );
  }
  if active.is_empty() {
    content.push_str("<p class=status-note>No providers enabled.</p>");
  } else {
    content.push_str("<div class=status-table-wrap><table class=status-table><thead><tr><th scope=col>Provider / availability</th><th scope=col>Allowance / remaining</th><th scope=col>Last 24 hours</th><th scope=col>Pacing / renewal</th></tr></thead><tbody>");
    for provider in active {
      let (state, class) = if ready(provider) {
        ("Ready", "ready")
      } else if provider["available"].as_bool() == Some(true) {
        ("Paced", "waiting")
      } else {
        ("Unavailable", "blocked")
      };
      let _ = write!(
        content,
        "<tr><th scope=row><span class=provider-name>{}</span><span class=\"provider-state {class}\">{state}</span>",
        escape(provider["name"].as_str().unwrap_or("Unknown"))
      );
      if let Some(tier) = provider["tier"].as_u64() {
        let _ = write!(content, "<p class=status-note>Tier {tier}</p>");
      }
      if let Some(reasons) = provider["unavailable_reasons"].as_array() {
        for reason in reasons.iter().filter_map(Value::as_str) {
          let _ = write!(content, "<p class=status-reason>{}</p>", escape(reason));
        }
      }
      if provider["forced_next"].as_bool() == Some(true) {
        content.push_str("<p class=status-note><strong>Tried first for the next search</strong></p>");
      }
      rejection(content, &provider["health"]["last_rejection"], now);
      actions(content, provider, fallback);
      details(content, "All details", provider);
      content.push_str("</th><td>");
      allowance_cell(content, provider, now);
      content.push_str("</td><td>");
      if let Some((used, target)) = routing_usage(provider) {
        let label = if provider["allocation"]["unit"] == "requests" { "attempts" } else { "search equivalents" };
        let _ = write!(content, "<strong>≈ {} {label}</strong>", number(used));
        if !fallback {
          let _ = write!(content, "<p class=status-note>Daily target: ≈ {} (soft)</p>", number(target));
          meter(content, "Daily routing target used", used, target);
        }
      } else {
        content.push_str("<span class=status-note>Usage unavailable</span>");
      }
      content.push_str("</td><td>");
      timing_cell(content, provider, now);
      content.push_str("</td></tr>");
    }
    content.push_str("</tbody></table></div>");
  }
  let disabled: Vec<_> = providers.iter().filter(|provider| !enabled(provider)).collect();
  if !disabled.is_empty() {
    let _ = write!(
      content,
      "<details class=disabled-providers><summary>Disabled providers ({})</summary><ul>",
      disabled.len()
    );
    for provider in disabled {
      let _ = write!(content, "<li><strong>{}</strong>", escape(provider["name"].as_str().unwrap_or("Unknown")));
      if let Some(reasons) = provider["unavailable_reasons"].as_array() {
        for reason in reasons.iter().filter_map(Value::as_str) {
          let _ = write!(content, " — {}", escape(reason));
        }
      }
      content.push_str("</li>");
    }
    content.push_str("</ul></details>");
  }
  content.push_str("</section>");
}

fn actions(content: &mut String, provider: &Value, fallback: bool) {
  let name = escape(provider["name"].as_str().unwrap_or_default());
  content.push_str("<div class=status-actions>");
  let _ = write!(
    content,
    "<form method=post action=\"/web/status/clear-cooldown?provider={name}\"><button>Clear cooldown</button></form>"
  );
  if !fallback {
    let _ = write!(
      content,
      "<form method=post action=\"/web/status/force-next?provider={name}\"><button title=\"Clears any cooldown, then tries this provider first for the next search.\">Use next</button></form>"
    );
  }
  content.push_str("</div>");
}

fn rejection(content: &mut String, rejection: &Value, now: i64) {
  let Some(kind) = rejection["kind"].as_str() else { return };
  let _ = write!(content, "<div class=last-rejection><p>Last rejection: {}</p>", escape(&kind.replace('_', " ")));
  if let Some(code) = rejection["code"].as_str() {
    let _ = write!(content, "<p>Code: <code>{}</code></p>", escape(code));
  }
  if let Some(status) = rejection["http_status"].as_u64() {
    let _ = write!(content, "<p>HTTP {status}</p>");
  }
  if let Some(ms) = rejection["retry_after_ms"].as_u64() {
    let _ = write!(content, "<p>Retry hint: {}</p>", duration(ms.div_ceil(1000)));
  }
  if let Some(observed) = rejection["observed_at"].as_i64() {
    let _ =
      write!(content, "<p>{} ago · since server startup</p>", duration(now.saturating_sub(observed).max(0) as u64));
  }
  content.push_str("</div>");
}

fn allowance_cell(content: &mut String, provider: &Value, now: i64) {
  if let Some(allowance) = Allowance::read(provider) {
    let suffix = if allowance.credit { "free credit left" } else { "requests left" };
    let _ = write!(content, "<strong>≈ {} {suffix}</strong>", allowance.amount(allowance.remaining));
    let _ = write!(
      content,
      "<p class=status-note>{} used / {} {}</p>",
      allowance.amount(allowance.used),
      allowance.amount(allowance.total),
      if allowance.credit { "allowance · current period" } else { "daily estimate · rolling 24h" }
    );
    if allowance.reserved > 0.0 {
      let _ = write!(
        content,
        "<p class=status-note>{} reserved by in-flight requests</p>",
        allowance.amount(allowance.reserved)
      );
    }
    meter(
      content,
      if allowance.credit { "Recorded spend and reservations" } else { "Daily allowance estimate used" },
      allowance.used + allowance.reserved,
      allowance.total,
    );
    if let Some(remaining) = allowance.searches_left(provider).filter(|_| allowance.credit) {
      let _ = write!(content, "<p class=status-note>≈ {} searches at reference cost</p>", number(remaining));
    }
    if allowance.credit && provider["budget"]["adjusted_remaining_cap_usd_micros"].is_number() {
      content.push_str("<p class=status-note>Remaining estimate also reflects the reported upstream cap.</p>");
    }
    if !allowance.credit && allowance.used >= allowance.total {
      content.push_str("<p class=status-note>Daily estimate reached; provider may still accept requests.</p>");
    }
  } else if provider["budget"].is_object() || provider["daily_quota"].is_object() {
    content.push_str(
      "<strong>Estimate unavailable</strong><p class=status-note>Allowance accounting could not be read reliably.</p>",
    );
  } else {
    content.push_str("<strong>Remaining quota unknown</strong><p class=status-note>No daily allowance or credit balance is tracked.</p>");
  }
  let report = &provider["usage"]["last_report"];
  if let Some(remaining) = report["remaining_usd_micros"].as_f64() {
    let _ = write!(content, "<p class=status-note>Last upstream balance: ${:.4}", remaining / 1_000_000.0);
    if let Some(observed) = report["observed_at"].as_i64() {
      let _ = write!(content, " · {} ago", duration(now.saturating_sub(observed).max(0) as u64));
    }
    content.push_str("</p>");
  }
  if let Some(error) = provider["usage"]["error"].as_str() {
    let _ = write!(content, "<p class=status-note>Upstream refresh: {}</p>", escape(error));
  }
}

fn meter(content: &mut String, label: &str, used: f64, total: f64) {
  if total > 0.0 {
    let ratio = used / total;
    let _ = write!(
      content,
      "<div class=quota-meter><progress max=1 value=\"{:.6}\" aria-label=\"{}\"></progress><span>{:.0}%</span></div>",
      ratio.clamp(0.0, 1.0),
      escape(label),
      ratio * 100.0
    );
  }
}

fn timing_cell(content: &mut String, provider: &Value, now: i64) {
  let rate = &provider["rate_limit"];
  if let Some(limit) = rate["requests_per_minute"].as_u64() {
    if let Some(used) = rate["requests_in_last_minute"].as_u64() {
      let _ = write!(content, "<strong>{used} / {limit}</strong><p class=status-note>attempts · rolling minute</p>");
      meter(content, "Rolling minute request limit used", used as f64, limit as f64);
    } else {
      let _ = write!(content, "<strong>{limit} attempts/minute</strong>");
    }
  }
  let pacing = rate["next_request_in_ms"].as_u64().unwrap_or(0).div_ceil(1000);
  if pacing > 0 {
    let _ = write!(content, "<p class=status-note>Next rate slot in {}</p>", duration(pacing));
  }
  let health = &provider["health"];
  let cooldown = health["cooldown_remaining_ms"].as_u64().unwrap_or(0).div_ceil(1000);
  let retry = cooldown.max(future_seconds(health["quota_resets_at"].as_i64(), now)).max(future_seconds(
    provider["daily_quota"]["next_request_at_ms"]
      .as_i64()
      .map(|ms| ms.div_euclid(1000).saturating_add(i64::from(ms % 1000 != 0))),
    now,
  ));
  if retry > 0 {
    let _ = write!(content, "<p class=status-note>Retry eligible in {}</p>", duration(retry));
  } else if provider["available"].as_bool() != Some(true) {
    content.push_str("<p class=status-note>Retry time unknown</p>");
  }
  if health["recovery_probe_required"].as_bool() == Some(true) {
    content.push_str("<p class=status-note>Recovery probe required</p>");
  }
  if provider["budget"].is_object() {
    match provider["budget"]["period_end"].as_i64() {
      Some(end) if end > now => {
        let _ = write!(content, "<p class=status-note>Period ends in {}</p>", duration((end - now) as u64));
      }
      Some(_) => content.push_str("<p class=status-note>Period ended; renewal unconfirmed</p>"),
      None => content.push_str("<p class=status-note>Reset time unknown</p>"),
    }
  } else if provider["daily_quota"].is_object() {
    content.push_str("<p class=status-note>Rolling 24h; no fixed reset</p>");
  } else {
    content.push_str("<p class=status-note>Upstream quota reset unknown</p>");
  }
}

fn future_seconds(until: Option<i64>, now: i64) -> u64 {
  until.map_or(0, |until| until.saturating_sub(now).max(0) as u64)
}

fn duration(seconds: u64) -> String {
  if seconds < 60 {
    format!("{seconds}s")
  } else if seconds < 3600 {
    format!("{}m {}s", seconds / 60, seconds % 60)
  } else if seconds < 86400 {
    format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
  } else {
    format!("{}d {}h", seconds / 86400, seconds % 86400 / 3600)
  }
}

fn details(content: &mut String, label: &str, value: &Value) {
  if let Ok(json) = serde_json::to_string_pretty(value) {
    let _ = write!(
      content,
      "<details class=status-details><summary>{}</summary><pre>{}</pre></details>",
      escape(label),
      escape(&json)
    );
  }
}
