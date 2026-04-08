#!/usr/bin/env python3
import argparse
import base64
import json
import os
import re
import select
import shutil
import subprocess
import tempfile
import time
from datetime import datetime
from pathlib import Path


CODEX_HOME = Path.home() / ".codex"
AUTH_FILE = CODEX_HOME / "auth.json"
POOL_DIR = CODEX_HOME / "account-pool" / "accounts"
APP_SERVER_CMD = ["codex", "app-server", "--listen", "stdio://"]
PROBE_TIMEOUT_SECONDS = 12


def ensure_pool_dir() -> None:
    POOL_DIR.mkdir(parents=True, exist_ok=True)


def load_json(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def save_json(path: Path, data: dict) -> None:
    with path.open("w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)


def save_auth_copy(path: Path, data: dict) -> None:
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def decode_jwt_payload(token: str | None) -> dict:
    if not token or token.count(".") < 2:
        return {}
    payload = token.split(".")[1]
    payload += "=" * (-len(payload) % 4)
    try:
        decoded = base64.urlsafe_b64decode(payload.encode("utf-8"))
        return json.loads(decoded.decode("utf-8"))
    except (ValueError, json.JSONDecodeError):
        return {}


def extract_email(auth_data: dict) -> str | None:
    tokens = auth_data.get("tokens", {})
    for key in ("id_token", "access_token"):
        payload = decode_jwt_payload(tokens.get(key))
        email = payload.get("email")
        if email:
            return str(email).strip().lower()
        profile = payload.get("https://api.openai.com/profile", {})
        if isinstance(profile, dict) and profile.get("email"):
            return str(profile["email"]).strip().lower()
    return None


def extract_account_id(auth_data: dict) -> str | None:
    tokens = auth_data.get("tokens", {})
    account_id = tokens.get("account_id")
    if account_id:
        return str(account_id).strip()

    for key in ("id_token", "access_token"):
        payload = decode_jwt_payload(tokens.get(key))
        auth_claim = payload.get("https://api.openai.com/auth", {})
        if isinstance(auth_claim, dict):
            claim_account_id = auth_claim.get("chatgpt_account_id")
            if claim_account_id:
                return str(claim_account_id).strip()
    return None


def safe_account_filename(email: str | None, account_id: str | None) -> str:
    raw = email or account_id or f"account-{int(time.time())}"
    safe = re.sub(r"[^A-Za-z0-9._@-]+", "_", raw).strip("._")
    return safe or f"account-{int(time.time())}"


def format_local_time(timestamp: int | None) -> str:
    if not timestamp:
        return "-"
    return datetime.fromtimestamp(timestamp).strftime("%m-%d %H:%M")


def percent_left(window: dict | None) -> str:
    if not window:
        return "-"
    used = window.get("usedPercent")
    if used is None:
        return "-"
    left = max(0, 100 - int(used))
    return f"{left}%"


def build_probe_messages() -> list[dict]:
    return [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {"name": "codex-account-pool", "version": "0.1"},
                "capabilities": None,
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "account/read",
            "params": {"refreshToken": True},
        },
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "account/rateLimits/read",
            "params": None,
        },
    ]


def parse_json_lines(buffer: str, responses: dict[int, dict]) -> None:
    for line in buffer.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        message_id = message.get("id")
        if isinstance(message_id, int):
            responses[message_id] = message


def run_app_server_probe(auth_data: dict, timeout_seconds: int = PROBE_TIMEOUT_SECONDS) -> dict:
    with tempfile.TemporaryDirectory(prefix="codex-account-pool-") as temp_dir:
        temp_home = Path(temp_dir)
        save_auth_copy(temp_home / "auth.json", auth_data)

        env = os.environ.copy()
        env["CODEX_HOME"] = str(temp_home)

        process = subprocess.Popen(
            APP_SERVER_CMD,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
            text=True,
            bufsize=1,
        )

        try:
            assert process.stdin is not None
            assert process.stdout is not None

            for message in build_probe_messages():
                process.stdin.write(json.dumps(message) + "\n")
            process.stdin.flush()

            responses: dict[int, dict] = {}
            started_at = time.time()

            while time.time() - started_at < timeout_seconds:
                remaining = max(0.1, started_at + timeout_seconds - time.time())
                readable, _, _ = select.select([process.stdout], [], [], remaining)
                if not readable:
                    continue

                line = process.stdout.readline()
                if not line:
                    if process.poll() is not None:
                        break
                    continue
                parse_json_lines(line, responses)
                if 2 in responses and 3 in responses:
                    break

            account_result = responses.get(2, {}).get("result", {})
            limits_result = responses.get(3, {}).get("result", {})
            if not account_result or not limits_result:
                raise RuntimeError("官方额度接口没有在预期时间内返回结果")
            return {
                "account": account_result,
                "rate_limits": limits_result,
            }
        finally:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()


