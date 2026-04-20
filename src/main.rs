mod auth;
mod config;
mod db;
mod probe;

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use rusqlite::Connection;
use serde_json::json;

use crate::auth::{FileLock, account_key, atomic_write_json, extract_meta, load_auth};
use crate::config::Paths;
use crate::db::{AccountRow, HistoryEvent, HistoryKind, ProbeUpdate, UpsertAccount};
#[derive(Parser)]
#[command(name = "codex-pool")]
#[command(about = "Codex account pool with isolated auth storage and SQLite state")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Add,
    Migrate,
    Ls(ListArgs),
    Refresh(RefreshArgs),
    Auto(RefreshArgs),
    Use(UseArgs),
    Daemon(DaemonArgs),
}

#[derive(Args, Clone)]
struct ListArgs {
    #[arg(long)]
    refresh: bool,
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    #[arg(long, default_value_t = 12)]
    timeout: u64,
}

#[derive(Args, Clone)]
struct RefreshArgs {
    #[arg(long)]
    selector: Option<String>,
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    #[arg(long, default_value_t = 12)]
    timeout: u64,
}

#[derive(Args, Clone)]
struct UseArgs {
    selector: String,
}

#[derive(Args, Clone)]
struct DaemonArgs {
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    #[arg(long, default_value_t = 12)]
    timeout: u64,
    #[arg(long, default_value_t = 300)]
    interval: u64,
    #[arg(long)]
    once: bool,
}

#[derive(Debug, Clone)]
struct RefreshResult {
    account_key: String,
    auth_path: PathBuf,
    email: Option<String>,
    ok: bool,
    line: String,
    plan: Option<String>,
    primary_used_percent: Option<i64>,
    secondary_used_percent: Option<i64>,
    primary_resets_at: Option<i64>,
    credits_has_credits: Option<bool>,
    credits_unlimited: Option<bool>,
    observed_last_refresh: Option<String>,
    rate_limits_available: bool,
    warnings: Vec<String>,
    failure_kind: Option<String>,
    skipped: bool,
    history_detail_json: String,
    score: (bool, bool, i64, i64, bool, bool, i64),
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;
    paths.ensure_layout()?;
    let conn = db::connect(&paths.db_path)?;

    match cli.command {
        Commands::Add => cmd_add(&paths, &conn),
        Commands::Migrate => cmd_migrate(&paths, &conn),
        Commands::Ls(args) => cmd_ls(&paths, &conn, args),
        Commands::Refresh(args) => cmd_refresh(&paths, &conn, args, false),
        Commands::Auto(args) => cmd_refresh(&paths, &conn, args, true),
        Commands::Use(args) => cmd_use(&paths, &conn, &args.selector),
        Commands::Daemon(args) => cmd_daemon(&paths, &conn, args),
    }
}

fn cmd_add(paths: &Paths, conn: &Connection) -> Result<()> {
    if !paths.global_auth_file.exists() {
        bail!(
            "current auth not found at {}",
            paths.global_auth_file.display()
        );
    }
    let auth = load_auth(&paths.global_auth_file)?;
    let meta = extract_meta(&auth);
    let account_key = account_key(&meta);
    let account_dir = paths.account_dir(&account_key);
    fs::create_dir_all(&account_dir)?;
    let auth_path = paths.account_auth_file(&account_key);
    atomic_write_json(&auth_path, &auth)?;

    db::upsert_account(
        conn,
        UpsertAccount {
            account_key: &account_key,
            email: meta.email.as_deref(),
            account_id: meta.account_id.as_deref(),
            auth_path: auth_path.to_string_lossy().as_ref(),
            last_refresh: meta.last_refresh.as_deref(),
        },
    )?;

    println!(
        "stored account={} email={} auth={}",
        account_key,
        meta.email.as_deref().unwrap_or("-"),
        auth_path.display()
    );
    db::record_history(
        conn,
        HistoryEvent {
            account_key: &account_key,
            kind: HistoryKind::Refresh,
            ok: true,
            message: "account imported into pool",
            detail_json: None,
        },
    )?;
    Ok(())
}

