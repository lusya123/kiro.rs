#!/usr/bin/env python3
"""Replay captured code identity requests, checking visible names and source syntax.

Never executes generated code. Python is parsed with ast; JavaScript uses node
--check when Node is installed. API keys stay in the local config file.
"""
import argparse
import ast
import concurrent.futures
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import urllib.error
import urllib.request

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("contract", Path(__file__).with_name("run-customer-contract.py"))
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)


def code_body(text):
    match = re.fullmatch(r"\s*```(?:python|javascript|js)?\s*\n(.*?)\n```\s*", text, re.S)
    return match.group(1) if match else text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--requests-dir", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    os.umask(0o077)
    args.out.mkdir(parents=True, exist_ok=False)
    node = shutil.which("node")

    def run(path):
        payload = json.loads(path.read_text())
        case = path.name.removesuffix(".request.json")
        expected = "Bob" if "bob" in case else "Claude"
        row = {"case": case, "model": payload["model"], "expected": expected, "passed": False}
        (args.out / path.name).write_text(json.dumps(payload, ensure_ascii=False, indent=2))
        request = urllib.request.Request(f"http://127.0.0.1:{config['port']}/v1/messages", data=json.dumps(payload).encode(), headers={
            "x-api-key": config["apiKey"], "content-type": "application/json", "anthropic-version": "2023-06-01"})
        try:
            try:
                response = urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=90)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                raw, status = response.read(), response.code
            (args.out / (case + ".response.raw")).write_bytes(raw)
            body = contract.message_body(raw, payload.get("stream", False) and status == 200)
            text = "".join(b.get("text", "") for b in body.get("content", []))
            source = code_body(text)
            row.update(status=status, text=text, contains_kiro="kiro" in text.lower())
            row["complete"] = not payload.get("stream") or body.get("stream_complete", False)
            if "python" in case:
                tree = ast.parse(source)
                values = [n.value.value for n in tree.body if isinstance(n, ast.Assign)
                    and any(isinstance(t, ast.Name) and t.id == "assistant_name" for t in n.targets)
                    and isinstance(n.value, ast.Constant)]
                row["syntax_checked"] = True
                row["name_correct"] = values == [expected]
            else:
                row["syntax_checked"] = bool(node)
                valid = bool(node) and subprocess.run([node, "--check"], input=source, text=True, capture_output=True, timeout=10).returncode == 0
                row["syntax_valid"] = valid
                row["name_correct"] = bool(re.search(r'\b(?:const|let|var)\s+assistantName\s*=\s*([\'"`])' + re.escape(expected) + r'\1\s*;', source))
            row["passed"] = status == 200 and row["complete"] and not row["contains_kiro"] and row["name_correct"] and row["syntax_checked"] and row.get("syntax_valid", True)
        except Exception as error:
            row["error"] = type(error).__name__
        print(json.dumps(row, ensure_ascii=False), flush=True)
        return row

    paths = sorted(args.requests_dir.glob("*.request.json"))
    if not paths:
        parser.error("No captured request files found")
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        rows = list(pool.map(run, paths))
    (args.out / "summary.json").write_text(json.dumps(rows, ensure_ascii=False, indent=2))
    raise SystemExit(0 if all(r["passed"] for r in rows) else 1)


if __name__ == "__main__":
    main()
