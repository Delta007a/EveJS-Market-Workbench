import unittest
import hashlib
import json
import tempfile
from decimal import Decimal
from pathlib import Path

from derive_tq_snapshot import AGGREGATION, HUBS, derive, references, state, buy_covers_station, jita_topology


class TqReferenceTests(unittest.TestCase):
    def test_jita_only_not_other_hub_median(self):
        hubs = [{"hub": "jita", "stationID": 60003760, "regionID": 10000002,
                 "sell": {"bestPrice": "4496"}, "buy": {"bestPrice": "3108"}},
                {"hub": "dodixie", "sell": {"bestPrice": "1"}, "buy": {"bestPrice": "32"}}]
        self.assertEqual(references(hubs), (Decimal("4496"), Decimal("3108")))
        hubs[0]["buy"]["bestPrice"] = None
        self.assertEqual(references(hubs), (Decimal("4496"), None))

    def test_missing_sides_do_not_use_average(self):
        self.assertEqual(state(None, None, Decimal("100")), "AVERAGE_ONLY")
        self.assertEqual(state(None, None, None), "NO_TQ_REFERENCE")
        self.assertEqual(state(Decimal("20"), None, Decimal("100")), "DIRECT_SELL_ONLY")
        self.assertEqual(state(None, Decimal("10"), Decimal("100")), "DIRECT_BUY_ONLY")

    def test_crossed_is_reported(self):
        self.assertEqual(state(Decimal("10"), Decimal("10"), None), "CROSSED_REFERENCE")
        self.assertEqual(state(Decimal("10"), Decimal("11"), None), "CROSSED_REFERENCE")
        self.assertEqual(state(Decimal("10"), Decimal("9"), None), "DIRECT_BOTH")

    def test_buy_range_includes_remote_public_structure_and_excludes_outside_range(self):
        distances = {100: 0, 101: 1, 102: 2}
        def covers(location, system, reach):
            return buy_covers_station({"location_id":location,"system_id":system,"range":reach}, 60003760, 100, distances)
        self.assertTrue(covers(60003760,100,"station"))
        self.assertFalse(covers(999,100,"station"))
        self.assertTrue(covers(999,100,"solarsystem"))
        self.assertFalse(covers(999,101,"solarsystem"))
        self.assertTrue(covers(1042508032148,101,"1"))
        self.assertFalse(covers(1042508032148,102,"1"))
        self.assertTrue(covers(999,102,"2"))
        self.assertFalse(covers(999,103,"40"))
        self.assertTrue(covers(999,103,"region"))
        with self.assertRaisesRegex(ValueError,"unsupported ESI Buy range"):
            covers(60003760,100,"unknown")

    def test_raw_replay_best_orders_deterministic_and_pinned(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            capture = root / "capture"
            capture.mkdir()

            def write(path, value):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps(value), encoding="utf-8")

            def order(oid, price, buy, station=60003760, volume=10, reach="station", system=100):
                return {"type_id": 34, "order_id": oid, "price": price,
                        "is_buy_order": buy, "location_id": station, "volume_remain": volume,
                        "range":reach,"system_id":system,"min_volume":1}

            jita_orders = [order(1, 32, True), order(2, 3108, True), order(3, 4496, False),
                           order(4, 9000, False), order(5, 9999, True, station=999),
                           order(6, 1, False, volume=0), order(7, 3108, True),
                           order(8, -1, False),order(9, 4000, True,1042508032148,reach="1",system=101),
                           order(12, 88888, True,999,reach="1",system=102),
                           order(13, 1, False,999,reach="region",system=101)]
            write(capture / "prices.json", [{"type_id": 34, "average_price": 999, "adjusted_price": 888}])
            for hub, (station, _) in HUBS.items():
                rows = jita_orders if hub == "jita" else [order(10, 1, False, station), order(11, 99999, True, station)]
                write(capture / "orders" / hub / "page-00001.json", rows)
            files = [{"path": p.relative_to(capture).as_posix(), "bytes": p.stat().st_size,
                      "sha256": hashlib.sha256(p.read_bytes()).hexdigest()} for p in capture.rglob("*.json")]
            write(capture / "capture.json", {"source": "official CCP ESI", "tlsVerification": "certificate-verifying",
                  "captureId": "test", "capturedAt": "2026-09-28T19:10:46Z", "rawFiles": files,
                  "regions": [{"key": h, "referenceStationId": s, "regionId": r} for h, (s, r) in HUBS.items()]})
            static, taxonomy = root / "static.json", root / "taxonomy.json"
            sde = root / "sde"
            def write_lines(name, rows):
                sde.mkdir(exist_ok=True)
                (sde/name).write_text(''.join(json.dumps(r)+'\n' for r in rows),encoding='utf-8')
            write_lines('_sde.jsonl',[{'buildNumber':1}])
            write_lines('npcStations.jsonl',[{'_key':60003760,'solarSystemID':100}])
            write_lines('mapStargates.jsonl',[
                {'_key':1,'solarSystemID':100,'destination':{'solarSystemID':101,'stargateID':2}},
                {'_key':2,'solarSystemID':101,'destination':{'solarSystemID':100,'stargateID':1}},
                {'_key':3,'solarSystemID':101,'destination':{'solarSystemID':102,'stargateID':4}},
                {'_key':4,'solarSystemID':102,'destination':{'solarSystemID':101,'stargateID':3}}])
            self.assertEqual(jita_topology(sde)[1],{100:0,101:1,102:2})
            write(static, {"types": [{"typeID": 34, "name": "Tritanium", "published": True, "marketGroupID": 1}]})
            write(taxonomy, {"types": [{"typeID": 34, "semanticClass": "ORDINARY_MARKET_COMPATIBLE"}]})
            output = root / "output"
            derive(capture, static, taxonomy, output, sde)
            first = (output / "tq-market-reference.json").read_bytes()
            row = json.loads(first)["types"][0]
            self.assertEqual((row["buyReference"], row["sellReference"]), ("4000", "4496"))
            self.assertEqual((row["buyHubCount"], row["sellHubCount"]), (1, 1))
            self.assertEqual(row["aggregation"], AGGREGATION)
            self.assertEqual(row["hubs"][0]["buy"]["bestOrderID"], 9)
            self.assertEqual(row["hubs"][0]["buy"]["bestOrder"]["locationID"],1042508032148)
            self.assertEqual(row["hubs"][0]["buy"]["bestOrder"]["jumpsToReference"],1)
            derive(capture, static, taxonomy, output, sde)
            self.assertEqual(first, (output / "tq-market-reference.json").read_bytes())
            write(capture / "orders/jita/page-00002.json", [])
            with self.assertRaisesRegex(ValueError, "unlisted or missing"):
                derive(capture, static, taxonomy, output, sde)
            self.assertEqual(first, (output / "tq-market-reference.json").read_bytes())
            (capture / "orders/jita/page-00001.json").write_text("[]", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "pinned raw file mismatch"):
                derive(capture, static, taxonomy, output, sde)


if __name__ == "__main__":
    unittest.main()