fn cmd_migrate(paths: &Paths, conn: &Connection) -> Result<()> {
    if !paths.legacy_accounts_dir.exists() {
        println!(
            "legacy pool not found at {}",
            paths.legacy_accounts_dir.display()
        );
        return Ok(());
    }
    let mut imported = 0usize;
    for entry in fs::read_dir(&paths.legacy_accounts_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let auth = load_auth(&path)?;
        let meta = extract_meta(&auth);
        let key = account_key(&meta);
        fs::create_dir_all(paths.account_dir(&key))?;
        let dst = paths.account_auth_file(&key);
        atomic_write_json(&dst, &auth)?;
        db::upsert_account(
            conn,
            UpsertAccount {
                account_key: &key,
                email: meta.email.as_deref(),
                account_id: meta.account_id.as_deref(),
                auth_path: dst.to_string_lossy().as_ref(),
                last_refresh: meta.last_refresh.as_deref(),
            },
        )?;
        imported += 1;
    }
    println!(
        "migrated {imported} accounts from {}",
        paths.legacy_accounts_dir.display()
    );
    Ok(())
}

fn cmd_ls(paths: &Paths, conn: &Connection, args: ListArgs) -> Result<()> {
    if args.refresh {
        let rows = select_rows(conn, None)?;
        let results = refresh_rows(
            paths,
            conn,
            rows,
            args.jobs,
            Duration::from_secs(args.timeout),
        )?;
        for line in results.into_iter().map(|row| row.line) {
            println!("{line}");
        }
        return Ok(());
    }

    let rows = db::list_accounts(conn)?;
    if rows.is_empty() {
        println!("account pool is empty; run `add` first");
        return Ok(());
    }
    for (index, row) in rows.iter().enumerate() {
        println!("{}", format_cached_row(index + 1, rows.len(), row));
    }
    Ok(())
}

fn cmd_refresh(
    paths: &Paths,
    conn: &Connection,
    args: RefreshArgs,
    auto_switch: bool,
) -> Result<()> {
    let rows = select_rows(conn, args.selector.as_deref())?;
    if rows.is_empty() {
        println!("no accounts matched");
        return Ok(());
    }
    let results = refresh_rows(
        paths,
        conn,
        rows,
        args.jobs,
        Duration::from_secs(args.timeout),
    )?;
    for result in &results {
        println!("{}", result.line);
    }

    if auto_switch {
        if let Some(best) = results
            .iter()
            .filter(|row| row.ok && !row.skipped)
            .min_by_key(|row| row.score.clone())
        {
            activate_account(paths, conn, &best.account_key, &best.auth_path)?;
            println!(
                "\nselected: {}",
                best.email.as_deref().unwrap_or(best.account_key.as_str())
            );
        } else {
            println!("\nno usable account found");
        }
    }
    Ok(())
}

fn cmd_daemon(paths: &Paths, conn: &Connection, args: DaemonArgs) -> Result<()> {
    let _daemon_guard = FileLock::try_acquire(&paths.daemon_lock_path)?.ok_or_else(|| {
        anyhow!(
            "daemon already running: {}",
            paths.daemon_lock_path.display()
        )
    })?;
    let interval = Duration::from_secs(args.interval.max(30));
    let timeout = Duration::from_secs(args.timeout);
    loop {
        let now = db::epoch_now();
        let rows = db::due_accounts(conn, now)?;
        if !rows.is_empty() {
            println!("daemon tick: {} accounts due", rows.len());
            let results = refresh_rows(paths, conn, rows, args.jobs, timeout)?;
            for result in results {
                println!("{}", result.line);
            }
        }
        if args.once {
            break;
        }
        thread::sleep(interval);
    }
    Ok(())
}

fn cmd_use(paths: &Paths, conn: &Connection, selector: &str) -> Result<()> {
    let row = db::get_account_by_selector(conn, selector)?
        .ok_or_else(|| anyhow!("account not found: {selector}"))?;
    activate_account(paths, conn, &row.account_key, Path::new(&row.auth_path))?;
    println!(
        "switched to {}",
        row.email.as_deref().unwrap_or(row.account_key.as_str())
    );
    Ok(())
}

