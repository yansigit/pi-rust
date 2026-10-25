import os,pathlib,shutil,subprocess
SRC=pathlib.Path(__file__).resolve().parent
ROOT=pathlib.Path('/root/bench-rpi-pi-bolt-20261021')
EXT=ROOT/'extensions';EXT.mkdir(exist_ok=True)
members=[]
for i in range(10):
    (EXT/f'echo{i}.ts').write_text((SRC/'extension.ts').read_text().replace('INDEX',str(i)))
    crate=EXT/f'echo{i}';(crate/'src').mkdir(parents=True,exist_ok=True)
    (crate/'src/lib.rs').write_text((SRC/'extension.rs').read_text().replace('INDEX',str(i)))
    (crate/'Cargo.toml').write_text(f'''[package]
name="bench-echo-{i}"
version="0.0.0"
edition="2021"
[lib]
crate-type=["cdylib"]
[dependencies]
rpi-plugin-sdk="=0.6.3"
serde_json="1"
''')
    members.append(f'echo{i}')
(EXT/'Cargo.toml').write_text('[workspace]\nresolver="2"\nmembers='+str(members).replace("'",'"')+'\n[profile.release]\nopt-level=3\nlto="thin"\nstrip="symbols"\n')
if (SRC/'extensions.Cargo.lock').exists():shutil.copyfile(SRC/'extensions.Cargo.lock',EXT/'Cargo.lock')
subprocess.run(['/root/.cargo/bin/cargo','build','--release','--locked','--manifest-path',str(EXT/'Cargo.toml')],check=True)
shutil.copyfile(EXT/'Cargo.lock',SRC/'extensions.Cargo.lock')
print('EXTENSIONS BUILT',flush=True)
