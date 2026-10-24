// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::OwnedMutexGuard;
use tokio::time::Instant;

use crate::providers::{ProviderError, ProviderErrorKind, ProviderResult};

const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_LEDGER_BYTES: u64 = 256 * 1024;
const MAX_REQUESTS: usize = 10_000;

/// Persistent free request accounting, independent of provider selection and
/// monetary budgets. Requests use a conservative rolling 24-hour window.
/// Share this handle when the same provider is used in several pools.
pub struct RequestQuota {
  directory: PathBuf,
  ledger_path: PathBuf,
  temporary_prefix: String,
  allowance: Option<u32>,
  _lock: File,
  state: Mutex<State>,
}

struct State {
  ledger: Ledger,
  failed: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
  version: u32,
  last_observed_at_ms: i64,
  requests_at_ms: Vec<i64>,
  remote_exhausted_until_ms: Option<i64>,
}

#[derive(Serialize)]
pub struct RequestQuotaSnapshot {
  pub window_seconds: u64,
  pub free_allowance_requests: u32,
  pub consumed_requests: u32,
  pub remaining_requests: u32,
  /// Earliest possible eligibility, in Unix milliseconds; not a remote reset.
  pub next_request_at_ms: Option<i64>,
  pub remote_exhausted_until_ms: Option<i64>,
  pub persistence_error: Option<&'static str>,
  #[serde(skip)]
  pub recovery_probe_required: bool,
}

impl RequestQuotaSnapshot {
  pub fn available(&self) -> bool {
    self.persistence_error.is_none()
      && self.consumed_requests < MAX_REQUESTS as u32
      && self.remote_exhausted_until_ms.is_none()
  }
}

impl RequestQuota {
  /// The adapter supplies its fixed allowance. Reopening the stable provider
  /// identifier preserves charges and cooldowns when provider code changes.
  pub fn open(directory: &Path, provider: &str, allowance: u32) -> ProviderResult<Arc<Self>> {
    Self::open_inner(directory, provider, Some(allowance), "quota")
  }

  /// All adapters share this persistent cooldown, including keyless providers
  /// without a daily request allowance. It uses a separate ledger and lock.
  pub fn cooldown(directory: &Path, provider: &str) -> ProviderResult<Arc<Self>> {
    Self::open_inner(directory, provider, None, "cooldown")
  }

