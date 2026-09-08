#!/usr/bin/env python3
"""Replay the screenshot's capability checks against a local instance.

These are explicit HTTP/error assertions, not the unavailable third-party test.
Credentials are read locally and never included in saved request artifacts.
"""
import argparse
import concurrent.futures
import json
from pathlib import Path
import urllib.error
import urllib.request


def cases(model):
    yield "temperature", {"temperature": 1.1}
    yield "web-search", {"tools": [{"type": "web_search_20250305", "name": "web_search"}]}
    yield "role", {"messages": [{"role": "invalid", "content": "hello"}]}
    yield "structured", {"output_config": {"format": {"type": "json_schema", "schema": {
        "type": "object", "properties": {"answer": {"type": "integer"}},
        "required": ["answer"], "additionalProperties": False}}}}
    yield "signature", {"messages": [{"role": "user", "content": "Calculate 19+23."},
        {"role": "assistant", "content": [{"type": "thinking", "thinking": "Calculate.", "signature": "invalid-signature"},
        {"type": "text", "text": "42"}]}, {"role": "user", "content": "Continue."}]}
    if "4-8" in model or model == "claude-opus-5":
        yield "manual-thinking", {"max_tokens": 2048, "thinking": {"type": "enabled", "budget_tokens": 1024}}
    yield "fallback", {"fallbacks": "default"}
    yield "advisor", {"tools": [{"type": "advisor_20260301", "name": "advisor", "model": "claude-opus-4-8"}]}
    yield "code-execution", {"tools": [{"type": "code_execution_20260521", "name": "code_execution"}]}
    yield "url-image", {"messages": [{"role": "user", "content": [
        {"type": "image", "source": {"type": "url", "url": "https://www.w3.org/Icons/w3c_home.png"}},
        {"type": "text", "text": "Describe the image."}]}]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--port", type=int)
    parser.add_argument("--historical", action="store_true", help="Assert the pre-POMO-comparison local contract")
    parser.add_argument("--models", nargs="+", default=["claude-opus-5", "claude-opus-4-8"])
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    args.out.mkdir(parents=True, exist_ok=False)
    endpoint = f"http://127.0.0.1:{args.port or config['port']}/v1/messages"
    jobs = []
    for model in args.models:
        for name, extra in cases(model):
            for stream in [False, True]:
                body = {"model": model, "max_tokens": 256, "stream": stream,
                    "messages": [{"role": "user", "content": 'Calculate 19+23. If JSON is requested, use {"answer":42}.'}], **extra}
                jobs.append((f"{model}--{name}--{'stream' if stream else 'plain'}", body))

    def run(job):
        name, body = job
        (args.out / f"{name}.request.json").write_text(json.dumps(body, ensure_ascii=False, indent=2))
        req = urllib.request.Request(endpoint, data=json.dumps(body).encode(), headers={
            "Content-Type": "application/json", "x-api-key": config['apiKey'],
            "anthropic-version": "2023-06-01", "anthropic-beta": "server-side-fallback-2026-07-01"})
        # Disable redirects so a local test cannot forward its secret elsewhere.
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, *a, **kw):
                return None
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        try:
            try:
                response = opener.open(req, timeout=180)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                status, raw = response.code, response.read()
            (args.out / f"{name}.response.raw").write_bytes(raw)
            try:
                error = json.loads(raw).get("error", {})
            except (ValueError, AttributeError):
                error = {}
            signature = "--signature--" in name
            target = body['model'] in ('claude-opus-5','claude-opus-4-8')
            fallback = '--fallback--' in name
            expected_type = "<nil>" if signature or (target and not args.historical and not fallback) else "invalid_request_error"
            passed = status == 400 and isinstance(error, dict) and error.get("type") == expected_type
            if signature:
                passed = passed and "Invalid `signature`" in error.get("message", "")
            if fallback:
                passed = passed and "`fallbacks` is not supported" in error.get("message", "")
            result = {"case": name, "status": status, "passed": passed, "error": error}
        except Exception as error:
            result = {"case": name, "passed": False, "transport_error": str(error)}
        print(json.dumps(result, ensure_ascii=False), flush=True)
        return result

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(run, jobs))
    summary = {"endpoint": endpoint, "total": len(results), "passed": sum(r['passed'] for r in results), "results": results}
    (args.out / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2))
    print(f"Passed {summary['passed']}/{summary['total']}", flush=True)
    raise SystemExit(0 if all(r['passed'] for r in results) else 1)


if __name__ == "__main__":
    main()
