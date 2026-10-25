// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use jiff::{Span, Timestamp, civil::Date, tz::TimeZone};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::providers::reset::{BudgetResetPolicy, budget_policy};
use crate::providers::{
  ProviderError, ProviderErrorKind, ProviderResult, RequestCost, SearchProvider, SearchRequest, SearchResponse,
  UsageSnapshot,
};
use crate::request_quota::RequestQuotaSnapshot;

const LEDGER_VERSION: u32 = 3;
const MAX_LEDGER_BYTES: usize = 4 * 1024 * 1024;
const MIGRATION_BACKUP: &str = "budget.pre-v3.json";

/// One process owns a ledger directory. Within that process the mutex serializes
/// durable transactions; no mutex guard is held during a provider request.
pub struct BudgetStore {
  directory: PathBuf,
  _lock: File,
  state: Mutex<StoreState>,
}

struct StoreState {
  ledger: Ledger,
  failed: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
  version: u32,
  next_reservation_id: u64,
  accounts: BTreeMap<String, BTreeMap<String, AccountState>>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AccountState {
  reset_policy: BudgetResetPolicy,
  period_start: Option<i64>,
  period_end: Option<i64>,
  free_limit_usd_micros: u64,
  consumed_usd_micros: u64,
  /// Settlements after a reported period expired must also count against the
  /// next observed period, even if they finished before its refresh arrived.
  #[serde(default)]
  carry_consumed_usd_micros: u64,
  #[serde(default)]
  carry_exhausted: bool,
  reservations: BTreeMap<u64, u64>,
  reported_remaining_cap_usd_micros: Option<u64>,
  reported_allowance_cap_usd_micros: Option<u64>,
}

#[derive(Clone)]
pub struct BudgetAccount {
  store: Arc<BudgetStore>,
  provider: String,
  account_id: String,
}

#[derive(Serialize)]
pub struct BudgetBalance {
  pub free_allowance_usd_micros: u64,
  pub free_remaining_usd_micros: u64,
  pub reserved_usd_micros: u64,
  pub consumed_usd_micros: u64,
  pub period_start: Option<i64>,
  pub period_end: Option<i64>,
  /// Estimates guide selection; only unavailable account timing blocks it.
  #[serde(skip)]
  pub dispatch_allowed: bool,
}

#[derive(Serialize)]
pub struct BudgetSnapshot {
  #[serde(flatten)]
  pub balance: BudgetBalance,
  pub configured_free_allowance_usd_micros: u64,
  pub adjusted_remaining_cap_usd_micros: Option<u64>,
  pub reported_allowance_cap_usd_micros: Option<u64>,
  pub reset_day: Option<u8>,
  pub persistence_error: Option<&'static str>,
}

/// Dispatched reservations are conservatively charged on cancellation. Cleanup
/// runs in a blocking worker so dropping a timed-out request never waits on I/O.
struct Reservation {
  account: BudgetAccount,
  id: u64,
  dispatched: bool,
  finished: bool,
  recovery: bool,
  dispatch_guard: Option<OwnedMutexGuard<()>>,
}

impl BudgetStore {
  pub fn open(directory: &Path) -> ProviderResult<Arc<Self>> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::DirBuilderExt;
      builder.mode(0o700);
    }
    builder.create(directory).map_err(|_| internal("Could not create budget state directory."))?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(0o600);
    }
    let lock = options.open(directory.join("budget.lock")).map_err(|_| internal("Could not open budget lock file."))?;
    lock.try_lock().map_err(|_| internal("Budget state directory is already in use or cannot be locked."))?;

    // Migration and recovery must agree on the period even if reading or
    // validating the ledger crosses a calendar boundary.
    let now = Timestamp::now();
    let (mut ledger, migration_backup) = match File::open(directory.join("budget.json")) {
      Ok(file) => {
        let mut bytes = Vec::new();
        file
          .take((MAX_LEDGER_BYTES + 1) as u64)
          .read_to_end(&mut bytes)
          .map_err(|_| internal("Could not read budget ledger."))?;
        if bytes.len() > MAX_LEDGER_BYTES {
          return Err(internal("Budget ledger exceeds the size limit."));
        }
        let (ledger, migrated) = parse_ledger(&bytes, now)?;
        (ledger, migrated.then_some(bytes))
      }
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
        (Ledger { version: LEDGER_VERSION, next_reservation_id: 1, accounts: BTreeMap::new() }, None)
      }
      Err(_) => return Err(internal("Could not open budget ledger.")),
    };
    if ledger.version != LEDGER_VERSION || ledger.next_reservation_id == 0 {
      return Err(internal("Unsupported or invalid budget ledger version or reservation counter."));
    }
    for (provider, accounts) in &mut ledger.accounts {
      for account in accounts.values_mut() {
        if account.reset_policy != budget_policy(provider) {
          return Err(internal("Stored budget reset policy differs from the provider rule; migration is required."));
        }
        account.validate(ledger.next_reservation_id)?;
        account.advance(now)?;
        // The exclusive process lock proves no previous owner is still running.
        // Its unresolved requests may have been billed: keep their full estimates.
        let recovered = account.reserved()?;
        account.charge(recovered, now)?;
        account.reservations.clear();
      }
    }
    // Validate and recover the whole ledger before publishing any migration.
    // Retain the exact original before the first one-way replacement.
    if let Some(bytes) = &migration_backup {
      preserve_migration_backup(directory, bytes)?;
    }
    persist(directory, &ledger)?;
    if migration_backup.is_some() {
      info!("Migrated budget ledger to version {LEDGER_VERSION}; original retained in {MIGRATION_BACKUP}.");
    }
    Ok(Arc::new(Self {
      directory: directory.to_owned(),
      _lock: lock,
      state: Mutex::new(StoreState { ledger, failed: false }),
    }))
  }

  pub fn account(self: &Arc<Self>, provider: &str, free_cents: u64) -> ProviderResult<BudgetAccount> {
    let free_limit =
      free_cents.checked_mul(10_000).ok_or_else(|| internal("Free budget exceeds the supported range."))?;
    if provider.is_empty() {
      return Err(internal("Budget account settings are invalid."));
    }
    let now = Timestamp::now();
    let reset_policy = budget_policy(provider);
    let account_id = self.transaction(|ledger| {
      let accounts = ledger.accounts.entry(provider.to_owned()).or_default();
      // One account per provider. Reuse the saved entry, including a former
      // custom label, so removing configuration cannot reset usage or quota caps.
      if accounts.len() > 1 {
        return Err(internal(
          "Multiple saved accounts for a provider; automatic single-account selection is ambiguous.",
        ));
      }
      let account_id = accounts.keys().next().cloned().unwrap_or_else(|| "default".to_owned());
      if let Some(account) = accounts.get_mut(&account_id) {
        account.advance(now)?;
        account.free_limit_usd_micros = free_limit;
      } else {
        let (period_start, period_end) = match reset_policy {
          BudgetResetPolicy::CalendarMonthUtc => {
            let (start, end) = period_at(now, 1)?;
            (Some(start), Some(end))
          }
          BudgetResetPolicy::ReportedPeriod => (None, None),
          BudgetResetPolicy::Unknown => (Some(now.as_second()), None),
        };
        accounts.insert(
          account_id.clone(),
          AccountState {
            reset_policy,
            period_start,
            period_end,
            free_limit_usd_micros: free_limit,
            consumed_usd_micros: 0,
            carry_consumed_usd_micros: 0,
            carry_exhausted: false,
            reservations: BTreeMap::new(),
            reported_remaining_cap_usd_micros: None,
            reported_allowance_cap_usd_micros: None,
          },
        );
      }
      Ok(account_id)
    })?;
    Ok(BudgetAccount { store: Arc::clone(self), provider: provider.to_owned(), account_id })
  }

  fn transaction<T>(&self, update: impl FnOnce(&mut Ledger) -> ProviderResult<T>) -> ProviderResult<T> {
    let mut state = self.state.lock().map_err(|_| internal("Budget ledger lock is poisoned."))?;
    if state.failed {
      return Err(internal("Budget persistence previously failed; restart after repairing the state directory."));
    }
    let mut next = state.ledger.clone();
    let result = update(&mut next)?;
    if let Err(error) = persist(&self.directory, &next) {
      // A rename may have succeeded before a sync failed. Never dispatch another
      // metered request using memory that could disagree with durable state.
      state.failed = true;
      return Err(error);
    }
    state.ledger = next;
    Ok(result)
  }
}

