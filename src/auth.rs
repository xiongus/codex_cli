use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct AuthMeta {
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub last_refresh: Option<String>,
}

pub fn load_auth(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(value)
}

pub fn extract_meta(auth: &Value) -> AuthMeta {
    let tokens = auth.get("tokens").and_then(Value::as_object);
    let access_payload = tokens
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(Value::as_str)
        .and_then(decode_jwt_payload);
    let id_payload = tokens
        .and_then(|tokens| tokens.get("id_token"))
        .and_then(Value::as_str)
        .and_then(decode_jwt_payload);

    let email = id_payload
        .as_ref()
        .and_then(payload_email)
        .or_else(|| access_payload.as_ref().and_then(payload_email))
        .map(normalize_text);

    let account_id = tokens
        .and_then(|tokens| tokens.get("account_id"))
        .and_then(Value::as_str)
        .map(normalize_text)
        .or_else(|| {
            access_payload
                .as_ref()
                .and_then(payload_account_id)
                .map(normalize_text)
        })
        .or_else(|| {
            id_payload
                .as_ref()
                .and_then(payload_account_id)
                .map(normalize_text)
        });

    let last_refresh = auth
        .get("last_refresh")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    AuthMeta {
        email,
        account_id,
        last_refresh,
    }
}

fn payload_email(payload: &Value) -> Option<&str> {
    payload.get("email").and_then(Value::as_str).or_else(|| {
        payload
            .get("https://api.openai.com/profile")?
            .get("email")?
            .as_str()
    })
}

fn payload_account_id(payload: &Value) -> Option<&str> {
    payload
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
}

fn normalize_text(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

pub fn account_key(meta: &AuthMeta) -> String {
    let raw = meta
        .email
        .as_deref()
        .or(meta.account_id.as_deref())
        .unwrap_or("unknown-account");
    let mut key = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '@' | '-') {
            key.push(ch);
        } else {
            key.push('_');
        }
    }
    let trimmed = key.trim_matches(&['.', '_'][..]);
    if trimmed.is_empty() {
        "unknown-account".to_string()
    } else {
        trimmed.to_string()
    }
}

fn decode_jwt_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let normalized = match payload.len() % 4 {
        0 => payload.to_string(),
        n => format!("{payload}{}", "=".repeat(4 - n)),
    };
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(normalized)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    let serialized = serde_json::to_vec_pretty(value)?;
    atomic_write_bytes(path, &serialized)
}

pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("no parent for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    let tmp_path = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut file =
            File::create(&tmp_path).with_context(|| format!("create {}", tmp_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write {}", tmp_path.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("newline {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

pub struct FileLock {
    file: File,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_with_mode(path, false)
    }

    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        match Self::acquire_with_mode(path, true) {
            Ok(lock) => Ok(Some(lock)),
            Err(err) if err.to_string().contains("already locked") => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn acquire_with_mode(path: &Path, nonblocking: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open lock {}", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let op = if nonblocking {
                libc::LOCK_EX | libc::LOCK_NB
            } else {
                libc::LOCK_EX
            };
            let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
            if rc != 0 {
                let errno = std::io::Error::last_os_error();
                if nonblocking && matches!(errno.raw_os_error(), Some(libc::EWOULDBLOCK)) {
                    bail!("already locked: {}", path.display());
                }
                bail!("failed to lock {}: {}", path.display(), errno);
            }
        }

        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}
