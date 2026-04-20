use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

#[derive(Debug, Clone)]
pub struct AccountRow {
    pub account_key: String,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub auth_path: String,
    pub last_refresh: Option<String>,
    pub last_refresh_ok_at: Option<i64>,
    pub refresh_fail_count: i64,
    pub last_probe_at: Option<i64>,
    pub last_probe_ok_at: Option<i64>,
    pub probe_fail_count: i64,
    pub last_error: Option<String>,
    pub plan: Option<String>,
    pub primary_used_percent: Option<i64>,
    pub secondary_used_percent: Option<i64>,
    pub primary_resets_at: Option<i64>,
    pub credits_has_credits: Option<bool>,
    pub credits_unlimited: Option<bool>,
    pub snapshot_updated_at: Option<i64>,
    pub cooldown_until: Option<i64>,
    pub next_refresh_due_at: Option<i64>,
    pub next_probe_due_at: Option<i64>,
    pub last_switched_at: Option<i64>,
    pub last_error_kind: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpsertAccount<'a> {
    pub account_key: &'a str,
    pub email: Option<&'a str>,
    pub account_id: Option<&'a str>,
    pub auth_path: &'a str,
    pub last_refresh: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct ProbeUpdate<'a> {
    pub account_key: &'a str,
    pub ok: bool,
    pub error: Option<&'a str>,
    pub plan: Option<&'a str>,
    pub primary_used_percent: Option<i64>,
    pub secondary_used_percent: Option<i64>,
    pub primary_resets_at: Option<i64>,
    pub credits_has_credits: Option<bool>,
    pub credits_unlimited: Option<bool>,
    pub observed_last_refresh: Option<&'a str>,
    pub next_probe_due_at: i64,
    pub next_refresh_due_at: i64,
    pub cooldown_until: Option<i64>,
    pub error_kind: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub enum HistoryKind {
    Probe,
    Refresh,
    Switch,
}

impl HistoryKind {
    fn as_str(self) -> &'static str {
        match self {
            HistoryKind::Probe => "probe",
            HistoryKind::Refresh => "refresh",
            HistoryKind::Switch => "switch",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HistoryEvent<'a> {
    pub account_key: &'a str,
    pub kind: HistoryKind,
    pub ok: bool,
    pub message: &'a str,
    pub detail_json: Option<&'a str>,
}

pub fn connect(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS accounts (
            account_key TEXT PRIMARY KEY,
            email TEXT,
            account_id TEXT,
            auth_path TEXT NOT NULL,
            last_refresh TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            last_refresh_ok_at INTEGER,
            refresh_fail_count INTEGER NOT NULL DEFAULT 0,
            last_probe_at INTEGER,
            last_probe_ok_at INTEGER,
            probe_fail_count INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            plan TEXT,
            primary_used_percent INTEGER,
            secondary_used_percent INTEGER,
            primary_resets_at INTEGER,
            credits_has_credits INTEGER,
            credits_unlimited INTEGER,
            snapshot_updated_at INTEGER,
            cooldown_until INTEGER,
            next_refresh_due_at INTEGER,
            next_probe_due_at INTEGER,
            last_switched_at INTEGER,
            last_error_kind TEXT
        );

        CREATE TABLE IF NOT EXISTS history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_key TEXT NOT NULL,
            kind TEXT NOT NULL,
            ok INTEGER NOT NULL,
            message TEXT NOT NULL,
            detail_json TEXT,
            created_at INTEGER NOT NULL,
            FOREIGN KEY(account_key) REFERENCES accounts(account_key)
        );
        "#,
    )?;
    ensure_account_column(
        conn,
        "cooldown_until",
        "ALTER TABLE accounts ADD COLUMN cooldown_until INTEGER",
    )?;
    ensure_account_column(
        conn,
        "next_refresh_due_at",
        "ALTER TABLE accounts ADD COLUMN next_refresh_due_at INTEGER",
    )?;
    ensure_account_column(
        conn,
        "next_probe_due_at",
        "ALTER TABLE accounts ADD COLUMN next_probe_due_at INTEGER",
    )?;
    ensure_account_column(
        conn,
        "last_switched_at",
        "ALTER TABLE accounts ADD COLUMN last_switched_at INTEGER",
    )?;
    ensure_account_column(
        conn,
        "last_error_kind",
        "ALTER TABLE accounts ADD COLUMN last_error_kind TEXT",
    )?;
    Ok(())
}

