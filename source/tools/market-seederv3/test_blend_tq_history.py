import datetime as dt
from decimal import Decimal
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

import blend_tq_history as blend
from derive_tq_snapshot import json_bytes
from generate_general_tq import evaluate

AS_OF = dt.date(2026, 9, 28)


def day(age, price='100', volume=2, orders=1):
    return {'date':(AS_OF-dt.timedelta(days=age)).isoformat(),
            'average':price,'volume':volume,'order_count':orders}


def row(sell='120', buy='80'):
    return {'typeID':34,'name':'Test','capturedAt':'2026-09-28T19:10:46Z',
            'sellReference':sell,'buyReference':buy,'averagePriceFallbackCandidate':'999',
            'adjustedPriceIndustryOnly':'888','warnings':[]}


class BlendTests(unittest.TestCase):
    def test_week_median_dampens_outlier_and_blends_each_side(self):
        h={'observations':[day(1,'100'),day(2,'102'),day(3,'9000')], 'capturedAt':'2026-10-03T00:00:00Z'}
        r=blend.blend_record(row(),h,'a'*64,AS_OF)
        self.assertEqual((r['buyReference'],r['sellReference']),('91','111'))
        self.assertEqual(r['historyBlend']['windowDays'],7)
        self.assertEqual(r['historyBlend']['anchor'],'102')

    def test_sparse_history_uses_month_then_half_year_then_year(self):
        for observations,window in (([day(2),day(14),day(25)],30),([day(65),day(100)],180),([day(260)],365)):
            with self.subTest(window=window):
                h=blend.historical_anchor(observations,AS_OF)
                self.assertEqual(h['windowDays'],window)
                self.assertEqual(h['anchor'],'100')

    def test_completed_days_and_real_trades_only(self):
        h=blend.historical_anchor([day(0),day(-1),day(1,volume=0),day(2,orders=0),day(3,'0'),day(366)],AS_OF)
        self.assertIsNone(h['anchor'])

    def test_no_history_retains_snapshot_with_explicit_warning(self):
        r=blend.blend_record(row(),{'observations':[],'capturedAt':'now'},'a'*64,AS_OF)
        self.assertEqual((r['buyReference'],r['sellReference']),('80','120'))
        self.assertEqual(r['historyBlend']['mode'],'SNAPSHOT_ONLY_NO_HISTORY')
        self.assertIn('NO_TRADED_HISTORY_WITHIN_365_DAYS_SNAPSHOT_ONLY',r['warnings'])

    def test_missing_side_not_invented_and_industry_average_not_blended(self):
        h={'observations':[day(1),day(2),day(3)],'capturedAt':'now'}
        r=blend.blend_record(row(buy=None),h,'a'*64,AS_OF)
        self.assertIsNone(r['buyReference'])
        self.assertEqual(r['sellReference'],'110')
        self.assertEqual(r['averagePriceFallbackCandidate'],'999')
        self.assertEqual(r['adjustedPriceIndustryOnly'],'888')

    def test_crossings_are_not_repaired_or_excluded(self):
        h={'observations':[day(1),day(2),day(3)],'capturedAt':'now'}
        r=blend.blend_record(row(sell='80',buy='120'),h,'a'*64,AS_OF)
        self.assertEqual(r['state'],'CROSSED_REFERENCE')
        self.assertEqual(evaluate(r),('CROSSED_REFERENCE','110.0','90.0'))
        self.assertEqual(evaluate({**row('0.01','0.01'),'state':'DIRECT_BOTH'}),('DIRECT_BOTH','0.01','0.01'))

    def test_bad_history_fails_and_duplicate_dates_fail(self):
        for observations in ([day(1,'NaN')],[day(1),day(1)]):
            with self.assertRaises(ValueError):
                blend.historical_anchor(observations,AS_OF)

    def test_replay_is_byte_deterministic_and_pins_enforced(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); cache=root/'raw';cache.mkdir()
            source=root/'snapshot.json'
            source.write_bytes(json_bytes({'formatVersion':1,'captureID':'pin','capturedAt':'2026-09-28T00:00:00Z',
                'aggregation':'jita_sell_station_buy_in_range_best_orders','hubs':{},'types':[row()]}))
            payload={'source':'official CCP ESI','typeId':34,'regionId':10000002,'capturedAt':'2026-10-03T00:00:00Z',
                     'observations':[day(1),day(2),day(3)]}
            raw=json_bytes(payload);(cache/'34.json').write_bytes(raw)
            manifest={'regionID':10000002,'requestFailures':0,'typeCount':1,'snapshotAsOfDate':AS_OF.isoformat(),
                      'files':[{'typeID':34,'path':'34.json','sha256':hashlib.sha256(raw).hexdigest()}]}
            (cache/'manifest.json').write_bytes(json_bytes(manifest))
            blend.run(source,cache,root/'a');blend.run(source,cache,root/'b')
            for p in (root/'a').iterdir():
                self.assertEqual(p.read_bytes(),(root/'b'/p.name).read_bytes())
            manifest['snapshotAsOfDate']='2026-09-27'
            (cache/'manifest.json').write_bytes(json_bytes(manifest))
            with self.assertRaises(ValueError): blend.run(source,cache,root/'c')
            manifest['snapshotAsOfDate']=AS_OF.isoformat()
            (cache/'manifest.json').write_bytes(json_bytes(manifest))
            (cache/'34.json').write_bytes(b'{}')
            with self.assertRaises(ValueError): blend.run(source,cache,root/'c')
            self.assertFalse((root/'c'/'tq-market-reference.json').exists())


if __name__=='__main__': unittest.main()
