"""Offline Jita snapshot + The Forge executed-price history, equally weighted."""
import argparse
from collections import Counter
import csv
import datetime as dt
from decimal import Decimal
import hashlib
import io
import json
from pathlib import Path
import statistics

from derive_tq_snapshot import atomic_bytes, decimal_text, json_bytes, sha, state

AGGREGATION = 'jita_snapshot_history_50_50'
# Completed calendar days, requiring multiple traded days before accepting a
# short window. The final annual window permits one print, explicitly warned.
WINDOWS = ((7, 3), (30, 3), (180, 2), (365, 1))


def historical_anchor(observations, as_of):
    seen = set()
    trades = []
    for row in observations:
        date = dt.date.fromisoformat(row['date'])
        if date in seen:
            raise ValueError('duplicate history date')
        seen.add(date)
        average = Decimal(str(row['average']))
        if not average.is_finite() or average < 0:
            raise ValueError('invalid historical average')
        if row['volume'] > 0 and row['order_count'] > 0 and average > 0 and date < as_of:
            trades.append((date, average, row['volume'], row['order_count']))
    for days, minimum in WINDOWS:
        selected = sorted(t for t in trades if t[0] >= as_of - dt.timedelta(days=days))
        if len(selected) >= minimum:
            return {'anchor': decimal_text(statistics.median(t[1] for t in selected)),
                    'windowDays': days, 'tradingDays': len(selected),
                    'totalTradedVolume': sum(t[2] for t in selected),
                    'lastTradeDate': selected[-1][0].isoformat(),
                    'observations': [{'date':t[0].isoformat(), 'average':decimal_text(t[1]),
                                      'volume':t[2], 'orderCount':t[3]} for t in selected]}
    return {'anchor':None,'windowDays':None,'tradingDays':0,'totalTradedVolume':0,
            'lastTradeDate':None,'observations':[]}


def blend_record(row, payload, cache_sha, as_of):
    result = dict(row)
    evidence = historical_anchor(payload['observations'], as_of)
    history = {'regionID':10000002, 'asOfDate':as_of.isoformat(),
               'historyCapturedAt':payload['capturedAt'], 'cacheSHA256':cache_sha,
               'statistic':'median_of_daily_executed_average', 'weight':'0.5',
               'snapshotBuy':row['buyReference'], 'snapshotSell':row['sellReference'],
               **evidence}
    history['mode'] = 'BLENDED' if evidence['anchor'] else 'SNAPSHOT_ONLY_NO_HISTORY'
    result['historyBlend'] = history
    result['aggregation'] = AGGREGATION
    result['warnings'] = [w for w in row['warnings'] if w != 'BUY_REFERENCE_AT_OR_ABOVE_SELL_REFERENCE']
    if evidence['anchor']:
        anchor = Decimal(evidence['anchor'])
        for side in ('sellReference', 'buyReference'):
            if row[side] is not None:
                result[side] = decimal_text((anchor + Decimal(row[side])) / 2)
        if evidence['windowDays'] > 7:
            result['warnings'].append('HISTORY_WINDOW_EXTENDED_' + str(evidence['windowDays']) + '_DAYS')
        if evidence['tradingDays'] < 3:
            result['warnings'].append('SPARSE_HISTORY_' + str(evidence['tradingDays']) + '_TRADING_DAYS')
    else:
        result['warnings'].append('NO_TRADED_HISTORY_WITHIN_365_DAYS_SNAPSHOT_ONLY')
        if payload.get('historyState') == 'unavailable_type_not_tradable':
            result['warnings'].append('ESI_HISTORY_UNAVAILABLE_TYPE_NOT_TRADABLE')
    result['state'] = state(Decimal(result['sellReference']) if result['sellReference'] else None,
                            Decimal(result['buyReference']) if result['buyReference'] else None,
                            Decimal(result['averagePriceFallbackCandidate']) if result['averagePriceFallbackCandidate'] else None)
    if result['state'] == 'CROSSED_REFERENCE':
        result['warnings'].append('BUY_REFERENCE_AT_OR_ABOVE_SELL_REFERENCE')
    return result


