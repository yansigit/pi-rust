"""Linux RPC benchmark; local deterministic Anthropic SSE; no live API credentials.

Run setup.py, build-fixtures.py, then run.py --runs 7 --warmup 2 --output results.json.
Each fresh process runs 100 read calls, 100 extension echo calls (if enabled),
and 40 short prompts, in that order. The local server validates every tool result.
"""
import argparse,hashlib,http.server,json,os,pathlib,platform,queue,random,signal,socket,statistics,subprocess,tempfile,threading,time,uuid,shutil
P=argparse.ArgumentParser();P.add_argument('--runs',type=int,default=7);P.add_argument('--warmup',type=int,default=2);P.add_argument('--output',required=True);P.add_argument('--tools',default='rpi,pi,pi-bolt');P.add_argument('--profiles',default='0,1,10');P.add_argument('--turns',type=int,default=100);P.add_argument('--prompts',type=int,default=40);A=P.parse_args()
ROOT=pathlib.Path('/root/bench-rpi-pi-bolt-20261021');NODE=ROOT/'node/node-v24.22.0-linux-x64/bin/node'
CMDS={'rpi':[str(ROOT/'rpi/rpi')],'pi':[str(NODE),str(ROOT/'pi/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js')],'pi-bolt':[str(ROOT/'bolt/pi-bolt-linux-x64/pi')]}
CPUS=sorted(os.sched_getaffinity(0)); AGENT_CPUS=CPUS[2:6];SERVER_CPUS=CPUS[:2];os.sched_setaffinity(0,SERVER_CPUS)
WORK=pathlib.Path(tempfile.mkdtemp(prefix='rpc-bench-',dir=ROOT)); TEXT='deterministic tool result\n'*8;(WORK/'fixture.txt').write_text(TEXT)
STATE={};HZ=os.sysconf('SC_CLK_TCK')
def reset(turns,tool,expected):
    STATE.clear();STATE.update(turns=turns,tool=tool,expected=expected,requests=0,errors=[],server_cpu_ms=0,request_bytes=0,schemas=None)
def content_text(x):
    if isinstance(x,str):return x
    if isinstance(x,list):return ''.join(c.get('text','') for c in x if isinstance(c,dict))
    return ''
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def setup(self):
        super().setup();self.connection.setsockopt(socket.IPPROTO_TCP,socket.TCP_NODELAY,1)
    def log_message(self,*a):pass
    def do_POST(self):
        cpu=time.thread_time();size=int(self.headers['Content-Length']);req=json.loads(self.rfile.read(size));step=STATE['requests'];STATE['requests']+=1;STATE['request_bytes']+=size
        names=sorted(t['name'] for t in req.get('tools',[]));STATE['schemas']=names
        if names!=STATE['expected']:STATE['errors'].append('tool schemas mismatch: '+str(names))
        if step:
            last=req['messages'][-1]['content'];results=[c for c in last if isinstance(c,dict) and c.get('type')=='tool_result'] if isinstance(last,list) else []
            if len(results)!=1 or results[0].get('is_error') or content_text(results[0].get('content'))!=TEXT or results[0].get('tool_use_id')!=f'call_{step-1}':STATE['errors'].append('bad tool result at '+str(step)+': '+str(results)[:300])
        if step<STATE['turns']:
            inp={'path':str(WORK/'fixture.txt')} if STATE['tool']=='read' else {'text':TEXT}
            block={'type':'tool_use','id':f'call_{step}','name':STATE['tool'],'input':{}};delta={'type':'input_json_delta','partial_json':json.dumps(inp)};stop='tool_use'
        else:block={'type':'text','text':''};delta={'type':'text_delta','text':'BENCH_DONE'};stop='end_turn'
        msg={'id':f'msg_{step}','type':'message','role':'assistant','model':'bench-model','content':[],'stop_reason':None,'stop_sequence':None,'usage':{'input_tokens':1,'output_tokens':1}}
        events=[('message_start',{'type':'message_start','message':msg}),('content_block_start',{'type':'content_block_start','index':0,'content_block':block}),('content_block_delta',{'type':'content_block_delta','index':0,'delta':delta}),('content_block_stop',{'type':'content_block_stop','index':0}),('message_delta',{'type':'message_delta','delta':{'stop_reason':stop,'stop_sequence':None},'usage':{'output_tokens':1}}),('message_stop',{'type':'message_stop'})]
        data=''.join('event: '+k+'\ndata: '+json.dumps(v,separators=(',',':'))+'\n\n' for k,v in events).encode()
        self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data);self.wfile.flush()
        STATE['server_cpu_ms']+=(time.thread_time()-cpu)*1000
