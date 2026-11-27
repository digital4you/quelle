#!/usr/bin/env python3
"""Before/after extraction and frozen-20-candidate ranking ablations.

Run separately from the Firecrawl comparison. Scores here are diagnostics;
primary-domain hits and term matches are not graded relevance judgments.
"""
import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import getpass
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import random
import secrets
import statistics
import subprocess
import threading
import time
from urllib.parse import parse_qs, urlencode, urlsplit

from run import HERE, REPO, free_port, metrics, ready, request, save


@contextmanager
def service(binary, replay, credential, output, label, reranker):
    port, token = free_port(), secrets.token_hex(24)
    env = {k:v for k,v in os.environ.items() if not k.startswith('QUELLE_') and k != 'BRAVE_SEARCH_API_KEY'}
    env.update(QUELLE_TOKEN=token, QUELLE_LISTEN=f'127.0.0.1:{port}',
               QUELLE_SEARCH_ENGINES='brave', BRAVE_SEARCH_URL=replay,
               BRAVE_SEARCH_API_KEY=credential,
               QUELLE_LIGHTPANDA=os.environ.get('QUELLE_LIGHTPANDA','lightpanda'))
    if reranker:
        env.update(QUELLE_RERANK_URL=reranker, QUELLE_RERANK_MODEL='Qwen/Qwen3-Reranker-4B', QUELLE_RERANK_TIMEOUT_MS='3000')
    with (output / (label+'.log')).open('w') as log:
        p = subprocess.Popen([str(binary)],env=env,cwd=REPO,stdout=log,stderr=log)
        try:
            base=f'http://127.0.0.1:{port}'
            ready(base+'/healthz',p)
            yield base,token
        finally:
            p.terminate()
            try:p.wait(timeout=10)
            except subprocess.TimeoutExpired:p.kill();p.wait()


def retrieve(case, endpoint, payload):
    base,token=endpoint
    result=request(base+'/api/content',payload,{'Authorization':'Bearer '+token})
    sources=[{'url':s['metadata']['url'],'title':s['metadata']['title'],'content':s['content'],
              'metadata':s['metadata']} for s in result['data'].get('sources',[])]
    errors=result['data'].get('errors',[])
    return {'raw':result,'sources':sources,'errors':errors,
            'metrics':metrics(case,sources,result['seconds'],errors)}


