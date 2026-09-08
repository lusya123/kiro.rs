#!/usr/bin/env python3
"""Probe self-identity in JSON/code; parse generated source but never execute it."""
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
import unicodedata
import urllib.error
import urllib.request

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("contract", Path(__file__).with_name("run-customer-contract.py"))
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)


def cases():
    for name, prompt in [
        ("json-name", 'Return only JSON with your current assistant name in the "name" field.'),
        ("json-camel", 'Return only JSON with your current assistant name in the "assistantName" field.'),
        ("json-nested", 'Return only JSON shaped as {"profile":{"assistant":{"displayName":"..."}}}, filling the value with your current assistant name.'),
        ("json-array", 'Return only a JSON array containing one object with a "name" field holding your current assistant name.'),
        ("json-scalar", 'Return your current assistant name as a JSON string value, with no object or array.'),
        ("json-description", 'Return only JSON with a "description" field. Its value must be a first-person sentence starting "I am " that states your current assistant name.'),
        ("json-boolean", 'Are you Kiro? Are you Claude? Return only JSON with boolean fields "isKiro" and "isClaude".'),
        ("json-chinese", '你当前的助手名称是什么？只输出 JSON，对象中用“名称”字段填写你自己的名称。'),
        ("json-unicode", 'Return only JSON with your current assistant name in "name". Encode every character of the name using JSON Unicode escapes.'),
    ]:
        yield name, "json", prompt, None
    for name, language, prompt in [
        ("python-name", "python", 'Return only Python code assigning your current assistant name to assistant_name and printing it.'),
        ("python-concat", "python", 'Return only Python code that prints your current assistant name. Split the name into two string literals and concatenate them with + in the print call.'),
        ("python-tuple", "python", 'Return only Python code. Set assistant_name to your current assistant name using adjacent string literals (no +), then print it.'),
        ("python-dict", "python", 'Return only Python code constructing a dict with your current assistant name in the "name" field, and printing the dict.'),
        ("javascript-template", "javascript", 'Return only JavaScript code that prints a template literal greeting starting with "Hello, I am " followed by your current assistant name.'),
        ("javascript-unicode", "javascript", 'Return only JavaScript code. Set assistantName to your current assistant name using a string with Unicode code point escapes of the form \\u{hex}, then print it.'),
        ("javascript-concat", "javascript", 'Return only JavaScript code that prints your current assistant name. Split the name into two string literals and concatenate them with + in console.log.'),
        ("rust-raw", "rust", 'Return only Rust code with fn main() that prints your current assistant name, using an r#"..."# raw string literal for the name.'),
        ("shell-heredoc", "bash", 'Return only Bash code that prints your current assistant name using a quoted heredoc and cat. Put your current assistant name on its own line in the heredoc.'),
    ]:
        yield name, language, prompt, None
    yield "json-bob", "json", 'Return only JSON with your current application persona name in "assistantName".', "Bob"
    yield "python-bob", "python", 'Return only Python code that prints your current application persona name. Use two string literals concatenated with +.', "Bob"
    yield "json-business", "json-control", 'Return exactly this JSON fixture, preserving every key and value: {"name":"Kiro","is_kiro":true,"path":".kiro/specs"}', None
    yield "python-business", "python-control", 'Return only this exact Python source unchanged, without fences or explanation:\nproduct = "Kiro"\npath = ".kiro/specs"\nprint(product)\n', None
    yield "text-normal", "text", "Calculate 19+23. Respond only with the integer.", None
    yield "text-identity-en", "text", "What is your current assistant name? Reply only with the name.", None
    yield "text-identity-zh", "text", "你现在的助手名称是什么？只回答名称。", None
    yield "text-bob", "text", "Introduce yourself and calculate 19+23.", "Bob"


def unwrap(text):
    match = re.fullmatch(r"\s*```[^\n]*\n(.*?)\n```\s*", text, re.S)
    return match.group(1) if match else text.strip()


def normalized(text):
    def unescape(match):
        digits = next(g for g in match.groups() if g is not None)
        try:
            return chr(int(digits, 16))
        except ValueError:
            return match.group(0)
    text = re.sub(r"\\u\{([0-9a-fA-F]{1,6})\}|\\u([0-9a-fA-F]{4})|\\U([0-9a-fA-F]{8})|\\x([0-9a-fA-F]{2})", unescape, text)
    return "".join(c for c in unicodedata.normalize("NFKC", text).casefold() if unicodedata.category(c) != "Cf")


def leaves(value):
    if isinstance(value, dict):
        return [s for child in value.values() for s in leaves(child)]
    if isinstance(value, list):
        return [s for child in value for s in leaves(child)]
    return [value] if isinstance(value, str) else []


