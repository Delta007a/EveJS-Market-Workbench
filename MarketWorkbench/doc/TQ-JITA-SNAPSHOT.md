# Jita snapshot with trading-history smoothing

Prepare a pinned offline reference **before** starting Workbench. Runtime never queries ESI.

## Snapshot sides

- Sell: lowest positive public Sell physically at Jita 4-4, station 60003760, The Forge 10000002.
- Buy: highest positive public Buy whose range covers Jita 4-4. Station/system/region and numeric jump ranges are resolved using the matching-build authoritative CCP stargate graph. Public ranged Buy orders originating in player structures may count: the player sells at the NPC station covered by that order, not at the structure.
- Positive remaining volume is required. Equal-price ties select the smallest order ID. Minimum volume is retained as evidence; best order price does not promise unlimited single-unit fills.
- Other hubs remain comparison evidence, never reference contributors. Private structure books are not collected.

## History

Official ESI `/markets/10000002/history/?type_id=...` supplies **regional executed daily averages**, not separate historical bid/ask and not station-specific Jita history.

For each type choose the first sufficient window of completed calendar days before the snapshot date:

| Window | Minimum days with positive executed volume/order count |
|---|---:|
| 7 days | 3 |
| 30 days | 3 |
| 180 days | 2 |
| 365 days | 1 |

The anchor is the median of those daily averages, which dampens isolated daily spikes. The snapshot day, future observations, zero-volume days and zero prices are excluded. Extended windows and fewer than three traded days are explicitly warned. These small completeness thresholds select a window; they do not assert liquidity or fair-value confidence.

Each available direct side is prepared independently:

```
Buy reference  = (history anchor + raw snapshot best eligible Buy) / 2
Sell reference = (history anchor + raw snapshot best station Sell) / 2
```

No traded history within a year: retain the snapshot side with a visible `SNAPSHOT_ONLY_NO_HISTORY` state. Missing snapshot sides stay missing. GENERAL_TQ's existing explicit 0.80 / 1.25 missing-side/average fallbacks are policy-owned and unchanged. Global ESI average is not executed history. Industry adjusted price is never used as trade price.

Buy >= Sell, including equality after rounding or distribution, is a **review warning**, not an automatic exclusion/build/audit error. Finite positive prices, source resolution, ambiguity, provenance and persisted-plan checks still apply. Historical distribution plans v1/v2 retain their original replay semantics; new plans v3 report crossings as warnings. Legacy V1 pricing/economic algorithms and its shipped policy are unchanged.

## Refresh sequence

1. Capture official public order books with the separate existing collector.
2. Run `derive_tq_snapshot.py --capture-root ... --static-types ... --taxonomy ... --sde-dir ... --output <new-range-snapshot>`.
3. Run `collect_tq_history.py --dataset <snapshot-json> --output <new-history-cache> --existing-cache ... --collector-library <esi_trade_reference.py>`. It resumes verified caches, uses bounded HTTPS concurrency, and honors ESI retry/error limits. Retry any failed requests before publication.
4. Run `blend_tq_history.py --snapshot <range-snapshot-json> --history-root <complete-cache> --output <new-blended-dataset>`. Cache identities and SHAs are validated. Raw history and raw snapshot are retained separately.
5. Generate GENERAL_TQ's evidence-state selectors with `generate_general_tq.py`, supplying the new dataset SHA. Profile parameters remain unchanged. Pin the resulting dataset/policy in the operational config.
6. Restart Workbench and create a fresh copy of GENERAL_TQ. Preview old custom policies explicitly: their exact lists/source choices remain as saved and can be stale relative to new evidence. Existing built/installed databases do not refresh themselves.

New output: `work/pricing-data/tq-market-reference-jita-history-v1`.
History: `work/pricing-data/tq-history-blend-v1/raw/history`.
Previous Jita and five-hub datasets, candidates and archives remain intact for provenance.

Raw capture timestamp and history capture timestamp are distinct. Historical observations after the raw snapshot date do not enter its prepared price. Per-type trace records raw sides, history window/days/volume/last trade, selected observations, cache SHA and final reference. Policy multipliers apply afterward. No gameplay DB is built or installed during preparation.
