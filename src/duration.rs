// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! Duration formatting shared by log lines.

use std::time::Duration;

/// Compact duration for logs, such as `24h`, `1h30m`, `15m` or `2s`.
pub fn span(duration: Duration) -> String {
  let seconds = duration.as_secs() + u64::from(duration.subsec_nanos() > 0);
  match (seconds / 3600, seconds % 3600 / 60, seconds % 60) {
    (0, 0, s) => format!("{s}s"),
    (0, m, 0) => format!("{m}m"),
    (0, m, s) => format!("{m}m{s}s"),
    (h, 0, _) => format!("{h}h"),
    (h, m, _) => format!("{h}h{m}m"),
  }
}