fn ensure_account_column(conn: &Connection, column_name: &str, ddl: &str) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(accounts)")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == column_name) {
        conn.execute(ddl, [])?;
    }
    Ok(())
}

pub fn upsert_account(conn: &Connection, update: UpsertAccount<'_>) -> Result<()> {
    let now = epoch_now();
    conn.execute(
        r#"
        INSERT INTO accounts (
            account_key, email, account_id, auth_path, last_refresh, created_at, updated_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
        ON CONFLICT(account_key) DO UPDATE SET
            email = excluded.email,
            account_id = excluded.account_id,
            auth_path = excluded.auth_path,
            last_refresh = COALESCE(excluded.last_refresh, accounts.last_refresh),
            updated_at = excluded.updated_at
        "#,
        params![
            update.account_key,
            update.email,
            update.account_id,
            update.auth_path,
            update.last_refresh,
            now,
        ],
    )?;
    Ok(())
}

pub fn list_accounts(conn: &Connection) -> Result<Vec<AccountRow>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            account_key, email, account_id, auth_path, last_refresh,
            last_refresh_ok_at, refresh_fail_count, last_probe_at, last_probe_ok_at,
            probe_fail_count, last_error, plan, primary_used_percent,
            secondary_used_percent, primary_resets_at, credits_has_credits,
            credits_unlimited, snapshot_updated_at, cooldown_until,
            next_refresh_due_at, next_probe_due_at, last_switched_at, last_error_kind
        FROM accounts
        ORDER BY COALESCE(email, account_key)
        "#,
    )?;

    let rows = stmt
        .query_map([], |row| {
            Ok(AccountRow {
                account_key: row.get(0)?,
                email: row.get(1)?,
                account_id: row.get(2)?,
                auth_path: row.get(3)?,
                last_refresh: row.get(4)?,
                last_refresh_ok_at: row.get(5)?,
                refresh_fail_count: row.get(6)?,
                last_probe_at: row.get(7)?,
                last_probe_ok_at: row.get(8)?,
                probe_fail_count: row.get(9)?,
                last_error: row.get(10)?,
                plan: row.get(11)?,
                primary_used_percent: row.get(12)?,
                secondary_used_percent: row.get(13)?,
                primary_resets_at: row.get(14)?,
                credits_has_credits: row.get(15)?,
                credits_unlimited: row.get(16)?,
                snapshot_updated_at: row.get(17)?,
                cooldown_until: row.get(18)?,
                next_refresh_due_at: row.get(19)?,
                next_probe_due_at: row.get(20)?,
                last_switched_at: row.get(21)?,
                last_error_kind: row.get(22)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_account_by_selector(conn: &Connection, selector: &str) -> Result<Option<AccountRow>> {
    let all = list_accounts(conn)?;
    if let Ok(index) = selector.parse::<usize>()
        && (1..=all.len()).contains(&index)
    {
        return Ok(Some(all[index - 1].clone()));
    }

    let normalized = selector.trim().to_ascii_lowercase();
    for row in all {
        if row.account_key == normalized {
            return Ok(Some(row));
        }
        if row.email.as_deref() == Some(normalized.as_str()) {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

pub fn record_probe(conn: &Connection, update: ProbeUpdate<'_>) -> Result<()> {
    let now = epoch_now();
    conn.execute(
        r#"
        UPDATE accounts
        SET
            last_probe_at = ?2,
            last_probe_ok_at = CASE WHEN ?3 THEN ?2 ELSE last_probe_ok_at END,
            probe_fail_count = CASE WHEN ?3 THEN 0 ELSE probe_fail_count + 1 END,
            last_error = CASE WHEN ?3 THEN NULL ELSE ?4 END,
            last_error_kind = CASE WHEN ?3 THEN NULL ELSE ?15 END,
            plan = CASE WHEN ?3 THEN ?5 ELSE plan END,
            primary_used_percent = CASE WHEN ?3 THEN ?6 ELSE primary_used_percent END,
            secondary_used_percent = CASE WHEN ?3 THEN ?7 ELSE secondary_used_percent END,
            primary_resets_at = CASE WHEN ?3 THEN ?8 ELSE primary_resets_at END,
            credits_has_credits = CASE WHEN ?3 THEN ?9 ELSE credits_has_credits END,
            credits_unlimited = CASE WHEN ?3 THEN ?10 ELSE credits_unlimited END,
            snapshot_updated_at = CASE WHEN ?3 THEN ?2 ELSE snapshot_updated_at END,
            last_refresh = COALESCE(?11, last_refresh),
            last_refresh_ok_at = CASE WHEN ?3 AND ?11 IS NOT NULL THEN ?2 ELSE last_refresh_ok_at END,
            next_probe_due_at = ?12,
            next_refresh_due_at = ?13,
            cooldown_until = ?14,
            updated_at = ?2
        WHERE account_key = ?1
        "#,
        params![
            update.account_key,
            now,
            update.ok,
            update.error,
            update.plan,
            update.primary_used_percent,
            update.secondary_used_percent,
            update.primary_resets_at,
            update.credits_has_credits,
            update.credits_unlimited,
            update.observed_last_refresh,
            update.next_probe_due_at,
            update.next_refresh_due_at,
            update.cooldown_until,
            update.error_kind,
        ],
    )?;
    Ok(())
}

pub fn touch_refresh_failure(
    conn: &Connection,
    account_key: &str,
    error: &str,
    error_kind: &str,
    cooldown_until: i64,
) -> Result<()> {
    let now = epoch_now();
    conn.execute(
        r#"
        UPDATE accounts
        SET
            refresh_fail_count = refresh_fail_count + 1,
            last_error = ?2,
            last_error_kind = ?4,
            cooldown_until = ?5,
            updated_at = ?3
        WHERE account_key = ?1
        "#,
        params![account_key, error, now, error_kind, cooldown_until],
    )?;
    Ok(())
}

pub fn mark_switch(conn: &Connection, account_key: &str) -> Result<()> {
    let now = epoch_now();
    conn.execute(
        "UPDATE accounts SET updated_at = ?2, last_switched_at = ?2 WHERE account_key = ?1",
        params![account_key, now],
    )?;
    Ok(())
}

pub fn record_history(conn: &Connection, event: HistoryEvent<'_>) -> Result<()> {
    let now = epoch_now();
    conn.execute(
        r#"
        INSERT INTO history (account_key, kind, ok, message, detail_json, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        "#,
        params![
            event.account_key,
            event.kind.as_str(),
            event.ok,
            event.message,
            event.detail_json,
            now
        ],
    )?;
    Ok(())
}

pub fn due_accounts(conn: &Connection, now: i64) -> Result<Vec<AccountRow>> {
    let rows = list_accounts(conn)?;
    Ok(rows
        .into_iter()
        .filter(|row| {
            let not_cooling_down = row.cooldown_until.is_none_or(|ts| ts <= now);
            let refresh_due = row.next_refresh_due_at.is_none_or(|ts| ts <= now);
            let probe_due = row.next_probe_due_at.is_none_or(|ts| ts <= now);
            not_cooling_down && (refresh_due || probe_due)
        })
        .collect())
}

pub fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
