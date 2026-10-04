#!/usr/bin/env python3
"""Offline-first EVE Online ESI trade evidence capture. Python stdlib only."""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import datetime as dt
import hashlib
import math
import json
import os
from pathlib import Path
import random
import re
import shutil
import ssl
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
CONFIG = HERE / "regions.json"
BASE = "https://esi.evetech.net/latest"
COMPAT = "2026-02-24"
UA = "EveJS-MarketPricingEvidence/1.0 (offline evidence capture; contact: market-evidence-maintainer)"
CTX = ssl.create_default_context()  # certificate and hostname validation stay enabled
ORDER_WORKERS = 8  # bounded; far below the 12,000-token order-group budget for this five-region one-shot capture
_cooldown_lock = threading.Lock()
_cooldown_until = 0.0


class CaptureError(RuntimeError):
    pass


def utcnow() -> dt.datetime:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0)


def stamp(value: dt.datetime | None = None) -> str:
    return (value or utcnow()).strftime("%Y%m%dT%H%M%SZ")


def iso(value: dt.datetime | None = None) -> str:
    return (value or utcnow()).isoformat().replace("+00:00", "Z")


def canonical_bytes(value: object) -> bytes:
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")


def atomic_bytes(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        os.replace(temp_name, path)
    finally:
        try:
            os.unlink(temp_name)
        except FileNotFoundError:
            pass


def atomic_json(path: Path, value: object) -> None:
    atomic_bytes(path, canonical_bytes(value))


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def load_regions(path: Path = CONFIG) -> list[dict]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:
        raise CaptureError(f"Cannot read region config {path}: {exc}") from exc
    if payload.get("schemaVersion") != 1 or not isinstance(payload.get("regions"), list):
        raise CaptureError("Region config schemaVersion/regions invalid")
    regions = payload["regions"]
    ids = [r.get("regionId") for r in regions]
    if not regions or len(ids) != len(set(ids)) or any(not isinstance(i, int) or i <= 0 for i in ids):
        raise CaptureError("Region IDs must be positive unique integers")
    for r in regions:
        if not all(isinstance(r.get(k), str) and r[k] for k in ("key", "name", "hub")):
            raise CaptureError("Each region requires key, name and hub strings")
    return regions


def request_json(url: str, *, max_attempts: int = 7) -> tuple[object, dict]:
    headers = {"Accept": "application/json", "User-Agent": UA, "X-Compatibility-Date": COMPAT}
    for attempt in range(max_attempts):
        global _cooldown_until
        with _cooldown_lock:
            wait = _cooldown_until - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        req = urllib.request.Request(url, headers=headers)
        try:
            with urllib.request.urlopen(req, context=CTX, timeout=90) as resp:
                body = resp.read()
                h = {k.lower(): v for k, v in resp.headers.items()}
                if resp.status != 200:
                    raise CaptureError(f"HTTP {resp.status} from {url}")
                try:
                    data = json.loads(body)
                except Exception as exc:
                    raise CaptureError(f"Invalid JSON from {url}: {exc}") from exc
                return data, {"status": resp.status, "headers": h, "bodySha256": hashlib.sha256(body).hexdigest()}
        except urllib.error.HTTPError as exc:
            h = {k.lower(): v for k, v in exc.headers.items()} if exc.headers else {}
            code = exc.code
            retryable = code in (420, 429, 500, 502, 503, 504)
            if not retryable or attempt + 1 == max_attempts:
                body = exc.read(2048).decode("utf-8", "replace")
                raise CaptureError(f"HTTP {code} from {url}; error-limit-remain={h.get('x-esi-error-limit-remain')}; body={body}") from exc
            delay = float(h.get("retry-after", "0") or 0)
            if code in (420, 429) and not delay:
                delay = 60
            if h.get("x-esi-error-limit-remain") == "0":
                delay = max(delay, float(h.get("x-esi-error-limit-reset", "60") or 60))
            if code in (420, 429):
                with _cooldown_lock:
                    _cooldown_until = max(_cooldown_until, time.monotonic() + delay)
            time.sleep(min(max(delay, 1) + random.random(), 900))
        except (urllib.error.URLError, TimeoutError) as exc:
            if attempt + 1 == max_attempts:
                raise CaptureError(f"Network failure for {url}: {exc}") from exc
            time.sleep(min(2 ** attempt + random.random(), 60))
    raise CaptureError(f"Request exhausted retries: {url}")


def validate_prices(data: object) -> list[dict]:
    if not isinstance(data, list):
        raise CaptureError("/markets/prices/ must return an array")
    # CCP omits average_price for many types and can omit either optional
    # metric. Preserve omission as missing evidence; never synthesize a value.
    required = {"type_id"}
    seen: set[int] = set()
    for row in data:
        if not isinstance(row, dict) or not required.issubset(row):
            raise CaptureError("Price row schema mismatch")
        tid = row["type_id"]
        if not isinstance(tid, int) or tid <= 0 or tid in seen:
            raise CaptureError("Price type_id invalid or duplicated")
        seen.add(tid)
        for k in ("average_price", "adjusted_price"):
            v = row.get(k)
            if v is not None and (not isinstance(v, (int, float)) or not math.isfinite(v) or v < 0):
                raise CaptureError(f"Invalid {k} for type {tid}")
    return sorted(data, key=lambda r: r["type_id"])


ORDER_FIELDS = {"duration", "is_buy_order", "issued", "location_id", "order_id", "price", "range", "type_id", "volume_remain", "volume_total"}


def validate_orders(data: object) -> list[dict]:
    if not isinstance(data, list):
        raise CaptureError("Market orders page must be an array")
    for row in data:
        if not isinstance(row, dict) or not ORDER_FIELDS.issubset(row):
            raise CaptureError("Market order row schema mismatch")
        if any(not isinstance(row[k], int) or row[k] <= 0 for k in ("type_id", "order_id", "location_id")):
            raise CaptureError("Market order IDs invalid")
        if (not isinstance(row["is_buy_order"], bool) or
            not isinstance(row["volume_remain"], int) or not isinstance(row["volume_total"], int) or
            not isinstance(row["price"], (int, float)) or not math.isfinite(row["price"]) or
            row["volume_remain"] < 0 or row["volume_total"] < row["volume_remain"] or row["price"] < 0):
            raise CaptureError(f"Market order numeric fields invalid ({row.get('order_id')})")
    return sorted(data, key=lambda r: (r["type_id"], r["is_buy_order"], r["price"], r["order_id"]))


def get_pages(region_id: int, stage: Path, region_key: str) -> tuple[list[dict], dict]:
    base = f"{BASE}/markets/{region_id}/orders/?order_type=all"
    page1, meta1 = request_json(base + "&page=1")
    first = validate_orders(page1)
    pages = int(meta1["headers"].get("x-pages", "1"))
    if pages < 1 or pages > 20000:
        raise CaptureError(f"Invalid X-Pages={pages} for region {region_id}")
    print(f"{region_key}: ESI reports {pages} order page(s); beginning full capture", flush=True)
    # Page caches expire five minutes after population. If page 1 is already
    # near expiry, start over after refresh so the page walk begins with a
    # complete cache window rather than racing a known expiry.
    cache_control = meta1["headers"].get("cache-control", "")
    max_age_match = re.search(r"(?:^|,)\s*max-age=(\d+)", cache_control)
    age_value = float(meta1["headers"].get("age", "0") or 0)
    if max_age_match:
        cache_remaining = int(max_age_match.group(1)) - age_value
        if cache_remaining < 60:
            time.sleep(max(0, cache_remaining) + 1)
            page1, meta1 = request_json(base + "&page=1")
            first = validate_orders(page1)
            pages = int(meta1["headers"].get("x-pages", "1"))
            if pages < 1 or pages > 20000:
                raise CaptureError(f"Invalid refreshed X-Pages={pages} for region {region_id}")
    (stage / "orders" / region_key).mkdir(parents=True, exist_ok=True)
    atomic_json(stage / "orders" / region_key / "page-00001.json", first)
    metas = [meta1]
    page_results: dict[int, tuple[list[dict], dict]] = {1: (first, meta1)}
    def fetch_page(page: int) -> tuple[int, list[dict], dict]:
        data, meta = request_json(base + f"&page={page}")
        reported_pages = meta["headers"].get("x-pages")
        if reported_pages is not None and int(reported_pages) != pages:
            raise CaptureError(f"Region {region_id} pagination drift: page {page} reports X-Pages={reported_pages}, expected {pages}")
        orders = validate_orders(data)
        atomic_json(stage / "orders" / region_key / f"page-{page:05d}.json", orders)
        return page, orders, meta
    with ThreadPoolExecutor(max_workers=ORDER_WORKERS, thread_name_prefix="esi-orders") as pool:
        futures = [pool.submit(fetch_page, page) for page in range(2, pages + 1)]
        completed = 0
        for future in as_completed(futures):
            page, orders, meta = future.result()
            page_results[page] = (orders, meta)
            completed += 1
            if completed % 100 == 0 or completed == pages - 1:
                print(f"{region_key}: received {completed + 1}/{pages} order pages", flush=True)
    all_orders = [order for page in range(1, pages + 1) for order in page_results[page][0]]
    metas = [page_results[page][1] for page in range(1, pages + 1)]
    # Official X-Pages guidance notes the set can drift across cache refreshes. Re-read page 1 and fail closed if its payload or page count changed.
    check_payload, check_meta = request_json(base + "&page=1")
    check = validate_orders(check_payload)
    check_pages = int(check_meta["headers"].get("x-pages", "1"))
    if check_pages != pages or canonical_bytes(check) != canonical_bytes(first):
        raise CaptureError(f"Region {region_id} pagination drift: first-page identity/X-Pages changed during capture")
    generation_responses = [*metas, check_meta]
    last_modified = [m.get("headers", {}).get("last-modified", "").strip() for m in generation_responses]
    has_last_modified = any(last_modified)
    if has_last_modified and (not all(last_modified) or len(set(last_modified)) != 1):
        raise CaptureError(f"Region {region_id} pagination generation mismatch: Last-Modified must be present and identical across all pages and page-1 recheck")
    order_ids = [r["order_id"] for r in all_orders]
    duplicates = len(order_ids) - len(set(order_ids))
    if duplicates:
        raise CaptureError(f"Region {region_id} pagination drift: {duplicates} duplicated order IDs across pages")
    print(f"{region_key}: captured {len(all_orders)} unique orders across {pages} pages", flush=True)
    dates = [m["headers"].get("date") for m in metas if m["headers"].get("date")]
    return all_orders, {"pages": pages, "orders": len(all_orders), "duplicateOrderIds": duplicates,
                        "lastModifiedGeneration": last_modified[0] if has_last_modified else None,
                        "responseDates": dates, "responseMetadata": metas, "page1Recheck": check_meta,
                        "cacheControl": [m["headers"].get("cache-control") for m in metas],
                        "xEsiErrorLimitRemain": [m["headers"].get("x-esi-error-limit-remain") for m in metas]}


def derive_orders(regions: list[dict], orders_by_region: dict[str, list[dict]], prices: list[dict], captured_at: str | None = None) -> dict:
    output: dict[int, dict] = {}
    for region in regions:
        key = region["key"]
        by_type: dict[int, list[dict]] = {}
        for order in orders_by_region[key]:
            by_type.setdefault(order["type_id"], []).append(order)
        for tid, rows in by_type.items():
            sells = [x for x in rows if not x["is_buy_order"]]
            buys = [x for x in rows if x["is_buy_order"]]
            best_sell = min(sells, key=lambda x: (x["price"], x["order_id"])) if sells else None
            best_buy = max(buys, key=lambda x: (x["price"], -x["order_id"])) if buys else None
            data = output.setdefault(tid, {"typeId": tid, "regions": {}})
            data["regions"][key] = {
                "regionId": region["regionId"], "regionName": region["name"],
                "regionBestBuy": best_buy, "regionBestSell": best_sell,
                "sameLocationTwoSided": bool(best_buy and best_sell and best_buy["location_id"] == best_sell["location_id"]),
                "crossSideSpreadDiagnostic": (best_sell["price"] - best_buy["price"]) if best_buy and best_sell else None,
                "orderCount": len(rows), "buyOrderCount": len(buys), "sellOrderCount": len(sells),
                "buyQuotedVolume": sum(x["volume_remain"] for x in buys),
                "sellQuotedVolume": sum(x["volume_remain"] for x in sells),
                "quotedVolume": sum(x["volume_remain"] for x in rows),
            }
    price_by_id = {x["type_id"]: x for x in prices}
    all_type_ids = sorted(set(output) | set(price_by_id))
    return {"schemaVersion": 1, "evidenceType": "trade-reference-derived", "capturedAt": captured_at or iso(),
            "priceInterpretation": {"average_price": "trade evidence supplied by ESI", "adjusted_price": "industry evidence only; not a trade price"},
            "orderInterpretation": {"regionBestBuy/regionBestSell": "contextual region-wide extrema; each side may be in a different location and neither is a fair-value quote",
                                    "crossSideSpreadDiagnostic": "non-executable cross-side diagnostic unless sameLocationTwoSided is true; access/executability is not asserted"},
            "types": [{"typeId": k, "averagePrice": price_by_id.get(k, {}).get("average_price"),
                       "adjustedPriceIndustryOnly": price_by_id.get(k, {}).get("adjusted_price"),
                       "regions": output.get(k, {}).get("regions", {})} for k in all_type_ids]}


def run_capture(regions: list[dict]) -> Path:
    captured = utcnow()
    capture_id = stamp(captured)
    raw_root = ROOT / "raw" / "captures"
    raw_root.mkdir(parents=True, exist_ok=True)
    final = raw_root / capture_id
    if final.exists():
        raise CaptureError(f"Capture ID already exists: {capture_id}")
    stage = raw_root / f".staging-{capture_id}"
    stage.mkdir()
    try:
        prices_data, prices_meta = request_json(f"{BASE}/markets/prices/")
        prices = validate_prices(prices_data)
        print(f"prices: captured {len(prices)} ESI average/adjusted rows", flush=True)
        atomic_json(stage / "prices.json", prices)
        orders_by_region: dict[str, list[dict]] = {}
        region_meta = {}
        for region in regions:
            orders, meta = get_pages(region["regionId"], stage, region["key"])
            orders_by_region[region["key"]] = orders
            region_meta[region["key"]] = meta
        atomic_json(stage / "region-orders.json", {"schemaVersion": 1, "regions": [
            {"key": r["key"], "regionId": r["regionId"], "orders": len(orders_by_region[r["key"]])} for r in regions]})
        derived_relative = f"../../derived/trade-reference-{capture_id}.json"
        atomic_json(stage / "derived.json", derive_orders(regions, orders_by_region, prices, iso(captured)))
        derived_hash = sha256(stage / "derived.json")
        # Hash all raw files in stable path order. Metadata records source and request semantics.
        files = sorted(p for p in stage.rglob("*") if p.is_file() and p.name != "derived.json")
        metadata = {
            "schemaVersion": 1, "captureId": capture_id, "capturedAt": iso(captured),
            "source": "official CCP ESI", "baseUrl": BASE, "compatibilityDate": COMPAT,
            "tlsVerification": "enabled via default certificate-verifying SSL context",
            "endpoints": {"prices": {"path": "/markets/prices/", "rows": len(prices), "metadata": prices_meta,
                "fieldSemantics": {"average_price": "ESI average price; trade evidence", "adjusted_price": "industry-adjusted price; never interpreted as trade price"}},
                "orders": {"pathTemplate": "/markets/{region_id}/orders/?order_type=all&page={page}", "regions": region_meta}},
            "regions": regions, "history": {"state": "not-captured", "strategy": "on-demand type+region cache; see report"},
            "rawFiles": [{"path": str(p.relative_to(stage)).replace("\\", "/"), "sha256": sha256(p), "bytes": p.stat().st_size} for p in files],
            "derivedFile": f"derived/trade-reference-{capture_id}.json", "derivedSha256": derived_hash,
        }
        atomic_json(stage / "capture.json", metadata)
        # Only publish a complete capture. A failed run leaves current pointer and prior snapshots intact.
        os.replace(stage, final)
        derived_path = ROOT / "derived" / f"trade-reference-{capture_id}.json"
        atomic_bytes(derived_path, (final / "derived.json").read_bytes())
        (final / "derived.json").unlink()
        atomic_json(ROOT / "raw" / "latest.json", {"captureId": capture_id, "capturePath": f"captures/{capture_id}", "captureMetadataSha256": sha256(final / "capture.json")})
        return final
    except Exception:
        # Keep incomplete captures for diagnosis/replay; never update latest.json.
        failed_root = ROOT / "raw" / "failed"
        failed_root.mkdir(parents=True, exist_ok=True)
        failed_id = f"{capture_id}-failed"
        failure = {"schemaVersion": 1, "captureId": capture_id, "capturedAt": iso(captured),
                   "status": "INVALID_CAPTURE", "error": str(sys.exc_info()[1]),
                   "rawFiles": [{"path": str(p.relative_to(stage)).replace("\\", "/"), "sha256": sha256(p), "bytes": p.stat().st_size}
                                for p in sorted(stage.rglob("*") ) if p.is_file()]}
        try:
            atomic_json(stage / "failure.json", failure)
            os.replace(stage, failed_root / failed_id)
        except Exception:
            # If preserving itself fails, retain staging in place where possible.
            pass
        raise


def history(args: argparse.Namespace, regions: list[dict]) -> None:
    root = ROOT / "raw" / "history-cache"
    if args.type_id_file:
        if args.type_id or args.region_keys:
            raise CaptureError("--type-id-file cannot be combined with --type-id or --region; the file supplies both IDs and region")
        target_file = args.type_id_file.resolve()
        try:
            target_payload = json.loads(target_file.read_text(encoding="utf-8-sig"))
        except Exception as exc:
            raise CaptureError(f"Cannot read history target file {target_file}: {exc}") from exc
        if target_payload.get("schemaVersion") != 1 or not isinstance(target_payload.get("targets"), list):
            raise CaptureError("History target file requires schemaVersion 1 and targets array")
        region_key = target_payload.get("regionKey")
        target_rows = target_payload["targets"]
        ids = [row.get("typeId") if isinstance(row, dict) else None for row in target_rows]
        if (not isinstance(region_key, str) or not ids or
            any(not isinstance(tid, int) or isinstance(tid, bool) or tid <= 0 for tid in ids) or
            len(ids) != len(set(ids))):
            raise CaptureError("History target regionKey and positive unique target typeIds are required")
        if target_payload.get("uniqueTargetCount") != len(ids):
            raise CaptureError("History target uniqueTargetCount does not match targets array")
        target_file_hash = sha256(target_file)
        targets = target_rows
        selected = [r for r in regions if r["key"] == region_key]
        if len(selected) != 1:
            raise CaptureError(f"History target region is not configured: {region_key}")
    else:
        if not args.type_id:
            raise CaptureError("Supply --type-id at least once or --type-id-file")
        if any(tid <= 0 for tid in args.type_id) or len(args.type_id) != len(set(args.type_id)):
            raise CaptureError("--type-id values must be positive and unique")
        selected = [r for r in regions if not args.region_keys or r["key"] in args.region_keys]
        targets = [{"typeId": tid} for tid in args.type_id]
        target_file_hash = None
    if not selected:
        raise CaptureError("No configured regions selected for history")

    captured_at = iso()
    batch_id = f"history-{stamp()}"
    jobs = [(region, target) for region in selected for target in targets]
    results = [{"regionKey": region["key"], "regionId": region["regionId"], "typeId": target["typeId"],
                "name": target.get("name"), "reason": target.get("reason"), "state": "not_requested"}
               for region, target in jobs]
    def save_failed_history_batch(failed_index: int, error: str) -> None:
        failed_dir = ROOT / "raw" / "history-failed"
        failed_dir.mkdir(parents=True, exist_ok=True)
        failed = {"schemaVersion": 1, "batchId": batch_id, "capturedAt": captured_at,
                  "status": "INCOMPLETE_HISTORY_CAPTURE", "source": "official CCP ESI",
                  "targetFile": str(args.type_id_file.resolve()) if args.type_id_file else None,
                  "targetFileSha256": target_file_hash, "targetCount": len(jobs),
                  "requestedFailedCount": sum(x["state"] == "requested_failed" for x in results),
                  "results": results, "error": error}
        atomic_json(failed_dir / f"{batch_id}.json", failed)
    for index, (region, target) in enumerate(jobs):
            tid = target["typeId"]
            path = root / region["key"] / f"{tid}.json"
            if path.exists() and not args.refresh:
                age = time.time() - path.stat().st_mtime
                if age < args.max_age_days * 86400:
                    try:
                        existing = json.loads(path.read_text(encoding="utf-8"))
                        valid_state = existing.get("historyState") in ("observations", "empty_response_no_observations")
                        valid_hash = (existing.get("responseBodySha256") == existing.get("responseMetadata", {}).get("bodySha256"))
                        valid_observations = validate_history(existing.get("observations"), region["key"], tid) == existing.get("observations")
                        if (existing.get("schemaVersion") == 1 and existing.get("regionKey") == region["key"] and
                            existing.get("regionId") == region["regionId"] and existing.get("typeId") == tid and
                            valid_state and valid_hash and valid_observations):
                            print(f"cached {region['key']} type {tid}")
                            results[index].update({"state": "reused", "observations": len(existing.get("observations", [])),
                                                   "cachePath": str(path.relative_to(ROOT)).replace("\\", "/"), "cacheSha256": sha256(path)})
                            continue
                    except Exception:
                        # A cache miss/corrupt entry is refreshed, but never treated as an empty response.
                        pass
            url = f"{BASE}/markets/{region['regionId']}/history/?type_id={tid}"
            try:
                data, meta = request_json(url)
                observations = validate_history(data, region["key"], tid)
            except Exception as exc:
                error = str(exc)
                results[index].update({"state": "requested_failed", "error": error, "endpoint": f"/markets/{region['regionId']}/history/?type_id={tid}"})
                if "HTTP 404" in error:
                    # A permanent missing history resource for one item should not block unrelated requested IDs.
                    save_failed_history_batch(index, error)
                    print(f"requested_failed {region['key']} type {tid}: HTTP 404; continuing bounded queue", flush=True)
                    continue
                save_failed_history_batch(index, error)
                raise CaptureError(f"History batch stopped at {region['key']} type {tid}; previous valid cache retained and diagnostic saved") from exc
            payload = {"schemaVersion": 1, "capturedAt": iso(), "source": "official CCP ESI", "regionKey": region["key"],
                       "regionId": region["regionId"], "typeId": tid, "endpoint": f"/markets/{region['regionId']}/history/?type_id={tid}",
                       "responseMetadata": meta, "responseBodySha256": meta["bodySha256"],
                       "historyState": "observations" if observations else "empty_response_no_observations",
                       "observations": observations}
            # Atomic replacement keeps a prior valid type/region cache intact unless a complete response validates.
            atomic_json(path, payload)
            results[index].update({"state": payload["historyState"], "observations": len(observations),
                                   "cachePath": str(path.relative_to(ROOT)).replace("\\", "/"), "cacheSha256": sha256(path),
                                   "responseBodySha256": meta["bodySha256"]})
            print(f"captured {region['key']} type {tid}: {len(observations)} history days", flush=True)
    manifest = {"schemaVersion": 1, "batchId": batch_id, "capturedAt": captured_at, "source": "official CCP ESI",
                "strategy": "explicit bounded history target list" if args.type_id_file else "explicit type ID list",
                "regionKeys": [r["key"] for r in selected], "targetFile": str(args.type_id_file.resolve()) if args.type_id_file else None,
                "targetFileSha256": target_file_hash, "targetCount": len(jobs),
                "capturedCount": sum(x["state"] in ("observations", "empty_response_no_observations") for x in results),
                "requestedFailedCount": sum(x["state"] == "requested_failed" for x in results), "results": results}
    if manifest["requestedFailedCount"]:
        manifest["status"] = "COMPLETED_WITH_REQUEST_FAILURES"
    else:
        manifest["status"] = "COMPLETE"
    manifest_dir = ROOT / "raw" / "history-captures"
    manifest_dir.mkdir(parents=True, exist_ok=True)
    manifest_path = manifest_dir / f"{batch_id}.json"
    atomic_json(manifest_path, manifest)
    atomic_json(ROOT / "raw" / "history-latest.json", {"batchId": batch_id,
                "manifestPath": str(manifest_path.relative_to(ROOT)).replace("\\", "/"), "manifestSha256": sha256(manifest_path)})
    print(f"history batch complete: {len(results)} type-region targets; manifest {manifest_path}")


def validate_history(data: object, region_key: str, type_id: int) -> list[dict]:
    fields = {"date", "order_count", "volume", "highest", "average", "lowest"}
    if not isinstance(data, list):
        raise CaptureError(f"History schema invalid for {region_key} type {type_id}: expected array")
    seen_dates = set()
    rows = []
    for row in data:
        if not isinstance(row, dict) or not fields.issubset(row):
            raise CaptureError(f"History schema invalid for {region_key} type {type_id}: required fields missing")
        date = row["date"]
        try:
            dt.date.fromisoformat(date)
        except (TypeError, ValueError) as exc:
            raise CaptureError(f"History date invalid for {region_key} type {type_id}: {date!r}") from exc
        if date in seen_dates:
            raise CaptureError(f"Duplicate history date for {region_key} type {type_id}: {date}")
        seen_dates.add(date)
        if any(not isinstance(row[k], int) or isinstance(row[k], bool) or row[k] < 0 for k in ("order_count", "volume")):
            raise CaptureError(f"History count/volume invalid for {region_key} type {type_id} on {date}")
        prices = [row[k] for k in ("highest", "average", "lowest")]
        if any(not isinstance(v, (int, float)) or isinstance(v, bool) or not math.isfinite(v) or v < 0 for v in prices):
            raise CaptureError(f"History price invalid for {region_key} type {type_id} on {date}")
        if row["highest"] < row["lowest"] or not row["lowest"] <= row["average"] <= row["highest"]:
            raise CaptureError(f"History price ordering invalid for {region_key} type {type_id} on {date}")
        rows.append(row)
    return sorted(rows, key=lambda x: x["date"])


def verify_replay(capture_id: str) -> None:
    """Verify a pinned capture and independently reconstruct its derived SHA offline."""
    capture_dir = ROOT / "raw" / "captures" / capture_id
    metadata = json.loads((capture_dir / "capture.json").read_text(encoding="utf-8"))
    if metadata.get("captureId") != capture_id:
        raise CaptureError("Capture metadata identity mismatch")
    for row in metadata.get("rawFiles", []):
        path = capture_dir / row["path"]
        if not path.is_file() or sha256(path) != row["sha256"] or path.stat().st_size != row["bytes"]:
            raise CaptureError(f"Raw manifest mismatch: {row['path']}")
    prices = validate_prices(json.loads((capture_dir / "prices.json").read_text(encoding="utf-8")))
    regions = metadata["regions"]
    orders_by_region: dict[str, list[dict]] = {}
    for region in regions:
        key = region["key"]
        region_meta = metadata["endpoints"]["orders"]["regions"][key]
        pages = int(region_meta["pages"])
        all_orders = []
        for page in range(1, pages + 1):
            path = capture_dir / "orders" / key / f"page-{page:05d}.json"
            if not path.is_file():
                raise CaptureError(f"Missing page in pinned capture: {key}/{page}")
            all_orders.extend(validate_orders(json.loads(path.read_text(encoding="utf-8"))))
        if len(all_orders) != region_meta["orders"] or len({x["order_id"] for x in all_orders}) != len(all_orders):
            raise CaptureError(f"Order row count or duplicate ID mismatch in region {key}")
        orders_by_region[key] = all_orders
    replay = derive_orders(regions, orders_by_region, prices, metadata["capturedAt"])
    replay_hash = hashlib.sha256(canonical_bytes(replay)).hexdigest()
    derived_path = ROOT / metadata["derivedFile"]
    file_hash = sha256(derived_path)
    if replay_hash != file_hash or replay_hash != metadata["derivedSha256"]:
        raise CaptureError(f"Derived replay mismatch: replay={replay_hash}; file={file_hash}; capture={metadata['derivedSha256']}")
    print(f"offline raw-to-derived replay matched: {capture_id} {replay_hash}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--regions", dest="regions_config", type=Path, default=CONFIG, help="config JSON; supports arbitrary region lists")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("capture", help="capture prices and all public regional order pages")
    hp = sub.add_parser("history", help="fetch sparse, on-demand history into an atomic persistent cache")
    hp.add_argument("--type-id", type=int, action="append")
    hp.add_argument("--type-id-file", type=Path, help="JSON target file with schemaVersion, regionKey, uniqueTargetCount and targets[].typeId")
    hp.add_argument("--region", dest="region_keys", action="append", help="region key; repeatable; default all configured")
    hp.add_argument("--max-age-days", type=float, default=7)
    hp.add_argument("--refresh", action="store_true")
    rp = sub.add_parser("verify-replay", help="verify raw hashes and independently reconstruct derived data offline")
    rp.add_argument("--capture-id", required=True)
    args = parser.parse_args()
    try:
        regions = load_regions(args.regions_config)
        if args.command == "capture":
            path = run_capture(regions)
            print(f"complete capture: {path}")
        elif args.command == "history":
            if args.max_age_days < 0:
                raise CaptureError("--max-age-days must be >= 0")
            history(args, regions)
        else:
            verify_replay(args.capture_id)
        return 0
    except (CaptureError, OSError) as exc:
        print(f"collector error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