impl AccountState {
  fn reserved(&self) -> ProviderResult<u64> {
    self.reservations.values().try_fold(0, |sum, &cost| checked_total(sum, cost))
  }

  fn validate(&self, next_id: u64) -> ProviderResult<()> {
    if self.reservations.keys().any(|&id| id == 0 || id >= next_id) {
      return Err(internal("Budget ledger contains an invalid reservation."));
    }
    for time in [self.period_start, self.period_end].into_iter().flatten() {
      Timestamp::from_second(time).map_err(|_| internal("Budget period is invalid."))?;
    }
    let valid = match (self.reset_policy, self.period_start, self.period_end) {
      (BudgetResetPolicy::CalendarMonthUtc, Some(start), Some(end)) => {
        period_at(Timestamp::from_second(start).map_err(|_| internal("Budget period is invalid."))?, 1)? == (start, end)
      }
      (BudgetResetPolicy::ReportedPeriod, None, None) => true,
      (BudgetResetPolicy::ReportedPeriod, Some(start), Some(end)) => start < end,
      (BudgetResetPolicy::Unknown, Some(_), None) => true,
      _ => false,
    };
    if !valid {
      return Err(internal("Budget ledger contains an invalid provider period."));
    }
    if self.carry_consumed_usd_micros > self.consumed_usd_micros
      || (self.reset_policy != BudgetResetPolicy::ReportedPeriod
        && (self.carry_consumed_usd_micros != 0 || self.carry_exhausted))
    {
      return Err(internal("Budget ledger contains invalid carried usage."));
    }
    checked_total(self.consumed_usd_micros, self.reserved()?)?;
    Ok(())
  }

