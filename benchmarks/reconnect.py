#!/usr/bin/env python3
"""Native single-file reconnect/fingerprint/protocol experiment. Public input only."""
import argparse
import csv
import io
import json
import pathlib
import platform
import re
import statistics
import subprocess
import sys
import time
from types import SimpleNamespace

import compare

HERE=pathlib.Path(__file__).resolve().parent
ARMS=["continue","reconnect-same","reconnect-chrome149","reconnect-firefox144","reconnect-http1"]
URL="https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4"
EXPECTED="0cd83d944a6ca7822b4a8306cecc60a36e859b041f6702c6a1ad9ead78924451"
SIZE=1128375
COLUMNS="interface,state,bytes_in,bytes_out,rx_dupe,rx_ooo,re-tx,rtt_avg,rcvsize"


def order(number): return compare.round_order(ARMS,number)


def accept(code,result,size,actual,expected):
    return code==0 and result.get("outcome")=="complete" and size==SIZE and actual==expected


def tcp_summary(raw):
    header=None; rows=[]
    for row in csv.reader(io.StringIO(raw)):
        if "rx_dupe" in row and "rtt_avg" in row:
            header=row; continue
        if not header or not row or not row[0].startswith("tcp"):
            continue
        fields=dict(zip(header,row))
        rows.append(fields)
    def maximum(column):
        values=[]
        for row in rows:
            match=re.fullmatch(r"([\d.]+)(?:\s*ms)?",row.get(column,"").strip())
            if match: values.append(float(match[1]))
        return max(values) if values else None
    return {"sampled_connection_rows":len(rows),
        "max_received_bytes":maximum("bytes_in"),
        "max_duplicate_received_bytes":maximum("rx_dupe"),
        "max_out_of_order_received_bytes":maximum("rx_ooo"),
        "max_client_retransmit_bytes":maximum("re-tx"),
        "max_rtt_ms":maximum("rtt_avg"),"max_receive_buffer_bytes":maximum("rcvsize"),
        "note":"per-socket lifetime maxima; do not sum repeated samples; client retransmits do not measure server-side retransmits"}


def trial(args,number,arm,address):
    label=f"{number}-{arm}"; dest=args.work/(label+".mp4")
    stdout=args.work/(label+".stdout"); stderr=args.work/(label+".stderr")
    tcp=args.work/(label+".tcp.csv"); monitor=None
    started=time.perf_counter(); timed_out=False
    with stdout.open("w") as out,stderr.open("w") as err,tcp.open("w") as network:
        process=subprocess.Popen([str(args.binary),URL,address,args.interface,arm,str(dest)],
            stdout=out,stderr=err,env=compare.clean_environment(),start_new_session=True)
        if sys.platform=="darwin":
            monitor=subprocess.Popen(["nettop","-n","-m","tcp","-p",str(process.pid),
                "-L","0","-s","1","-x","-J",COLUMNS],stdout=network,stderr=subprocess.DEVNULL,
                env=compare.clean_environment())
        try: process.wait(timeout=70)
        except subprocess.TimeoutExpired:
            timed_out=True; process.terminate()
            try:process.wait(timeout=3)
            except subprocess.TimeoutExpired:process.kill();process.wait()
        finally:
            if monitor:
                monitor.terminate()
                try:monitor.wait(timeout=3)
                except subprocess.TimeoutExpired:monitor.kill();monitor.wait()
    try: result=json.loads(stdout.read_text())
    except ValueError: result={"outcome":"no_structured_result"}
    size=dest.stat().st_size if dest.exists() else 0
    actual=compare.digest(dest) if size else None
    row={"round":number+1,"arm":arm,"exit_code":process.returncode,
        "external_timeout":timed_out,"wall_elapsed_s":round(time.perf_counter()-started,6),
        "measurement":result,"bytes":size,"sha256":actual,
        "ok":accept(process.returncode,result,size,actual,EXPECTED),
        "tcp":tcp_summary(tcp.read_text(errors="replace"))}
    if row["ok"]:dest.unlink()
    return row


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--binary",type=pathlib.Path,required=True)
    parser.add_argument("--interface",required=True)
    parser.add_argument("--work",type=pathlib.Path,required=True)
    parser.add_argument("--report",type=pathlib.Path,required=True)
    parser.add_argument("--rounds",type=int,default=5)
    parser.add_argument("--doh",default="https://dns.alidns.com/resolve")
    parser.add_argument("--doh-ip",default="223.5.5.5")
    args=parser.parse_args()
    if args.rounds<5 or args.rounds%5:parser.error("rounds must be a positive multiple of five")
    if not args.interface.startswith(("en","eth","wl")):parser.error("physical interface required")
    if args.work.exists() and any(args.work.iterdir()):parser.error("work must be new and empty")
    args.binary=args.binary.resolve();args.work.mkdir(parents=True,exist_ok=True)
    args.report.parent.mkdir(parents=True,exist_ok=True)
    host="interactive-examples.mdn.mozilla.net"
    address=compare.resolve_public(host,args)
    report={"schema":1,"sample":{"id":"mdn-flower","url":URL,"bytes":SIZE,"sha256":EXPECTED},
        "source_commit":compare.output(["git","-C",str(HERE.parent),"rev-parse","HEAD"]),
        "binary_sha256":compare.digest(args.binary),"harness_sha256":compare.digest(HERE/"reconnect.py"),
        "host":{"os":platform.system(),"arch":platform.machine()},
        "method":{"planned_trials":args.rounds*len(ARMS),"rounds":args.rounds,
            "order":"five-way rotating Latin square","single_file_at_a_time":True,"connection_cap":1,
            "network":"native physical interface binding; one public DoH IP snapshot shared by all requests; no proxy or relay",
            "initial":"Chrome131, HTTP/2, matched Windows browser UA, GET without Range",
            "forced_interruption":"after retaining first 256 KiB for reconnect arms; new Client and TCP/TLS connection",
            "reattempts":"same URL and pinned IP; same Range/If-Range, strong ETag and exact Content-Range validation; 200 restarts safely",
            "timing":"entire initial attempt plus cut/reconnect/remaining body; SHA256 afterward",
            "interpretation":"forced interruption is a controlled experiment, not natural slow-link recovery success rate",
            "telemetry":"nettop restricted to each test PID; raw IPs, ports and process names remain in scratch only"},
        "runs":[],"summaries":{}}
    for number in range(args.rounds):
        for arm in order(number):
            row=trial(args,number,arm,address);report["runs"].append(row)
            compare.save(report,args)
            print(json.dumps({"round":number+1,"arm":arm,"ok":row["ok"],
                "elapsed_s":row["measurement"].get("elapsed_s"),"outcome":row["measurement"].get("outcome"),
                "responses":row["measurement"].get("responses"),"tcp":row["tcp"]}),flush=True)
    for arm in ARMS:
        rows=[r for r in report["runs"] if r["arm"]==arm]; ok=[r for r in rows if r["ok"]]
        values=[r["measurement"]["elapsed_s"] for r in ok]
        report["summaries"][arm]={"planned":args.rounds,"attempted":len(rows),"passed":len(ok),"failed":len(rows)-len(ok),
            "median_total_elapsed_s":statistics.median(values) if values else None,
            "min_total_elapsed_s":min(values) if values else None,"max_total_elapsed_s":max(values) if values else None}
    report["complete"]=True;compare.save(report,args)


if __name__=="__main__":main()
