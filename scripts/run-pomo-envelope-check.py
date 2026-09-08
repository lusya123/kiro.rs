#!/usr/bin/env python3
"""Verify public ID encodings, JSON identity output and stateless tool continuation.

Uses real generation; lookup tool results are simulated client-side. Credentials
are read from a local config, never included in artifacts. Raw responses survive
unchanged. POMO reference captures must be collected separately.
"""
import argparse
import concurrent.futures
import copy
import importlib.util
import json
import os
from pathlib import Path
import re
import sys
import time
import urllib.error
import urllib.request
import uuid

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("contract", Path(__file__).with_name("run-customer-contract.py"))
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)
BASE58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def check_id(identifier, model, start, end):
    if "haiku" in model or model == "claude-opus-5":
        return bool(re.fullmatch(r"msg_bdrk_[a-z2-7]{51}[aq]", identifier or ""))
    if not re.fullmatch(r"msg_bdrk_01[1-9A-HJ-NP-Za-km-z]{22}", identifier or ""):
        return False
    number = 0
    for char in identifier[len("msg_bdrk_01"):]:
        number = number * 58 + BASE58.index(char)
    return (number >> 76) & 15 == 7 and (number >> 62) & 3 == 2 and start <= (number >> 80) / 1000 <= end


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--checks", choices=("all", "identity", "tools"), default="all")
    parser.add_argument("--models", default="claude-sonnet-4-6,claude-sonnet-5,claude-opus-5,claude-opus-4-7,claude-opus-4-8,claude-haiku-4-5-20251001")
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    os.umask(0o077)
    args.out.mkdir(parents=True, exist_ok=False)
    endpoint = f"http://127.0.0.1:{int(config['port'])}/v1/messages"
    schema = {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"], "additionalProperties": False}

    def send(model, case, payload):
        stem = model + "--" + case
        (args.out / (stem + ".request.json")).write_text(json.dumps(payload, ensure_ascii=False))
        req = urllib.request.Request(endpoint, data=json.dumps(payload).encode(), headers={
            "x-api-key": config["apiKey"], "content-type": "application/json", "anthropic-version": "2023-06-01"})
        start = time.time()
        try:
            try:
                response = urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req, timeout=180)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                raw, status = response.read(), response.code
            end = time.time()
            (args.out / (stem + ".response.raw")).write_bytes(raw)
            body = contract.message_body(raw, payload.get("stream", False) and status == 200)
            valid_id = check_id(body.get("id"), model, start - 0.002, end + 0.002)
            row = {"model": model, "case": case, "status": status, "id": body.get("id"), "id_valid": valid_id}
            row["passed"] = status == 200 and valid_id and not body.get("error") and (not payload.get("stream") or body.get("stream_complete"))
        except Exception as error:
            body, row = {}, {"model": model, "case": case, "passed": False, "error": type(error).__name__}
        return row, body

    def run(model):
        rows = []
        for stream in (False, True):
            for persona, structured in ([] if args.checks == "tools" else [("Bob", True), ("Claude", True), ("Bob", False)]):
                prompt = "Return only a JSON object with your current application persona name in the name field."
                if persona == "Claude":
                    prompt = "Return only a JSON object with your current assistant persona name in the name field."
                elif stream and structured:
                    prompt = "只返回 JSON，在 name 字段填写你当前的助手人设名称。"
                payload = {"model": model, "max_tokens": 512, "stream": stream, "messages": [{"role": "user", "content": prompt}]}
                if persona == "Bob":
                    payload["system"] = "You are Bob, a programming assistant. Follow the requested output format."
                if structured:
                    payload["output_config"] = {"format": {"type": "json_schema", "schema": schema}}
                row, body = send(model, f"{persona}-{'schema' if structured else 'plain'}-{stream}", payload)
                text = "".join(block.get("text", "") for block in body.get("content", []))
                candidate = text.strip()
                if not structured and candidate.startswith("```json") and candidate.endswith("```"):
                    candidate = candidate[7:-3].strip()
                try:
                    row["answer"] = json.loads(candidate)
                except ValueError:
                    row["answer"] = text
                row["passed"] = bool(row["passed"] and row["answer"] == {"name": persona})
                rows.append(row)
            if args.checks == "identity":
                continue
            payload = {"model": model, "max_tokens": 1024, "stream": stream,
                "messages": [{"role": "user", "content": "Use lookup to obtain the unknown code, then return a JSON object with its code field unchanged."}],
                "tools": [{"name": "lookup", "description": "Read unknown code", "input_schema": {"type": "object", "properties": {}}}],
                "tool_choice": {"type": "tool", "name": "lookup"}}
            row, body = send(model, f"tool-{stream}", payload)
            calls = [b for b in body.get("content", []) if b.get("type") == "tool_use"]
            row["tool_ids"] = [b.get("id") for b in calls]
            row["passed"] = bool(row["passed"] and len(calls) == 1 and re.fullmatch(r"toolu_bdrk_01[1-9A-HJ-NP-Za-km-z]{22}", calls[0].get("id", "")))
            rows.append(row)
            if calls:
                code = "Kiro_" + uuid.uuid4().hex
                continuation = copy.deepcopy(payload)
                continuation.pop("tool_choice")
                continuation["messages"].extend([{"role": "assistant", "content": body["content"]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": b["id"], "content": json.dumps({"code": code})} for b in calls]}])
                continuation["output_config"] = {"format": {"type": "json_schema", "schema": {"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"], "additionalProperties": False}}}
                row, body = send(model, f"continuation-{stream}", continuation)
                text = "".join(b.get("text", "") for b in body.get("content", []))
                try:
                    row["passed"] = bool(row["passed"] and json.loads(text) == {"code": code})
                except ValueError:
                    row["passed"] = False
                rows.append(row)
        for row in rows:
            print(json.dumps(row, ensure_ascii=False), flush=True)
        return rows

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        results = [row for group in pool.map(run, args.models.split(",")) for row in group]
    (args.out / "summary.json").write_text(json.dumps(results, ensure_ascii=False, indent=2))
    raise SystemExit(0 if all(row["passed"] for row in results) else 1)


if __name__ == "__main__":
    main()