  fn advance(&mut self, now: Timestamp) -> ProviderResult<()> {
    if self.period_start.is_some_and(|start| now.as_second() < start) {
      return Err(internal("Clock precedes the stored budget period; refusing to reset usage."));
    }
    if self.reset_policy == BudgetResetPolicy::CalendarMonthUtc
      && self.period_end.is_some_and(|end| now.as_second() >= end)
    {
      let (start, end) = period_at(now, 1)?;
      self.period_start = Some(start);
      self.period_end = Some(end);
      self.clear_consumption();
    }
    // Account-based cycles require an observed new period. Merely passing a
    // reported end, or restarting with unknown timing, cannot replenish credits.
    Ok(())
  }

  fn clear_consumption(&mut self) {
    self.consumed_usd_micros = std::mem::take(&mut self.carry_consumed_usd_micros);
    self.reported_remaining_cap_usd_micros = std::mem::take(&mut self.carry_exhausted).then_some(0);
    self.reported_allowance_cap_usd_micros = None;
    // Preserve reservations: in-flight requests may be billed in the new period.
  }

  fn charge(&mut self, cost: u64, now: Timestamp) -> ProviderResult<()> {
    self.consumed_usd_micros = checked_total(self.consumed_usd_micros, cost)?;
    if self.awaiting_reported_period(now) {
      self.carry_consumed_usd_micros = checked_total(self.carry_consumed_usd_micros, cost)?;
    }
    if let Some(remaining) = &mut self.reported_remaining_cap_usd_micros {
      *remaining = remaining.saturating_sub(cost);
    }
    Ok(())
  }

  fn awaiting_reported_period(&self, now: Timestamp) -> bool {
    self.reset_policy == BudgetResetPolicy::ReportedPeriod && self.period_end.is_some_and(|end| now.as_second() >= end)
  }

  fn balance(&self) -> ProviderResult<BudgetBalance> {
    let reserved = self.reserved()?;
    let committed = checked_total(self.consumed_usd_micros, reserved)?;
    let free_limit = self
      .reported_allowance_cap_usd_micros
      .map_or(self.free_limit_usd_micros, |allowance| self.free_limit_usd_micros.min(allowance));
    let period_available = self.reset_policy != BudgetResetPolicy::ReportedPeriod
      || self.period_end.is_some_and(|end| Timestamp::now().as_second() < end);
    let free = if period_available { free_limit.saturating_sub(committed) } else { 0 };
    let reported = self.reported_remaining_cap_usd_micros.map(|remaining| remaining.saturating_sub(reserved));
    Ok(BudgetBalance {
      free_allowance_usd_micros: free_limit,
      free_remaining_usd_micros: reported.map_or(free, |cap| free.min(cap)),
      consumed_usd_micros: self.consumed_usd_micros,
      reserved_usd_micros: reserved,
      period_start: self.period_start,
      period_end: self.period_end,
      dispatch_allowed: self.free_limit_usd_micros > 0 && period_available,
    })
  }
}

