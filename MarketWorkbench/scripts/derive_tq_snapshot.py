"""Prepare an offline Jita snapshot before starting Market Workbench.

No network, manifest prices, synthetic EveJS orders, or adjusted_price enter quotes.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import io
import json
import os
from collections import Counter, defaultdict, deque
from decimal import Decimal
from pathlib import Path

HUBS = {
    "jita": (60003760, 10000002),
    "amarr": (60008494, 10000043),
    "dodixie": (60011866, 10000032),
    "rens": (60004588, 10000030),
    "hek": (60005686, 10000042),
}
VERSION = 1
AGGREGATION = "jita_sell_station_buy_in_range_best_orders"


def jita_topology(sde_dir: Path):
    def rows(name):
        with (sde_dir / name).open(encoding="utf-8") as stream:
            return [json.loads(line) for line in stream if line.strip()]
    header = rows("_sde.jsonl")[0]
    station = next(s for s in rows("npcStations.jsonl") if s["_key"] == HUBS["jita"][0])
    system = int(station["solarSystemID"])
    gates = {int(g["_key"]): g for g in rows("mapStargates.jsonl")}
    graph = defaultdict(set)
    for gid, gate in gates.items():
        destination = gate["destination"]
        other = gates[int(destination["stargateID"])]
        if (other["solarSystemID"] != destination["solarSystemID"]
                or other["destination"]["stargateID"] != gid
                or other["destination"]["solarSystemID"] != gate["solarSystemID"]):
            raise ValueError("nonreciprocal SDE stargate")
        graph[int(gate["solarSystemID"])].add(int(destination["solarSystemID"]))
    distances = {system: 0}
    queue = deque([system])
    while queue:
        current = queue.popleft()
        for neighbor in sorted(graph[current]):
            if neighbor not in distances:
                distances[neighbor] = distances[current] + 1
                queue.append(neighbor)
    return system, distances, header["buildNumber"]


def buy_covers_station(order: dict, station: int, system: int, distances: dict[int, int]) -> bool:
    """Public region endpoint fixes the region; numeric ranges use real SDE jumps."""
    reach = order["range"]
    if reach not in {"station", "solarsystem", "region", "1", "2", "3", "4", "5", "10", "20", "30", "40"}:
        raise ValueError(f"unsupported ESI Buy range: {reach}")
    origin = int(order["system_id"])
    if reach == "station":
        return int(order["location_id"]) == station
    if reach == "solarsystem":
        return origin == system
    if reach == "region":
        return True
    return origin in distances and distances[origin] <= int(reach)


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def decimal_text(value: Decimal | None) -> str | None:
    if value is None:
        return None
    return format(value, "f")


def references(hubs: list[dict]) -> tuple[Decimal | None, Decimal | None]:
    """Other hubs are evidence only. Missing Jita sides remain missing."""
    jita = next(h for h in hubs if h["hub"] == "jita")
    if (jita["stationID"], jita["regionID"]) != HUBS["jita"]:
        raise ValueError("Jita station/region identity mismatch")
    return tuple(Decimal(jita[s]["bestPrice"]) if jita[s]["bestPrice"] is not None else None
                 for s in ("sell", "buy"))


def state(sell: Decimal | None, buy: Decimal | None, average: Decimal | None) -> str:
    if sell is not None and buy is not None:
        return "CROSSED_REFERENCE" if buy >= sell else "DIRECT_BOTH"
    if sell is not None:
        return "DIRECT_SELL_ONLY"
    if buy is not None:
        return "DIRECT_BUY_ONLY"
    return "AVERAGE_ONLY" if average is not None else "NO_TQ_REFERENCE"


def atomic_bytes(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_name(path.name + ".tmp")
    try:
        temp.write_bytes(data)
        os.replace(temp, path)
    finally:
        temp.unlink(missing_ok=True)


def json_bytes(value: object) -> bytes:
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")


def derive(capture_root: Path, static_types: Path, taxonomy_path: Path, output: Path, sde_dir: Path) -> dict:
    capture_file = capture_root / "capture.json"
    capture = json.loads(capture_file.read_text(encoding="utf-8"))
    if capture.get("source") != "official CCP ESI" or "certificate-verifying" not in capture.get("tlsVerification", ""):
        raise ValueError("capture is not verified official ESI")
    raw_files = {entry["path"]: entry for entry in capture["rawFiles"]}
    for relative, expected in raw_files.items():
        file = capture_root / relative
        if not file.resolve().is_relative_to(capture_root.resolve()):
            raise ValueError("raw manifest path escapes capture directory")
        if sha(file) != expected["sha256"] or file.stat().st_size != expected["bytes"]:
            raise ValueError(f"pinned raw file mismatch: {relative}")
    prices = json.loads((capture_root / "prices.json").read_text(encoding="utf-8"), parse_float=Decimal)
    price_by_type = {int(row["type_id"]): row for row in prices}
    sde = json.loads(static_types.read_text(encoding="utf-8"))
    marketable = {int(row["typeID"]): row for row in sde["types"] if row.get("published") and row.get("marketGroupID") is not None}
    taxonomy = json.loads(taxonomy_path.read_text(encoding="utf-8"))
    tax_by_type = {int(row["typeID"]): row for row in taxonomy["types"]}
    if set(marketable) != set(tax_by_type):
        raise ValueError("taxonomy and SDE marketable sets differ")
    jita_system, distances, sde_build = jita_topology(sde_dir)
    static_manifest = static_types.parents[2] / "manifest.json"
    if static_manifest.is_file() and json.loads(static_manifest.read_bytes())["build"] != sde_build:
        raise ValueError("static types and Buy-range SDE build mismatch")

    # Sell requires stock at Jita 4-4. Buy requires its range to cover that station;
    # a public ranged Buy from another station/structure can be filled in Jita.
    # The other four hubs remain historical exact-station comparison evidence.
    books: dict[int, dict[str, dict[str, dict]]] = defaultdict(lambda: defaultdict(lambda: defaultdict(dict)))
    region_by_hub = {row["key"]: row for row in capture["regions"]}
    for hub, (station, region) in HUBS.items():
        meta_region = region_by_hub[hub]
        if int(meta_region["regionId"]) != region or int(meta_region["referenceStationId"]) != station:
            raise ValueError(f"region identity mismatch: {hub}")
        pages = sorted((capture_root / "orders" / hub).glob("page-*.json"))
        expected_pages = {p for p in raw_files if p.startswith(f"orders/{hub}/page-") and p.endswith(".json")}
        if not pages or {p.relative_to(capture_root).as_posix() for p in pages} != expected_pages:
            raise ValueError(f"unlisted or missing order page: {hub}")
        for page in pages:
            rows = json.loads(page.read_text(encoding="utf-8"), parse_float=Decimal)
            for order in rows:
                if int(order["volume_remain"]) <= 0:
                    continue
                if hub == "jita" and order["is_buy_order"]:
                    eligible = buy_covers_station(order, station, jita_system, distances)
                else:
                    eligible = int(order["location_id"]) == station
                if not eligible:
                    continue
                price = Decimal(str(order["price"]))
                if not price.is_finite() or price <= 0:
                    continue
                type_id = int(order["type_id"])
                if type_id not in marketable:
                    continue
                side = "buy" if order["is_buy_order"] else "sell"
                slot = books[type_id][hub][side]
                slot["orderCount"] = slot.get("orderCount", 0) + 1
                slot["quotedVolume"] = slot.get("quotedVolume", 0) + int(order["volume_remain"])
                old = slot.get("bestPrice")
                if old is None or (price > old if side == "buy" else price < old) or (price == old and int(order["order_id"]) < slot["bestOrderId"]):
                    slot["bestPrice"] = price
                    slot["bestOrderId"] = int(order["order_id"])
                    if hub == "jita":
                        slot["bestOrder"] = {"locationID": int(order["location_id"]),
                            "systemID": int(order["system_id"]), "range": order["range"],
                            "jumpsToReference": distances.get(int(order["system_id"])),
                            "minVolume": int(order["min_volume"])}

    records = []
    ordinary = []
    unresolved = []
    counts = Counter()
    ordinary_counts = Counter()
    family_counts: dict[str, Counter] = defaultdict(Counter)
    for type_id, item in sorted(marketable.items()):
        price_row = price_by_type.get(type_id, {})
        avg = price_row.get("average_price")
        adjusted = price_row.get("adjusted_price")
        avg = Decimal(str(avg)) if avg is not None else None
        avg = avg if avg is not None and avg.is_finite() and avg > 0 else None
        adjusted = Decimal(str(adjusted)) if adjusted is not None else None
        hubs = []
        for hub, (station, region) in HUBS.items():
            entry = {"hub": hub, "stationID": station, "regionID": region}
            for side in ("sell", "buy"):
                slot = books[type_id][hub].get(side, {})
                entry[side] = {
                    "bestPrice": decimal_text(slot.get("bestPrice")),
                    "bestOrderID": slot.get("bestOrderId"),
                    "orderCount": slot.get("orderCount", 0),
                    "quotedVolume": slot.get("quotedVolume", 0),
                }
                if "bestOrder" in slot:
                    entry[side]["bestOrder"] = slot["bestOrder"]
            hubs.append(entry)
        sell, buy = references(hubs)
        status = state(sell, buy, avg)
        warnings = []
        if status == "CROSSED_REFERENCE":
            warnings.append("BUY_REFERENCE_AT_OR_ABOVE_SELL_REFERENCE")
        if sell is None:
            warnings.append("NO_DIRECT_SELL_REFERENCE")
        if buy is None:
            warnings.append("NO_DIRECT_BUY_REFERENCE")
        tax = tax_by_type[type_id]
        record = {
            "typeID": type_id, "name": item["name"], "categoryID": item.get("categoryID"),
            "groupID": item.get("groupID"), "marketGroupID": item.get("marketGroupID"),
            "capturedAt": capture["capturedAt"], "averagePriceFallbackCandidate": decimal_text(avg),
            "adjustedPriceIndustryOnly": decimal_text(adjusted),
            "sellReference": decimal_text(sell), "buyReference": decimal_text(buy),
            "sellHubCount": int(sell is not None),
            "buyHubCount": int(buy is not None),
            "aggregation": AGGREGATION,
            "provenance": {"captureID": capture["captureId"], "endpoint": "/markets/{region_id}/orders/", "source": "official_esi"},
            "hubs": hubs, "state": status, "warnings": warnings,
        }
        records.append(record)
        counts[status] += 1
        if tax["semanticClass"] == "ORDINARY_MARKET_COMPATIBLE":
            ordinary.append(type_id)
            ordinary_counts[status] += 1
            family_counts[tax.get("family") or "Unknown"][status] += 1
        if status != "DIRECT_BOTH":
            unresolved.append({"typeID": type_id, "name": item["name"], "state": status, "ordinary": type_id in ordinary})

    dataset = {"formatVersion": VERSION, "captureID": capture["captureId"], "capturedAt": capture["capturedAt"],
               "aggregation": AGGREGATION, "hubs": HUBS, "types": records}
    coverage = {"formatVersion": VERSION, "marketableTypeCount": len(records), "ordinaryTypeCount": len(ordinary),
                "allMarketableStates": dict(sorted(counts.items())), "ordinaryStates": dict(sorted(ordinary_counts.items())),
                "ordinaryByFamily": {k: dict(sorted(v.items())) for k, v in sorted(family_counts.items())}}
    outputs = {"tq-market-reference.json": json_bytes(dataset), "coverage.json": json_bytes(coverage),
               "unresolved.json": json_bytes({"types": unresolved})}
    csv_buffer = io.StringIO(newline="")
    writer = csv.writer(csv_buffer, lineterminator="\n")
    writer.writerow(["typeID", "name", "capturedAt", "sellReference", "buyReference", "sellHubCount", "buyHubCount", "averagePriceFallbackCandidate", "adjustedPriceIndustryOnly", "state"])
    for row in records:
        writer.writerow([row.get(k) for k in ("typeID", "name", "capturedAt", "sellReference", "buyReference", "sellHubCount", "buyHubCount", "averagePriceFallbackCandidate", "adjustedPriceIndustryOnly", "state")])
    outputs["tq-market-reference.csv"] = csv_buffer.getvalue().encode("utf-8")
    manifest = {"formatVersion": VERSION, "captureID": capture["captureId"], "capturedAt": capture["capturedAt"],
                "generatorSHA256": sha(Path(__file__)), "algorithm": AGGREGATION,
                "referenceStationID": HUBS["jita"][0], "referenceRegionID": HUBS["jita"][1],
                "referenceSystemID": jita_system, "sdeBuild": sde_build,
                "buyEligibility": "public The Forge Buy orders whose station/system/jump/region range covers Jita 4-4",
                "sellEligibility": "Sell stock physically located at Jita 4-4 only",
                "rangeInputSHA256": {name: sha(sde_dir / name) for name in ("_sde.jsonl", "npcStations.jsonl", "mapStargates.jsonl")},
                "otherHubs": "informational only; never contribute to references",
                "officialEsiCaptureManifestSHA256": sha(capture_file), "staticItemTypesSHA256": sha(static_types),
                "ordinaryTaxonomySHA256": sha(taxonomy_path),
                "files": {name: {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)} for name, data in sorted(outputs.items())}}
    outputs["manifest.json"] = json_bytes(manifest)
    for name, data in outputs.items():
        atomic_bytes(output / name, data)
    return coverage


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--capture-root", type=Path, required=True)
    parser.add_argument("--static-types", type=Path, required=True)
    parser.add_argument("--taxonomy", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sde-dir", type=Path, required=True, help="matching-build pinned CCP SDE JSONL directory")
    args = parser.parse_args()
    print(json.dumps(derive(args.capture_root, args.static_types, args.taxonomy, args.output, args.sde_dir), sort_keys=True))
