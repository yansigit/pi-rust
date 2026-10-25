"""Same-machine rerun of the archived benchmark, retaining both end boundaries."""
import argparse
import collections
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import signal
import statistics
import subprocess
import sys
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--binary', required=True)
p.add_argument('--before-binary', required=True)
p.add_argument('--before-source-manifest', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
p.add_argument('--baseline', type=Path, default=Path('/mnt/d/Projects/blogs/docs/performance/rpi-pi-bolt-20261021/run.py'))
p.add_argument('--source-manifest', type=Path, default=Path('/mnt/c/Users/bigfish/.codex/visualizations/2026/10/24/01a16b62-d01f-7c52-8aa1-2fd82f1230a9/perf-source-manifest.json'))
p.add_argument('--runs', type=int, default=7)
p.add_argument('--warmup', type=int, default=2)
p.add_argument('--tools', default='rpi,rpi-old,pi,pi-bolt')
p.add_argument('--profiles', default='0,1,10')
p.add_argument('--persistence', default='on,off')
p.add_argument('--scenarios', default='chain')
p.add_argument('--turns', type=int, default=100)
p.add_argument('--prompts', type=int, default=40)
p.add_argument('--fixture-repeats', type=int, default=1024)
p.add_argument('--profile', choices=['cpu', 'syscalls'])
p.add_argument('--worker-threads', type=int)
a = p.parse_args()
a.output.parent.mkdir(parents=True, exist_ok=True)
if a.output.exists():
    p.error('output already exists; preserve prior samples and choose another path')
source = a.baseline.read_text()
# Reuse the archived model, tool/result validation, affinity, process isolation
# and RSS sampler. Only add a current executable alias and richer measurements.
prefix = source.split('\ndef sha(p):')[0]
prefix = prefix.replace("self.name=='rpi'", "self.name.startswith('rpi')")
prefix = prefix.replace("name=='rpi'", "name.startswith('rpi')")
if a.worker_threads is not None:
    prefix = prefix.replace("env={'PATH':", "env={'TOKIO_WORKER_THREADS':'" + str(a.worker_threads) + "','PATH':")
sys.argv = [str(a.baseline), '--runs', str(a.runs), '--warmup', str(a.warmup),
            '--output', str(a.output), '--turns', str(a.turns), '--prompts', str(a.prompts)]
s = {'__file__': str(a.baseline), '__name__': 'archived_benchmark'}
exec(compile(prefix, str(a.baseline), 'exec'), s)
s['TEXT'] = 'deterministic tool result\n' * a.fixture_repeats
(s['WORK'] / 'fixture.txt').write_text(s['TEXT'])
commands = {k: v[:] for k, v in s['CMDS'].items()}
commands['rpi-old'] = commands['rpi'][:]
commands['rpi'] = [str(Path(a.binary).resolve())]
commands['rpi-before'] = [str(Path(a.before_binary).resolve())]
s['CMDS'] = {k: v[:] for k, v in commands.items()}
Base = s['Client']


class Client(Base):
    def ready(self):
        now, response = self.response(self.send('get_state'))
        self.old_rpc = 'result' in response
        state = response.get('data', response.get('result'))
        if 'modelId' in state:
            assert state['modelId'] == 'bench-model' and state['provider'] == 'bench', state
        else:
            model = state['model']
            assert model == 'bench-model' or isinstance(model, dict) and model['provider'] == 'bench', model
        return (now - self.started) * 1000

    def read(self, stream, error):
        for line in stream:
            if error:
                self.stderr.append(line)
                continue
            try:
                value = json.loads(line)
                value['_benchmarkBytes'] = len(line.encode())
                self.q.put((time.perf_counter(), value))
            except json.JSONDecodeError:
                self.stderr.append('NON_JSON ' + line)
        self.q.put((time.perf_counter(), {'type': 'eof'}))

    def prompt(self, text):
        start = time.perf_counter()
        before = s['proc'](self.p.pid)
        self.events = []
        request = self.send('prompt', message=text, content=text)
        ack = False
        final = settled = None
        deadline = start + 90
        while not (ack and settled is not None):
            now, event = self.next(deadline)
            if event.get('type') == 'agent_end':
                final = now
            if event.get('type') == 'agent_settled':
                if self.name.startswith('rpi'):
                    assert event.get('outcome') == 'completed', event
                settled = now
            if event.get('type') == 'response' and event.get('id') == request:
                assert event.get('success') is not False and event.get('status') != 'error', event
                ack = True
                if self.name.startswith('rpi') and self.old_rpc:
                    settled = now
        after = s['proc'](self.p.pid)
        assert final is not None, self.events
        assert 'BENCH_DONE' in json.dumps(self.events), self.events[-5:]
        sizes = collections.Counter()
        counts = collections.Counter()
        for event in self.events:
            sizes[event['type']] += event.get('_benchmarkBytes', 0)
            counts[event['type']] += 1
        return {'wall_ms': (settled - start) * 1000,
                'agent_end_ms': (final - start) * 1000,
                'settle_gap_ms': (settled - final) * 1000,
                'cpu_ms': after['cpu_ms'] - before['cpu_ms'],
                'rpc_bytes': dict(sizes), 'rpc_events': dict(counts)}


def files(client, previous):
    paths = set(s['WORK'].rglob('*.jsonl')) - previous
    rows = []
    for path in sorted(paths):
        counts = collections.Counter()
        lines = path.read_text().splitlines()
        for line in lines:
            item = json.loads(line)
            counts['/'.join(str(item.get(k, '')) for k in ['kind', 'type', 'recordType'])] += 1
        rows.append({'relative_path': str(path.relative_to(s['WORK'])),
                     'bytes': path.stat().st_size, 'lines': len(lines), 'types': dict(counts)})
    return rows


def sample(name, extensions, persistence, scenario, rep):
    s['CMDS'][name] = commands[name] + ([] if persistence == 'on' else ['--no-session'])
    previous = set(s['WORK'].rglob('*.jsonl'))
    c = Client(name, extensions)
    profiler = None
    row = {'tool': name, 'extensions': extensions, 'persistence': persistence,
           'scenario': scenario, 'rep': rep, 'warmup': rep < 0, 'command': c.command}
    expected = sorted(['read'] + [f'bench_echo_{i}' for i in range(extensions)])
    try:
        row['ready_ms'] = c.ready()
        row['ready_memory'] = c.memory()
        if a.profile:
            output = a.output.with_suffix('.perf.data' if a.profile == 'cpu' else '.strace.txt')
            profiler_env = dict(os.environ, LD_LIBRARY_PATH='/root/rpi-perf-20261024/tools/unpacked/usr/lib/x86_64-linux-gnu')
            if a.profile == 'cpu':
                profile_command = ['/root/rpi-perf-20261024/tools/unpacked/usr/lib/linux-tools-6.8.0-147/perf',
                                   'record', '-F', '997', '-e', 'cpu-clock', '--call-graph', 'dwarf,8192',
                                   '-p', str(c.p.pid), '-o', str(output)]
            else:
                profile_command = ['/root/rpi-perf-20261024/tools/unpacked/usr/bin/strace',
                                   '-c', '-f', '-p', str(c.p.pid), '-o', str(output)]
            profiler = subprocess.Popen(profile_command, env=profiler_env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            time.sleep(0.2)
            assert profiler.poll() is None, profiler.stderr.read().decode()
        if scenario == 'chain':
            for kind in ['read'] + (['echo'] if extensions else []):
                s['reset'](a.turns, 'read' if kind == 'read' else 'bench_echo_0', expected)
                row[kind] = c.prompt('Run deterministic ' + kind + ' benchmark.')
                assert s['STATE']['requests'] == a.turns + 1 and not s['STATE']['errors'], s['STATE']
                row[kind]['validation'] = dict(s['STATE'])
                row[kind]['memory'] = c.memory()
                row[kind]['session_files'] = files(c, previous)
        else:
            assert scenario == 'fresh', scenario
        prompts = []
        for index in range(a.prompts):
            s['reset'](0, 'read', expected)
            prompts.append(c.prompt(f'Short prompt {index}.'))
            assert s['STATE']['requests'] == 1 and not s['STATE']['errors'], s['STATE']
        row['prompts'] = {key: sum(value[key] for value in prompts)
                          for key in ['wall_ms', 'agent_end_ms', 'settle_gap_ms', 'cpu_ms']}
        row['prompts'].update(individual=prompts, memory=c.memory(), session_files=files(c, previous))
        row['stderr'] = ''.join(c.stderr)
        assert 'failed to load' not in row['stderr'].lower(), row['stderr']
        return row
    except Exception:
        print('SAMPLE STDERR', ''.join(c.stderr), flush=True)
        raise
    finally:
        if profiler is not None:
            profiler.send_signal(signal.SIGINT)
            _, errors = profiler.communicate(timeout=15)
            print('PROFILER', errors.decode()[-500:], flush=True)
        c.close()


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


names = a.tools.split(',')
groups = [(name, int(profile), persist, scenario) for name in names
          for profile in a.profiles.split(',') for persist in a.persistence.split(',')
          for scenario in a.scenarios.split(',')]
report = {'created_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
          'platform': platform.platform(), 'cpuinfo': next(line for line in Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('model name')),
          'agent_cpus': s['AGENT_CPUS'], 'driver_cpus': s['SERVER_CPUS'],
          'baseline_driver_sha256': sha(a.baseline), 'driver_sha256': sha(__file__),
          'source_snapshot': json.loads(a.source_manifest.read_text()),
          'before_source_snapshot': json.loads(a.before_source_manifest.read_text()),
          'fixture_bytes': len(s['TEXT'].encode()),
          'versions': {name: subprocess.check_output(commands[name] + ['--version'], text=True).strip() for name in names},
          'binary_sha256': {name: {path: sha(path) for path in commands[name]} for name in names},
          'extension_sha256': {str(path): sha(path) for path in (s['ROOT'] / 'extensions/target/release').glob('libbench_echo_*.so')},
          'runs': a.runs, 'warmup': a.warmup, 'turns': a.turns, 'prompt_count': a.prompts,
          'worker_threads_override': a.worker_threads,
          'notes': 'All programs wait for completed settlement. agent_end_ms separately records the same final agent event boundary. /proc CPU resolution 10 ms. Chain exactly matches archived workloads; fresh runs only short prompts in a new session. Same fixed fixture binaries; no live model or credentials.',
          'workdir': str(s['WORK']), 'samples': [], 'errors': []}


def save():
    a.output.write_text(json.dumps(report, ensure_ascii=False, indent=2))


try:
    rng = random.Random(20261021)
    for rep in range(-a.warmup, a.runs):
        order = groups[:]
        rng.shuffle(order)
        for name, profile, persist, scenario in order:
            print('RUN', rep, name, profile, persist, scenario, flush=True)
            try:
                row = sample(name, profile, persist, scenario, rep)
                report['samples'].append(row)
                save()
                print('OK', ' '.join(f'{key}={row[key]["agent_end_ms"]:.2f}/{row[key]["wall_ms"]:.2f}ms' for key in ['read', 'echo', 'prompts'] if key in row), flush=True)
            except Exception as error:
                report['errors'].append({'tool': name, 'extensions': profile, 'persistence': persist,
                                         'scenario': scenario, 'rep': rep, 'error': repr(error)})
                save()
                raise
finally:
    save()
    s['SERVER'].shutdown()
print('COMPLETE', a.output, flush=True)