def run(snapshot, history_root, output):
    data = json.loads(snapshot.read_bytes())
    if data['aggregation'] != 'jita_sell_station_buy_in_range_best_orders':
        raise ValueError('blend requires range-aware Jita snapshot')
    manifest_bytes = (history_root / 'manifest.json').read_bytes()
    manifest = json.loads(manifest_bytes)
    if manifest['regionID'] != 10000002 or manifest['requestFailures']:
        raise ValueError('history capture incomplete or wrong region')
    as_of = dt.date.fromisoformat(data['capturedAt'][:10])
    if manifest['snapshotAsOfDate'] != as_of.isoformat():
        raise ValueError('history capture snapshot date mismatch')
    files = {r['typeID']:r for r in manifest['files']}
    ids = {r['typeID'] for r in data['types']}
    if len(files) != len(manifest['files']) or files.keys() != ids or manifest['typeCount'] != len(ids):
        raise ValueError('history/snapshot census mismatch')
    rows = []
    for row in data['types']:
        entry = files[row['typeID']]
        relative = Path(entry['path'])
        if relative.is_absolute() or '..' in relative.parts:
            raise ValueError('invalid history cache path')
        path = history_root / relative
        if sha(path) != entry['sha256']:
            raise ValueError('history cache SHA mismatch')
        payload = json.loads(path.read_bytes())
        if payload['typeId'] != row['typeID'] or payload['regionId'] != 10000002 or payload['source'] != 'official CCP ESI':
            raise ValueError('history cache identity mismatch')
        if dt.date.fromisoformat(payload['capturedAt'][:10]) < as_of:
            raise ValueError('history cache predates the snapshot')
        rows.append(blend_record(row, payload, entry['sha256'], as_of))
    result = {**data, 'aggregation':AGGREGATION, 'types':rows}
    counts = Counter(str(r['historyBlend']['windowDays']) for r in rows)
    coverage = {'marketableTypes':len(rows), 'historyWindows':dict(sorted(counts.items())),
                'states':dict(sorted(Counter(r['state'] for r in rows).items())),
                'asOfDate':as_of.isoformat(), 'historyRegion':'The Forge',
                'windows':[{'days':d,'minimumTradingDays':n} for d,n in WINDOWS]}
    buf = io.StringIO(newline='')
    fields = ('typeID','name','buyReference','sellReference','state')
    writer = csv.writer(buf,lineterminator='\n')
    writer.writerow((*fields,'historyAnchor','windowDays','tradingDays','snapshotBuy','snapshotSell'))
    for r in rows:
        h=r['historyBlend']
        writer.writerow([r[k] for k in fields]+[h[k] for k in ('anchor','windowDays','tradingDays','snapshotBuy','snapshotSell')])
    outputs = {'tq-market-reference.json':json_bytes(result), 'coverage.json':json_bytes(coverage),
               'tq-market-reference.csv':buf.getvalue().encode('utf-8'),
               'unresolved.json':json_bytes({'types':[{'typeID':r['typeID'],'name':r['name'],'state':r['state']} for r in rows if r['state']=='NO_TQ_REFERENCE']})}
    final_manifest = {'formatVersion':1,'aggregation':AGGREGATION,'generatorSHA256':sha(Path(__file__)),
                      'snapshotSHA256':sha(snapshot),'historyManifestSHA256':hashlib.sha256(manifest_bytes).hexdigest(),
                      'capturedAt':data['capturedAt'],'historyAsOfDate':as_of.isoformat(),
                      'fallback':'If no qualifying traded history in 365 days, retain snapshot side with warning; never invent history or a missing side.',
                      'files':{name:{'sha256':hashlib.sha256(value).hexdigest(),'bytes':len(value)} for name,value in sorted(outputs.items())}}
    outputs['manifest.json']=json_bytes(final_manifest)
    for name,value in outputs.items():
        atomic_bytes(output/name,value)
    return coverage


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    for name in ('snapshot','history-root','output'):
        p.add_argument('--'+name,type=Path,required=True)
    args=p.parse_args()
    print(json.dumps(run(args.snapshot,args.history_root,args.output),sort_keys=True))
