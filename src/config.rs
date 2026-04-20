use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    pub global_auth_file: PathBuf,
    pub service_root: PathBuf,
    pub accounts_dir: PathBuf,
    pub db_path: PathBuf,
    pub lock_path: PathBuf,
    pub daemon_lock_path: PathBuf,
    pub locks_dir: PathBuf,
    pub legacy_accounts_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let codex_home = PathBuf::from(home).join(".codex");
        let service_root = codex_home.join("account-pool-rs");
        let accounts_dir = service_root.join("accounts");
        let db_path = service_root.join("state.db");
        let lock_path = service_root.join("auth.switch.lock");
        let daemon_lock_path = service_root.join("daemon.lock");
        let locks_dir = service_root.join("locks");
        let legacy_accounts_dir = codex_home.join("account-pool").join("accounts");
        Ok(Self {
            global_auth_file: codex_home.join("auth.json"),
            service_root,
            accounts_dir,
            db_path,
            lock_path,
            daemon_lock_path,
            locks_dir,
            legacy_accounts_dir,
        })
    }

    pub fn ensure_layout(&self) -> Result<()> {
        std::fs::create_dir_all(&self.service_root)
            .with_context(|| format!("create {}", self.service_root.display()))?;
        std::fs::create_dir_all(&self.accounts_dir)
            .with_context(|| format!("create {}", self.accounts_dir.display()))?;
        std::fs::create_dir_all(&self.locks_dir)
            .with_context(|| format!("create {}", self.locks_dir.display()))?;
        Ok(())
    }

    pub fn account_dir(&self, account_key: &str) -> PathBuf {
        self.accounts_dir.join(account_key)
    }

    pub fn account_auth_file(&self, account_key: &str) -> PathBuf {
        self.account_dir(account_key).join("auth.json")
    }

    pub fn account_lock_file(&self, account_key: &str) -> PathBuf {
        self.locks_dir.join(format!("{account_key}.lock"))
    }
}
