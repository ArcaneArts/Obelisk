"""Audit the scalar-index release log without invoking the database engine.

Usage: python3 docs/measurements/scalar-index-matrix-audit.py LOG --require-complete
The recorded process must also have exited successfully; output alone cannot
establish that its final assertions and disposable-folder cleanup completed.
"""

from pathlib import Path
from collections import Counter
import json,re,sys
source=Path(sys.argv[1]).read_text()
rows=[]
for line in source.splitlines():
    if not line.startswith('| '): continue
    parts=[v.strip() for v in line.strip('|').split('|')]
    if len(parts)!=16 or not parts[1].isdigit(): continue
    rows.append(dict(op=parts[0],size=int(parts[1]),profile=parts[2],ms=float(parts[3]),counters=[int(v) for v in parts[4:]]))
sizes=[100,1000,10000,100000]
profiles=['zero','one','three','three+replacement']
expected={(n,p) for n in sizes for p in profiles}
opens={(r['size'],r['profile']) for r in rows if r['op']=='open'}
for r in rows:
    c=r['counters']
    if r['op']=='open':
        assert c[0]==1 and c[2]==(386 if r['profile']=='zero' else 467),r
        assert all(v==0 for v in c[3:8]),r
        assert c[1]==(800 if r['profile']=='zero' else 881),r
    if r['op'].startswith('query-'):
        count=min(8,(r['size']+999)//1000)
        assert c[6]==(count if r['op']=='query-full' else 0),r
        assert c[7]==(count if r['op']=='query-covered' else 0),r
        assert c[3]<=4*count and c[5]<=10,r
accounting=[]
for line in source.splitlines():
    match=re.match(r'uploads size=(\d+) profile=([^:]+):',line)
    if not match: continue
    size=int(match[1]);profile=match[2]
    groups={name:tuple(map(int,(attempts,objects_,bytes_))) for name,attempts,objects_,bytes_ in re.findall(r'(committed|abandoned|unclassified|pending): Uploads \{ attempts: (\d+), objects: (\d+), bytes: (\d+) \}',line)}
    assert len(groups)==4,line
    assert groups['committed'][0]==12,line
    assert groups['unclassified']==(0,0,0) and groups['pending']==(0,0,0),line
    head=int(re.search(r'successful_head_bytes=(\d+)',line)[1])
    assert head==12*207,line
    row=next(r for r in rows if r['op']=='contention' and r['size']==size and r['profile']==profile)
    c=row['counters']
    assert groups['abandoned'][0]==c[11],line
    assert groups['committed'][1]+groups['abandoned'][1]==c[9],line
    assert groups['committed'][2]+groups['abandoned'][2]+head==c[10],line
    accounting.append((size,profile))
completed={int(n) for n in re.findall(r'^checkpoint size=(\d+):',source,re.M)}
builds=Counter((int(n),index) for n,index in re.findall(r'^build complete: size=(\d+) index=([^ ]+)',source,re.M))
for size in completed:
    assert all((size,p) in opens and (size,p) in accounting for p in profiles),size
    assert builds[size,'primary']==2 and builds[size,'rank']==1 and builds[size,'enabled-rank']==1,size
    for profile in ['one','one-after-writes','three','three-after-writes','three+replacement','replacement-after-writes','replacement-active']:
        assert all(any(r['size']==size and r['profile']==profile and r['op']==op for r in rows) for op in ['query-key','query-covered','query-full']),(size,profile)
if '--require-complete' in sys.argv:
    assert completed==set(sizes),sorted(completed)
    assert opens==expected and set(accounting)==expected,(opens,accounting)
summary=dict(completed_sizes=sorted(completed),startup_profiles_checked=len(opens),upload_accounting_profiles_checked=len(accounting),query_rows_checked=sum(r['op'].startswith('query-') for r in rows),missing_startup_profiles=sorted(expected-opens))
print(json.dumps(summary,indent=2))