impl BudgetAccount {
  /// Project a rollover on a copy, without writing or changing accounting state.
  /// Even after a write failure, operators can inspect the last in-memory ledger.
  fn snapshot(&self) -> ProviderResult<BudgetSnapshot> {
    let state = self.store.state.lock().map_err(|_| internal("Budget ledger lock is poisoned."))?;
    let mut account = state
      .ledger
      .accounts
      .get(&self.provider)
      .and_then(|accounts| accounts.get(&self.account_id))
      .cloned()
      .ok_or_else(|| internal("Budget account is not registered."))?;
    let failed = state.failed;
    drop(state);
    account.advance(Timestamp::now())?;
    Ok(BudgetSnapshot {
      balance: account.balance()?,
      configured_free_allowance_usd_micros: account.free_limit_usd_micros,
      adjusted_remaining_cap_usd_micros: account.reported_remaining_cap_usd_micros,
      reported_allowance_cap_usd_micros: account.reported_allowance_cap_usd_micros,
      reset_day: (account.reset_policy == BudgetResetPolicy::CalendarMonthUtc).then_some(1),
      persistence_error: failed.then_some("Budget persistence failed; repair the state directory and restart."),
    })
  }

  fn state<'a>(&self, ledger: &'a mut Ledger) -> ProviderResult<&'a mut AccountState> {
    ledger
      .accounts
      .get_mut(&self.provider)
      .and_then(|accounts| accounts.get_mut(&self.account_id))
      .ok_or_else(|| internal("Budget account is not registered."))
  }

  pub fn balance(&self) -> ProviderResult<BudgetBalance> {
    self.store.transaction(|ledger| {
      let account = self.state(ledger)?;
      account.advance(Timestamp::now())?;
      account.balance()
    })
  }

  fn reserve(
    &self,
    cost: u64,
    deadline: Option<Instant>,
    dispatch_guard: OwnedMutexGuard<()>,
    recovery: bool,
  ) -> ProviderResult<Reservation> {
    let id = self.store.transaction(|ledger| {
      // The worker may have waited for the ledger lock beyond the deadline.
      check_deadline(deadline)?;
      let id = ledger.next_reservation_id;
      ledger.next_reservation_id =
        id.checked_add(1).ok_or_else(|| internal("Budget reservation IDs are exhausted."))?;
      let account = self.state(ledger)?;
      account.advance(Timestamp::now())?;
      let balance = account.balance()?;
      if !balance.dispatch_allowed {
        return Err(
          ProviderError::new(ProviderErrorKind::InsufficientBudget, "Provider account period is not available.")
            .without_charge(),
        );
      }
      if account.reported_remaining_cap_usd_micros == Some(0) && !recovery {
        // Preserve legacy exhaustion and quota errors whose caller timed out
        // during settlement. The router persists a day-long cooldown before
        // granting a single recovery attempt; restart is not a renewal.
        return Err(
          ProviderError::new(ProviderErrorKind::QuotaExhausted, "Provider quota requires a recovery probe.")
            .without_charge(),
        );
      }
      account.reservations.insert(id, cost);
      Ok(id)
    })?;
    debug!("{} reserved {cost} free USD micros (reservation {id})", self.provider);
    Ok(Reservation {
      account: self.clone(),
      id,
      dispatched: false,
      finished: false,
      recovery,
      dispatch_guard: Some(dispatch_guard),
    })
  }

  fn settle(
    &self,
    id: u64,
    charged: bool,
    exhausted: bool,
    reported_cost: Option<u64>,
    recovered: bool,
  ) -> ProviderResult<()> {
    let cost = self.store.transaction(|ledger| {
      let account = self.state(ledger)?;
      let now = Timestamp::now();
      account.advance(now)?;
      let cost = account
        .reservations
        .remove(&id)
        .ok_or_else(|| internal("Budget reservation was already settled or is missing."))?;
      if recovered && account.reported_remaining_cap_usd_micros == Some(0) {
        // Acceptance is evidence of usable capacity, not an exact billing date.
        // Keep calendar/report-based history; an unknown period starts a new
        // local estimate with this successful recovery request included.
        if account.reset_policy == BudgetResetPolicy::Unknown {
          account.clear_consumption();
          account.period_start = Some(now.as_second());
        }
        account.reported_remaining_cap_usd_micros = None;
      }
      if charged {
        account.charge(reported_cost.unwrap_or(cost), now)?;
      }
      if exhausted {
        account.reported_remaining_cap_usd_micros = Some(0);
        if account.awaiting_reported_period(now) {
          account.carry_exhausted = true;
        }
      }
      Ok(cost)
    })?;
    debug!(
      "{} settled reservation {id}, reserved {cost} USD micros, reported cost {reported_cost:?}: charged={charged}, quota_exhausted={exhausted}",
      self.provider
    );
    Ok(())
  }

  fn reconcile(&self, snapshot: &UsageSnapshot, period_start: Option<i64>) -> ProviderResult<()> {
    let applied = self.store.transaction(|ledger| {
      let account = self.state(ledger)?;
      account.advance(Timestamp::now())?;
      if account.period_start != period_start {
        // The query crossed a reset: its snapshot cannot safely describe this period.
        return Ok(false);
      }
      let reported_start = snapshot
        .period_start
        .map(Timestamp::try_from)
        .transpose()
        .map_err(|_| internal("Reported usage start time is invalid."))?
        .map(|time| time.as_second());
      let reported_end = snapshot
        .resets_at
        .map(Timestamp::try_from)
        .transpose()
        .map_err(|_| internal("Reported usage reset time is invalid."))?
        .map(|time| time.as_second());
      if account.reset_policy == BudgetResetPolicy::ReportedPeriod {
        let (Some(start), Some(end)) = (reported_start, reported_end) else {
          return Err(internal("Provider usage must identify both billing-period boundaries."));
        };
        let now = Timestamp::now().as_second();
        if start > now || end <= now || start >= end {
          return Err(internal("Provider usage did not identify a current billing period."));
        }
        match (account.period_start, account.period_end) {
          (None, None) => {} // Adopt timing without clearing migrated consumption.
          (Some(old_start), Some(old_end)) if start == old_start && end == old_end => {}
          (Some(_), Some(old_end)) if start >= old_end => account.clear_consumption(),
          _ => return Err(internal("Provider billing period moved backwards or changed ambiguously.")),
        }
        account.period_start = Some(start);
        account.period_end = Some(end);
      } else if reported_end.is_some() && reported_end != account.period_end {
        return Err(internal("Provider usage reset does not match the local budget period."));
      }
      // Reports can only tighten free capacity, never refund uncertain charges.
      // Pending requests remain reserved separately.
      if let Some(remaining) = snapshot.remaining_usd_micros {
        account.reported_remaining_cap_usd_micros =
          Some(account.reported_remaining_cap_usd_micros.map_or(remaining, |old| old.min(remaining)));
      }
      if let Some(allowance) = snapshot.allowance_usd_micros {
        account.reported_allowance_cap_usd_micros =
          Some(account.reported_allowance_cap_usd_micros.map_or(allowance, |old| old.min(allowance)));
      }
      Ok(true)
    })?;
    debug!(
      "{} provider usage for period {}: remaining {}, allowance {} USD micros; applied={applied}",
      self.provider,
      period_start.map_or_else(|| "None".to_owned(), |timestamp| timestamp.to_string()),
      snapshot.remaining_usd_micros.map_or_else(|| "None".to_owned(), |remaining| remaining.to_string()),
      snapshot.allowance_usd_micros.map_or_else(|| "None".to_owned(), |allowance| allowance.to_string())
    );
    Ok(())
  }
}

