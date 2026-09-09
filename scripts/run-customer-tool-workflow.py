#!/usr/bin/env python3
"""Test real local model tool calls with simulated client-side read/save results.

Requires read -> save -> final JSON, with unpredictable read content. No filesystem
tool is executed and no successful save is inferred from an assistant's claim.
"""
import argparse
import concurrent.futures
import copy
import importlib.util
import json
import os
from pathlib import Path
import sys
import urllib.error
import urllib.request
import uuid

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "customer_contract", Path(__file__).with_name("run-customer-contract.py"))
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--models", default=(
        "claude-sonnet-4-6,claude-sonnet-5,claude-opus-5,claude-opus-4-7,claude-opus-4-8"))
    parser.add_argument("--thinking", choices=("none", "adaptive"), default="adaptive")
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    endpoint = f"http://127.0.0.1:{int(config['port'])}/v1/messages"
    os.umask(0o077)
    args.out.mkdir(parents=True, exist_ok=False)

    def send(model, stage, payload):
        stem = model + "--" + stage
        (args.out / (stem + ".request.json")).write_text(json.dumps(payload, indent=2))
        request = urllib.request.Request(endpoint, data=json.dumps(payload).encode(), headers={
            "x-api-key": config["apiKey"], "content-type": "application/json",
            "anthropic-version": "2023-06-01"})
        try:
            response = urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=180)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            raw, status = response.read(), response.code
        (args.out / (stem + ".response.raw")).write_bytes(raw)
        return status, contract.message_body(raw, status == 200)

    def run(model):
        rows = []
        source, destination = ".kiro/steering/source.json", ".kiro/specs/test.json"
        code = 'Kiro = "' + uuid.uuid4().hex + '"\n'
        expected = {"saved": True, "path": destination, "content": code}
        properties = {"path": {"type": "string"}, "content": {"type": "string"}}
        schema = {"type": "object", "properties": {"saved": {"type": "boolean"}, **properties},
                  "required": ["saved", "path", "content"], "additionalProperties": False}
        payload = {"model": model, "max_tokens": 2048, "stream": True, "messages": [{
            "role": "user", "content": f'Make exactly one read_fixture call for path "{source}", '
            f'then exactly one save_fixture call for path "{destination}" using the content returned '
            'by read_fixture verbatim. Do not guess the content. After both tools succeed, '
            'return the save result using the JSON schema.'}], "tools": [
                {"name": "read_fixture", "description": "Read fixture content.", "input_schema": {
                    "type": "object", "properties": {"path": properties["path"]},
                    "required": ["path"], "additionalProperties": False}},
                {"name": "save_fixture", "description": "Save fixture content.", "input_schema": {
                    "type": "object", "properties": properties, "required": ["path", "content"],
                    "additionalProperties": False}}],
            "output_config": {"format": {"type": "json_schema", "schema": schema}}}
        if args.thinking == "adaptive":
            payload["thinking"] = {"type": "adaptive"}
        stages = [("read", "read_fixture", {"path": source}, {"path": source, "content": code}),
                  ("write", "save_fixture", {"path": destination, "content": code}, expected),
                  ("final", None, None, None)]
        for stage, tool, arguments, result in stages:
            row = {"model": model, "stage": stage, "passed": False}
            try:
                status, body = send(model, stage, payload)
                row["status"] = status
                if status == 200 and body.get("stream_complete") and not body.get("error"):
                    calls = [b for b in body.get("content", []) if b.get("type") == "tool_use"]
                    if tool:
                        row["passed"] = (body.get("stop_reason") == "tool_use" and len(calls) == 1
                                         and calls[0].get("name") == tool and calls[0].get("input") == arguments)
                        if row["passed"]:
                            payload["messages"].extend([
                                {"role": "assistant", "content": copy.deepcopy(body["content"])},
                                {"role": "user", "content": [{"type": "tool_result",
                                    "tool_use_id": calls[0]["id"], "content": json.dumps(result)}]}])
                    else:
                        text = "".join(b.get("text", "") for b in body.get("content", []))
                        row["passed"] = not calls and body.get("stop_reason") == "end_turn" and json.loads(text) == expected
            except Exception as error:
                row["error_type"] = type(error).__name__
            rows.append(row)
            print(json.dumps(row), flush=True)
            if not row["passed"]:
                break
        return rows

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        rows = [row for group in pool.map(run, args.models.split(",")) for row in group]
    (args.out / "summary.json").write_text(json.dumps(rows, indent=2))
    raise SystemExit(0 if all(r["passed"] for r in rows) else 1)


if __name__ == "__main__":
    main()
