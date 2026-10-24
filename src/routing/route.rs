// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use crate::router::SearchRouter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolSelection {
  GeneralWeb,
  Fallback,
}

/// General web providers share a router; each fallback source keeps its own
/// router so pacing, health and ranking remain independent.
pub struct ProviderPool<'a> {
  pub name: &'static str,
  pub router: &'a SearchRouter,
  pub selection: PoolSelection,
}

impl<'a> ProviderPool<'a> {
  pub fn fallback(name: &'static str, router: &'a SearchRouter) -> Self {
    Self { name, router, selection: PoolSelection::Fallback }
  }

  pub fn general_web(name: &'static str, router: &'a SearchRouter) -> Self {
    Self { name, router, selection: PoolSelection::GeneralWeb }
  }
}