impl Reservation {
  async fn finish(
    mut self,
    charged: bool,
    exhausted: bool,
    reported_cost: Option<u64>,
    success: bool,
  ) -> ProviderResult<()> {
    // Queue before marking finished: cancellation while awaiting the worker must
    // not lose a known refund/quota cap or queue a second settlement from Drop.
    let task = self.schedule_settlement(charged, exhausted, reported_cost, self.recovery && success)?;
    self.finished = true;
    task.await.map_err(|_| internal("Budget settlement worker failed."))?
  }

  fn schedule_settlement(
    &mut self,
    charged: bool,
    exhausted: bool,
    reported_cost: Option<u64>,
    recovered: bool,
  ) -> ProviderResult<JoinHandle<ProviderResult<()>>> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| internal("Budget runtime is unavailable."))?;
    let account = self.account.clone();
    let id = self.id;
    let dispatch_guard = self.dispatch_guard.take();
    Ok(runtime.spawn_blocking(move || {
      // An undispatched reservation retains its admission guard until cleanup.
      // Dispatched requests settle independently in atomic ledger transactions.
      let _dispatch_guard = dispatch_guard;
      let result = account.settle(id, charged, exhausted, reported_cost, recovered);
      if let Err(error) = &result {
        error!("Could not settle search reservation: {error}");
      }
      result
    }))
  }
}

