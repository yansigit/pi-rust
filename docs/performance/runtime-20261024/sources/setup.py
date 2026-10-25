"""Download pinned release artifacts into an isolated Linux benchmark directory."""
import hashlib, json, pathlib, subprocess, tarfile, urllib.request, shutil
ROOT = pathlib.Path('/root/bench-rpi-pi-bolt-20261021')
ROOT.mkdir(exist_ok=True)
def fetch(url, name):
    p = ROOT / name
    if not p.exists():
        cache = pathlib.Path('/mnt/d/Projects/blogs/.rpi/writing/rpi-pi-bolt-20261021/downloads') / url.rsplit('/',1)[-1]
        print('Downloading',url,flush=True)
        if cache.exists(): shutil.copyfile(cache,p)
        else:
            with urllib.request.urlopen(url, timeout=30) as r, p.open('wb') as f: shutil.copyfileobj(r,f)
    return p
def extract(p, dest):
    dest.mkdir(exist_ok=True)
    with tarfile.open(p) as t: t.extractall(dest, filter='data')
for repo,tag,name,check in [
    ('opensec-git/Pi-Bolt','bolt-v0.9.6','pi-bolt-linux-x64.tar.xz','SHA256SUMS'),
    ('bigfish1913/pi-rust','v0.6.3','rpi-v0.6.3-x86_64-unknown-linux-gnu.tar.gz','rpi-v0.6.3-x86_64-unknown-linux-gnu.tar.gz.sha256'),
    ('node','v24.22.0','node-v24.22.0-linux-x64.tar.xz','SHASUMS256.txt')]:
    base = f'https://nodejs.org/dist/{tag}/' if repo=='node' else f'https://github.com/{repo}/releases/download/{tag}/'
    p=fetch(base+name,name); hashes=fetch(base+check,repo.split('/')[-1]+'-sha256.txt').read_text()
    digest=hashlib.sha256(p.read_bytes()).hexdigest()
    assert any(digest in line and (name in line or len(line.split())==1) for line in hashes.splitlines()), (name,digest)
    print('Verified', name, digest,flush=True)
    extract(p,ROOT/('node' if repo=='node' else 'bolt' if 'Bolt' in repo else 'rpi'))
print('Artifacts',*[str(p) for p in ROOT.glob('*/*')],sep='\n',flush=True)
nodebin=ROOT/'node/node-v24.22.0-linux-x64/bin'
import os
env=dict(os.environ, PATH=str(nodebin)+':/usr/bin:/bin')
pi=ROOT/'pi';pi.mkdir(exist_ok=True)
src=pathlib.Path(__file__).resolve().parent
lock=src/'pi.package-lock.json'
if lock.exists():
    shutil.copyfile(lock,pi/'package-lock.json')
    (pi/'package.json').write_text(json.dumps({'dependencies':{'@earendil-works/pi-coding-agent':'1.3.0'}}))
    cmd=[str(nodebin/'npm'),'ci','--ignore-scripts','--no-audit','--no-fund','--prefix',str(pi)]
else:cmd=[str(nodebin/'npm'),'install','--ignore-scripts','--no-audit','--no-fund','--prefix',str(pi),'@earendil-works/pi-coding-agent@1.3.0']
subprocess.run(cmd,env=env,check=True)
print('SETUP DONE',flush=True)
