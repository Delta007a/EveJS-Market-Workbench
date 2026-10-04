"""Generate the non-active GENERAL_TQ v1 policy and structural preview offline."""

import argparse
import hashlib
import json
from collections import Counter
from decimal import Decimal, ROUND_HALF_UP
from pathlib import Path

STATES = ("DIRECT_BOTH", "DIRECT_SELL_ONLY", "DIRECT_BUY_ONLY", "AVERAGE_ONLY",
          "CROSSED_REFERENCE", "NO_TQ_REFERENCE", "ROUNDING_CROSS")
FALLBACK_BUY = Decimal("0.80")
FALLBACK_SELL = Decimal("1.25")


def rounded(value):
    # Match market-common::round_isk: binary f64 multiplication, then ties away
    # from zero at the integer-cent boundary. All accepted references are > 0.
    scaled = float(value) * 100.0
    cents = int(Decimal.from_float(scaled).to_integral_value(rounding=ROUND_HALF_UP))
    return max(0.01, cents / 100.0)


def evaluate(row):
    status = row["state"]
    if status == "NO_TQ_REFERENCE":
        return status, None, None
    sell = float(row["sellReference"]) if row["sellReference"] else None
    buy = float(row["buyReference"]) if row["buyReference"] else None
    average = float(row["averagePriceFallbackCandidate"]) if row["averagePriceFallbackCandidate"] else None
    if status == "DIRECT_SELL_ONLY":
        buy = sell * float(FALLBACK_BUY)
    elif status == "DIRECT_BUY_ONLY":
        sell = buy * float(FALLBACK_SELL)
    elif status == "AVERAGE_ONLY":
        sell, buy = average * float(FALLBACK_SELL), average * float(FALLBACK_BUY)
    if sell is None or buy is None:
        raise ValueError(f"incomplete evidence for {row['typeID']} {status}")
    sell, buy = rounded(sell), rounded(buy)
    return status, str(buy), str(sell)


def run(dataset, taxonomy, policy_path, preview_path, unresolved_path, expected_sha):
    dataset_bytes = dataset.read_bytes()
    expected = expected_sha.lower()
    if hashlib.sha256(dataset_bytes).hexdigest() != expected:
        raise ValueError("TQ dataset SHA256 differs from pinned input")
    data = json.loads(dataset_bytes)
    compatibility = json.loads(taxonomy.read_text(encoding="utf-8"))
    ordinary = {int(r["typeID"]) for r in compatibility["types"] if r["semanticClass"] == "ORDINARY_MARKET_COMPATIBLE"}
    by_type = {int(r["typeID"]): r for r in data["types"]}
    if not ordinary <= by_type.keys():
        raise ValueError("ordinary taxonomy not covered by TQ reference dataset")
    groups = {state: [] for state in STATES}
    preview_rows = []
    for type_id in sorted(ordinary):
        row = by_type[type_id]
        status, buy, sell = evaluate(row)
        groups[status].append(type_id)
        preview_rows.append({"typeID": type_id, "name": row["name"], "evidenceState": row["state"],
                             "decision": status, "buy": buy, "sell": sell})
    profiles = [
        {"id": "general_tq_direct", "sides": "buy_sell",
         "sell": {"source": "tq_snapshot", "multiplier": "1.00"},
         "buy": {"source": "tq_snapshot", "multiplier": "1.00"}},
        {"id": "general_tq_sell_only", "sides": "buy_sell",
         "sell": {"source": "tq_snapshot", "multiplier": "1.00"},
         "buy": {"source": "tq_snapshot_sell_fallback", "multiplier": str(FALLBACK_BUY)}},
        {"id": "general_tq_buy_only", "sides": "buy_sell",
         "sell": {"source": "tq_snapshot_buy_fallback", "multiplier": str(FALLBACK_SELL)},
         "buy": {"source": "tq_snapshot", "multiplier": "1.00"}},
        {"id": "general_tq_average_only", "sides": "buy_sell",
         "sell": {"source": "tq_average_price", "multiplier": str(FALLBACK_SELL)},
         "buy": {"source": "tq_average_price", "multiplier": str(FALLBACK_BUY)}},
    ]
    rules = []
    for state, profile in (("DIRECT_BOTH", "general_tq_direct"),
                           ("DIRECT_SELL_ONLY", "general_tq_sell_only"),
                           ("DIRECT_BUY_ONLY", "general_tq_buy_only"),
                           ("AVERAGE_ONLY", "general_tq_average_only")):
        ids = groups[state] + (groups['CROSSED_REFERENCE'] if state == 'DIRECT_BOTH' else [])
        if ids:
            rules.append({"id": state.lower(), "selector": {"type_ids": sorted(ids)},
                          "priority": 10, "profile": profile})
    for state in ("NO_TQ_REFERENCE",):
        if groups[state]:
            rules.append({"id": state.lower(), "selector": {"type_ids": groups[state]},
                          "priority": 100, "sides": "unseeded"})
    policy = {"format_version": 1, "catalog_contract": {"sde_build": 3396210, "fact_registry_version": 1},
              "profiles": profiles, "rules": rules}
    counts = {state: len(groups[state]) for state in STATES}
    quoted = sum(counts[s] for s in STATES[:5])
    if sum(counts.values()) != len(ordinary) or quoted + counts['NO_TQ_REFERENCE'] != len(ordinary):
        raise ValueError("coverage partition failed")
    preview = {"preset": "GENERAL_TQ", "active": False, "datasetSHA256": expected,
               "ordinaryTypeCount": len(ordinary), "quotedBuySellCount": quoted,
               "stateCounts": counts, "fallbackBuyMultiplier": str(FALLBACK_BUY),
               "fallbackSellMultiplier": str(FALLBACK_SELL),
               "buyGeSellWarnings":sum(r['buy'] is not None and float(r['buy'])>=float(r['sell']) for r in preview_rows), "rows": preview_rows}
    unresolved = [r for r in preview_rows if r["buy"] is None]
    for path, obj in ((policy_path, policy), (preview_path, preview), (unresolved_path, unresolved)):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(obj, ensure_ascii=False, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    return {"ordinary": len(ordinary), "quoted": quoted, "states": counts,
            "policySHA256": hashlib.sha256(policy_path.read_bytes()).hexdigest()}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for field in ("dataset", "taxonomy", "policy", "preview", "unresolved"):
        parser.add_argument("--" + field, type=Path, required=True)
    parser.add_argument("--dataset-sha256", required=True, help="explicit pinned prepared snapshot identity")
    args = parser.parse_args()
    print(json.dumps(run(args.dataset, args.taxonomy, args.policy, args.preview, args.unresolved, args.dataset_sha256), sort_keys=True))