impl Drop for Reservation {
  fn drop(&mut self) {
    if !self.finished
      && let Err(error) = self.schedule_settlement(self.dispatched, false, None, false)
    {
      // The durable reservation still constrains capacity and is recovered on
      // restart, even when shutdown prevents scheduling its settlement.
      error!("Could not queue cancelled search settlement; durable reservation retained: {error}");
    }
  }
}

/// The adapter cannot issue a metered request without a durable reservation.
pub struct BudgetedProvider {
  inner: Box<dyn SearchProvider>,
  account: BudgetAccount,
  dispatch: Arc<AsyncMutex<()>>,
}

impl BudgetedProvider {
  pub fn new(inner: impl SearchProvider + 'static, account: BudgetAccount) -> ProviderResult<Self> {
    let balance = account.balance()?;
    debug!(
      "{} local budget (USD micros): consumed {}, reserved {}, free remaining {}, reset timestamp {}",
      inner.name(),
      balance.consumed_usd_micros,
      balance.reserved_usd_micros,
      balance.free_remaining_usd_micros,
      balance.period_end.map_or_else(|| "None".to_owned(), |timestamp| timestamp.to_string())
    );
    Ok(Self { inner: Box::new(inner), account, dispatch: Arc::default() })
  }

  pub async fn balance(&self) -> ProviderResult<BudgetBalance> {
    let account = self.account.clone();
    budget_worker(move || account.balance()).await
  }

  pub async fn snapshot(&self) -> ProviderResult<BudgetSnapshot> {
    let account = self.account.clone();
    budget_worker(move || account.snapshot()).await
  }

  /// Track costs durably, while the upstream free-only account enforces quota.
  pub async fn search_with_budget(
    &self,
    request: SearchRequest<'_>,
    deadline: Option<Instant>,
    recovery: bool,
  ) -> ProviderResult<SearchResponse> {
    check_deadline(deadline)?;
    let cost = self.inner.estimate_cost(&request)?;
    let RequestCost::UsdMicros(cost) = cost else {
      return self.inner.search(request).await;
    };
    // Serialize reservation writes, then release admission before provider HTTP.
    // The ledger transaction tracks every outstanding reservation independently.
    let dispatch_guard = Arc::clone(&self.dispatch).lock_owned().await;
    check_deadline(deadline)?;
    let account = self.account.clone();
    let mut reservation = budget_worker(move || {
      // Do not start a queued reservation after its request has expired.
      check_deadline(deadline)?;
      account.reserve(cost, deadline, dispatch_guard, recovery)
    })
    .await?;
    // A write may finish after timeout. A late reservation must never cause a
    // provider dispatch, even if the result wins a race with the timeout future.
    check_deadline(deadline)?;
    reservation.dispatched = true;
    drop(reservation.dispatch_guard.take());
    let result = self.inner.search(request).await;
    let charged = !matches!(&result, Err(error) if error.not_charged);
    let exhausted = matches!(&result, Err(error) if error.kind == ProviderErrorKind::QuotaExhausted);
    let reported_cost = result.as_ref().ok().and_then(|response| response.reported_cost_usd_micros);
    // Settle consumption and a remote quota cap in one durable transaction.
    // A known rollover or successful recovery probe clears the cap; restarting
    // alone does not. Estimated exhaustion must not mask usable upstream quota.
    reservation.finish(charged, exhausted, reported_cost, result.is_ok()).await?;
    result
  }
}

impl SearchProvider for BudgetedProvider {
  fn requests_per_minute(&self) -> Option<NonZeroU32> {
    self.inner.requests_per_minute()
  }

  fn minimum_request_interval(&self) -> std::time::Duration {
    self.inner.minimum_request_interval()
  }

  fn additional_rate_limits(&self) -> Vec<crate::providers::RequestRateLimit> {
    self.inner.additional_rate_limits()
  }

  fn max_concurrent_requests(&self) -> Option<NonZeroU32> {
    self.inner.max_concurrent_requests()
  }