  fn open_inner(directory: &Path, provider: &str, allowance: Option<u32>, suffix: &str) -> ProviderResult<Arc<Self>> {
    if provider.is_empty()
      || provider.len() > 64
      || !provider.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_'))
    {
      return Err(internal("Request quota provider must be a filename-safe lowercase identifier of 1 to 64 bytes."));
    }
    if allowance.is_some_and(|allowance| allowance == 0 || allowance > MAX_REQUESTS as u32) {
      return Err(internal("Free request allowance must be between 1 and 10000."));
    }
    let ledger_path = directory.join(format!("{provider}-{suffix}.json"));
    let temporary_prefix = format!("{provider}-{suffix}-");
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::DirBuilderExt;
      builder.mode(0o700);
    }
    builder.create(directory).map_err(|_| internal("Could not create request quota directory."))?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(0o600);
    }
    let lock = options
      .open(directory.join(format!("{provider}-{suffix}.lock")))
      .map_err(|_| internal("Could not open request quota lock."))?;
    lock.try_lock().map_err(|_| internal("Provider request quota is already in use or cannot be locked."))?;
    let ledger = match File::open(&ledger_path) {
      Ok(file) => {
        let mut bytes = Vec::new();
        file
          .take(MAX_LEDGER_BYTES + 1)
          .read_to_end(&mut bytes)
          .map_err(|_| internal("Could not read request quota ledger."))?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
          return Err(internal("Request quota ledger exceeds the size limit."));
        }
        let ledger: Ledger = serde_json::from_slice(&bytes)
          .map_err(|_| internal("Request quota ledger is invalid; refusing to reset usage."))?;
        if ledger.version != 1
          || ledger.requests_at_ms.len() > MAX_REQUESTS
          || ledger.requests_at_ms.windows(2).any(|pair| pair[0] > pair[1])
          || ledger.requests_at_ms.iter().any(|&time| time < 0 || time > ledger.last_observed_at_ms)
          || ledger.remote_exhausted_until_ms.is_some_and(|until| until < 0)
        {
          return Err(internal("Request quota ledger is invalid; refusing to reset usage."));
        }
        ledger
      }
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ledger {
        version: 1,
        last_observed_at_ms: Timestamp::now().as_millisecond(),
        requests_at_ms: Vec::new(),
        remote_exhausted_until_ms: None,
      },
      Err(_) => return Err(internal("Could not open request quota ledger.")),
    };
    check_clock(&ledger)?;
    persist(directory, &ledger_path, &temporary_prefix, &ledger)?;
    Ok(Arc::new(Self {
      directory: directory.to_owned(),
      ledger_path,
      temporary_prefix,
      allowance,
      _lock: lock,
      state: Mutex::new(State { ledger, failed: false }),
    }))
  }

  pub async fn snapshot(self: &Arc<Self>) -> ProviderResult<RequestQuotaSnapshot> {
    let quota = Arc::clone(self);
    worker(move || {
      let state = quota.state.lock().map_err(|_| internal("Request quota lock is poisoned."))?;
      let now = check_clock(&state.ledger)?;
      let active: Vec<i64> =
        state.ledger.requests_at_ms.iter().copied().filter(|&time| time > now - WINDOW_MS).collect();
      let consumed_requests = active.len() as u32;
      let allowance = quota.allowance.unwrap_or(1);
      let remaining_requests = allowance.saturating_sub(consumed_requests);
      let remote_exhausted_until_ms = state.ledger.remote_exhausted_until_ms.filter(|&until| until > now);
      // The provider's allowance is an estimate, not a local cap. Only the
      // storage bound can require waiting for a recorded request to expire.
      let local_expiry = if active.len() >= MAX_REQUESTS { active.first().map(|&time| time + WINDOW_MS) } else { None };
      Ok(RequestQuotaSnapshot {
        window_seconds: (WINDOW_MS / 1000) as u64,
        free_allowance_requests: allowance,
        consumed_requests,
        remaining_requests,
        next_request_at_ms: local_expiry.into_iter().chain(remote_exhausted_until_ms).max(),
        remote_exhausted_until_ms,
        persistence_error: state.failed.then_some("Request quota persistence failed; restart after repairing storage."),
        recovery_probe_required: state.ledger.remote_exhausted_until_ms.is_some(),
      })
    })
    .await
  }

  /// Charge before tool dispatch. Late writes, crashes and cancellation retain
  /// the charge conservatively; dropping a request never blocks on disk I/O.
  pub async fn charge(self: &Arc<Self>, deadline: Instant) -> ProviderResult<()> {
    let quota = Arc::clone(self);
    worker(move || {
      check_deadline(deadline)?;
      quota.update(|ledger, now| {
        check_deadline(deadline)?;
        ledger.requests_at_ms.retain(|&time| time > now - WINDOW_MS);
        if ledger.requests_at_ms.len() >= MAX_REQUESTS
          || ledger.remote_exhausted_until_ms.is_some_and(|until| until > now)
        {
          return Err(ProviderError::new(ProviderErrorKind::InsufficientBudget, "Free request quota is unavailable."));
        }
        if quota.allowance.is_some() {
          ledger.requests_at_ms.push(now);
        }
        Ok(())
      })
    })
    .await?;
    check_deadline(deadline)
  }

  /// Admit exactly one recovery request after a cooldown. Reserve its next
  /// retry durably before dispatch, so cancellation or restart cannot release
  /// a burst of probes. Normal successful traffic needs no disk writes here.
  pub async fn admit(self: &Arc<Self>, deadline: Instant) -> ProviderResult<Option<i64>> {
    let quota = Arc::clone(self);
    worker(move || {
      check_deadline(deadline)?;
      let state = quota.state.lock().map_err(|_| internal("Request quota lock is poisoned."))?;
      if state.failed {
        return Err(internal("Request quota persistence failed; restart after repairing storage."));
      }
      check_clock(&state.ledger)?;
      let retry_at = state.ledger.remote_exhausted_until_ms;
      drop(state);
      if retry_at.is_none() {
        return Ok(None);
      }
      quota.update(|ledger, now| {
        check_deadline(deadline)?;
        if ledger.remote_exhausted_until_ms.is_some_and(|until| until > now) {
          return Err(ProviderError::new(ProviderErrorKind::InsufficientBudget, "Provider quota is cooling down."));
        }
        // A concurrent successful probe may already have restored normal use.
        if ledger.remote_exhausted_until_ms.is_none() {
          return Ok(None);
        }
        let until = now.checked_add(WINDOW_MS).ok_or_else(|| internal("Request quota timestamp overflowed."))?;
        ledger.remote_exhausted_until_ms = Some(until);
        Ok(Some(until))
      })
    })
    .await
  }

  /// Queue before yielding, retaining the admission gate through persistence
  /// even if the caller is cancelled. A recovery success only clears its lease.
  pub async fn finish_attempt(
    self: &Arc<Self>,
    lease: Option<i64>,
    delay: Option<Duration>,
    success: bool,
    guard: OwnedMutexGuard<()>,
  ) -> ProviderResult<()> {
    let quota = Arc::clone(self);
    worker(move || {
      let _guard = guard;
      if delay.is_none() && (!success || lease.is_none()) {
        return Ok(());
      }
      quota.update(|ledger, now| {
        if let Some(delay) = delay {
          // A known response replaces this probe's provisional lease. For
          // ordinary throttling its explicit retry hint may be shorter.
          if lease.is_some() && ledger.remote_exhausted_until_ms == lease {
            ledger.remote_exhausted_until_ms = None;
          }
          suspend_until(ledger, now, delay)?;
        } else if ledger.remote_exhausted_until_ms == lease {
          ledger.remote_exhausted_until_ms = None;
        }
        Ok(())
      })
    })
    .await
  }

  /// Operator override: forget a remote cooldown and any recovery lease.
  pub async fn clear_cooldown(self: &Arc<Self>) -> ProviderResult<()> {
    let quota = Arc::clone(self);
    worker(move || {
      quota.update(|ledger, _| {
        ledger.remote_exhausted_until_ms = None;
        Ok(())
      })
    })
    .await
  }

  /// Persist the caller's explicit retry hint or its missing-hint fallback.
  pub async fn exhaust(self: &Arc<Self>, delay: Duration) -> ProviderResult<Duration> {
    self.suspend(delay).await
  }

  async fn suspend(self: &Arc<Self>, delay: Duration) -> ProviderResult<Duration> {
    let quota = Arc::clone(self);
    worker(move || quota.update(|ledger, now| suspend_until(ledger, now, delay))).await
  }

  fn update<T>(&self, update: impl FnOnce(&mut Ledger, i64) -> ProviderResult<T>) -> ProviderResult<T> {
    let mut state = self.state.lock().map_err(|_| internal("Request quota lock is poisoned."))?;
    if state.failed {
      return Err(internal("Request quota persistence failed; restart after repairing storage."));
    }
    let now = check_clock(&state.ledger)?;
    let mut next = state.ledger.clone();
    let result = update(&mut next, now)?;
    next.last_observed_at_ms = now;
    if let Err(error) = persist(&self.directory, &self.ledger_path, &self.temporary_prefix, &next) {
      state.failed = true;
      return Err(error);
    }
    state.ledger = next;
    Ok(result)
  }
}