fn activate_account(
    paths: &Paths,
    conn: &Connection,
    account_key: &str,
    source_auth: &Path,
) -> Result<()> {
    let _guard = FileLock::acquire(&paths.lock_path)?;
    let auth = load_auth(source_auth)?;
    atomic_write_json(&paths.global_auth_file, &auth)?;
    db::mark_switch(conn, account_key)?;
    db::record_history(
        conn,
        HistoryEvent {
            account_key,
            kind: HistoryKind::Switch,
            ok: true,
            message: "global auth switched",
            detail_json: None,
        },
    )?;
    Ok(())
}

fn select_rows(conn: &Connection, selector: Option<&str>) -> Result<Vec<AccountRow>> {
    match selector {
        Some(selector) => Ok(db::get_account_by_selector(conn, selector)?
            .into_iter()
            .collect()),
        None => db::list_accounts(conn),
    }
}

fn refresh_rows(
    paths: &Paths,
    conn: &Connection,
    rows: Vec<AccountRow>,
    jobs: usize,
    timeout: Duration,
) -> Result<Vec<RefreshResult>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let jobs = jobs.max(1);
    let mut results = Vec::with_capacity(rows.len());
    for chunk in rows.chunks(jobs) {
        let handles = chunk
            .iter()
            .cloned()
            .map(|row| {
                let paths = paths.clone();
                thread::spawn(move || refresh_one(&paths, row, timeout))
            })
            .collect::<Vec<_>>();

        for handle in handles {
            let result = handle
                .join()
                .map_err(|_| anyhow!("refresh worker panicked"))??;
            persist_refresh_result(conn, &result)?;
            results.push(result);
        }
    }
    Ok(results)
}

fn refresh_one(paths: &Paths, row: AccountRow, timeout: Duration) -> Result<RefreshResult> {
    let auth_path = PathBuf::from(&row.auth_path);
    let Some(_guard) = FileLock::try_acquire(&paths.account_lock_file(&row.account_key))? else {
        let line = format!(
            "[refresh] {}  status=skipped  reason=account lock busy",
            row.email.as_deref().unwrap_or(row.account_key.as_str())
        );
        return Ok(RefreshResult {
            account_key: row.account_key.clone(),
            auth_path,
            email: row.email.clone(),
            ok: true,
            line,
            plan: row.plan.clone(),
            primary_used_percent: row.primary_used_percent,
            secondary_used_percent: row.secondary_used_percent,
            primary_resets_at: row.primary_resets_at,
            credits_has_credits: row.credits_has_credits,
            credits_unlimited: row.credits_unlimited,
            observed_last_refresh: row.last_refresh.clone(),
            rate_limits_available: row.snapshot_updated_at.is_some(),
            warnings: vec!["account lock busy".to_string()],
            failure_kind: Some("lock_busy".to_string()),
            skipped: true,
            history_detail_json: json!({"warning":"account lock busy"}).to_string(),
            score: score(
                row.primary_used_percent,
                row.secondary_used_percent,
                row.primary_resets_at,
                row.credits_has_credits,
                row.credits_unlimited,
            ),
        });
    };
    match probe::run_probe(&auth_path, timeout) {
        Ok(output) => {
            atomic_write_json(&auth_path, &output.refreshed_auth)?;
            let primary_used_percent = if output.snapshot.rate_limits_available {
                output.snapshot.primary_used_percent
            } else {
                row.primary_used_percent
            };
            let secondary_used_percent = if output.snapshot.rate_limits_available {
                output.snapshot.secondary_used_percent
            } else {
                row.secondary_used_percent
            };
            let primary_resets_at = if output.snapshot.rate_limits_available {
                output.snapshot.primary_resets_at
            } else {
                row.primary_resets_at
            };
            let credits_has_credits = if output.snapshot.rate_limits_available {
                output.snapshot.credits_has_credits
            } else {
                row.credits_has_credits
            };
            let credits_unlimited = if output.snapshot.rate_limits_available {
                output.snapshot.credits_unlimited
            } else {
                row.credits_unlimited
            };
            let email = output
                .snapshot
                .account_meta
                .email
                .clone()
                .or(row.email.clone());
            let line = format_live_row(
                &row,
                email.as_deref(),
                output.snapshot.plan.as_deref(),
                primary_used_percent,
                secondary_used_percent,
                primary_resets_at,
                credits_has_credits,
                true,
                output.snapshot.warnings.first().map(String::as_str),
            );
            Ok(RefreshResult {
                account_key: row.account_key.clone(),
                auth_path,
                email,
                ok: true,
                line,
                plan: output.snapshot.plan.clone(),
                primary_used_percent,
                secondary_used_percent,
                primary_resets_at,
                credits_has_credits,
                credits_unlimited,
                observed_last_refresh: output.snapshot.account_meta.last_refresh.clone(),
                rate_limits_available: output.snapshot.rate_limits_available,
                warnings: output.snapshot.warnings.clone(),
                failure_kind: None,
                skipped: false,
                history_detail_json: json!({
                    "plan": output.snapshot.plan,
                    "primary_used_percent": primary_used_percent,
                    "secondary_used_percent": secondary_used_percent,
                    "primary_resets_at": primary_resets_at,
                    "credits_has_credits": credits_has_credits,
                    "credits_unlimited": credits_unlimited,
                    "observed_last_refresh": output.snapshot.account_meta.last_refresh,
                    "rate_limits_available": output.snapshot.rate_limits_available,
                    "warnings": output.snapshot.warnings,
                })
                .to_string(),
                score: score(
                    primary_used_percent,
                    secondary_used_percent,
                    primary_resets_at,
                    credits_has_credits,
                    credits_unlimited,
                ),
            })
        }
        Err(err) => {
            let message = err.to_string();
            let line = format_live_row(
                &row,
                row.email.as_deref(),
                row.plan.as_deref(),
                row.primary_used_percent,
                row.secondary_used_percent,
                row.primary_resets_at,
                row.credits_has_credits,
                false,
                Some(&message),
            );
            Ok(RefreshResult {
                account_key: row.account_key.clone(),
                auth_path,
                email: row.email.clone(),
                ok: false,
                line,
                plan: row.plan.clone(),
                primary_used_percent: row.primary_used_percent,
                secondary_used_percent: row.secondary_used_percent,
                primary_resets_at: row.primary_resets_at,
                credits_has_credits: row.credits_has_credits,
                credits_unlimited: row.credits_unlimited,
                observed_last_refresh: row.last_refresh.clone(),
                rate_limits_available: row.snapshot_updated_at.is_some(),
                warnings: vec![message.clone()],
                failure_kind: Some(err.kind.as_str().to_string()),
                skipped: false,
                history_detail_json: json!({
                    "error": message,
                    "error_kind": err.kind.as_str(),
                })
                .to_string(),
                score: score(
                    row.primary_used_percent,
                    row.secondary_used_percent,
                    row.primary_resets_at,
                    row.credits_has_credits,
                    row.credits_unlimited,
                ),
            })
        }
    }
}