  fn name(&self) -> &'static str {
    self.inner.name()
  }

  fn request_quota(&self) -> BoxFuture<'_, ProviderResult<Option<RequestQuotaSnapshot>>> {
    self.inner.request_quota()
  }

  fn clear_quota_cooldown(&self) -> BoxFuture<'_, ProviderResult<()>> {
    self.inner.clear_quota_cooldown()
  }

  fn estimate_cost(&self, request: &SearchRequest<'_>) -> ProviderResult<RequestCost> {
    self.inner.estimate_cost(request)
  }

  fn search<'a>(&'a self, request: SearchRequest<'a>) -> BoxFuture<'a, ProviderResult<SearchResponse>> {
    Box::pin(self.search_with_budget(request, None, false))
  }

  fn usage(&self) -> BoxFuture<'_, ProviderResult<Option<UsageSnapshot>>> {
    Box::pin(async {
      let period_start = self.balance().await?.period_start;
      let usage = self.inner.usage().await?;
      if let Some(snapshot) = &usage {
        let snapshot = snapshot.clone();
        let account = self.account.clone();
        budget_worker(move || account.reconcile(&snapshot, period_start)).await?;
      }
      Ok(usage)
    })
  }
}

async fn budget_worker<T: Send + 'static>(
  operation: impl FnOnce() -> ProviderResult<T> + Send + 'static,
) -> ProviderResult<T> {
  tokio::task::spawn_blocking(operation).await.map_err(|_| internal("Budget worker failed."))?
}

fn check_deadline(deadline: Option<Instant>) -> ProviderResult<()> {
  if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
    return Err(ProviderError::new(ProviderErrorKind::Transient, "Search provider exceeded the request deadline."));
  }
  Ok(())
}

/// Migrate once without granting capacity. Preserve charges, reservations and
/// caps; discard unverified configured reset schedules rather than reusing them.
fn parse_ledger(bytes: &[u8], now: Timestamp) -> ProviderResult<(Ledger, bool)> {
  let invalid = || internal("Budget ledger is invalid; refusing to reset usage.");
  let mut value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
  let version = value.get("version").and_then(serde_json::Value::as_u64).ok_or_else(invalid)?;
  let migrated = version == 1 || version == 2;
  if migrated {
    let providers = value.get_mut("accounts").and_then(serde_json::Value::as_object_mut).ok_or_else(invalid)?;
    for (provider, accounts) in providers {
      for account in accounts.as_object_mut().ok_or_else(invalid)?.values_mut() {
        let account = account.as_object_mut().ok_or_else(invalid)?;
        // Reject fields that the old schema would have rejected instead of
        // overwriting an unexpected policy or accepting new carry fields.
        if account.keys().any(|field| {
          !(matches!(
            field.as_str(),
            "reset_day"
              | "period_start"
              | "period_end"
              | "free_limit_usd_micros"
              | "consumed_usd_micros"
              | "reservations"
              | "reported_remaining_cap_usd_micros"
              | "reported_allowance_cap_usd_micros"
          ) || (version == 1 && field == "paid_limit_usd_micros"))
        }) {
          return Err(invalid());
        }
        if version == 1 {
          let paid = account.remove("paid_limit_usd_micros").and_then(|v| v.as_u64()).ok_or_else(invalid)?;
          let free = account.get("free_limit_usd_micros").and_then(|v| v.as_u64()).ok_or_else(invalid)?;
          checked_total(free, paid)?;
        }
        let reset_day = account.remove("reset_day").and_then(|v| v.as_u64()).ok_or_else(invalid)?;
        let start = account.get("period_start").and_then(|v| v.as_i64()).ok_or_else(invalid)?;
        let end = account.get("period_end").and_then(|v| v.as_i64()).ok_or_else(invalid)?;
        if !(1..=31).contains(&reset_day)
          || now.as_second() < start
          || period_at(Timestamp::from_second(start).map_err(|_| invalid())?, reset_day as u8)? != (start, end)
        {
          return Err(invalid());
        }
        let policy = budget_policy(provider);
        let (start, end) = match policy {
          BudgetResetPolicy::CalendarMonthUtc => {
            let (start, end) = period_at(now, 1)?;
            (Some(start), Some(end))
          }
          BudgetResetPolicy::ReportedPeriod => (None, None),
          BudgetResetPolicy::Unknown => (Some(start), None),
        };
        account.insert("reset_policy".to_owned(), serde_json::to_value(policy).map_err(|_| invalid())?);
        account.insert("period_start".to_owned(), serde_json::json!(start));
        account.insert("period_end".to_owned(), serde_json::json!(end));
      }
    }
    value["version"] = serde_json::json!(LEDGER_VERSION);
  }
  let ledger = serde_json::from_value(value).map_err(|_| invalid())?;
  Ok((ledger, migrated))
}

