// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

//! MCP over stdio for search plugins; see PLUGINS.md. Each session owns one
//! plugin process and its process group. Dropping an unfinished session kills
//! the group at once; `finish` allows a short exit grace in the background.

#![cfg_attr(not(test), expect(dead_code, reason = "Plugin discovery and search adopt this transport next."))]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use log::info;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

use super::mcp::{initialize_params, matching_reply, negotiated_protocol, unexpected};
use super::{ProviderError, ProviderErrorKind, ProviderResult};

const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;
const EXIT_GRACE: Duration = Duration::from_secs(1);
const MAX_STDERR_LINES: usize = 64;
const MAX_STDERR_LINE_CHARS: usize = 1000;

pub(super) struct Session {
  name: &'static str,
  /// Also the process group id. The leader is reaped only after `finish`, or
  /// by Tokio after drop, so this id cannot be reused while the session lives.
  pid: u32,
  child: Option<Child>,
  stdin: Option<ChildStdin>,
  stdout: BufReader<ChildStdout>,
  stderr: Option<JoinHandle<()>>,
  next_id: u64,
}

impl Session {
  /// Spawn the plugin with exactly the configured environment, then complete
  /// MCP initialization. The caller bounds the whole session with a deadline.
  pub(super) async fn start(
    name: &'static str,
    program: &Path,
    args: &[String],
    env: &BTreeMap<String, String>,
  ) -> ProviderResult<Self> {
    let mut command = Command::new(program);
    command
      .args(args)
      .env_clear()
      .envs(env)
      .current_dir("/")
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .process_group(0)
      .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
      transient("Search plugin could not be started.").with_upstream_detail(Some(error.to_string()))
    })?;
    let (Some(pid), Some(stdin), Some(stdout), Some(stderr)) =
      (child.id(), child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
      return Err(transient("Search plugin could not be started."));
    };
    let mut session = Self {
      name,
      pid,
      child: Some(child),
      stdin: Some(stdin),
      stdout: BufReader::new(stdout),
      stderr: Some(log_stderr(name, pid, stderr)),
      next_id: 1,
    };
    let result = session.request("initialize", initialize_params()).await?;
    negotiated_protocol(&result)?;
    session.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await?;
    Ok(session)
  }

  /// Plugin requests received while waiting are answered and notifications are
  /// ignored. JSON-RPC errors other than invalid requests are unexpected.
  pub(super) async fn request(&mut self, method: &'static str, params: Value) -> ProviderResult<Value> {
    let id = self.next_id;
    self.next_id += 1;
    self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await?;
    loop {
      let message = self.read_message().await?;
      if let Some(method) = message.get("method") {
        if let Some(request_id) = message.get("id") {
          let reply = if method.as_str() == Some("ping") {
            json!({ "jsonrpc": "2.0", "id": request_id, "result": {} })
          } else {
            json!({ "jsonrpc": "2.0", "id": request_id, "error": { "code": -32601, "message": "Method not found." } })
          };
          self.send(&reply).await?;
        }
        continue;
      }
      if let Some(result) = matching_reply(message, id, |_| None)? {
        return Ok(result);
      }
    }
  }

  /// Close stdin and let the plugin exit by itself, killing its process group
  /// after the grace period. Runs in the background so replies are not delayed.
  pub(super) fn finish(mut self) {
    let (Some(child), stdin, stderr) = (self.child.take(), self.stdin.take(), self.stderr.take()) else { return };
    tokio::spawn(shut_down(self.pid, child, stdin, stderr));
  }

  async fn send(&mut self, message: &Value) -> ProviderResult<()> {
    let stdin = self.stdin.as_mut().ok_or_else(|| transient("Search plugin input is closed."))?;
    // Compact JSON never contains a raw newline, so each message is one line.
    let mut line = message.to_string().into_bytes();
    line.push(b'\n');
    stdin.write_all(&line).await.map_err(|_| transient("Search plugin stopped reading its input."))
  }

  async fn read_message(&mut self) -> ProviderResult<Value> {
    loop {
      let mut line = Vec::new();
      let read = (&mut self.stdout)
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|_| transient("Search plugin output could not be read."))?;
      if read == 0 {
        return Err(transient("Search plugin exited before replying."));
      }
      if line.len() > MAX_LINE_BYTES {
        return Err(unexpected("Search plugin message exceeded the size limit."));
      }
      if line.iter().all(u8::is_ascii_whitespace) {
        continue;
      }
      return serde_json::from_slice(&line).map_err(|_| unexpected("Search plugin wrote output that is not JSON."));
    }
  }
}

impl Drop for Session {
  fn drop(&mut self) {
    if let Some(task) = self.stderr.take() {
      task.abort();
    }
    let Some(mut child) = self.child.take() else { return };
    kill_group(self.pid);
    info!("[plugin {} {}] Killed before finishing.", self.name, self.pid);
    // Reap now rather than when Tokio next spawns a child. Without a runtime,
    // kill_on_drop leaves reaping to Tokio's orphan queue.
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
      runtime.spawn(async move {
        let _ = child.wait().await;
      });
    }
  }
}

async fn shut_down(pid: u32, mut child: Child, stdin: Option<ChildStdin>, stderr: Option<JoinHandle<()>>) {
  drop(stdin);
  if tokio::time::timeout(EXIT_GRACE, child.wait()).await.is_err() {
    kill_group(pid);
    let _ = child.wait().await;
  }
  if let Some(mut task) = stderr
    && tokio::time::timeout(Duration::from_millis(100), &mut task).await.is_err()
  {
    task.abort();
  }
}