fn suspend_until(ledger: &mut Ledger, now: i64, delay: Duration) -> ProviderResult<Duration> {
  let delay = i64::try_from(delay.as_nanos().div_ceil(1_000_000))
    .map_err(|_| internal("Request quota delay exceeds the supported range."))?;
  let until = now.checked_add(delay).ok_or_else(|| internal("Request quota timestamp overflowed."))?;
  let until = match ledger.remote_exhausted_until_ms {
    Some(old) => until.max(old.checked_add(1).ok_or_else(|| internal("Request quota timestamp overflowed."))?),
    None => until,
  };
  ledger.remote_exhausted_until_ms = Some(until);
  Ok(Duration::from_millis((until - now) as u64))
}

fn check_clock(ledger: &Ledger) -> ProviderResult<i64> {
  let now = Timestamp::now().as_millisecond();
  if ledger.last_observed_at_ms < 0 || now < ledger.last_observed_at_ms {
    return Err(internal("Clock precedes request quota history; refusing to reset usage."));
  }
  Ok(now)
}

fn persist(directory: &Path, ledger_path: &Path, temporary_prefix: &str, ledger: &Ledger) -> ProviderResult<()> {
  let bytes = serde_json::to_vec(ledger).map_err(|_| internal("Could not serialize request quota ledger."))?;
  if bytes.len() as u64 > MAX_LEDGER_BYTES {
    return Err(internal("Request quota ledger exceeds the size limit."));
  }
  let mut temporary = tempfile::Builder::new()
    .prefix(temporary_prefix)
    .tempfile_in(directory)
    .map_err(|_| internal("Could not create temporary request quota ledger."))?;
  temporary
    .write_all(&bytes)
    .and_then(|_| temporary.as_file().sync_all())
    .map_err(|_| internal("Could not durably write request quota ledger."))?;
  temporary.persist(ledger_path).map_err(|_| internal("Could not replace request quota ledger."))?;
  #[cfg(unix)]
  File::open(directory)
    .and_then(|file| file.sync_all())
    .map_err(|_| internal("Could not sync request quota directory."))?;
  Ok(())
}

async fn worker<T: Send + 'static>(
  operation: impl FnOnce() -> ProviderResult<T> + Send + 'static,
) -> ProviderResult<T> {
  tokio::task::spawn_blocking(operation).await.map_err(|_| internal("Request quota worker failed."))?
}

fn check_deadline(deadline: Instant) -> ProviderResult<()> {
  if Instant::now() >= deadline {
    return Err(ProviderError::new(ProviderErrorKind::Transient, "Request quota admission exceeded its deadline."));
  }
  Ok(())
}

fn internal(message: &str) -> ProviderError {
  ProviderError::new(ProviderErrorKind::Internal, message)
}
