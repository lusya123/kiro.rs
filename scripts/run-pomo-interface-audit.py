#!/usr/bin/env python3
"""Capture unchanged responses for per-interface POMO/local comparison.

Uses protocol-specific requests. An HTTP 200 alone is never a passing result.
This collector deliberately records observations without guessing expectations.
"""
import argparse
import concurrent.futures
import importlib.util
import json
import os
from pathlib import Path
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("capability", Path(__file__).with_name("run-public-capability-check.py"))
capability = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capability)


def cases(model, interface):
    if interface in ("messages", "cc"):
        base = {"model": model, "max_tokens": 256,
            "messages": [{"role": "user", "content": 'Calculate 19+23. If JSON is requested, use {"answer":42}.'}]}
        yield "conversation", base
        for name, extra in capability.cases(model):
            if name == "url-image":
                extra["messages"][0]["content"][0]["source"]["url"] = "https://www.w3.org/Icons/w3c_home.png"
            yield name, {**base, **extra}
        for name, prompt, extra in [
            ("identity", 'Return only a JSON object with your current assistant name in the "name" field.', {}),
            ("bob-json", 'Return only a JSON object with your application name in the "name" field.',
                {"system":"You are Bob, a programming assistant. Follow the requested output format."}),
            ("bob-code", "Return only Python code. Set assistant_name to your current application persona name, then print assistant_name. Use the name as a string literal.",
                {"system":"You are Bob, a programming assistant. Follow the requested output format."}),
        ]:
            yield name, {**base, **extra, "messages":[{"role":"user","content":prompt}]}
    elif interface == "chat":
        base = {"model":model,"max_tokens":256,"messages":[{"role":"user","content":"Calculate 19+23. Return only the answer."}]}
        yield "conversation", base
        for name, extra in [
            ("temperature", {"temperature":1.1}),
            ("role", {"messages":[{"role":"invalid","content":"hello"}]}),
            ("structured", {"response_format":{"type":"json_schema","json_schema":{"name":"answer","strict":True,"schema":{
                "type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"],"additionalProperties":False}}}}),
            ("url-image", {"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/image.png"}},
                {"type":"text","text":"Describe this image."}]}]}),
            ("web-search", {"tools":[{"type":"web_search_20250305","name":"web_search"}]}),
            ("advisor", {"tools":[{"type":"advisor_20260301","name":"advisor"}]}),
            ("code-execution", {"tools":[{"type":"code_execution_20260521","name":"code_execution"}]}),
            ("fallback", {"fallbacks":"default"}),
            ("bob-json", {"messages":[{"role":"system","content":"You are Bob, a programming assistant. Follow the requested output format."},
                {"role":"user","content":'Return only a JSON object with your application name in the "name" field.'}]}),
        ]:
            yield name, {**base, **extra}
    else:
        yield "conversation", {"model":model,"max_output_tokens":256,"input":"Calculate 19+23. Return only the answer."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--models", nargs="+", default=["claude-opus-5","claude-opus-4-8"])
    parser.add_argument("--interfaces", nargs="+", choices=["messages","cc","chat","responses"], default=["messages","chat","responses"])
    parser.add_argument("--cases", nargs="+")
    args = parser.parse_args()
    base = args.base_url.rstrip("/")
    parsed = urllib.parse.urlparse(base)
    if parsed.scheme != "https" and parsed.hostname not in ("127.0.0.1","localhost","::1"):
        parser.error("Remote endpoints require HTTPS")
    config = json.loads(args.config.read_text())
    os.umask(0o077)
    args.out.mkdir(parents=True, exist_ok=False)
    paths = {"messages":"/v1/messages","cc":"/cc/v1/messages","chat":"/v1/chat/completions","responses":"/v1/responses"}
    jobs = [(model, interface, name, {**body,"stream":stream}) for model in args.models
        for interface in args.interfaces for name,body in cases(model,interface)
        if not args.cases or name in args.cases for stream in (False,True)]

    def run(job):
        model, interface, name, body = job
        stem = f"{model}--{interface}--{name}--{'stream' if body['stream'] else 'plain'}"
        (args.out/(stem+'.request.json')).write_text(json.dumps(body,ensure_ascii=False,indent=2))
        headers = {"Content-Type":"application/json","x-api-key":config['apiKey'],"Authorization":"Bearer "+config['apiKey'],"anthropic-version":"2023-06-01"}
        if name == "fallback":
            headers['anthropic-beta'] = "server-side-fallback-2026-07-01"
        req = urllib.request.Request(base+paths[interface], data=json.dumps(body).encode(), headers=headers)
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self,*a,**kw):return None
        handlers = [NoRedirect()]
        if parsed.hostname in ("127.0.0.1","localhost","::1"):
            handlers.append(urllib.request.ProxyHandler({}))
        row = {"model":model,"interface":interface,"case":name,"stream":body['stream'],"stem":stem}
        start = time.monotonic()
        try:
            try:r = urllib.request.build_opener(*handlers).open(req,timeout=90)
            except urllib.error.HTTPError as error:r = error
            with r:
                raw = r.read()
                row.update(status=r.code,content_type=r.headers.get('Content-Type',''))
            (args.out/(stem+'.response.raw')).write_bytes(raw)
            try:
                value = json.loads(raw)
                row['error'] = value.get('error')
                row['id'] = value.get('id')
                row['text'] = ''.join(b.get('text','') for b in value.get('content',[]) if isinstance(b,dict))
                if value.get('choices'):
                    row['text'] = value['choices'][0].get('message',{}).get('content','')
            except (ValueError,TypeError,AttributeError):
                row['sse_complete'] = any(marker in raw for marker in [b'event: message_stop',b'"type":"message_stop"',b'"type": "message_stop"',b'[DONE]',b'event: response.completed'])
                row['sse_error'] = b'event: error' in raw or b'"type":"error"' in raw or b'"type": "error"' in raw
            row['seconds'] = round(time.monotonic()-start,2)
        except Exception as error:
            row['transport_error'] = type(error).__name__
        print(json.dumps(row,ensure_ascii=False).replace(config['apiKey'],'[REDACTED]'),flush=True)
        return row
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(run,jobs))
    (args.out/'summary.json').write_text(json.dumps(results,ensure_ascii=False,indent=2))


if __name__ == '__main__':
    main()