/// Callers guarantee the group leader has not been reaped.
fn kill_group(pid: u32) {
  let Ok(pid) = libc::pid_t::try_from(pid) else { return };
  if pid <= 0 {
    return;
  }
  // SAFETY: this still-owned child created this process group; its leader has
  // not been reaped. All signals are confined to this plugin's group.
  unsafe {
    libc::kill(-pid, libc::SIGKILL);
  }
}

/// Bounded per process. Debug formatting escapes control characters, so plugin
/// output cannot forge log lines.
fn log_stderr(name: &'static str, pid: u32, stderr: ChildStderr) -> JoinHandle<()> {
  tokio::spawn(async move {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    let mut logged = 0;
    loop {
      line.clear();
      // Bounded reads split very long lines instead of buffering them whole.
      match (&mut reader).take(4096).read_until(b'\n', &mut line).await {
        Ok(0) | Err(_) => break,
        Ok(_) => {}
      }
      if logged == MAX_STDERR_LINES {
        continue;
      }
      logged += 1;
      let text: String = String::from_utf8_lossy(&line).trim_end().chars().take(MAX_STDERR_LINE_CHARS).collect();
      info!("[plugin {name} {pid}] {text:?}");
      if logged == MAX_STDERR_LINES {
        info!("[plugin {name} {pid}] Further stderr output is discarded.");
      }
    }
  })
}

fn transient(message: &'static str) -> ProviderError {
  ProviderError::new(ProviderErrorKind::Transient, message)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Answers initialize and consumes notifications/initialized.
  const HANDSHAKE: &str = "read -r _; printf '%s\\n' \
    '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}}}}'; \
    read -r _; ";

  async fn start(body: &str, env: &[(&str, &str)]) -> ProviderResult<Session> {
    let args = ["-c".to_owned(), format!("{HANDSHAKE}{body}")];
    let env = env.iter().map(|&(name, value)| (name.to_owned(), value.to_owned())).collect();
    Session::start("test", Path::new("/bin/sh"), &args, &env).await
  }

  async fn request(body: &str) -> ProviderResult<Value> {
    let mut session = start(body, &[]).await?;
    let result = session.request("tools/call", json!({})).await;
    session.finish();
    result
  }

  async fn exited(pid: u32) -> bool {
    for _ in 0..50 {
      // SAFETY: signal 0 only checks for existence.
      if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
      {
        return true;
      }
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
  }

  #[tokio::test]
  async fn returns_matching_result() {
    let result = request("read -r _; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}'").await;
    assert_eq!(result.unwrap(), json!({ "ok": true }));
  }

  #[tokio::test]
  async fn answers_plugin_requests_and_ignores_notifications() {
    let result = request(
      "read -r _; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}' \
       '{\"jsonrpc\":\"2.0\",\"id\":\"p\",\"method\":\"ping\"}' '{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"roots/list\"}'; \
       read -r ping; read -r other; \
       printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ping\":%s,\"other\":%s}}\\n' \"$ping\" \"$other\"",
    )
    .await
    .unwrap();
    assert_eq!(result["ping"], json!({ "jsonrpc": "2.0", "id": "p", "result": {} }));
    assert_eq!(result["other"]["id"], 7);
    assert_eq!(result["other"]["error"]["code"], -32601);
  }

  #[tokio::test]
  async fn plugin_receives_only_configured_environment() {
    let mut session = start(
      "read -r _; printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"key\":\"%s\",\"home\":\"%s\",\"cwd\":\"%s\"}}\\n' \
       \"$KEY\" \"${HOME-unset}\" \"$PWD\"",
      &[("KEY", "value")],
    )
    .await
    .unwrap();
    let result = session.request("tools/call", json!({})).await.unwrap();
    session.finish();
    assert_eq!(result, json!({ "key": "value", "home": "unset", "cwd": "/" }));
  }

  #[tokio::test]
  async fn json_rpc_invalid_params_is_invalid_request() {
    let error = request(
      "read -r _; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32602,\"message\":\"Unknown tool.\"}}'",
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
  }

  #[tokio::test]
  async fn malformed_output_is_unexpected() {
    for body in ["read -r _; printf 'hello\\n'", "read -r _; /usr/bin/head -c 2200000 /dev/zero"] {
      assert_eq!(request(body).await.unwrap_err().kind, ProviderErrorKind::UnexpectedResponse, "{body}");
    }
  }

  #[tokio::test]
  async fn process_failures_are_transient() {
    assert_eq!(request("read -r _; exit 3").await.unwrap_err().kind, ProviderErrorKind::Transient);
    let args = ["-c".to_owned(), "exit 0".to_owned()];
    let early = Session::start("test", Path::new("/bin/sh"), &args, &BTreeMap::new()).await;
    assert_eq!(early.err().unwrap().kind, ProviderErrorKind::Transient);
    let missing = Session::start("test", Path::new("/nonexistent/plugin"), &[], &BTreeMap::new()).await;
    assert_eq!(missing.err().unwrap().kind, ProviderErrorKind::Transient);
  }

  #[tokio::test]
  async fn drop_kills_process_group() {
    let mut session = start(
      "read -r _; /bin/sleep 30 & printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"pid\":%s}}\\n' \"$!\"; wait",
      &[],
    )
    .await
    .unwrap();
    let result = session.request("tools/call", json!({})).await.unwrap();
    let (leader, descendant) = (session.pid, result["pid"].as_u64().unwrap() as u32);
    drop(session);
    assert!(exited(descendant).await, "descendant survived");
    assert!(exited(leader).await, "leader survived");
  }

  #[tokio::test]
  async fn finish_kills_plugin_that_ignores_end_of_input() {
    let mut session =
      start("read -r _; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}'; exec /bin/sleep 30", &[])
        .await
        .unwrap();
    session.request("tools/call", json!({})).await.unwrap();
    let pid = session.pid;
    session.finish();
    assert!(exited(pid).await, "plugin survived finish");
  }
}