SERVER=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=SERVER.serve_forever,daemon=True).start()
def proc(pid):
    p=pathlib.Path('/proc')/str(pid);st=(p/'stat').read_text().rsplit(')',1)[1].split();status={k:v.strip() for k,v in (l.split(':',1) for l in (p/'status').read_text().splitlines())}
    return {'cpu_ms':(int(st[11])+int(st[12]))*1000/HZ,'rss_mib':int(status['VmRSS'].split()[0])/1024,'hwm_mib':int(status['VmHWM'].split()[0])/1024,'threads':int(status['Threads']),'child_pids':(p/'task'/str(pid)/'children').read_text().strip()}
class Client:
    def __init__(self,name,n):
        self.name=name;self.q=queue.Queue();self.stderr=[];self.events=[];self.home=WORK/uuid.uuid4().hex;self.home.mkdir()
        model={'id':'bench-model','name':'Benchmark','reasoning':False,'input':['text'],'cost':{'input':0,'output':0,'cacheRead':0,'cacheWrite':0},'contextWindow':10000000,'maxTokens':1024}
        (self.home/'models.json').write_text(json.dumps({'providers':{'bench':{'api':'anthropic-messages','baseUrl':f'http://127.0.0.1:{SERVER.server_port}','apiKey':'bench-placeholder','models':[model]}}}))
        (self.home/'settings.json').write_text(json.dumps({'compaction':{'enabled':False},'retry':{'enabled':False},'packages':[],'extensions':[],'skills':[],'promptTemplates':[]}))
        env={'PATH':str(NODE.parent)+':/usr/bin:/bin','HOME':str(self.home),'XDG_CONFIG_HOME':str(self.home/'config'),'XDG_CACHE_HOME':str(self.home/'cache'),'LANG':'C.UTF-8','TERM':'dumb','NO_COLOR':'1','PI_CODING_AGENT_DIR':str(self.home),'RPI_CODING_AGENT_DIR':str(self.home),'PI_OFFLINE':'1','RPI_OFFLINE':'1','PI_SKIP_VERSION_CHECK':'1','ANTHROPIC_API_KEY':'bench-placeholder'}
        cmd=['taskset','-c',','.join(map(str,AGENT_CPUS))]+CMDS[name]+['--mode','rpc','--model','bench/bench-model','--tools',','.join(['read']+[f'bench_echo_{i}' for i in range(n)]),'--no-skills','--no-prompt-templates']
        if n==0:cmd+=['--no-extensions']
        else:
            for i in range(n):cmd+=['-e',str(ROOT/f'extensions/target/release/libbench_echo_{i}.so' if name=='rpi' else ROOT/f'extensions/echo{i}.ts')]
        self.command=cmd;self.started=time.perf_counter();self.p=subprocess.Popen(cmd,cwd=WORK,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8',bufsize=1,start_new_session=True)
        threading.Thread(target=self.read,args=(self.p.stdout,False),daemon=True).start();threading.Thread(target=self.read,args=(self.p.stderr,True),daemon=True).start()
    def read(self,s,err):
        for line in s:
            if err:self.stderr.append(line);continue
            try:self.q.put((time.perf_counter(),json.loads(line)))
            except json.JSONDecodeError:self.stderr.append('NON_JSON '+line)
        self.q.put((time.perf_counter(),{'type':'eof'}))
    def send(self,kind,**kw):
        id=uuid.uuid4().hex;self.p.stdin.write(json.dumps(dict(type=kind,id=id,**kw))+'\n');self.p.stdin.flush();return id
    def next(self,deadline):
        try:t,m=self.q.get(timeout=max(.01,deadline-time.perf_counter()))
        except queue.Empty:raise RuntimeError('Timeout '+self.name+' '+''.join(self.stderr)[-2500:])
        if m['type']=='eof':raise RuntimeError('Exited '+self.name+' '+''.join(self.stderr)[-2500:])
        self.events.append(m);return t,m
    def response(self,id):
        deadline=time.perf_counter()+30
        while True:
            t,m=self.next(deadline)
            if m.get('type')=='response' and m.get('id')==id:
                if m.get('success') is False or m.get('status')=='error':raise RuntimeError(str(m))
                return t,m
    def ready(self):
        t,m=self.response(self.send('get_state'))
        self.old_rpc='result' in m
        model=m.get('data',m.get('result'))['model']
        assert model=='bench-model' or isinstance(model,dict) and model['provider']=='bench',m
        return (t-self.started)*1000
    def prompt(self,text):
        start=time.perf_counter();before=proc(self.p.pid);self.events=[];id=self.send('prompt',message=text,content=text);deadline=start+90;ack=False;done=False
        while not (ack and done):
            t,m=self.next(deadline)
            if m.get('type')=='response' and m.get('id')==id:
                if m.get('success') is False or m.get('status')=='error':raise RuntimeError(str(m))
                ack=True
                if self.name=='rpi' and self.old_rpc:done=True;end=t
            if m.get('type')==('agent_settled' if self.name=='rpi' else 'agent_end'):
                if self.name=='rpi':assert m.get('outcome')=='completed',m
                done=True;end=t
        after=proc(self.p.pid)
        assert 'BENCH_DONE' in json.dumps(self.events),str(self.events)[-1000:]
        return {'wall_ms':(end-start)*1000,'cpu_ms':after['cpu_ms']-before['cpu_ms'],'after':after}
    def memory(self):
        time.sleep(.25);samples=[]
        for _ in range(5):samples.append(proc(self.p.pid));time.sleep(.02)
        assert not any(s['child_pids'] for s in samples),samples
        return {'rss_mib':statistics.median(s['rss_mib'] for s in samples),'hwm_mib':max(s['hwm_mib'] for s in samples),'threads':samples[-1]['threads'],'samples':samples}
    def close(self):
        if self.p.poll() is None:os.killpg(self.p.pid,signal.SIGKILL)
        self.p.wait();self.p.stdin.close()
def single(name,n,rep,warmup):
    c=Client(name,n);expected=sorted(['read']+[f'bench_echo_{i}' for i in range(n)])
    row={'tool':name,'extensions':n,'rep':rep,'warmup':warmup,'command':c.command}
    try:
        row['ready_ms']=c.ready();row['ready_cpu_ms']=proc(c.p.pid)['cpu_ms'];row['ready_memory']=c.memory()
        for kind in ['read']+(['echo'] if n else []):
            reset(A.turns,'read' if kind=='read' else 'bench_echo_0',expected)
            row[kind]=c.prompt('Run deterministic '+kind+' benchmark.')
            assert STATE['requests']==A.turns+1 and not STATE['errors'],dict(STATE)
            row[kind]['validation']=dict(STATE);row[kind]['memory']=c.memory()
        data=[]
        for i in range(A.prompts):
            reset(0,'read',expected);m=c.prompt(f'Short prompt {i}.');assert STATE['requests']==1 and not STATE['errors'],dict(STATE);data.append(m)
        row['prompts']={'count':A.prompts,'wall_ms':sum(x['wall_ms'] for x in data),'cpu_ms':sum(x['cpu_ms'] for x in data),'individual':data,'memory':c.memory()}
        row['stderr']=''.join(c.stderr)
        assert 'failed to load' not in row['stderr'].lower(),row['stderr']
        return row
    finally:c.close()
def sha(p):return hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
report={'created_utc':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),'platform':platform.platform(),'cpuinfo':next(l.split(':',1)[1].strip() for l in pathlib.Path('/proc/cpuinfo').read_text().splitlines() if l.startswith('model name')),'meminfo':pathlib.Path('/proc/meminfo').read_text().splitlines()[:3],'agent_cpus':AGENT_CPUS,'server_cpus':SERVER_CPUS,'clock_ticks_per_second':HZ,'versions':{},'sha256':{},'runs':A.runs,'warmup':A.warmup,'turns':A.turns,'prompts':A.prompts,'samples':[],'errors':[],'method':'Warm filesystem cache. Fresh isolated process each sample. Four agent CPUs, two mock/harness CPUs. RPC ready=get_state response. RSS=median five /proc status readings after 250ms idle; VmHWM=kernel process peak. CPU=/proc stat user+system, 10ms resolution. 100 sequential read calls then 100 extension echoes (when loaded), then 40 short prompts in same session. No TUI or cloud inference. Extensions loaded at runtime, not compiled into Pi-Bolt.'}
for name in A.tools.split(','):
    report['versions'][name]=subprocess.check_output(CMDS[name]+['--version'],text=True).strip()
    for p in CMDS[name]:report['sha256'][p]=sha(p)
