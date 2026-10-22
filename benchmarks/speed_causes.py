#!/usr/bin/env python3
"""Public single-file control: extra HEAD, matched headers, and aria2 defaults."""
import argparse,hashlib,http.server,json,pathlib,platform,statistics,sys,threading
import compare

HERE=pathlib.Path(__file__).resolve().parent
ARMS=['rvdown-cli','wreq-head-get','wreq-get','aria2-matched','aria2-default']
URL='https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4'
SIZE=1128375
EXPECTED='0cd83d944a6ca7822b4a8306cecc60a36e859b041f6702c6a1ad9ead78924451'

def order(n):return compare.round_order(ARMS,n)
def accept(code,timed_out,size,actual,expected,measurement=None):
    return code==0 and not timed_out and size==SIZE and actual==expected and (measurement is None or measurement.get('outcome')=='complete')
def command(arm,dest,args,url=URL,local=False):
    if arm.startswith('wreq-'):
        return [str(args.probe),url,args.relay_url,'head-get' if arm=='wreq-head-get' else 'get',str(dest)]
    result=compare.command_for('rvdown-1' if arm=='rvdown-cli' else 'aria2-8',url,dest,args,local)
    if arm.startswith('aria2'):
        result=[x.replace('--max-connection-per-server=8','--max-connection-per-server=1').replace('--split=8','--split=1') for x in result]
        result.append('--no-netrc=true')
        if arm=='aria2-default':result=[x for x in result if not x.startswith(('--user-agent=','--header='))]
    return result

class HeaderGate(http.server.BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*args):pass
    def do_GET(self):
        allowed=self.path=='/open' or self.headers.get('Referer')=='https://fixture.example/'
        self.server.trace.append({'method':'GET','referer_present':bool(self.headers.get('Referer')),
            'user_agent':self.headers.get('User-Agent'),'header_names':sorted(self.headers.keys()),'status':200 if allowed else 403})
        data=self.server.data if allowed else b''
        self.send_response(200 if allowed else 403);self.send_header('Content-Length',str(len(data)));self.send_header('Connection','close');self.end_headers()
        self.wfile.write(data)

