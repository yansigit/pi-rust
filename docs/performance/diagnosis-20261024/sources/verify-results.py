"""Offline validation of the diagnosis archive and every summary metric."""
import csv
import hashlib
import json
from pathlib import Path
import statistics

root = Path(__file__).resolve().parents[1]
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
rows = list(csv.DictReader((root/'summary.csv').open(newline='')))
validation = json.loads((root/'validation.json').read_text())
baseline = root.parent/'runtime-20261024/sources/archived-run.py'
before = json.loads((root/'source-before.json').read_text())
fixed = json.loads((root/'source-fixed.json').read_text())
controls = json.loads((root/'source-controls.json').read_text())
seen = 0
for name, expected in validation.items():
    data = json.loads((root/'results'/(name+'.json')).read_text())
    assert data['errors'] == []
    assert data['baseline_driver_sha256'] == sha(baseline)
    if name.startswith('controls'):
        driver = root/'sources/control-compare.py'
        assert data['source_snapshot'] == controls
        assert data['before_source_snapshot'] == before
        for tool in {s['tool'] for s in data['samples']}:
            assert data['binary_sha256'][tool]['/root/rpi-cause-20261024/'+tool] == sha(root/'sources'/tool)
    else:
        driver = root.parent/'history-20261024/sources'/('large-compare.py' if name in ('large-repeat','fixed-large') else 'compare.py')
        assert data['source_snapshot'] == (fixed if name.startswith('fixed') else before)
    assert data['driver_sha256'] == sha(driver)
    assert len(data['samples']) == expected['total_processes']
    formal = [s for s in data['samples'] if not s['warmup']]
    assert len(formal) == expected['formal_samples']
    for s in data['samples']:
        for stage in ('read','echo'):
            v = s[stage]['validation']
            assert not v['errors'] and v['requests'] == data['turns']+1 and v['turns'] == data['turns']
        assert len(s['prompts']['individual']) == data['prompt_count']
        if s['tool'].startswith('rpi') and s['persistence'] == 'on':
            t,p = data['turns'],data['prompt_count']
            assert [sum(f['lines'] for f in s[k]['session_files']) for k in ('read','echo','prompts')] == [10*t+11,20*t+21,20*t+21+10*p]
    for persist in ('on','off'):
        group = [s for s in data['samples'] if s['tool'].startswith('rpi') and s['persistence'] == persist]
        if not group: continue
        for stage in ('read','echo'):
            assert len({s[stage]['validation']['request_bytes'] for s in group}) == 1
            complete = [s for s in group if s['tool'] not in ('rpi-quiet','rpi-both')]
            assert len({json.dumps(s[stage]['rpc_events'],sort_keys=True) for s in complete}) <= 1
        for stage in ('read','echo','prompts'):
            assert len({json.dumps([f['types'] for f in s[stage]['session_files']],sort_keys=True) for s in group}) == 1
    for row in (r for r in rows if r['experiment'] == name):
        vals = [s[row['stage']] for s in formal if s['tool'] == row['tool'] and s['persistence'] == row['persistence']]
        assert len(vals) == int(row['n']) == data['runs']
        times = [v['wall_ms'] for v in vals]
        metrics = dict(median_ms=statistics.median(times),min_ms=min(times),max_ms=max(times),
            median_agent_end_ms=statistics.median(v['agent_end_ms'] for v in vals),
            median_cpu_ms=statistics.median(v['cpu_ms'] for v in vals),
            median_rss_mib=statistics.median(v['memory']['rss_mib'] for v in vals))
        assert all(abs(float(row[k])-value)<1e-9 for k,value in metrics.items())
        seen += 1
    print(name,'PASS',len(data['samples']),'processes')
assert seen == len(rows) == 90
print('summary.csv PASS',seen,'rows')