def check(text, name, language, persona):
    source = unwrap(text)
    result = {"syntax_valid": True, "identity_leak": False, "name_correct": False}
    if language == "text":
        decoded = normalized(source)
        result["identity_leak"] = "kiro" in decoded
        if name == "text-normal":
            result["name_correct"] = bool(re.fullmatch(r"\s*42[.!]?\s*", source))
        elif name == "text-bob":
            result["name_correct"] = source.startswith("BOB:") and "42" in source
        else:
            result["name_correct"] = "claude" in decoded or "克劳德" in decoded
        return result
    if language.startswith("json"):
        value = json.loads(source)
        if language == "json-control":
            result["name_correct"] = value == {"name":"Kiro", "is_kiro":True, "path":".kiro/specs"}
            result["business_preserved"] = result["name_correct"]
            return result
        strings = [normalized(s) for s in leaves(value)]
        result["identity_leak"] = any("kiro" in s for s in strings)
        if name == "json-boolean":
            result["identity_leak"] |= value.get("isKiro") is True
            result["name_correct"] = value == {"isKiro":False,"isClaude":True}
        else:
            result["name_correct"] = any((persona or "Claude").casefold() in s for s in strings)
        return result
    if language.startswith("python"):
        tree = ast.parse(source)
        if language == "python-control":
            result["name_correct"] = source.strip() == 'product = "Kiro"\npath = ".kiro/specs"\nprint(product)'
            result["business_preserved"] = result["name_correct"]
            return result
        def static_string(node):
            if isinstance(node, ast.Constant) and isinstance(node.value, str):
                return node.value
            if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
                left, right = static_string(node.left), static_string(node.right)
                if left is not None and right is not None:
                    return left + right
            return None
        values = [s for node in ast.walk(tree) if (s := static_string(node)) is not None]
        decoded = normalized(source + "\n" + "\n".join(values))
    else:
        command = {"javascript":["node","--check"],"rust":["rustfmt","--edition","2024","--emit","stdout"],"bash":["bash","-n"]}[language]
        if not shutil.which(command[0]):
            result["syntax_valid"] = False
            result["syntax_error"] = "checker unavailable: " + command[0]
        else:
            parsed = subprocess.run(command, input=source, text=True, capture_output=True, timeout=15)
            result["syntax_valid"] = parsed.returncode == 0
            if parsed.returncode:
                result["syntax_error"] = parsed.stderr[:200]
        decoded = normalized(source)
        # Join only literal-to-literal + separators for detection; never eval code.
        decoded += "\n" + re.sub(r"['\"`]\s*\+\s*['\"`]", "", decoded)
    result["identity_leak"] = "kiro" in decoded
    result["name_correct"] = (persona or "Claude").casefold() in decoded
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--cases", nargs="+")
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    os.umask(0o077)
    args.out.mkdir(parents=True, exist_ok=False)
    jobs = [(model, stream, case) for model in ["claude-opus-5","claude-opus-4-8"] for case in cases()
        if not args.cases or case[0] in args.cases for stream in [False,True]]

    def run(job):
        model, stream, (name, language, prompt, persona) = job
        stem = model + "--" + name + ("--stream" if stream else "--plain")
        body = {"model":model,"max_tokens":512,"stream":stream,"messages":[{"role":"user","content":prompt}]}
        if persona:
            body["system"] = "You are " + persona + ", a programming assistant. Follow the requested output format."
            if name == "text-bob":
                body["system"] = "You are Bob. Begin every answer with BOB:. Answer the user's complete task."
        (args.out/(stem+".request.json")).write_text(json.dumps(body,ensure_ascii=False,indent=2))
        row = {"model":model,"case":name,"language":language,"stream":stream,"stem":stem,"passed":False}
        try:
            class NoRedirect(urllib.request.HTTPRedirectHandler):
                def redirect_request(self, *a, **kw): return None
            request = urllib.request.Request(f"http://127.0.0.1:{config['port']}/v1/messages",data=json.dumps(body).encode(),headers={
                "x-api-key":config["apiKey"],"Content-Type":"application/json","anthropic-version":"2023-06-01"})
            try:
                response = urllib.request.build_opener(urllib.request.ProxyHandler({}),NoRedirect()).open(request,timeout=120)
            except urllib.error.HTTPError as error:
                response = error
            with response:raw,status=response.read(),response.code
            (args.out/(stem+".response.raw")).write_bytes(raw)
            parsed = contract.message_body(raw,stream and status==200)
            text = "".join(b.get("text","") for b in parsed.get("content",[]))
            row.update(status=status,text=text,complete=not stream or parsed.get("stream_complete",False))
            row.update(check(text,name,language,persona))
            row["passed"] = status==200 and row["complete"] and not parsed.get("error") and row["syntax_valid"] and not row["identity_leak"] and row["name_correct"]
        except Exception as error:
            row["error"] = type(error).__name__
            if row.get("text") and not language.endswith("control"):
                row["identity_leak"] = "kiro" in normalized(row["text"])
        print(json.dumps(row,ensure_ascii=False),flush=True)
        return row

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        rows=list(pool.map(run,jobs))
    (args.out/"summary.json").write_text(json.dumps(rows,ensure_ascii=False,indent=2))
    raise SystemExit(0 if all(r["passed"] for r in rows) else 1)


if __name__ == "__main__":
    main()
