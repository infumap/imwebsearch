// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use std::error::Error;
use std::fmt::{Display, Formatter};

pub type InfuResult<T> = Result<T, InfuError>;

#[derive(Debug)]
pub struct InfuError {
  message: String,
  http_status: Option<u16>,
}

impl InfuError {
  pub fn new(message: &str) -> InfuError {
    InfuError { message: message.to_string(), http_status: None }
  }

  pub fn from_http_status(status: u16) -> Self {
    Self { message: format!("Failed to fetch URL: HTTP {status}"), http_status: Some(status) }
  }

  /// An HTTP failure with a more useful explanation than the status alone.
  pub fn from_http_status_with_message(status: u16, message: String) -> Self {
    Self { message, http_status: Some(status) }
  }

  pub fn http_status(&self) -> Option<u16> {
    self.http_status
  }
}

impl Display for InfuError {
  fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
    write!(f, "{}", self.message)
  }
}

impl Error for InfuError {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    None
  }
}

impl From<String> for InfuError {
  fn from(err: String) -> Self {
    Self::new(&err)
  }
}

impl From<&str> for InfuError {
  fn from(err: &str) -> Self {
    Self::new(err)
  }
}