fn persist_refresh_result(conn: &Connection, result: &RefreshResult) -> Result<()> {
    if result.skipped {
        db::record_history(
            conn,
            HistoryEvent {
                account_key: &result.account_key,
                kind: HistoryKind::Refresh,
                ok: true,
                message: "refresh skipped",
                detail_json: Some(&result.history_detail_json),
            },
        )?;
        return Ok(());
    }
    let now = db::epoch_now();
    let next_probe_due_at = compute_next_probe_due_at(
        now,
        result.ok,
        result.rate_limits_available,
        result.primary_used_percent,
    );
    let next_refresh_due_at =
        compute_next_refresh_due_at(now, result.ok, result.observed_last_refresh.as_deref());
    let cooldown_until = compute_cooldown_until(now, result.ok, result.failure_kind.as_deref());

    if result.ok {
        db::record_probe(
            conn,
            ProbeUpdate {
                account_key: &result.account_key,
                ok: true,
                error: None,
                plan: result.plan.as_deref(),
                primary_used_percent: result.primary_used_percent,
                secondary_used_percent: result.secondary_used_percent,
                primary_resets_at: result.primary_resets_at,
                credits_has_credits: result.credits_has_credits,
                credits_unlimited: result.credits_unlimited,
                observed_last_refresh: result.observed_last_refresh.as_deref(),
                next_probe_due_at,
                next_refresh_due_at,
                cooldown_until,
                error_kind: None,
            },
        )?;
        db::record_history(
            conn,
            HistoryEvent {
                account_key: &result.account_key,
                kind: HistoryKind::Probe,
                ok: true,
                message: "probe succeeded",
                detail_json: Some(&result.history_detail_json),
            },
        )?;
        db::record_history(
            conn,
            HistoryEvent {
                account_key: &result.account_key,
                kind: HistoryKind::Refresh,
                ok: true,
                message: "refresh persisted",
                detail_json: Some(&result.history_detail_json),
            },
        )?;
        if !result.warnings.is_empty() {
            db::record_history(
                conn,
                HistoryEvent {
                    account_key: &result.account_key,
                    kind: HistoryKind::Probe,
                    ok: true,
                    message: "probe completed with warnings",
                    detail_json: Some(&result.history_detail_json),
                },
            )?;
        }
    } else {
        let error_kind = result.failure_kind.as_deref().unwrap_or("refresh_failed");
        db::touch_refresh_failure(
            conn,
            &result.account_key,
            &result.line,
            error_kind,
            cooldown_until.unwrap_or(now + 300),
        )?;
        db::record_probe(
            conn,
            ProbeUpdate {
                account_key: &result.account_key,
                ok: false,
                error: Some(&result.line),
                plan: None,
                primary_used_percent: None,
                secondary_used_percent: None,
                primary_resets_at: None,
                credits_has_credits: None,
                credits_unlimited: None,
                observed_last_refresh: result.observed_last_refresh.as_deref(),
                next_probe_due_at,
                next_refresh_due_at,
                cooldown_until,
                error_kind: Some(error_kind),
            },
        )?;
        db::record_history(
            conn,
            HistoryEvent {
                account_key: &result.account_key,
                kind: HistoryKind::Probe,
                ok: false,
                message: "probe failed",
                detail_json: Some(&result.history_detail_json),
            },
        )?;
        db::record_history(
            conn,
            HistoryEvent {
                account_key: &result.account_key,
                kind: HistoryKind::Refresh,
                ok: false,
                message: "refresh failed",
                detail_json: Some(&result.history_detail_json),
            },
        )?;
    }
    Ok(())
}