def header_fixture(args):
    server=http.server.ThreadingHTTPServer(('127.0.0.1',0),HeaderGate);server.data=bytes(range(256))*128;server.trace=[]
    thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start();rows=[]
    try:
        for name,path,referer,expected_ok in [('open-default','open',False,True),('gated-default','gated',False,False),('gated-referer','gated',True,True)]:
            dest=args.work/(name+'.bin');server.trace=[]
            cmd=command('aria2-default',dest,args,f'http://127.0.0.1:{server.server_port}/{path}',local=True)
            if referer:cmd.insert(-1,'--referer=https://fixture.example/')
            measured,_=compare.measure(cmd,args,'fixture-'+name)
            size=dest.stat().st_size if dest.exists() else 0;digest=compare.digest(dest) if size else None
            downloaded=measured['exit_code']==0 and size==len(server.data) and digest==hashlib.sha256(server.data).hexdigest()
            rows.append({'name':name,'fixture_only':True,'expected_download_success':expected_ok,
                'actual_download_success':downloaded,'expected_behavior_observed':downloaded==expected_ok,
                'exit_code':measured['exit_code'],'bytes':size,'requests':list(server.trace)})
            if downloaded:dest.unlink()
    finally:server.shutdown();server.server_close();thread.join()
    return rows

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--rvdown',type=pathlib.Path,required=True);parser.add_argument('--probe',type=pathlib.Path,required=True)
    parser.add_argument('--aria2',default='aria2c');parser.add_argument('--interface',required=True)
    parser.add_argument('--work',type=pathlib.Path,required=True);parser.add_argument('--report',type=pathlib.Path,required=True)
    parser.add_argument('--rounds',type=int,default=5);parser.add_argument('--timeout',type=int,default=70)
    parser.add_argument('--doh',default='https://dns.alidns.com/resolve');parser.add_argument('--doh-ip',default='223.5.5.5')
    args=parser.parse_args()
    if args.rounds<5 or args.rounds%5:parser.error('rounds must be a positive multiple of five')
    if not args.interface.startswith(('en','eth','wl')):parser.error('physical interface required')
    if args.work.exists() and any(args.work.iterdir()):parser.error('work must be empty')
    args.work.mkdir(parents=True,exist_ok=True);args.rvdown=args.rvdown.resolve();args.probe=args.probe.resolve()
    address=compare.resolve_public('interactive-examples.mdn.mozilla.net',args)
    relay=compare.DirectRelay({'interactive-examples.mdn.mozilla.net':address},args.interface)
    worker=threading.Thread(target=relay.serve_forever,daemon=True);worker.start()
    args.relay_url=f'http://127.0.0.1:{relay.server_address[1]}'
    report={'schema':1,'source_commit':compare.output(['git','-C',str(HERE.parent),'rev-parse','HEAD']),
        'host':{'os':platform.system(),'arch':platform.machine()},'sample':{'url':URL,'bytes':SIZE,'sha256':EXPECTED},
        'binary_sha256':{'rvdown':compare.digest(args.rvdown),'request_probe':compare.digest(args.probe)},
        'harness_sha256':compare.digest(HERE/'speed_causes.py'),'aria2_version':compare.output([args.aria2,'--version']).splitlines()[0],
        'method':{'planned':args.rounds*len(ARMS),'rounds':args.rounds,'order':'five-way rotating Latin square',
            'connections':1,'network':'shared benchmark-owned loopback CONNECT relay, physical-interface-bound upstream, same public DoH snapshot/IP; end-to-end TLS, no external proxy',
            'headers':'Rvdown/native/matched aria2 use same benchmark UA and identity encoding, no Referer; default aria2 gets no caller UA/headers/Referer; all aria2 disable config/netrc',
            'timing':'main elapsed_s includes process startup and final file write; hash afterward; native transport_elapsed_s excludes final buffer write',
            'interpretation':'wreq native controls isolate requests on the same HTTP2 stack; they are not a replacement production downloader'},
        'header_fixture':header_fixture(args),'runs':[],'summaries':{}}
    compare.save(report,args)
    try:
        for n in range(args.rounds):
            for arm in order(n):
                label=f'{n+1}-{arm}';dest=args.work/(label+'.mp4');before=relay.connections
                measured,events=compare.measure(command(arm,dest,args),args,label)
                native=events[-1] if arm.startswith('wreq-') and events else None
                size=dest.stat().st_size if dest.exists() else 0;digest=compare.digest(dest) if size else None
                row={'round':n+1,'arm':arm,**measured,'bytes':size,'sha256':digest,
                    'upstream_tcp_connections':relay.connections-before,
                    'ok':accept(measured['exit_code'],measured['timed_out'],size,digest,EXPECTED,native)}
                if native:row['measurement']={k:native.get(k) for k in ['outcome','transport_elapsed_s','bytes','responses']}
                report['runs'].append(row);compare.save(report,args)
                print(json.dumps(row),flush=True)
                if row['ok']:dest.unlink()
        for arm in ARMS:
            rows=[r for r in report['runs'] if r['arm']==arm];ok=[r for r in rows if r['ok']]
            report['summaries'][arm]={'planned':args.rounds,'attempted':len(rows),'passed':len(ok),'failed':len(rows)-len(ok),
                'median_elapsed_s':statistics.median(r['elapsed_s'] for r in ok) if ok else None,
                'min_elapsed_s':min((r['elapsed_s'] for r in ok),default=None),'max_elapsed_s':max((r['elapsed_s'] for r in ok),default=None)}
        report['complete']=True;compare.save(report,args)
    finally:relay.shutdown();relay.server_close();worker.join()

if __name__=='__main__':main()
