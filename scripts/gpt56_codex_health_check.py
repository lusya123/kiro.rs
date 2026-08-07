#!/usr/bin/env python3
"""Report-only GPT-5.6/Codex compatibility check for Sub2API Kiro accounts.

The probe deliberately exercises the two fields that have caused production
502s: a replayed Responses ``reasoning`` item and ``text.format.strict``. It
never changes account state and never writes API keys to reports.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import datetime as dt
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


DEFAULT_ACCOUNT_REGEX = r"^gpt-(?:5189[2-9]|519[0-9]{2})$"
DEFAULT_ADMIN_BASE = "http://127.0.0.1:8080/api/v1/admin"
DEFAULT_MODEL = "gpt-5.6-sol"
USER_AGENT = "kiro-rs-gpt56-codex-health-check/1.0"
SENTINEL = "KIRO_CODEX_HEALTH_OK"


@dataclasses.dataclass(frozen=True)
class HttpResult:
    status: int
    body: str
    content_type: str


def responses_url(base_url: str) -> str:
    base = str(base_url or "").rstrip("/")
    if not base:
        return ""
    return f"{base}/responses" if base.endswith("/v1") else f"{base}/v1/responses"


def secret_file_is_private(path: Path) -> bool:
    return os.name == "nt" or path.stat().st_mode & 0o077 == 0


def read_json_file(path: Path, *, may_contain_secrets: bool = False) -> Any:
    if may_contain_secrets and not secret_file_is_private(path):
        raise RuntimeError(f"secret-bearing file must have mode 0600: {path}")
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


def http_request(
    url: str,
    *,
    headers: dict[str, str],
    payload: dict[str, Any] | None = None,
    timeout: float = 30.0,
) -> HttpResult:
    data = None
    method = "GET"
    if payload is not None:
        data = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        method = "POST"
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    # These targets are intentionally local/private. Never send them through a
    # workstation or server-wide HTTP proxy.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(request, timeout=timeout) as response:
            body = response.read().decode("utf-8", "replace")
            return HttpResult(
                status=response.status,
                body=body,
                content_type=response.headers.get("content-type", ""),
            )
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", "replace")
        return HttpResult(
            status=exc.code,
            body=body,
            content_type=exc.headers.get("content-type", "") if exc.headers else "",
        )


def admin_get(url: str, admin_key: str, timeout: float) -> dict[str, Any]:
    result = http_request(
        url,
        headers={
            "Accept": "application/json",
            "User-Agent": USER_AGENT,
            "x-api-key": admin_key,
        },
        timeout=timeout,
    )
    if result.status // 100 != 2:
        raise RuntimeError(f"admin API returned HTTP {result.status}")
    parsed = json.loads(result.body)
    if not isinstance(parsed, dict):
        raise RuntimeError("admin API response is not a JSON object")
    return parsed


def fetch_accounts(
    admin_base: str,
    admin_key: str,
    *,
    interval: float,
    timeout: float,
) -> list[dict[str, Any]]:
    accounts: list[dict[str, Any]] = []
    page = 1
    while True:
        if page > 1 and interval > 0:
            time.sleep(interval)
        query = urllib.parse.urlencode({"page": page, "page_size": 50})
        response = admin_get(f"{admin_base.rstrip('/')}/accounts?{query}", admin_key, timeout)
        items = (response.get("data") or {}).get("items")
        if not isinstance(items, list):
            raise RuntimeError(f"admin account page {page} has no data.items array")
        accounts.extend(item for item in items if isinstance(item, dict))
        if len(items) < 50:
            return accounts
        page += 1


def credential_dict(account: dict[str, Any]) -> dict[str, Any]:
    credentials = account.get("credentials") or {}
    if isinstance(credentials, dict):
        return credentials
    if isinstance(credentials, list):
        for item in credentials:
            if isinstance(item, dict) and item.get("base_url"):
                return item
    return {}


def enrich_credentials_from_postgres(
    accounts: list[dict[str, Any]],
    *,
    container: str,
    database: str,
    user: str,
    timeout: float,
) -> int:
    ids = sorted(
        {
            int(account["id"])
            for account in accounts
            if str(account.get("id", "")).isdigit()
        }
    )
    if not ids:
        return 0
    sql = (
        "SELECT COALESCE(json_agg(json_build_object("
        "'id',id,'base_url',credentials->>'base_url','api_key',credentials->>'api_key'"
        ")), '[]'::json) FROM accounts WHERE deleted_at IS NULL AND id IN ("
        + ",".join(str(account_id) for account_id in ids)
        + ");"
    )
    command = [
        "docker",
        "exec",
        container,
        "psql",
        "-U",
        user,
        "-d",
        database,
        "-t",
        "-A",
        "-c",
        sql,
    ]
    completed = subprocess.run(
        command,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    if completed.returncode != 0:
        error = completed.stderr.strip().splitlines()
        detail = error[-1][:200] if error else "unknown docker/psql error"
        raise RuntimeError(f"credential enrichment failed: {detail}")
    rows = json.loads(completed.stdout.strip() or "[]")
    by_id = {
        int(row["id"]): row
        for row in rows
        if isinstance(row, dict) and str(row.get("id", "")).isdigit()
    }
    enriched = 0
    for account in accounts:
        row = by_id.get(int(account["id"])) if str(account.get("id", "")).isdigit() else None
        if not row:
            continue
        credentials = credential_dict(account)
        if not isinstance(account.get("credentials"), dict):
            credentials = {}
            account["credentials"] = credentials
        for key in ("base_url", "api_key"):
            if row.get(key) and not credentials.get(key):
                credentials[key] = row[key]
                enriched += 1
    return enriched


def merge_credentials_file(accounts: list[dict[str, Any]], path: Path) -> int:
    raw = read_json_file(path, may_contain_secrets=True)
    if not isinstance(raw, dict):
        raise RuntimeError("credentials file must be an object keyed by account id or name")
    merged = 0
    for account in accounts:
        override = raw.get(str(account.get("id"))) or raw.get(str(account.get("name")))
        if not isinstance(override, dict):
            continue
        credentials = credential_dict(account)
        if not isinstance(account.get("credentials"), dict):
            credentials = {}
            account["credentials"] = credentials
        for key in ("base_url", "api_key"):
            if override.get(key):
                credentials[key] = override[key]
                merged += 1
    return merged


def codex_probe_payload(model: str, account_id: int | str) -> dict[str, Any]:
    return {
        "model": model,
        "input": [
            {
                "type": "reasoning",
                "id": f"rs_health_{account_id}",
                "summary": [],
                "content": [],
                "encrypted_content": "opaque-health-check-reasoning",
                "status": "completed",
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": f"Reply exactly {SENTINEL}"}],
            },
        ],
        "include": ["reasoning.encrypted_content"],
        "reasoning": {"effort": "low", "summary": "auto"},
        "text": {"format": {"type": "text", "strict": True}},
        "max_output_tokens": 32,
        "store": False,
        "stream": True,
    }


def parse_sse(body: str) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    for line in body.splitlines():
        line = line.strip()
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if not data or data == "[DONE]":
            continue
        try:
            event = json.loads(data)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict):
            events.append(event)
    return events


def response_objects(result: HttpResult) -> list[dict[str, Any]]:
    events = parse_sse(result.body)
    if events:
        return [
            event.get("response") if isinstance(event.get("response"), dict) else event
            for event in events
        ]
    try:
        parsed = json.loads(result.body)
    except json.JSONDecodeError:
        return []
    return [parsed] if isinstance(parsed, dict) else []


def output_text(value: Any) -> str:
    parts: list[str] = []

    def visit(node: Any) -> None:
        if isinstance(node, dict):
            if node.get("type") in {"output_text", "text"} and isinstance(node.get("text"), str):
                parts.append(node["text"])
            for key, child in node.items():
                if key not in {"encrypted_content", "reasoning"}:
                    visit(child)
        elif isinstance(node, list):
            for child in node:
                visit(child)

    visit(value)
    return "".join(parts)


def classify_failure(status: int | None, detail: str) -> str:
    normalized = detail.lower()
    if "input item type" in normalized and "reasoning" in normalized:
        return "protocol_reasoning_unsupported"
    if "text.format.strict" in normalized:
        return "protocol_text_strict_unsupported"
    if "temporarily suspended" in normalized or "user id" in normalized and "suspend" in normalized:
        return "upstream_account_suspended"
    if "all credentials" in normalized or "no available accounts" in normalized:
        return "credentials_unavailable"
    if "connection refused" in normalized or "timed out" in normalized or "unreachable" in normalized:
        return "endpoint_unreachable"
    if status in {401, 403}:
        return "authentication_or_permission"
    if status == 429 or "rate limit" in normalized or "too many request" in normalized:
        return "rate_limited"
    if status is not None and status >= 500:
        return "upstream_server_error"
    if status is not None and status >= 400:
        return "invalid_request_or_unsupported"
    return "invalid_response"


def redact(text: str, secrets: list[str]) -> str:
    redacted = text
    for secret in secrets:
        if secret:
            redacted = redacted.replace(secret, "<redacted>")
    return redacted[:600].replace("\n", " ")


def probe_account(account: dict[str, Any], model: str, timeout: float) -> dict[str, Any]:
    started = time.monotonic()
    account_id = account.get("id")
    name = str(account.get("name") or "")
    credentials = credential_dict(account)
    base_url = str(credentials.get("base_url") or "")
    api_key = str(credentials.get("api_key") or credentials.get("key") or "")
    common = {
        "id": account_id,
        "name": name,
        "model": model,
        "schedulable": account.get("schedulable") is True,
        "base_url": base_url,
    }
    if not base_url or not api_key:
        return {
            **common,
            "verdict": "unknown",
            "category": "missing_direct_credentials",
            "reason": "direct base_url/api_key is unavailable",
            "elapsed_ms": round((time.monotonic() - started) * 1000),
        }
    try:
        result = http_request(
            responses_url(base_url),
            headers={
                "Authorization": f"Bearer {api_key}",
                "Content-Type": "application/json",
                "Accept": "text/event-stream, application/json",
                "User-Agent": USER_AGENT,
            },
            payload=codex_probe_payload(model, account_id),
            timeout=timeout,
        )
    except (OSError, urllib.error.URLError, TimeoutError) as exc:
        detail = redact(str(exc), [api_key])
        return {
            **common,
            "verdict": "fail",
            "category": classify_failure(None, detail),
            "reason": detail,
            "elapsed_ms": round((time.monotonic() - started) * 1000),
        }

    objects = response_objects(result)
    completed = any(
        value.get("status") == "completed"
        or value.get("type") == "response.completed"
        or value.get("object") == "response" and value.get("status") == "completed"
        for value in objects
    )
    observed_text = "".join(output_text(value) for value in objects)
    if result.status // 100 == 2 and completed and SENTINEL in observed_text:
        return {
            **common,
            "verdict": "ok",
            "category": "codex_compatible",
            "http_status": result.status,
            "elapsed_ms": round((time.monotonic() - started) * 1000),
        }
    detail = redact(result.body, [api_key])
    category = "sentinel_mismatch" if completed else classify_failure(result.status, detail)
    return {
        **common,
        "verdict": "fail",
        "category": category,
        "http_status": result.status,
        "reason": detail,
        "elapsed_ms": round((time.monotonic() - started) * 1000),
    }


def probe_with_retries(
    account: dict[str, Any], model: str, timeout: float, retries: int
) -> dict[str, Any]:
    attempts = []
    for attempt in range(retries + 1):
        result = probe_account(account, model, timeout)
        attempts.append(result)
        if result["verdict"] in {"ok", "unknown"}:
            break
        if attempt < retries:
            time.sleep(min(2**attempt, 5))
    final = dict(attempts[-1])
    final["attempts"] = len(attempts)
    if final["verdict"] == "ok" and len(attempts) > 1:
        final["transient_failures"] = [attempt["category"] for attempt in attempts[:-1]]
    return final


def write_report(report: dict[str, Any], out_dir: Path) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    timestamp = report["started_at"].replace(":", "").replace("-", "")[:15]
    destination = out_dir / f"run-{timestamp}.json"
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=out_dir, prefix=".gpt56-", delete=False
    ) as handle:
        json.dump(report, handle, ensure_ascii=False, indent=2)
        handle.write("\n")
        temporary = Path(handle.name)
    os.replace(temporary, destination)
    latest_temporary = out_dir / ".latest.json.tmp"
    latest_temporary.write_text(destination.read_text(encoding="utf-8"), encoding="utf-8")
    os.replace(latest_temporary, out_dir / "latest.json")
    return destination


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--admin-base", default=os.getenv("SUB2API_ADMIN_BASE", DEFAULT_ADMIN_BASE))
    parser.add_argument("--admin-key-env", default="SUB2API_ADMIN_KEY")
    parser.add_argument("--accounts-file", type=Path)
    parser.add_argument("--credentials-file", type=Path)
    parser.add_argument("--no-db-enrich", action="store_true")
    parser.add_argument("--postgres-container", default="sub2api-postgres")
    parser.add_argument("--postgres-db", default="sub2api")
    parser.add_argument("--postgres-user", default="sub2api")
    parser.add_argument("--account-regex", default=DEFAULT_ACCOUNT_REGEX)
    parser.add_argument("--model", action="append", default=[])
    parser.add_argument("--parallel", type=int, default=12)
    parser.add_argument("--timeout", type=float, default=45.0)
    parser.add_argument("--retries", type=int, default=1)
    parser.add_argument(
        "--admin-request-interval",
        type=float,
        default=float(os.getenv("SUB2API_ADMIN_REQUEST_INTERVAL", "2.2")),
    )
    parser.add_argument("--out-dir", type=Path, default=Path("./gpt56-health-check-runs"))
    parser.add_argument("--exit-zero", action="store_true", help="always exit 0 after writing a report")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.parallel < 1 or args.retries < 0 or args.timeout <= 0:
        raise SystemExit("parallel/timeout must be positive and retries must be non-negative")
    account_pattern = re.compile(args.account_regex)
    models = args.model or [DEFAULT_MODEL]
    started_at = dt.datetime.now(dt.timezone.utc).astimezone().isoformat(timespec="seconds")
    started = time.monotonic()

    if args.accounts_file:
        raw_accounts = read_json_file(args.accounts_file, may_contain_secrets=True)
        if isinstance(raw_accounts, dict):
            raw_accounts = raw_accounts.get("accounts") or raw_accounts.get("data") or []
        if not isinstance(raw_accounts, list):
            raise RuntimeError("accounts file must contain an array")
        accounts = [account for account in raw_accounts if isinstance(account, dict)]
    else:
        admin_key = os.getenv(args.admin_key_env, "")
        if not admin_key:
            raise RuntimeError(f"required environment variable is empty: {args.admin_key_env}")
        accounts = fetch_accounts(
            args.admin_base,
            admin_key,
            interval=args.admin_request_interval,
            timeout=args.timeout,
        )

    selected = [
        account
        for account in accounts
        if account.get("status") == "active" and account_pattern.search(str(account.get("name") or ""))
    ]
    if not selected:
        raise RuntimeError(
            f"no active accounts matched --account-regex {args.account_regex!r}; refusing to report a false green"
        )
    if args.credentials_file:
        merge_credentials_file(selected, args.credentials_file)
    elif not args.no_db_enrich and selected:
        enrich_credentials_from_postgres(
            selected,
            container=args.postgres_container,
            database=args.postgres_db,
            user=args.postgres_user,
            timeout=args.timeout,
        )

    jobs = [(account, model) for account in selected for model in models]
    results: list[dict[str, Any]] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.parallel) as pool:
        futures = [
            pool.submit(probe_with_retries, account, model, args.timeout, args.retries)
            for account, model in jobs
        ]
        for future in concurrent.futures.as_completed(futures):
            results.append(future.result())
    results.sort(key=lambda item: (str(item.get("name")), str(item.get("model"))))

    totals = {
        "accounts_fetched": len(accounts),
        "accounts_selected": len(selected),
        "probes": len(results),
        "ok": sum(result["verdict"] == "ok" for result in results),
        "failed": sum(result["verdict"] == "fail" for result in results),
        "unknown": sum(result["verdict"] == "unknown" for result in results),
    }
    report = {
        "schema_version": 1,
        "probe": "gpt56_codex_responses_compatibility",
        "report_only": True,
        "started_at": started_at,
        "finished_at": dt.datetime.now(dt.timezone.utc).astimezone().isoformat(timespec="seconds"),
        "duration_seconds": round(time.monotonic() - started, 3),
        "account_regex": args.account_regex,
        "models": models,
        "totals": totals,
        "results": results,
    }
    report_path = write_report(report, args.out_dir)
    print(
        f"GPT-5.6 Codex check: accounts={totals['accounts_selected']} "
        f"probes={totals['probes']} ok={totals['ok']} failed={totals['failed']} "
        f"unknown={totals['unknown']} report={report_path}"
    )
    for result in results:
        if result["verdict"] != "ok":
            print(
                f"- {result['verdict']}: id={result.get('id')} name={result.get('name')} "
                f"model={result.get('model')} category={result.get('category')}"
            )
    unhealthy = totals["failed"] + totals["unknown"]
    return 0 if args.exit_zero or unhealthy == 0 else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, json.JSONDecodeError, re.error) as exc:
        print(f"fatal: {exc}", file=sys.stderr)
        raise SystemExit(2)