def summarize(records):
    result={}
    for group in ('extraction','ranking'):
        arms={name for row in records for name in row.get(group,{})}
        result[group]={}
        for name in sorted(arms):
            values=[r[group][name] for r in records if name in r.get(group,{})]
            seconds=sorted(v['metrics']['seconds'] for v in values)
            result[group][name]={
                'queries':len(values),'median_seconds':statistics.median(seconds),
                'p95_seconds':seconds[max(0,__import__('math').ceil(len(seconds)*.95)-1)],
                'sources':sum(v['metrics']['sources'] for v in values),
                'errors':sum(v['metrics']['errors'] for v in values),
                'official_sources':sum(v['metrics']['official_sources'] for v in values),
                'all_signals':sum(all(v['metrics']['signal_matches']) for v in values),
                'ranking_fallbacks':sum(bool(v['raw']['data'].get('diagnostics',{}).get('rankingFallback')) for v in values),
                'ranking_median_ms':statistics.median(v['raw']['data'].get('diagnostics',{}).get('rankingMs',0) for v in values),
            }
    return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--baseline',type=Path,required=True)
    parser.add_argument('--snapshots',type=Path,required=True,help='Original five-result Brave archive')
    parser.add_argument('--reranker',default='http://127.0.0.1:8004/rerank')
    parser.add_argument('--cases',type=int,default=25)
    parser.add_argument('--ranking-snapshots',type=Path,help='Replay a prior 20-result run without new Brave requests')
    parser.add_argument('--skip-extraction',action='store_true',help='Run only ranking ablations; retain the separate original extraction report')
    args=parser.parse_args()
    out=args.output.resolve();out.mkdir(parents=True,exist_ok=False)
    cases=json.loads((HERE/'queries.json').read_text())
    random.Random(20261127).shuffle(cases);cases=cases[:args.cases]
    current=REPO/'target/release/quelle'
    manifest={'started':datetime.now(timezone.utc).isoformat(),'queries':[c['id'] for c in cases],
              'baseline_sha256':hashlib.sha256(args.baseline.read_bytes()).hexdigest(),
              'current_sha256':hashlib.sha256(current.read_bytes()).hexdigest(),
              'controls':{'country':'CH','search_language':'per query','safesearch':'strict','spellcheck':False,
                          'ranking_candidates':20,'page_limit':5,'follow_up':False,'preferred_domains':[],
                          'reranker_model':'Qwen/Qwen3-Reranker-4B','reranker_timeout_ms':3000,
                          'chat_model':None,'renderer':'Lightpanda','fresh_process_per_arm':True}}
    save(out/'manifest.json',manifest)
    cache={};credential=secrets.token_hex(24)
    class Replay(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def do_GET(self):
            params=parse_qs(urlsplit(self.path).query)
            row=cache.get(params.get('q',[''])[0]) if self.headers.get('X-Subscription-Token')==credential else None
            status=row['status'] if row else 404
            data=json.loads(json.dumps(row['data'])) if row else {'error':'unrecorded_query'}
            if isinstance(data.get('web'),dict):data['web']['results']=data['web'].get('results',[])[:int(params.get('count',['20'])[0])]
            body=json.dumps(data).encode()
            self.send_response(status or 502);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(body)));self.end_headers()
            try:self.wfile.write(body)
            except BrokenPipeError:pass
    server=ThreadingHTTPServer(('127.0.0.1',0),Replay)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    replay=f'http://127.0.0.1:{server.server_port}/search'
    rows=[{'case':case} for case in cases]
    def checkpoint():
        save(out/'records.json',rows);save(out/'summary.json',summarize(rows))
    try:
        with service(args.baseline,replay,credential,out,'before',None) as before, service(current,replay,credential,out,'after',None) as after:
            for i,row in enumerate(rows):
                if args.skip_extraction:break
                case=row['case'];brave=json.loads((args.snapshots/(case['id']+'-brave.json')).read_text())
                urls=[h['url'] for h in brave['data'].get('web',{}).get('results',[])][:5]
                row['extraction']={}
                if not urls:continue
                for name in (['before','after'] if i%2==0 else ['after','before']):
                    row['extraction'][name]=retrieve(case,before if name=='before' else after,{'urls':urls,'limit':5})
                print('extract',case['id'],{n:v['metrics'] for n,v in row['extraction'].items()},flush=True)
                checkpoint()
        key=None if args.ranking_snapshots else os.environ.get('BRAVE_SEARCH_API_KEY') or getpass.getpass('Brave API key: ')
        # Fresh services isolate extracted-document caches between ranking arms.
        with service(current,replay,credential,out,'engine',None) as engine, service(current,replay,credential,out,'heuristic',None) as heuristic, service(current,replay,credential,out,'rerank',args.reranker) as rerank:
            endpoints={'engine':engine,'heuristic':heuristic,'rerank':rerank}
            for i,row in enumerate(rows):
                case=row['case']
                if args.ranking_snapshots:brave=json.loads((args.ranking_snapshots/(case['id']+'-brave20.json')).read_text())
                else:
                    params={'q':case['query'],'country':'CH','search_lang':case['language'],'ui_lang':'fr-CH' if case['language']=='fr' else 'de-CH',
                            'count':20,'spellcheck':'false','safesearch':'strict','result_filter':'web','text_decorations':'false'}
                    brave=request('https://api.search.brave.com/res/v1/web/search?'+urlencode(params),headers={'X-Subscription-Token':key,'X-Loc-Country':'CH'},timeout=30)
                cache[case['query']]=brave;save(out/(case['id']+'-brave20.json'),brave)
                row['ranking']={}
                order=['engine','heuristic','rerank'];order=order[i%3:]+order[:i%3]
                for name in order:
                    payload={'query':case['query'],'language':case['language'],'limit':5,'candidateLimit':20,'selection':name,'followUp':False}
                    row['ranking'][name]=retrieve(case,endpoints[name],payload)
                    print('rank',i+1,case['id'],name,row['ranking'][name]['metrics'],flush=True)
                    checkpoint()
                if not args.ranking_snapshots:time.sleep(1.1)
        manifest['completed']=datetime.now(timezone.utc).isoformat()
        save(out/'manifest.json',manifest)
    finally:
        server.shutdown();server.server_close();checkpoint()
    print(json.dumps(summarize(rows),indent=2),flush=True)


if __name__=='__main__':main()
