"""Validate archived samples and recalculate every summary.csv metric."""
import csv
import hashlib
import json
from pathlib import Path
import statistics

root = Path(__file__).resolve().parents[1]
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
rows = list(csv.DictReader((root / 'summary.csv').open(newline='')))
seen = 0
for filename, driver in [('comparison.json', 'compare.py'), ('large-output.json', 'large-compare.py')]:
    report = json.loads((root / 'results' / filename).read_text())
    assert not report['errors']
    assert report['driver_sha256'] == sha(root / 'sources' / driver)
    assert report['baseline_driver_sha256'] == sha(root.parent / 'runtime-20261024/sources/archived-run.py')
    assert report['source_snapshot'] == json.loads((root / 'source-after.json').read_text())
    assert report['before_source_snapshot'] == json.loads((root / 'source-before.json').read_text())
    for sample in report['samples']:
        for stage in ('read', 'echo'):
            validation = sample[stage]['validation']
            assert not validation['errors']
            assert validation['turns'] == report['turns']
            assert validation['requests'] == report['turns'] + 1
        assert len(sample['prompts']['individual']) == report['prompt_count']
        if sample['tool'].startswith('rpi') and sample['persistence'] == 'on':
            turns, prompts = report['turns'], report['prompt_count']
            expected = [10*turns+11, 20*turns+21, 20*turns+21+10*prompts]
            actual = [sum(f['lines'] for f in sample[stage]['session_files']) for stage in ('read', 'echo', 'prompts')]
            assert actual == expected
    for persistence in ('on', 'off'):
        pair = [s for s in report['samples'] if s['tool'] in ('rpi', 'rpi-before') and s['persistence'] == persistence]
        for stage in ('read', 'echo'):
            assert len({s[stage]['validation']['request_bytes'] for s in pair}) == 1
            assert len({json.dumps(s[stage]['rpc_events'], sort_keys=True) for s in pair}) == 1
        for stage in ('read', 'echo', 'prompts'):
            assert len({json.dumps([f['types'] for f in s[stage]['session_files']], sort_keys=True) for s in pair}) == 1
    for row in (r for r in rows if r['experiment'] == filename):
        samples = [s for s in report['samples'] if not s['warmup'] and s['tool'] == row['tool'] and s['persistence'] == row['persistence']]
        assert len(samples) == int(row['n']) == report['runs']
        values = [s[row['stage']] for s in samples]
        times = [v['wall_ms'] for v in values]
        expected = dict(median_ms=statistics.median(times), min_ms=min(times), max_ms=max(times),
                        median_agent_end_ms=statistics.median(v['agent_end_ms'] for v in values),
                        median_cpu_ms=statistics.median(v['cpu_ms'] for v in values),
                        median_rss_mib=statistics.median(v['memory']['rss_mib'] for v in values))
        assert all(abs(float(row[k])-v) < 1e-9 for k, v in expected.items())
        seen += 1
    print(filename, 'PASS', len(report['samples']), 'processes')
assert seen == len(rows) == 36
print('summary.csv PASS', seen, 'rows')
