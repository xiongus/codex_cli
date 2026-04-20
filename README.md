# codex_cli

Rust implementation of the Codex account pool.

This version replaces the old Python script and fixes the core design problem:

- each managed account has its own persisted `auth.json`
- runtime state lives in SQLite
- probing happens in isolated temporary `CODEX_HOME` directories
- the global `~/.codex/auth.json` is only the currently activated projection

## Layout

Managed data is stored outside the repo under:

```text
~/.codex/account-pool-rs/
├── accounts/<account_key>/auth.json
└── state.db
```

## Commands

```bash
cargo run -- add
cargo run -- migrate
cargo run -- ls
cargo run -- ls --refresh
cargo run -- refresh
cargo run -- refresh --selector your@email.com
cargo run -- auto
cargo run -- use your@email.com
cargo run -- daemon --once
cargo run -- daemon --interval 300
```

After installation, the binary name is:

```bash
codex-pool
```

## What it does now

- imports the current `~/.codex/auth.json` into the managed pool
- probes accounts through `codex app-server`
- persists refreshed auth back to the account's own `auth.json`
- stores quota snapshots and probe status in SQLite
- switches the active global auth with a file lock and atomic rename
- probes accounts concurrently with `--jobs`
- migrates accounts from the legacy `~/.codex/account-pool/accounts` directory
- persists simple scheduling state in SQLite for next probe/refresh
- records probe/refresh/switch events in a `history` table
- provides a minimal `daemon` loop to refresh due accounts in the background
- enforces a singleton daemon lock and per-account refresh locks
- classifies refresh failures so cooldown behavior is not one-size-fits-all

## What is still missing

- smarter backoff tiers based on repeated failures
- separate refresh/probe job tables instead of per-account scheduling fields
- richer history inspection commands
- installable launchd/systemd-style service wrapper

The current daemon is intentionally simple: it scans due accounts from SQLite, respects cooldown windows, refreshes in parallel, and writes back the next due times.
