import sys,json,pathlib,hashlib,threading,http.server,time,subprocess,statistics,argparse
HERE=pathlib.Path(__file__).resolve().parent
import compare
from types import SimpleNamespace

DATA=bytes(range(256))*512
class Handler(http.server.BaseHTTPRequestHandler):
 protocol_version='HTTP/1.1'
 def log_message(self,*_): pass
 def do_HEAD(self): self.serve(False)
 def do_GET(self): self.serve(True)
 def serve(self,body):
  slow=self.path=='/slow'; data=bytes([13])*len(DATA) if slow else DATA
  self.send_response(200); self.send_header('Content-Length',str(len(data)))
  self.send_header('Accept-Ranges','bytes'); self.send_header('ETag','"slow"' if slow else '"fast"'); self.end_headers()
  if not body:return
  with self.server.lock:self.server.counts['slow' if slow else 'fast']+=1
  try:
   for offset in range(0,len(data),1024):
    self.wfile.write(data[offset:offset+1024]); self.wfile.flush()
    if slow:time.sleep(.05)
  except (BrokenPipeError,ConnectionResetError):pass

parser=argparse.ArgumentParser()
parser.add_argument('--work',type=pathlib.Path,required=True)
parser.add_argument('--report',type=pathlib.Path,required=True)
parser.add_argument('--rvdown',type=pathlib.Path,required=True)
opts=parser.parse_args()
work=opts.work
if work.exists() and any(work.iterdir()): parser.error('work must be empty')
work.mkdir(parents=True,exist_ok=True)
binary=opts.rvdown.resolve()
report={'method':'actual CLI, 128 KiB deterministic bytes, original drips 1 KiB/50 ms, distinct fast backup; all SHA256 checked; loopback not WAN',
        'binary_sha256':compare.digest(binary),'core_source_commit':compare.output(['git','-C',str(HERE.parent),'rev-parse','HEAD']),'harness_sha256':compare.digest(HERE/'failover.py'),'planned_trials':6,'runs':[]}
args=SimpleNamespace(work=work,timeout=20)
class QuietServer(http.server.ThreadingHTTPServer):
 def handle_error(self,*_): pass
server=QuietServer(('127.0.0.1',0),Handler); server.daemon_threads=True; server.lock=threading.Lock()
threading.Thread(target=server.serve_forever,daemon=True).start(); base=f'http://127.0.0.1:{server.server_port}'
for n in range(3):
 for profile in (['disabled','enabled'] if n%2==0 else ['enabled','disabled']):
  server.counts={'slow':0,'fast':0}; dest=work/f'{n}-{profile}.bin'
  command=[str(binary),'get',base+'/slow','--fallback',base+'/fast','--out',str(dest),'--json','--connections','1','--retry-attempts','3','--no-proxy','--no-verify','--no-checksum']
  if profile=='enabled':command+=['--low-speed-limit','65536','--low-speed-window','1']
  measurement,events=compare.measure(command,args,f'{n}-{profile}')
  expected=hashlib.sha256(DATA if profile=='enabled' else bytes([13])*len(DATA)).hexdigest()
  outcome=events[-1] if events else {}; actual=compare.digest(dest) if dest.exists() else None
  row={'round':n+1,'profile':profile,**measurement,'sha256_verified':actual==expected,'source': 'backup' if outcome.get('source_url','').endswith('/fast') else 'original',
       'ok':measurement['exit_code']==0 and actual==expected,'requests':dict(server.counts)}
  report['runs'].append(row); print(json.dumps(row),flush=True)
  opts.report.write_text(json.dumps(report,indent=2)+'\n')
  if row['ok']:dest.unlink()
server.shutdown(); report['complete']=True
opts.report.write_text(json.dumps(report,indent=2)+'\n')