fn format_cached_row(index: usize, total: usize, row: &AccountRow) -> String {
    let status = if row.cooldown_until.is_some_and(|ts| ts > db::epoch_now()) {
        "cooling_down"
    } else if row.probe_fail_count > 0 {
        "stale"
    } else {
        "cached"
    };
    format!(
        "[{index}/{total}] {}  account_id={}  plan={}  5h={} left  weekly={} left  reset_5h={}  credits={}  last_refresh={}  last_probe={}  last_ok={}  refresh_failures={}  snapshot={}  status={status}{}",
        row.email.as_deref().unwrap_or(row.account_key.as_str()),
        row.account_id.as_deref().unwrap_or("-"),
        row.plan.as_deref().unwrap_or("-"),
        percent_left(row.primary_used_percent),
        percent_left(row.secondary_used_percent),
        format_epoch(row.primary_resets_at),
        bool_to_yes_no(row.credits_has_credits),
        row.last_refresh.as_deref().unwrap_or("-"),
        format_epoch(row.last_probe_at),
        format_epoch(row.last_probe_ok_at.or(row.last_refresh_ok_at)),
        row.refresh_fail_count,
        format_epoch(row.snapshot_updated_at),
        format_error_suffix(row),
    )
}

fn format_live_row(
    row: &AccountRow,
    email: Option<&str>,
    plan: Option<&str>,
    primary_used_percent: Option<i64>,
    secondary_used_percent: Option<i64>,
    primary_resets_at: Option<i64>,
    credits_has_credits: Option<bool>,
    ok: bool,
    error: Option<&str>,
) -> String {
    if ok {
        let status = display_status(primary_used_percent, secondary_used_percent);
        let base = format!(
            "[refresh] {}  plan={}  5h={} left  weekly={} left  reset_5h={}  credits={}  status={status}",
            email.unwrap_or(row.account_key.as_str()),
            plan.unwrap_or("-"),
            percent_left(primary_used_percent),
            percent_left(secondary_used_percent),
            format_epoch(primary_resets_at),
            bool_to_yes_no(credits_has_credits),
        );
        error
            .map(|warning| format!("{base}  warning={warning}"))
            .unwrap_or(base)
    } else {
        format!(
            "[refresh] {}  status=error  reason={}",
            email.unwrap_or(row.account_key.as_str()),
            error.unwrap_or("unknown")
        )
    }
}

