#!/usr/bin/env python3
"""Read-only live Sub2API route audit; run as root on the Docker host.
Never emits credentials or prompt content. Requires psql in sub2api-postgres.
"""
import subprocess,json,collections,re
from urllib.parse import urlsplit
from pathlib import Path

def inspect(names):return json.loads(subprocess.check_output(['docker','inspect',*names],timeout=60))
def env(c):return dict(e.split('=',1) for e in c['Config']['Env'] if '=' in e)
allnames=subprocess.check_output(['docker','ps','-a','--format','{{.Names}}'],text=True).splitlines()
items=inspect([n for n in allnames if re.fullmatch(r'kiro-rs-(?:external-)?\d+',n)])
standard=set();external={}
for c in items:
 name=c['Name'].lstrip('/');cache=env(c).get('KIRO_CLUSTER_CACHE_ADDR');labels=c['Config'].get('Labels') or {}
 if cache=='127.0.0.1:46380':standard.add(int(name.rsplit('-',1)[1]))
 elif cache=='127.0.0.1:46381' and labels.get('kiro.cluster')=='aws-b-external':
  external[int(name.rsplit('-',1)[1])]=c
  src=labels.get('kiro.source-container','')
  if re.fullmatch(r'kiro-rs-\d+',src):standard.add(int(src.rsplit('-',1)[1]))
pg=env(inspect(['sub2api-postgres'])[0]);q="""BEGIN READ ONLY; SET LOCAL statement_timeout='15s'; SELECT row_to_json(t) FROM (SELECT a.id,a.name,a.platform,a.status,a.schedulable,a.credentials->>'base_url' AS url,coalesce((SELECT json_agg(group_id ORDER BY group_id) FROM account_groups WHERE account_id=a.id),'[]') AS group_ids FROM accounts a WHERE a.deleted_at IS NULL) t; COMMIT;"""
r=subprocess.run(['docker','exec','sub2api-postgres','psql','-X','-A','-t','-U',pg.get('POSTGRES_USER','postgres'),'-d',pg.get('POSTGRES_DB','sub2api'),'-c',q],capture_output=True,text=True,timeout=30)
r.check_returncode()
rows=[json.loads(l) for l in r.stdout.splitlines() if l.startswith('{')]
targets=[];external_accounts=[]
for a in rows:
 try:
  u=urlsplit(a.pop('url') or '');p=u.port
  if u.hostname not in ['127.0.0.1','localhost','43.156.228.59','10.3.4.14','172.18.0.1','172.17.0.1','172.20.0.1']:continue
  if a['platform']!='anthropic':continue
  if p in standard: a['port']=p; targets.append(a)
  elif p in external:a['port']=p;external_accounts.append(a)
 except Exception:continue
print(json.dumps({'standard_accounts_to_preserve_but_exclude':targets,'external_accounts':external_accounts,'standard_account_counts':dict(collections.Counter(f"{a['status']}/{a['schedulable']}" for a in targets)),'external_container_counts':dict(collections.Counter(c['State']['Status'] for c in external.values())),'external_images':dict(collections.Counter(c['Config']['Image'] for c in external.values()))},ensure_ascii=False,indent=2))
