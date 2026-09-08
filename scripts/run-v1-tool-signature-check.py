#!/usr/bin/env python3
"""Verify real V1 tool calls, ID encodings, continuation and signature tampering.

Tool results are supplied by this client; no generated code is executed.
"""
import argparse
import base64
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
import urllib.parse
import urllib.request
import uuid

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("envelope", Path(__file__).with_name("run-pomo-envelope-check.py"))
envelope = importlib.util.module_from_spec(spec)
spec.loader.exec_module(envelope)


def signature_rejected(status, error, case):
    detail=error.get('message','')
    valid_detail='Invalid `signature`' in detail
    if 'signature-last-' in case:
        valid_detail=valid_detail or 'blocks in the latest assistant message cannot be modified' in detail
    if 'signature-empty-' in case:
        valid_detail=valid_detail or 'each thinking block must contain thinking' in detail
    return status==400 and error.get('type')=='<nil>' and valid_detail


def fixture_value_preserved(text, code, report_format):
    # "Report the code unchanged" permits a label/fence, unlike "return only".
    if report_format:
        return bool(re.search(r'(?<![A-Za-z0-9_])'+re.escape(code)+r'(?![A-Za-z0-9_])',text))
    return text.strip()==code


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--models", nargs="+", default=["claude-opus-5","claude-opus-4-8"])
    parser.add_argument("--fixture-tool", action="store_true", help="Replay the previously verified read_fixture scenario")
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    base = args.base_url.rstrip("/")
    parsed = urllib.parse.urlparse(base)
    if parsed.scheme != 'https' and parsed.hostname not in ('127.0.0.1','localhost','::1'):
        parser.error('Remote endpoints require HTTPS')
    os.umask(0o077)
    args.out.mkdir(parents=True, exist_ok=False)

    def send(model, case, payload, invalid=False):
        stem = model+'--'+case
        (args.out/(stem+'.request.json')).write_text(json.dumps(payload,ensure_ascii=False,indent=2))
        req = urllib.request.Request(base+'/v1/messages',data=json.dumps(payload).encode(),headers={
            'x-api-key':config['apiKey'],'Content-Type':'application/json','anthropic-version':'2023-06-01'})
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self,*a,**kw):return None
        handlers = [NoRedirect()]
        if parsed.hostname in ('127.0.0.1','localhost','::1'):
            handlers.append(urllib.request.ProxyHandler({}))
        row = {'model':model,'case':case,'passed':False}
        start = time.time()
        try:
            try:response = urllib.request.build_opener(*handlers).open(req,timeout=120)
            except urllib.error.HTTPError as error:response = error
            with response:raw,status = response.read(),response.code
            end = time.time()
            (args.out/(stem+'.response.raw')).write_bytes(raw)
            body = envelope.contract.message_body(raw,payload.get('stream',False) and status==200)
            row['status'] = status
            if invalid:
                error = body.get('error',{})
                row['passed'] = signature_rejected(status,error,case)
                row['error_message'] = error.get('message')
            else:
                row['id_valid'] = envelope.check_id(body.get('id'),model,start-.01,end+.01)
                row['passed'] = status==200 and row['id_valid'] and not body.get('error') and (not payload.get('stream') or body.get('stream_complete'))
        except Exception as error:
            row['transport_error'] = type(error).__name__
            body = {}
        return row,body

    def run(model):
        rows=[]
        for stream in [False,True]:
            key='key-'+uuid.uuid4().hex
            payload={'model':model,'max_tokens':2048,'stream':stream,'thinking':{'type':'adaptive'},
                'messages':[{'role':'user','content':f'Call lookup with key "{key}" to read its unknown code. After receiving the tool result, return only the exact code string, preserving it literally.'}],
                'tools':[{'name':'lookup','description':'Read the unknown code for a key. The client will provide the result.',
                    'input_schema':{'type':'object','properties':{'key':{'type':'string'}},'required':['key'],'additionalProperties':False}}]}
            expected_name,expected_input='lookup',{'key':key}
            if args.fixture_tool:
                payload['max_tokens']=4096
                payload['messages'][0]['content']='Call read_fixture to obtain the current fixture code, then report that code unchanged. It is unknown until the tool returns; do not invent it.'
                payload['tools']=[{'name':'read_fixture','description':'Read the fixture code.',
                    'input_schema':{'type':'object','properties':{},'additionalProperties':False}}]
                expected_name,expected_input='read_fixture',{}
            row,first=send(model,'tool-'+str(stream),payload)
            calls=[b for b in first.get('content',[]) if b.get('type')=='tool_use']
            signed=[b for b in first.get('content',[]) if b.get('type')=='thinking' and b.get('signature')]
            row['tool_valid']=len(calls)==1 and calls[0].get('name')==expected_name and calls[0].get('input')==expected_input and bool(re.fullmatch(r'toolu_bdrk_01[1-9A-HJ-NP-Za-km-z]{22}',calls[0].get('id','')))
            row['has_signature']=bool(signed)
            row['passed']=bool(row['passed'] and row['tool_valid'])
            rows.append(row)
            if not row['passed']:
                continue
            code='Kiro_business_payload_'+uuid.uuid4().hex
            follow=copy.deepcopy(payload)
            follow['messages'].extend([{'role':'assistant','content':first['content']},
                {'role':'user','content':[{'type':'tool_result','tool_use_id':calls[0]['id'],'content':code}]}])
            row,body=send(model,'continuation-'+str(stream),follow)
            text=''.join(b.get('text','') for b in body.get('content',[]))
            row['literal_preserved']=fixture_value_preserved(text,code,args.fixture_tool)
            row['passed']=bool(row['passed'] and row['literal_preserved'])
            rows.append(row)
            signature_follow=follow
            if not signed:
                # A valid tool call need not include reasoning. Obtain a real
                # signed block independently instead of inventing a signature.
                reasoning={'model':model,'max_tokens':4096,'stream':stream,
                    'thinking':{'type':'adaptive','display':'omitted'},'output_config':{'effort':'high'},
                    'messages':[{'role':'user','content':'Find the smallest positive integer x such that x mod 97 = 12, x mod 89 = 34, x mod 83 = 56, and x mod 79 = 78. Return x and verify all four remainders.'}]}
                row,body=send(model,'reasoning-'+str(stream),reasoning)
                signed=[b for b in body.get('content',[]) if b.get('type')=='thinking' and b.get('signature')]
                row['has_signature']=bool(signed)
                row['passed']=bool(row['passed'] and signed)
                rows.append(row)
                if not row['passed']:
                    continue
                signature_follow=copy.deepcopy(reasoning)
                signature_follow['messages'].extend([{'role':'assistant','content':body['content']},
                    {'role':'user','content':'Thanks. Now calculate 19+23 and return only the result.'}])
                row,_=send(model,'signature-original-'+str(stream),signature_follow)
                rows.append(row)
            for mutation in ['middle','last','empty']:
                altered=copy.deepcopy(signature_follow)
                block=next(b for b in altered['messages'][-2]['content'] if b.get('type')=='thinking' and b.get('signature'))
                if mutation=='empty':block['signature']=''
                else:
                    raw=bytearray(base64.b64decode(block['signature']))
                    raw[len(raw)//2 if mutation=='middle' else -1]^=1
                    block['signature']=base64.b64encode(raw).decode()
                row,_=send(model,'signature-'+mutation+'-'+str(stream),altered,True)
                rows.append(row)
        for row in rows:print(json.dumps(row),flush=True)
        return rows

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        rows=[r for group in pool.map(run,args.models) for r in group]
    (args.out/'summary.json').write_text(json.dumps(rows,indent=2))
    raise SystemExit(0 if len(rows)>=10*len(args.models) and all(r['passed'] for r in rows) else 1)


if __name__=='__main__':main()
