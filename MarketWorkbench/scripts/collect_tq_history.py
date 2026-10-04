"""Bounded official ESI history capture. Workbench itself remains offline."""
import argparse
import concurrent.futures
import datetime as dt
import hashlib
import importlib.util
import json
from pathlib import Path
import threading
import time


def known_unavailable(error, url):
    message = str(error)
    if not message.startswith(f'HTTP 400 from {url};') or '; body=' not in message:
        return None
    body = message.split('; body=', 1)[1]
    try:
        if json.loads(body) != {'error': 'Type not tradable on market!'}:
            return None
    except ValueError:
        return None
    return {'httpStatus': 400, 'body': body, 'bodySha256': hashlib.sha256(body.encode('utf-8')).hexdigest(), 'error': message}


def run(dataset, output, existing_cache, collector_library, workers=16, requests_per_second=8):
    spec = importlib.util.spec_from_file_location('esi_history_capture', collector_library)
    esi = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(esi)
    collector_bytes = Path(__file__).read_bytes()
    collector_sha = hashlib.sha256(collector_bytes).hexdigest()
    library_sha = esi.sha256(collector_library)
    snapshot_sha = esi.sha256(dataset)
    snapshot = json.loads(dataset.read_bytes())
    records = snapshot['types']
    as_of = dt.date.fromisoformat(snapshot['capturedAt'][:10])
    ids = sorted({int(r['typeID']) for r in records})
    if len(ids) != len(records) or any(i <= 0 for i in ids):
        raise ValueError('invalid dataset type census')
    output.mkdir(parents=True, exist_ok=True)
    source_name = f'collector-source-{collector_sha}.py'
    esi.atomic_bytes(output/source_name, collector_bytes)
    lock = threading.Lock()
    next_request = 0.
    results = []

    def fetch(tid):
        nonlocal next_request
        target = output / 'types' / f'{tid}.json'
        old = target if target.is_file() else existing_cache / f'{tid}.json'
        if old.is_file():
            try:
                payload = json.loads(old.read_bytes())
                if payload['source'] != 'official CCP ESI' or payload['regionId'] != 10000002 or payload['typeId'] != tid:
                    raise ValueError('cache identity mismatch')
                if dt.date.fromisoformat(payload['capturedAt'][:10]) < as_of:
                    raise ValueError('cache predates this snapshot; refresh required')
                esi.validate_history(payload['observations'], 'jita', tid)
                if not target.exists():
                    esi.atomic_bytes(target, old.read_bytes())
                return {'typeID':tid,'path':f'types/{tid}.json','sha256':esi.sha256(target),'state':payload['historyState'],'cached':True}
            except (ValueError, KeyError):
                pass
        with lock:
            delay = max(0, next_request - time.monotonic())
            next_request = max(next_request, time.monotonic()) + 1 / requests_per_second
        if delay:
            time.sleep(delay)
        endpoint = f'/markets/10000002/history/?type_id={tid}'
        try:
            data, meta = esi.request_json(esi.BASE + endpoint)
            observations = esi.validate_history(data, 'jita', tid)
            payload = {'schemaVersion':1,'source':'official CCP ESI','capturedAt':esi.iso(),
                'regionId':10000002,'regionKey':'jita','typeId':tid,'endpoint':endpoint,
                'responseMetadata':meta,'responseBodySha256':meta['bodySha256'],
                'observations':observations,'historyState':'observations' if observations else 'empty_response_no_observations'}
            esi.atomic_json(target, payload)
            return {'typeID':tid,'path':f'types/{tid}.json','sha256':esi.sha256(target),'state':payload['historyState'],'cached':False}
        except Exception as error:
            unavailable = known_unavailable(error, esi.BASE + endpoint)
            if unavailable:
                payload = {'schemaVersion':1, 'source':'official CCP ESI', 'capturedAt':esi.iso(),
                    'regionId':10000002, 'regionKey':'jita', 'typeId':tid, 'endpoint':endpoint,
                    'responseMetadata':unavailable, 'responseBodySha256':unavailable['bodySha256'],
                    'observations':[], 'historyState':'unavailable_type_not_tradable'}
                esi.atomic_json(target, payload)
                return {'typeID':tid, 'path':f'types/{tid}.json', 'sha256':esi.sha256(target), 'state':payload['historyState'], 'cached':False}
            return {'typeID':tid,'state':'REQUEST_FAILED','error':str(error)}

    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {pool.submit(fetch, tid):tid for tid in ids}
        for future in concurrent.futures.as_completed(futures):
            results.append(future.result())
            if len(results) % 100 == 0 or len(results) == len(ids):
                counts = {}
                for r in results:
                    counts[r['state']] = counts.get(r['state'],0)+1
                progress = {'completed':len(results),'total':len(ids),'seconds':round(time.monotonic()-started), 'states':counts}
                esi.atomic_json(output/'progress.json', progress)
                print(json.dumps(progress), flush=True)
    manifest = {'formatVersion':1,'source':'official CCP ESI','regionID':10000002,
        'snapshotDatasetSHA256':snapshot_sha,'collectorLibrarySHA256':library_sha,
        'collectorSHA256':collector_sha,'collectorSource':source_name,'capturedAt':esi.iso(),
        'snapshotAsOfDate':as_of.isoformat(),
        'typeCount':len(ids),'requestFailures':sum(r['state']=='REQUEST_FAILED' for r in results),
        'files':sorted(results,key=lambda r:r['typeID'])}
    esi.atomic_json(output/'manifest.json',manifest)
    return manifest


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    for name in ('dataset','output','existing-cache','collector-library'):
        p.add_argument('--'+name,type=Path,required=True)
    p.add_argument('--workers',type=int,default=16)
    p.add_argument('--requests-per-second',type=float,default=8)
    args=p.parse_args()
    if not 1<=args.workers<=32 or not 0<args.requests_per_second<=10:
        p.error('use 1–32 workers and at most 10 requests per second')
    v=run(args.dataset,args.output,args.existing_cache,args.collector_library,args.workers,args.requests_per_second)
    print(json.dumps({'complete':True,'types':v['typeCount'],'failures':v['requestFailures']}))
