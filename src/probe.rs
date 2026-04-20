use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::auth::{AuthMeta, atomic_write_json, extract_meta, load_auth};

const APP_SERVER_CMD: [&str; 4] = ["codex", "app-server", "--listen", "stdio://"];

#[derive(Debug, Clone)]
pub struct ProbeSnapshot {
    pub account_meta: AuthMeta,
    pub plan: Option<String>,
    pub primary_used_percent: Option<i64>,
    pub secondary_used_percent: Option<i64>,
    pub primary_resets_at: Option<i64>,
    pub credits_has_credits: Option<bool>,
    pub credits_unlimited: Option<bool>,
    pub rate_limits_available: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ProbeOutput {
    pub refreshed_auth: Value,
    pub snapshot: ProbeSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailureKind {
    AppServerSpawn,
    AppServerExit,
    AccountReadTimeout,
    AuthFileRead,
}

impl ProbeFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeFailureKind::AppServerSpawn => "app_server_spawn",
            ProbeFailureKind::AppServerExit => "app_server_exit",
            ProbeFailureKind::AccountReadTimeout => "account_read_timeout",
            ProbeFailureKind::AuthFileRead => "auth_file_read",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProbeFailure {
    pub kind: ProbeFailureKind,
    pub message: String,
}

impl std::fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProbeFailure {}

pub fn run_probe(
    account_auth_path: &Path,
    timeout: Duration,
) -> std::result::Result<ProbeOutput, ProbeFailure> {
    let original_auth = load_auth(account_auth_path).map_err(|err| ProbeFailure {
        kind: ProbeFailureKind::AuthFileRead,
        message: format!("read auth file failed: {err}"),
    })?;
    let tmp = TempDir::new()
        .context("create temp dir")
        .map_err(|err| ProbeFailure {
            kind: ProbeFailureKind::AuthFileRead,
            message: format!("create temp dir failed: {err}"),
        })?;
    let temp_home = tmp.path();
    atomic_write_json(&temp_home.join("auth.json"), &original_auth).map_err(|err| {
        ProbeFailure {
            kind: ProbeFailureKind::AuthFileRead,
            message: format!("write temp auth file failed: {err}"),
        }
    })?;

    let mut child = Command::new(APP_SERVER_CMD[0])
        .args(&APP_SERVER_CMD[1..])
        .env("CODEX_HOME", temp_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| ProbeFailure {
            kind: ProbeFailureKind::AppServerSpawn,
            message: format!("spawn codex app-server failed: {err}"),
        })?;

    let mut stdin = child.stdin.take().ok_or_else(|| ProbeFailure {
        kind: ProbeFailureKind::AppServerSpawn,
        message: "capture stdin failed".to_string(),
    })?;
    for message in build_probe_messages() {
        writeln!(stdin, "{message}").map_err(|err| ProbeFailure {
            kind: ProbeFailureKind::AppServerSpawn,
            message: format!("write app-server request failed: {err}"),
        })?;
    }
    drop(stdin);

    let stdout = child.stdout.take().ok_or_else(|| ProbeFailure {
        kind: ProbeFailureKind::AppServerSpawn,
        message: "capture stdout failed".to_string(),
    })?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let _ = tx.send(line);
        }
    });

    let started = Instant::now();
    let mut responses = std::collections::HashMap::<i64, Value>::new();
    while started.elapsed() < timeout {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(line)) => {
                if let Ok(message) = serde_json::from_str::<Value>(&line)
                    && let Some(id) = message.get("id").and_then(Value::as_i64)
                {
                    responses.insert(id, message);
                    if responses.contains_key(&2) && responses.contains_key(&3) {
                        break;
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait().map_err(|err| ProbeFailure {
                    kind: ProbeFailureKind::AppServerExit,
                    message: format!("poll codex app-server failed: {err}"),
                })? && !status.success()
                {
                    return Err(ProbeFailure {
                        kind: ProbeFailureKind::AppServerExit,
                        message: format!("codex app-server exited with {status}"),
                    });
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if child
        .try_wait()
        .map_err(|err| ProbeFailure {
            kind: ProbeFailureKind::AppServerExit,
            message: format!("poll codex app-server failed: {err}"),
        })?
        .is_none()
    {
        let _ = child.kill();
        let _ = child.wait();
    }

    let account_result = responses
        .get(&2)
        .and_then(|value| value.get("result"))
        .cloned()
        .ok_or_else(|| ProbeFailure {
            kind: ProbeFailureKind::AccountReadTimeout,
            message: format!("account/read did not return within {:?}", timeout),
        })?;
    let limits_result = responses
        .get(&3)
        .and_then(|value| value.get("result"))
        .cloned();

    let refreshed_auth = load_auth(&temp_home.join("auth.json"))
        .map_err(|err| ProbeFailure {
            kind: ProbeFailureKind::AuthFileRead,
            message: format!("read refreshed auth failed: {err}"),
        })
        .unwrap_or(original_auth);
    let auth_meta = extract_meta(&refreshed_auth);
    let snapshot = parse_snapshot(account_result, limits_result, auth_meta, timeout);
    Ok(ProbeOutput {
        refreshed_auth,
        snapshot,
    })
}

fn build_probe_messages() -> Vec<String> {
    vec![
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {"name": "codex-pool-rs", "version": "0.1"},
                "capabilities": Value::Null,
            },
        })
        .to_string(),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "account/read",
            "params": {"refreshToken": true},
        })
        .to_string(),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "account/rateLimits/read",
            "params": Value::Null,
        })
        .to_string(),
    ]
}

fn parse_snapshot(
    account_result: Value,
    limits_result: Option<Value>,
    account_meta: AuthMeta,
    timeout: Duration,
) -> ProbeSnapshot {
    let account = account_result
        .get("account")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut warnings = Vec::new();
    let rate_limits_available = limits_result.is_some();
    if !rate_limits_available {
        warnings.push(format!(
            "account/rateLimits/read did not return within {:?}",
            timeout
        ));
    }
    let snapshot = limits_result
        .as_ref()
        .map(primary_snapshot)
        .unwrap_or_else(|| json!({}));
    let primary = snapshot
        .get("primary")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let secondary = snapshot
        .get("secondary")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let credits = snapshot
        .get("credits")
        .cloned()
        .unwrap_or_else(|| json!({}));

    ProbeSnapshot {
        account_meta,
        plan: account
            .get("planType")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                snapshot
                    .get("planType")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            }),
        primary_used_percent: primary.get("usedPercent").and_then(Value::as_i64),
        secondary_used_percent: secondary.get("usedPercent").and_then(Value::as_i64),
        primary_resets_at: primary.get("resetsAt").and_then(Value::as_i64),
        credits_has_credits: credits.get("hasCredits").and_then(Value::as_bool),
        credits_unlimited: credits.get("unlimited").and_then(Value::as_bool),
        rate_limits_available,
        warnings,
    }
}

fn primary_snapshot(limits_result: &Value) -> Value {
    limits_result
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .and_then(|map| {
            map.get("codex")
                .cloned()
                .or_else(|| map.values().next().cloned())
        })
        .or_else(|| limits_result.get("rateLimits").cloned())
        .unwrap_or_else(|| json!({}))
}