fn display_status(
    primary_used_percent: Option<i64>,
    secondary_used_percent: Option<i64>,
) -> &'static str {
    match (primary_used_percent, secondary_used_percent) {
        (None, None) => "quota_unknown",
        (Some(primary), _) if primary >= 100 => "full",
        (Some(_), _) => "usable",
        (None, Some(secondary)) if secondary >= 100 => "full",
        (None, Some(_)) => "quota_unknown",
    }
}

fn percent_left(used_percent: Option<i64>) -> String {
    used_percent
        .map(|used| (100 - used).max(0).to_string() + "%")
        .unwrap_or_else(|| "-".to_string())
}

fn bool_to_yes_no(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        _ => "no",
    }
}

fn format_epoch(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|ts| UNIX_EPOCH.checked_add(Duration::from_secs(ts as u64)))
        .map(chrono::DateTime::<chrono::Local>::from)
        .map(|dt| dt.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn format_error_suffix(row: &AccountRow) -> String {
    let mut suffix = String::new();
    if let Some(cooldown_until) = row.cooldown_until {
        suffix.push_str(&format!(
            "  cooldown_until={}",
            format_epoch(Some(cooldown_until))
        ));
    }
    if let Some(next_probe_due_at) = row.next_probe_due_at {
        suffix.push_str(&format!(
            "  next_probe={}",
            format_epoch(Some(next_probe_due_at))
        ));
    }
    if let Some(next_refresh_due_at) = row.next_refresh_due_at {
        suffix.push_str(&format!(
            "  next_refresh={}",
            format_epoch(Some(next_refresh_due_at))
        ));
    }
    if let Some(last_switched_at) = row.last_switched_at {
        suffix.push_str(&format!(
            "  last_switch={}",
            format_epoch(Some(last_switched_at))
        ));
    }
    row.last_error
        .as_deref()
        .map(|err| {
            let kind = row.last_error_kind.as_deref().unwrap_or("-");
            format!("{suffix}  last_error_kind={kind}  last_error={err}")
        })
        .unwrap_or(suffix)
}

fn compute_next_probe_due_at(
    now: i64,
    ok: bool,
    rate_limits_available: bool,
    primary_used_percent: Option<i64>,
) -> i64 {
    if !ok {
        return now + 300;
    }
    if !rate_limits_available {
        return now + 900;
    }
    match primary_used_percent.unwrap_or(100) {
        used if used >= 95 => now + 600,
        used if used >= 80 => now + 1800,
        _ => now + 3600,
    }
}

fn compute_next_refresh_due_at(now: i64, ok: bool, observed_last_refresh: Option<&str>) -> i64 {
    if !ok {
        return now + 300;
    }
    let stale_bonus = observed_last_refresh
        .and_then(parse_rfc3339_epoch)
        .map(|ts| now.saturating_sub(ts))
        .unwrap_or(0);
    if stale_bonus > 86_400 {
        now + 1800
    } else {
        now + 7200
    }
}

fn compute_cooldown_until(now: i64, ok: bool, failure_kind: Option<&str>) -> Option<i64> {
    if ok {
        return None;
    }
    let seconds = match failure_kind {
        Some("rate_limits_timeout") => 180,
        Some("account_read_timeout") => 300,
        Some("app_server_spawn") | Some("app_server_exit") => 600,
        Some("auth_file_read") => 900,
        Some("lock_busy") => 60,
        _ => 300,
    };
    Some(now + seconds)
}

fn parse_rfc3339_epoch(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp())
}

fn score(
    primary_used_percent: Option<i64>,
    secondary_used_percent: Option<i64>,
    primary_resets_at: Option<i64>,
    credits_has_credits: Option<bool>,
    credits_unlimited: Option<bool>,
) -> (bool, bool, i64, i64, bool, bool, i64) {
    let primary_known = primary_used_percent.is_some();
    let secondary_known = secondary_used_percent.is_some();
    let primary_used = primary_used_percent.unwrap_or(100);
    (
        !primary_known && !secondary_known,
        primary_used >= 100,
        primary_used,
        secondary_used_percent.unwrap_or(100),
        !credits_unlimited.unwrap_or(false),
        !credits_has_credits.unwrap_or(false),
        primary_resets_at.unwrap_or(i64::MAX),
    )
}
