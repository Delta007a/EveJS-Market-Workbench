# Six user presets — community v1

These are editable user presets, not replacements for LEGACY_V1 or GENERAL_TQ.
Use the updated included Workbench EXE: older versions do not support the new
NPC acquisition and linked T1 quote sources.

## How to open

In the portable package, click **Import Preset** and select one of the six JSON
files from `tools\marketworkbench\presets\community-v1\` in your EveJS folder
(or `presets\community-v1` relative to the Workbench folder). Review the summary
and save the new user copy. These presets are not preloaded into your user storage. Item Policy, Quick Setup,
exact rules, Distribution and distribution seed are included. The files contain
logical source names and no absolute installation paths. Local pinned inputs
are resolved by the receiving Workbench; missing sources fail explicitly.

Buy means the NPC buys from the player. Sell means the NPC sells to the player.
Percentages apply to the selected source, not necessarily the same price on both sides.
These presets have been previewed; they are not deployed or economically balanced proofs.

## Presets

| Preset | Main idea | Distribution |
|---|---|---|
| Farmer Hardcore | Farmer-inspired T1 production challenge, Production Cost Sell 300% / Buy 100%; NPC essentials; advanced goods Buy-only | Jita only |
| TQ Galaxy | Snapshot 100% at hubs; ordinary goods; no automatic special blueprint/hull sales | Five hubs + all eligible NPC stations |
| Zero to Hero | Sell only normal NPC books/BPO/formulas/essential goods; ordinary priced goods Buy 100% snapshot | Five hubs |
| Industrialist | Buy back finished goods; sell resources and production intermediates to support player industry | Five hubs + regional NPC suppliers |
| Explorer | Ordinary T1 equipment supplied; advanced and rare loot acquired from players | Five hubs + sparse regional NPC suppliers |
| Solo Progression | Progressive T1 production quotes, lazy intermediates, linked meta quotes, discounted ordinary BPO/formulas | Five hubs |

TQ Galaxy secondaries: assortment 5–30%, Buy 40–80%, Sell 105–190%, Strong remoteness.
Industrialist secondaries: assortment 15–35%, Buy 90–100%, Sell 110–130%, Light remoteness.
Explorer secondaries: assortment 5–15%, Buy 70–90%, Sell 120–160%, Strong remoteness.
Each has its own fixed reproducible seed. Main hubs keep the preset's canonical quotes.

## Solo Progression

- Resources Sell 130%, Buy 60%; ore and ice Buy 40%.
- Compressed discount applies only to Buy: 54% for other compressed resources,
  36% for compressed ore/ice; Sell remains 130%.
- Lazy PI P1–P4 and long-chain intermediates Sell 200%, Buy 100%.
  PI explicitly uses TQ. Supported manufactured/reaction intermediates use Production Cost.
  Missing funded intermediate routes select explicit TQ profiles at the same preset
  percentages; this is visible policy, not a silent runtime source fallback.
- Implants Sell 200% TQ / Buy 100% TQ.
- Ordinary proven T1 BPO and reaction formulas Sell 10% / Buy 5% NPC acquisition reference.
- Meta modules use an actual eligible SDE T1 variation parent: Sell twice the parent's
  final Sell, Buy exactly the parent's final Buy. No price/name guess identifies the parent.
- Advanced finished T2/T3, faction/deadspace/officer goods remain Buy-only at snapshot.

| T1 hull group | Sell / Buy, % Production Cost |
|---|---:|
| Corvette, Shuttle, Frigate | 120 / 80 |
| Destroyer | 140 / 85 |
| Cruiser, Hauler, Mining Barge | 170 / 100 |
| Battlecruiser | 200 / 115 |
| Battleship, Industrial Command | 250 / 130 |
| Freighter, Dreadnought, Carrier, Force Auxiliary, Capital Industrial | 300 / 150 |
| Supercarrier | 350 / 175 |
| Titan | 400 / 200 |

Equipment/ammunition/rig/drone size tiers use real SDE dogma attributes. Larger
tiers receive higher Production Cost percentages. No parent or unavailable required
cost leaves an explicit snapshot Buy-only policy where possible, otherwise exclusion.
50 of the 615 ordinary meta-equipment types have no supported synthetic Sell;
565 have a verified linked T1 price.

## Limits and provenance

The pinned Jita order book is dated 2026-09-28, not today's live market. The current
reference blends snapshot sides with executed regional history where available.
Missing-side and average-only fallback remain the existing explicit GENERAL_TQ contracts.
Industry adjusted_price is not a trading quote. No new network collection was performed.

All 129 legacy ProductionIntermediate IDs (114 + 15) were considered. EXERT 49720
remains explicitly unseeded. Nonmarket/special blueprint families are not automatically
added; special ordinary blueprint types are not sold. No executable economy safety
claim is made for production Buy premiums, discounted recipes or crossed TQ quotes.
Build and audit remain explicit user actions. Nothing was built or installed by preset creation.

## Preview counts

| Preset | Sell types | Buy types | Selected unresolved |
|---|---:|---:|---:|
| Farmer Hardcore | 4,741 | 12,703 | 0 |
| TQ Galaxy | 12,549 | 12,677 | 0 |
| Zero to Hero | 2,283 | 12,703 | 0 |
| Industrialist | 3,517 | 12,703 | 0 |
| Explorer | 4,100 | 12,703 | 0 |
| Solo Progression | 6,170 | 12,710 | 0 |
