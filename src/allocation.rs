// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Soft routing history, independent of provider budgets and remote cooldowns.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio::time::Instant;

use crate::providers::{ProviderError, ProviderErrorKind, ProviderResult};

const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
const BUCKET_MS: i64 = 60 * 1000;
const MAX_LEDGER_BYTES: u64 = 8 * 1024 * 1024;

pub struct AllocationStore {
  directory: PathBuf,
  _lock: File,
  state: Mutex<State>,
  /// Hold through selection, admission and durable recording, never provider HTTP.
  pub selection: Arc<AsyncMutex<()>>,
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
  sequence: u64,
  providers: BTreeMap<String, ProviderUsage>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProviderUsage {
  last_selected_at_ms: Option<i64>,
  last_sequence: Option<u64>,
  buckets: VecDeque<Bucket>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Bucket {
  start_ms: i64,
  units: u64,
}

#[derive(Default)]
pub struct AllocationUsage {
  pub consumed_units: u64,
  pub last_selected_at_ms: Option<i64>,
  pub last_sequence: Option<u64>,
}

impl AllocationStore {
  pub fn open(directory: &Path) -> ProviderResult<Arc<Self>> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::DirBuilderExt;
      builder.mode(0o700);
    }
    builder.create(directory).map_err(|_| internal("Could not create routing allocation directory."))?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(0o600);
    }
    let lock = options
      .open(directory.join("allocation.lock"))
      .map_err(|_| internal("Could not open routing allocation lock."))?;
    lock.try_lock().map_err(|_| internal("Routing allocation history is already in use or cannot be locked."))?;
    let ledger = match File::open(directory.join("allocation.json")) {
      Ok(file) => {
        let mut bytes = Vec::new();
        file
          .take(MAX_LEDGER_BYTES + 1)
          .read_to_end(&mut bytes)
          .map_err(|_| internal("Could not read routing allocation history."))?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
          return Err(internal("Routing allocation history exceeds its storage bound."));
        }
        let ledger: Ledger =
          serde_json::from_slice(&bytes).map_err(|_| internal("Routing allocation history is invalid."))?;
        if ledger.version != 1 || ledger.last_observed_at_ms < 0 {
          return Err(internal("Routing allocation history is invalid."));
        }
        for usage in ledger.providers.values() {
          if usage.buckets.len() > (WINDOW_MS / BUCKET_MS + 1) as usize
            || usage.last_sequence.is_some_and(|sequence| sequence == 0 || sequence > ledger.sequence)
            || usage.last_selected_at_ms.is_some() != usage.last_sequence.is_some()
            || usage.last_selected_at_ms.is_some_and(|time| time < 0 || time > ledger.last_observed_at_ms)
            || usage.buckets.iter().any(|bucket| {
              bucket.start_ms < 0 || bucket.start_ms % BUCKET_MS != 0 || bucket.start_ms > ledger.last_observed_at_ms
            })
            || usage.buckets.iter().zip(usage.buckets.iter().skip(1)).any(|(a, b)| a.start_ms >= b.start_ms)
          {
            return Err(internal("Routing allocation history is invalid."));
          }
        }
        ledger
      }
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ledger {
        version: 1,
        last_observed_at_ms: Timestamp::now().as_millisecond(),
        sequence: 0,
        providers: BTreeMap::new(),
      },
      Err(_) => return Err(internal("Could not open routing allocation history.")),
    };
    check_clock(&ledger)?;
    Ok(Arc::new(Self {
      directory: directory.to_owned(),
      _lock: lock,
      state: Mutex::new(State { ledger, failed: false }),
      selection: Arc::default(),
    }))
  }

  /// Inspect without pruning, advancing timestamps or writing to disk.
  pub async fn snapshot(self: &Arc<Self>) -> ProviderResult<BTreeMap<String, AllocationUsage>> {
    let store = Arc::clone(self);
    worker(move || {
      let state = store.state.lock().map_err(|_| internal("Routing allocation lock is poisoned."))?;
      if state.failed {
        return Err(internal("Routing allocation persistence failed; repair storage and restart."));
      }
      let now = check_clock(&state.ledger)?;
      state
        .ledger
        .providers
        .iter()
        .map(|(name, usage)| {
          let consumed_units = usage
            .buckets
            .iter()
            .filter(|bucket| bucket.start_ms > now - WINDOW_MS - BUCKET_MS)
            .try_fold(0u64, |sum, bucket| {
              sum.checked_add(bucket.units).ok_or_else(|| internal("Routing allocation usage overflowed."))
            })?;
          Ok((
            name.clone(),
            AllocationUsage {
              consumed_units,
              last_selected_at_ms: usage.last_selected_at_ms,
              last_sequence: usage.last_sequence,
            },
          ))
        })
        .collect()
    })
    .await
  }

  /// Record admitted work before provider dispatch. Keep the selection gate in
  /// the worker: cancellation cannot let a later caller observe stale history.
  /// Retain uncertain attempts; completion never refunds a routing allocation.
  pub async fn record(
    self: &Arc<Self>,
    provider: &'static str,
    units: u64,
    deadline: Instant,
    guard: OwnedMutexGuard<()>,
  ) -> ProviderResult<()> {
    let store = Arc::clone(self);
    worker(move || {
      let _guard = guard;
      if Instant::now() >= deadline {
        return Err(ProviderError::new(ProviderErrorKind::Transient, "Routing allocation deadline expired."));
      }
      let mut state = store.state.lock().map_err(|_| internal("Routing allocation lock is poisoned."))?;
      if state.failed {
        return Err(internal("Routing allocation persistence failed; repair storage and restart."));
      }
      let now = check_clock(&state.ledger)?;
      let mut ledger = state.ledger.clone();
      ledger.sequence =
        ledger.sequence.checked_add(1).ok_or_else(|| internal("Routing allocation sequence overflowed."))?;
      ledger.last_observed_at_ms = now;
      for usage in ledger.providers.values_mut() {
        usage.buckets.retain(|bucket| bucket.start_ms > now - WINDOW_MS - BUCKET_MS);
      }
      let usage = ledger.providers.entry(provider.to_owned()).or_default();
      let start_ms = now / BUCKET_MS * BUCKET_MS;
      if let Some(bucket) = usage.buckets.back_mut().filter(|bucket| bucket.start_ms == start_ms) {
        bucket.units =
          bucket.units.checked_add(units).ok_or_else(|| internal("Routing allocation usage overflowed."))?;
      } else {
        usage.buckets.push_back(Bucket { start_ms, units });
      }
      usage.last_sequence = Some(ledger.sequence);
      usage.last_selected_at_ms = Some(now);
      if let Err(error) = store.persist(&ledger) {
        state.failed = true;
        return Err(error);
      }
      state.ledger = ledger;
      Ok(())
    })
    .await
  }

  fn persist(&self, ledger: &Ledger) -> ProviderResult<()> {
    let bytes = serde_json::to_vec(ledger).map_err(|_| internal("Could not serialize routing allocation history."))?;
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
      return Err(internal("Routing allocation history exceeds its storage bound."));
    }
    let mut temporary = tempfile::Builder::new()
      .prefix("allocation-")
      .tempfile_in(&self.directory)
      .map_err(|_| internal("Could not create temporary routing allocation history."))?;
    temporary
      .write_all(&bytes)
      .and_then(|_| temporary.as_file().sync_all())
      .map_err(|_| internal("Could not durably write routing allocation history."))?;
    temporary
      .persist(self.directory.join("allocation.json"))
      .map_err(|_| internal("Could not replace routing allocation history."))?;
    #[cfg(unix)]
    File::open(&self.directory)
      .and_then(|file| file.sync_all())
      .map_err(|_| internal("Could not sync routing allocation directory."))?;
    Ok(())
  }
}

fn check_clock(ledger: &Ledger) -> ProviderResult<i64> {
  let now = Timestamp::now().as_millisecond();
  if now < ledger.last_observed_at_ms {
    return Err(internal("Clock moved backwards relative to routing allocation history."));
  }
  Ok(now)
}

async fn worker<T: Send + 'static>(
  operation: impl FnOnce() -> ProviderResult<T> + Send + 'static,
) -> ProviderResult<T> {
  tokio::task::spawn_blocking(operation).await.map_err(|_| internal("Routing allocation worker failed."))?
}

fn internal(message: &str) -> ProviderError {
  ProviderError::new(ProviderErrorKind::Internal, message)
}
