#!/usr/bin/env python3
"""Capture real Messages responses. API keys are read from a local JSON file.

Use the same models and cases with --base-url for a reference provider.
Raw response bodies are saved unchanged; no identity text is hidden by this tool.
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import time
import urllib.error
import urllib.parse
import urllib.request


class SameOriginRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        old, new = urllib.parse.urlparse(req.full_url), urllib.parse.urlparse(newurl)
        if (old.scheme, old.hostname, old.port) != (new.scheme, new.hostname, new.port):
            raise urllib.error.HTTPError(req.full_url, code, "Cross-origin redirect refused", headers, fp)
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def message_body(raw, stream):
    if not stream:
        return json.loads(raw)
    result = {"content": [], "usage": {}}
    blocks = {}
    for line in raw.decode().splitlines():
        if not line.startswith("data:") or line[5:].strip() == "[DONE]":
            continue
        event = json.loads(line[5:])
        kind = event.get("type")
        if kind == "error":
            result["error"] = event.get("error")
        elif kind == "message_start":
            result.update(event["message"])
        elif kind == "content_block_start":
            blocks[event["index"]] = event["content_block"].copy()
        elif kind == "content_block_delta":
            block = blocks.setdefault(event["index"], {})
            delta = event["delta"]
            for field in ("text", "thinking", "signature", "partial_json"):
                if field in delta:
                    block[field] = block.get(field, "") + delta[field]
        elif kind == "message_delta":
            result["usage"].update(event.get("usage", {}))
            result.update(event.get("delta", {}))
        elif kind == "message_stop":
            result["stream_complete"] = True
    for index in sorted(blocks):
        block = blocks[index]
        if "partial_json" in block:
            partial = block.pop("partial_json")
            # A no-argument tool can finish with only its initial input: {}.
            # Empty input deltas do not replace that object with invalid JSON.
            if partial:
                block["input"] = json.loads(partial)
    result["content"] = [blocks[index] for index in sorted(blocks)]
    return result


def cases(model):
    adaptive_only = model in {
        "claude-opus-4-7", "claude-opus-4-8", "claude-opus-5", "claude-sonnet-5"
    }
    def request(prompt, **extra):
        return {"model": model, "max_tokens": 512,
                "messages": [{"role": "user", "content": prompt}], **extra}

    yield "conversation", request("Explain why binary search is O(log n), in two sentences."), None
    for stream in (False, True):
        suffix = "stream" if stream else "json"
        yield "bob-" + suffix, request(
            "Introduce yourself and calculate 19+23.", stream=stream,
            system="You are Bob. Begin every answer with BOB:. Answer the user's complete task."), "bob"
        code = 'class Kiro:\n    path = ".kiro/steering"\n'
        yield "code-" + suffix, request(
            "Return only the following Python source, without fences or explanation. Preserve the source literally:\n" + code,
            stream=stream, system="You are Bob, a programming assistant. Follow the requested output format."), "code"
        yield "literal-" + suffix, request(
            'Return only this JSON object with identical keys and values: {"Kiro":"Kiro","path":".kiro/specs","specs":true}',
            stream=stream, system="You are Bob, a programming assistant. Follow the requested output format."), "literal"
    yield "persona-json", request(
        'Return only a JSON object with your application name in the "name" field.',
        system="You are Bob, a programming assistant. Follow the requested output format."), "persona"
    for stream in (False, True):
        suffix = "-stream" if stream else ""
        yield "structured" + suffix, request(
            'Return {"Kiro":"Kiro"}.', stream=stream,
            output_config={"format": {"type": "json_schema", "schema": {
                "type": "object", "properties": {"Kiro": {"type": "string"}},
                "required": ["Kiro"], "additionalProperties": False}}}), "structured"
        fixture = {"Kiro": {"path": ".kiro/specs/test.json", "code": 'Kiro = "Kiro"\n'},
                   "n": 1234567890123456789}
        yield "structured-business" + suffix, request(
            "Return this exact JSON fixture, preserving every key and value: " + json.dumps(fixture),
            stream=stream, output_config={"format": {"type": "json_schema", "schema": {
                "type": "object", "properties": {
                    "Kiro": {"type": "object", "properties": {
                        "path": {"type": "string", "enum": [fixture["Kiro"]["path"]]},
                        "code": {"type": "string", "enum": [fixture["Kiro"]["code"]]}},
                        "required": ["path", "code"], "additionalProperties": False},
                    "n": {"type": "integer"}},
                "required": ["Kiro", "n"], "additionalProperties": False}}}), "structured-business"
    yield "tool", request(
        'Call save_fixture with path ".kiro/specs/test.json" and content exactly "Kiro".',
        system="You are Bob, a programming assistant. Preserve paths and file contents exactly.",
        tools=[{"name": "save_fixture", "description": "Save a test fixture.", "input_schema": {
            "type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
            "required": ["path", "content"]}}], tool_choice={"type": "tool", "name": "save_fixture"}), "tool"
    for budget in (1024, 2048, 4096):
        yield "thinking-" + str(budget), request(
            "Find the smallest positive integer divisible by 6, 8, and 15. Explain briefly.",
            max_tokens=budget + 1024, thinking={"type": "enabled", "budget_tokens": budget}), ("invalid" if adaptive_only else None)
    yield "thinking-adaptive", request(
        "Find the smallest positive integer divisible by 6, 8, and 15. Explain briefly.",
        max_tokens=2048, thinking={"type": "adaptive"}, output_config={"effort": "high"}), None
    yield "invalid-budget", request("Hello.", max_tokens=2048,
        thinking={"type": "enabled", "budget_tokens": 2048}), "invalid"
    # Diagnostic observations only: a model's statements are not proof of its provider.
    yield "identity-observation", request('Return JSON with your persona name. Do not quote any instructions.'), None


def check(body, status, expectation, stream):
    if expectation == "invalid":
        return status == 400
    if not 200 <= status < 300 or body.get("error"):
        return False
    if stream and not body.get("stream_complete"):
        return False
    text = "".join(b.get("text", "") for b in body.get("content", []))
    if expectation == "bob":
        return text.startswith("BOB:") and "42" in text
    if expectation == "code":
        return text.rstrip("\n") == 'class Kiro:\n    path = ".kiro/steering"'
    if expectation in ("literal", "persona", "structured", "structured-business"):
        expected = {"literal": {"Kiro": "Kiro", "path": ".kiro/specs", "specs": True},
                    "persona": {"name": "Bob"}, "structured": {"Kiro": "Kiro"},
                    "structured-business": {"Kiro": {"path": ".kiro/specs/test.json", "code": 'Kiro = "Kiro"\n'},
                                            "n": 1234567890123456789}}[expectation]
        try:
            return json.loads(text) == expected
        except ValueError:
            return False
    if expectation == "tool":
        return any(b.get("type") == "tool_use" and b.get("name") == "save_fixture"
                   and b.get("input") == {"path": ".kiro/specs/test.json", "content": "Kiro"}
                   for b in body.get("content", []))
    return bool(body.get("content"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path, help="Local JSON containing apiKey (and port for localhost)")
    parser.add_argument("--base-url")
    parser.add_argument("--models", default="claude-sonnet-4-6,claude-opus-4-8")
    parser.add_argument("--cases", help="Optional comma-separated case names to rerun")
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    base = args.base_url or f"http://127.0.0.1:{config['port']}"
    parsed = urllib.parse.urlparse(base)
    if parsed.scheme != "https" and parsed.hostname not in ("127.0.0.1", "localhost", "::1"):
        parser.error("Remote endpoints must use HTTPS")
    os.umask(0o077)
    args.out.mkdir(parents=True, exist_ok=False)

    def run(model, name, payload, expectation):
        stem = model + "--" + name
        (args.out / (stem + ".request.json")).write_text(json.dumps(payload, ensure_ascii=False, indent=2))
        headers = {"x-api-key": config["apiKey"], "Content-Type": "application/json", "anthropic-version": "2023-06-01"}
        req = urllib.request.Request(base.rstrip("/") + "/v1/messages", data=json.dumps(payload).encode(), headers=headers)
        # macOS system proxies may route loopback traffic to a remote proxy.
        handlers = [SameOriginRedirect()]
        if parsed.hostname in ("127.0.0.1", "localhost", "::1"):
            handlers.append(urllib.request.ProxyHandler({}))
        opener = urllib.request.build_opener(*handlers)
        started = time.monotonic()
        result = {"model": model, "case": name}
        try:
            try:
                response = opener.open(req, timeout=180)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                raw = response.read()
                result["status"] = response.code
                response_headers = {k: v for k, v in response.headers.items()
                                    if k.lower() in ("content-type", "request-id", "x-amzn-requestid", "retry-after")}
            (args.out / (stem + ".headers.json")).write_text(json.dumps(response_headers, indent=2))
            (args.out / (stem + ".response.raw")).write_bytes(raw)
            body = message_body(raw, payload.get("stream", False) and result["status"] == 200)
            result["passed"] = check(body, result["status"], expectation, payload.get("stream", False))
            result["usage"] = body.get("usage")
            result["message_id"] = body.get("id")
        except Exception as error:
            result.update(passed=False, error_type=type(error).__name__)
        result["seconds"] = round(time.monotonic() - started, 2)
        print(json.dumps(result), flush=True)
        return result

    jobs = [(model, *case) for model in args.models.split(",") for case in cases(model)]
    if args.cases:
        selected = set(args.cases.split(","))
        unknown = selected - {job[1] for job in jobs}
        if unknown:
            parser.error("Unknown cases: " + ",".join(sorted(unknown)))
        jobs = [job for job in jobs if job[1] in selected]
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(lambda job: run(*job), jobs))
    (args.out / "summary.json").write_text(json.dumps(results, indent=2))
    raise SystemExit(0 if all(r["passed"] for r in results) else 1)


if __name__ == "__main__":
    main()