def get_primary_snapshot(rate_limits_result: dict) -> dict:
    by_limit_id = rate_limits_result.get("rateLimitsByLimitId")
    if isinstance(by_limit_id, dict) and by_limit_id:
        if "codex" in by_limit_id and isinstance(by_limit_id["codex"], dict):
            return by_limit_id["codex"]
        first_snapshot = next(iter(by_limit_id.values()))
        if isinstance(first_snapshot, dict):
            return first_snapshot
    snapshot = rate_limits_result.get("rateLimits", {})
    return snapshot if isinstance(snapshot, dict) else {}


def score_probe_result(snapshot: dict) -> tuple:
    primary = snapshot.get("primary") or {}
    secondary = snapshot.get("secondary") or {}
    credits = snapshot.get("credits") or {}

    primary_used = int(primary.get("usedPercent", 100))
    secondary_used = int(secondary.get("usedPercent", 100))
    primary_reset = int(primary.get("resetsAt") or 2**31 - 1)
    has_credits = bool(credits.get("hasCredits"))
    unlimited = bool(credits.get("unlimited"))

    return (
        primary_used >= 100,
        primary_used,
        secondary_used,
        not unlimited,
        not has_credits,
        primary_reset,
    )


def add_current_account() -> int:
    ensure_pool_dir()
    if not AUTH_FILE.exists():
        print("当前没有可添加的账号，未找到 ~/.codex/auth.json")
        return 1

    auth_data = load_json(AUTH_FILE)
    email = extract_email(auth_data)
    account_id = extract_account_id(auth_data)
    file_name = safe_account_filename(email, account_id) + ".json"
    dst = POOL_DIR / file_name

    save_auth_copy(dst, auth_data)
    print(f"已加入账号池: {email or account_id or file_name}")
    print(f"stored: {dst}")
    return 0


def auto_switch_account() -> int:
    ensure_pool_dir()
    accounts = sorted(POOL_DIR.glob("*.json"))
    if not accounts:
        print("账号池为空，请先执行 add")
        return 1

    current_auth = load_json(AUTH_FILE) if AUTH_FILE.exists() else None
    best_candidate = None

    print(f"检测账号池，共 {len(accounts)} 个账号")
    for index, account_path in enumerate(accounts, start=1):
        auth_data = load_json(account_path)
        stored_email = extract_email(auth_data) or account_path.stem

        try:
            probe_result = run_app_server_probe(auth_data)
            account = probe_result["account"].get("account") or {}
            rate_limits = probe_result["rate_limits"]
            snapshot = get_primary_snapshot(rate_limits)
            primary = snapshot.get("primary") or {}
            secondary = snapshot.get("secondary") or {}
            credits = snapshot.get("credits") or {}

            email = account.get("email") or stored_email
            plan = account.get("planType") or snapshot.get("planType") or "-"
            status = "usable" if int(primary.get("usedPercent", 100)) < 100 else "full"

            print(
                f"[{index}/{len(accounts)}] {email}  "
                f"plan={plan}  "
                f"5h={percent_left(primary)} left  "
                f"weekly={percent_left(secondary)} left  "
                f"reset_5h={format_local_time(primary.get('resetsAt'))}  "
                f"credits={'yes' if credits.get('hasCredits') else 'no'}  "
                f"status={status}"
            )

            candidate = {
                "path": account_path,
                "auth_data": auth_data,
                "email": email,
                "snapshot": snapshot,
                "score": score_probe_result(snapshot),
            }
            if best_candidate is None or candidate["score"] < best_candidate["score"]:
                best_candidate = candidate
        except Exception as exc:
            print(f"[{index}/{len(accounts)}] {stored_email}  status=error  reason={exc}")

    if best_candidate is None:
        print("没有探测到可切换的账号")
        return 1

    selected_auth = best_candidate["auth_data"]
    if current_auth == selected_auth:
        print(f"\nselected: {best_candidate['email']}")
        print("当前已经是最佳账号，无需切换")
        return 0

    shutil.copy2(best_candidate["path"], AUTH_FILE)
    print(f"\nselected: {best_candidate['email']}")
    print(f"switched auth -> {AUTH_FILE}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Codex 账号池 CLI")
    subparsers = parser.add_subparsers(dest="command")

    subparsers.add_parser("add", help="将当前账号加入账号池")
    subparsers.add_parser("auto", help="自动选择当前最合适的账号并切换")

    args = parser.parse_args()

    if args.command == "add":
        return add_current_account()
    if args.command == "auto":
        return auto_switch_account()

    parser.print_help()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