report['versions']['node']=subprocess.check_output([str(NODE),'--version'],text=True).strip()
report['sha256']['run.py']=sha(__file__)
report['sha256']['npm-package-lock']=sha(ROOT/'pi/package-lock.json')
report['sha256']['rust-extension-Cargo.lock']=sha(ROOT/'extensions/Cargo.lock')
for p in sorted((ROOT/'extensions').glob('echo*.ts')):report['sha256'][str(p)]=sha(p)
for p in sorted((ROOT/'extensions/target/release').glob('libbench_echo_*.so')):report['sha256'][str(p)]=sha(p)
def save():pathlib.Path(A.output).write_text(json.dumps(report,ensure_ascii=False,indent=2))
try:
    groups=[(name,n) for name in A.tools.split(',') for n in map(int,A.profiles.split(','))];rng=random.Random(20261021)
    for rep in range(-A.warmup,A.runs):
        order=groups[:];rng.shuffle(order)
        for name,n in order:
            print('RUN',rep,name,n,flush=True)
            try:
                row=single(name,n,rep,rep<0);report['samples'].append(row);save()
                print('OK',name,n,'ready',round(row['ready_ms'],2),'RSS',round(row['ready_memory']['rss_mib'],2),'read',round(row['read']['wall_ms'],2),flush=True)
            except Exception as e:
                report['errors'].append({'tool':name,'extensions':n,'rep':rep,'error':repr(e)});save();raise
finally:
    save();SERVER.shutdown();shutil.rmtree(WORK)
print('COMPLETE',A.output,flush=True)