/// Publish a private, durable original before replacing a legacy ledger. A
/// retry after an interrupted migration may reuse only the identical backup.
fn preserve_migration_backup(directory: &Path, bytes: &[u8]) -> ProviderResult<()> {
  let path = directory.join(MIGRATION_BACKUP);
  match File::open(&path) {
    Ok(mut file) => {
      let mut saved = Vec::new();
      (&mut file)
        .take((MAX_LEDGER_BYTES + 1) as u64)
        .read_to_end(&mut saved)
        .map_err(|_| internal("Could not read pre-migration budget backup."))?;
      if saved != bytes {
        return Err(internal("Pre-migration budget backup differs from the legacy ledger; refusing to overwrite it."));
      }
      file.sync_all().map_err(|_| internal("Could not sync pre-migration budget backup."))?;
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      let mut temporary = tempfile::Builder::new()
        .prefix("budget-migration-")
        .tempfile_in(directory)
        .map_err(|_| internal("Could not create pre-migration budget backup."))?;
      temporary
        .write_all(bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|_| internal("Could not durably write pre-migration budget backup."))?;
      temporary.persist_noclobber(path).map_err(|_| internal("Could not publish pre-migration budget backup."))?;
    }
    Err(_) => return Err(internal("Could not open pre-migration budget backup.")),
  }
  #[cfg(unix)]
  File::open(directory)
    .and_then(|file| file.sync_all())
    .map_err(|_| internal("Could not sync pre-migration budget backup directory."))?;
  Ok(())
}

fn persist(directory: &Path, ledger: &Ledger) -> ProviderResult<()> {
  let bytes = serde_json::to_vec_pretty(ledger).map_err(|_| internal("Could not serialize budget ledger."))?;
  if bytes.len() > MAX_LEDGER_BYTES {
    return Err(internal("Budget ledger exceeds the size limit."));
  }
  let temporary = directory.join("budget.json.tmp");
  match fs::remove_file(&temporary) {
    Ok(()) => (),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
    Err(_) => return Err(internal("Could not remove an interrupted budget write.")),
  }
  let mut options = OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  let mut file = options.open(&temporary).map_err(|_| internal("Could not create temporary budget ledger."))?;
  file
    .write_all(&bytes)
    .and_then(|_| file.sync_all())
    .map_err(|_| internal("Could not durably write budget ledger."))?;
  drop(file);
  fs::rename(&temporary, directory.join("budget.json")).map_err(|_| internal("Could not replace budget ledger."))?;
  #[cfg(unix)]
  File::open(directory)
    .and_then(|file| file.sync_all())
    .map_err(|_| internal("Could not sync budget state directory."))?;
  Ok(())
}

fn period_at(now: Timestamp, reset_day: u8) -> ProviderResult<(i64, i64)> {
  if !(1..=31).contains(&reset_day) {
    return Err(internal("Monthly reset day is invalid."));
  }
  let date = now.to_zoned(TimeZone::UTC).date();
  let month = Date::new(date.year(), date.month(), 1).map_err(|_| internal("Could not compute budget month."))?;
  let month = if now.as_second() < boundary(month, reset_day)? {
    month.checked_add(Span::new().months(-1)).map_err(|_| internal("Could not compute previous budget month."))?
  } else {
    month
  };
  let next = month.checked_add(Span::new().months(1)).map_err(|_| internal("Could not compute next budget month."))?;
  Ok((boundary(month, reset_day)?, boundary(next, reset_day)?))
}

fn boundary(month: Date, reset_day: u8) -> ProviderResult<i64> {
  let day = (reset_day as i8).min(month.days_in_month());
  Date::new(month.year(), month.month(), day)
    .and_then(|date| date.at(0, 0, 0, 0).to_zoned(TimeZone::UTC))
    .map(|date| date.timestamp().as_second())
    .map_err(|_| internal("Could not compute budget reset boundary."))
}

fn checked_total(left: u64, right: u64) -> ProviderResult<u64> {
  left.checked_add(right).ok_or_else(|| internal("Budget values exceed the supported range."))
}

fn internal(message: &str) -> ProviderError {
  ProviderError::new(ProviderErrorKind::Internal, message)
}
