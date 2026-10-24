// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Application identity for outbound MCP client metadata and API User-Agents.

pub const NAME: &str = "imwebsearch";

/// Wikimedia rejects API requests that carry no User-Agent.
pub const USER_AGENT: &str = concat!("imwebsearch/", env!("CARGO_PKG_VERSION"));
