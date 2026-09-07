#!/usr/bin/env python3
"""Opt-in live inference regression. Uses ANTHROPIC_API_KEY from the environment.

Makes 16 small billable requests by default. No credentials or request headers
are printed. Use a disposable local test deployment and a known test credential.
Acceptance checks HTTP 200 and a complete substantive answer. Language adherence
is reported separately: Kiro reminder rendering cannot guarantee native system
priority when a user instruction conflicts with a reminder.
"""
import argparse
import json
import os
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path


def probe(base, key, model, path, stream):
    payload = {'model': model, 'max_tokens': 128, 'stream': stream,
               'messages': [
                   {'role': 'user', 'content': f'实验编号 {uuid.uuid4().hex}。请用中文解释：为什么同温度的金属勺摸起来比木勺凉？'},
                   {'role': 'system', 'content': 'For this turn answer in English. Explain the metal/wood comparison using heat transfer, in one or two sentences.'},
               ]}
    req = urllib.request.Request(base.rstrip('/') + path, data=json.dumps(payload).encode(),
                                 headers={'x-api-key': key, 'content-type': 'application/json',
                                          'anthropic-version': '2023-06-01'})
    record = {'model': model, 'path': path, 'stream': stream}
    start = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=60) as response:
            body = response.read().decode()
            record['status'] = response.status
        if stream:
            events = [json.loads(line[6:]) for line in body.splitlines()
                      if line.startswith('data: ') and line[6:] != '[DONE]']
            text = ''.join(e.get('delta', {}).get('text', '') for e in events)
            completed = any(e.get('type') == 'message_stop' for e in events)
        else:
            result = json.loads(body)
            text = ''.join(b.get('text', '') for b in result.get('content', []) if b.get('type') == 'text')
            completed = result.get('stop_reason') == 'end_turn'
        lower = text.lower()
        semantic_match = (len(text) > 50 and all(word in lower for word in ['metal', 'wood', 'heat'])
                          and not any('\u4e00' <= c <= '\u9fff' for c in text))
        record.update(text=text, completed=completed, reminder_language_obeyed=semantic_match,
                      passed=record['status'] == 200 and completed and len(text.strip()) > 50)
    except urllib.error.HTTPError as exc:
        record.update(status=exc.code, passed=False)
        # Do not persist provider bodies: they may contain sensitive diagnostics.
        exc.close()
    except Exception as exc:
        record.update(error_type=type(exc).__name__, passed=False)
    record['elapsed_seconds'] = round(time.monotonic() - start, 3)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base-url', default='http://127.0.0.1:19890')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--models', nargs='+', default=[
        'claude-sonnet-5', 'claude-sonnet-4-6', 'claude-opus-5', 'claude-opus-4-8'])
    args = parser.parse_args()
    key = os.environ['ANTHROPIC_API_KEY']
    results = []
    for model in args.models:
        for path in ['/v1/messages', '/cc/v1/messages']:
            for stream in [False, True]:
                result = probe(args.base_url, key, model, path, stream)
                results.append(result)
                args.output.write_text(json.dumps(results, ensure_ascii=False, indent=2) + '\n')
                print(json.dumps(result, ensure_ascii=False), flush=True)
                if not result['passed']:
                    raise SystemExit(1)


if __name__ == '__main__':
    main()
