//! The one place that decides what gets a seed row — `doc/JITA_INDEX_SEED_PLAN.md` §4.1.
//!
//! [`SeedPlan::resolve`] takes the classified universe, the cost report, the price manifest,
//! the NPC catalog and the config, and answers two questions once, for every bucket, the same
//! way:
//!
//! * **what is seeded** — every type the manifest holds a usable price for, and
//! * **what is dropped** — every type it does not, named, with the blame attached.
//!
//! ### Dropping is the uniform rule, and it is not guessing
//!
//! Bucket D has always worked this way: no captured price, no seed row, reported and moved
//! on (§2.4). Buckets A and B originally carried a stricter rule — the build MUST fail
//! while any A∪B type is unpriced — to stop the seeder inventing a price for an index type.
//! That intent is preserved here: **a dropped type is never given a made-up price**. What
//! changed is the consequence of an unpriceable type. Five A∪B types at SDE build 3396210
//! are permanently unpriceable because their own inputs are untraded event/exclusive items
//! CCP has no price for at all:
//!
//! | type   | name                                      | blocked by                          |
//! |--------|-------------------------------------------|-------------------------------------|
//! | 3581   | Purloined Sansha Data Analyzer            | 3583, 3584                          |
//! | 33581  | Small Mobile 'Hybrid' Siphon Unit         | itself — no basePrice, and ESI has  |
//! |        |                                           | only an `adjusted_price` (rule A)   |
//! | 36902  | Coalesced Element                         | the five Drifter Elements 34556-60  |
//! | 47028  | Neural Lace 'Blackglass' Net Intrusion …  | Project Discovery DNA 47020-47027   |
//! | 49787  | Hiemal Tricarboxyl Vapor                  | itself (cycle guard, Triglavian gas)|
//!
//! Failing the build forever on those five is not useful, so they are dropped and named
//! like any other unpriceable type.
//!
//! ### The safety net that replaces the old gate
//!
//! Turning a hard failure into a drop removes a real guard: a missing, stale or truncated
//! manifest would then quietly seed almost nothing instead of failing. `[index_seed]
//! max_unpriced_drops` (default 25) is what catches that. Exceed it and [`SeedPlan::resolve`]
//! fails with the count and the remedy. Bucket-D drops deliberately do **not** count against
//! it: 217 junk candidates drop on today's data and that is routine, by design, not a signal.
//!
//! ### One side, one mechanism (`[index_seed] defer_to_npc_catalog`)
//!
//! Plan §2.3 gives bucket C its own mechanism: TQ's own NPC orders, at TQ's own stations and
//! TQ's own fixed prices, imported verbatim by [`crate::overlay`]. Where our computed
//! `3 x cost` ask or `1 x cost` bid ends up facing a fixed NPC price for the same type at
//! another station, the gap between two unrelated price bases can be a haul loop — 7 of
//! ours-sell → TQ-buys (worst x165.7) and 5 of TQ-sells → ours-buys (worst x8.41) on the
//! first overlay build. No multiplier closes them: the other leg is a price we do not set.
//!
//! The first cut of this rule was `[index_seed] defer_to_npc_catalog = true`, which dropped a
//! type from the index/junk seed the moment the overlay touched it on either side. That works
//! and it is far too blunt: 340 types, 289 of them previously seeded, for 12 real loops. Six
//! Mutanites, Magmatic Gas, Superionic Ice, Neo-Jadarite and Nephrite lost their Jita ask
//! outright, and 13 recipes stopped being buildable end to end from seed asks.
//!
//! The loop only exists when the two prices **cross**, so that is now the whole test, asked
//! per side ([`crate::config::NpcDeferralMode::ConflictingPrices`], the default):
//!
//! * our **ask** for a type is dropped iff `our_ask < best_npc_bid` — buy from us, haul, sell
//!   into TQ's buyback;
//! * our **bid** is dropped iff `our_bid > best_npc_ask` — buy from TQ, haul, sell into us.
//!
//! The two are independent: a type may keep its ask and lose its bid, or the reverse, or keep
//! both while the overlay holds it. A type that loses **both** is not seeded at all and is
//! reported under [`DropReason::DeferredToNpcCatalog`], exactly as before. That is a different
//! outcome from "we could not price it", so it is counted separately and never charged to
//! `max_unpriced_drops`. `"whole-type"` restores the original all-or-nothing rule and `"off"`
//! defers nothing; with no catalog imported (`--skip-overlay`) there is nothing to defer *to*,
//! and [`SeedPlan::deferral`] records which of those states a run was in.
//!
//! **This is audit check 4's own condition** (`crate::audit::check_cross_station`: best bid
//! anywhere over best ask anywhere), which is what makes the relationship exact rather than
//! approximate — see [`SeedPlan::resolve`] for the four cases that proof runs through.
//!
//! ### The invariant the seed writer depends on
//!
//! Every type in [`SeedPlan::seedable`] carries a manifest price that is finite and greater
//! than zero — checked in [`SeedPlan::verify_prices`] before the plan is ever returned, and
//! reported as an error naming the type when it is violated. This is the real guard against
//! seeding a guessed or zero price: a zero ask hands out free items, and a NaN cannot even
//! be marshalled to the client.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use crate::build::{SeedPricing, SeedPricingPair};
use crate::classify::{
    BucketFlags, Classification, MarketAssignment, MarketRole, PricingProfile,
    RareLiquidityRequirement, load_expanded_lp_reward_type_ids, rare_liquidity_requirement,
};
use crate::config::{NpcDeferralMode, SeederConfig};
use crate::cost::CostReport;
use crate::manifest::{CapturePolicy, PriceManifest, PriceSource, SeedPrice};
use crate::policy::Source as PolicySource;
use crate::policy_resolve::{Resolution, ResolvedItemPolicy};
use crate::staticdata::{StaticData, VariantFamilies};

const SKILLBOOK_FIXED_FALLBACK_ISK: f64 = 5_000_000.0;

/// Why one type is not seeded. Every outcome means "no seed row, reported by name"; they
/// differ in whether the drop is routine, and in whether it is about price at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DropReason {
    /// An A∪B index type the price manifest holds no entry for. Rare and worth reading:
    /// these are the drops [`crate::config::IndexSeedConfig::max_unpriced_drops`] budgets.
    UnpricedIndexType,
    /// A bucket-D junk candidate with no **captured** market price (§2.4). Routine — 217 of
    /// them today — and deliberately outside the budget above.
    UncapturedJunk,
    /// The NPC catalog already seeds this type somewhere, so the index/junk pass stands down
    /// (`[index_seed] defer_to_npc_catalog`). **Not a pricing failure**: these types have a
    /// perfectly good manifest price and are still traded in-game, at TQ's stations. Counted
    /// and reported on its own, and outside the unpriced budget.
    DeferredToNpcCatalog,
    /// A non-core role had no approved snapshot, manifest, or static fallback reference.
    UnpricedRoleReference,
    /// A required gameplay Rare has no exact/sibling/funded anchor. Kept visible and unseeded;
    /// a small isolated remainder is reviewable, while a systemic population still fails.
    RequiredRareReferenceUnavailable,
    /// An optional cosmetic reward had no approved exact reference and receives no side.
    OptionalRareUnreferenced,
    /// An LP/faction reward blueprint without an approved reusable-recipe policy is unseeded.
    OptionalRareBlueprint,
}

impl DropReason {
    pub fn key(self) -> &'static str {
        match self {
            Self::UnpricedIndexType => "unpriced-index-type",
            Self::UncapturedJunk => "uncaptured-junk",
            Self::DeferredToNpcCatalog => "deferred-to-npc-catalog",
            Self::UnpricedRoleReference => "unpriced-role-reference",
            Self::RequiredRareReferenceUnavailable => "required-rare-reference-unavailable",
            Self::OptionalRareUnreferenced => "optional-rare-unreferenced",
            Self::OptionalRareBlueprint => "optional-rare-blueprint",
        }
    }

    /// True only for [`DropReason::UnpricedIndexType`]: the junk budget is the whole bucket
    /// and a deferral is not a missing price, so counting either here would make the knob
    /// meaningless.
    pub fn counts_against_budget(self) -> bool {
        matches!(self, Self::UnpricedIndexType)
    }
}

/// One NPC price and the station it sits at, so a deferral can name what beat it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NpcPrice {
    pub price: f64,
    pub station_id: u64,
}

/// The best NPC price for one type on each side, across every station the overlay seeds it at:
/// the **cheapest** ask and the **dearest** bid, which are the two a player would actually use.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NpcQuote {
    /// Cheapest overlay ask anywhere; `None` when TQ sells this type nowhere.
    pub ask: Option<NpcPrice>,
    /// Dearest overlay bid anywhere; `None` when TQ buys it nowhere.
    pub bid: Option<NpcPrice>,
}

impl NpcQuote {
    pub fn is_empty(&self) -> bool {
        self.ask.is_none() && self.bid.is_none()
    }
}

/// What the NPC catalog overlay holds, per type and per side — the book [`SeedPlan::resolve`]
/// prices our own rows against.
///
/// Built from the overlay import the build and the audit already run, so the deferral is
/// decided against exactly the rows that will be written. A caller with no overlay (the
/// `--skip-overlay` paths, and `refresh-prices`, which only resolves prices) passes
/// [`NpcCatalog::none`] and nothing is deferred.
///
/// **Rows at the index station are counted too**, deliberately, even though the index pass
/// `INSERT OR REPLACE`s over them when it seeds that side. Excluding them would be circular —
/// whether TQ's Jita ask survives depends on whether we keep our Jita ask, which is the very
/// question being asked — and including them is the conservative direction: it can only defer
/// more. It also buys a property the narrower rule would lose, that our surviving bid can
/// never cross a surviving TQ ask *at Jita itself*, which is a same-station crossing audit
/// check 4 does not look at (`build::verify_seed_tables` is what would catch it, as a failed
/// build).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NpcCatalog {
    quotes: BTreeMap<u32, NpcQuote>,
    imported: bool,
}

impl NpcCatalog {
    /// No catalog was imported. Distinct from an imported-but-empty one, because "nothing to
    /// defer to" and "the overlay found nothing" are different facts about a run.
    pub fn none() -> Self {
        Self::default()
    }

    /// Collapses the overlay book to one best ask and one best bid per type.
    pub fn from_overlay(import: &crate::overlay::OverlayImport) -> Self {
        let mut quotes: BTreeMap<u32, NpcQuote> = BTreeMap::new();
        for ((station_id, type_id), row) in &import.sell {
            let quote = quotes.entry(*type_id).or_default();
            let candidate = NpcPrice {
                price: row.price(),
                station_id: *station_id,
            };
            if quote.ask.is_none_or(|best| candidate.price < best.price) {
                quote.ask = Some(candidate);
            }
        }
        for ((station_id, type_id), row) in &import.buy {
            let quote = quotes.entry(*type_id).or_default();
            let candidate = NpcPrice {
                price: row.price(),
                station_id: *station_id,
            };
            if quote.bid.is_none_or(|best| candidate.price > best.price) {
                quote.bid = Some(candidate);
            }
        }
        Self {
            quotes,
            imported: true,
        }
    }

    /// Test/tooling constructor: an explicit catalog without an overlay import.
    pub fn from_quotes(quotes: impl IntoIterator<Item = (u32, NpcQuote)>) -> Self {
        Self {
            quotes: quotes.into_iter().collect(),
            imported: true,
        }
    }

    /// True when the catalog holds this type on **either** side — the `"whole-type"` test.
    pub fn contains(&self, type_id: u32) -> bool {
        self.quotes.contains_key(&type_id)
    }

    /// The two best NPC prices for one type; empty when the catalog does not hold it.
    pub fn quote(&self, type_id: u32) -> NpcQuote {
        self.quotes.get(&type_id).copied().unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.quotes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.quotes.is_empty()
    }

    /// False when the caller never imported an overlay.
    pub fn imported(&self) -> bool {
        self.imported
    }
}

/// Which sides of the book the index/junk pass will write for one type.
///
/// Both, by default and in the overwhelming majority of cases. A type with neither is not
/// seedable at all and never reaches [`SeedPlan::seedable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedSides {
    /// Write a `seed_stock` row at the index station.
    pub ask: bool,
    /// Write a `seed_buy_orders` row at the index station.
    pub bid: bool,
}

impl SeedSides {
    pub const BOTH: Self = Self {
        ask: true,
        bid: true,
    };
    pub const SELL_ONLY: Self = Self {
        ask: true,
        bid: false,
    };
    pub const BUY_ONLY: Self = Self {
        ask: false,
        bid: true,
    };

    pub fn is_empty(self) -> bool {
        !self.ask && !self.bid
    }

    /// `None` when both sides are written.
    pub fn deferred_side(self) -> Option<DeferredSide> {
        match (self.ask, self.bid) {
            (true, true) => None,
            (false, true) => Some(DeferredSide::Ask),
            (true, false) => Some(DeferredSide::Bid),
            (false, false) => Some(DeferredSide::Both),
        }
    }
}

impl Default for SeedSides {
    fn default() -> Self {
        Self::BOTH
    }
}

/// What a deferral took off one type. Report order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeferredSide {
    /// No `seed_stock` row: nothing buys this type from us at the index station.
    Ask,
    /// No `seed_buy_orders` row: nothing sells this type to us at the index station.
    Bid,
    /// No index/junk row at all — the type is dropped, under
    /// [`DropReason::DeferredToNpcCatalog`].
    Both,
}

impl DeferredSide {
    pub const ALL: [Self; 3] = [Self::Ask, Self::Bid, Self::Both];

    pub fn key(self) -> &'static str {
        match self {
            Self::Ask => "ask deferred",
            Self::Bid => "bid deferred",
            Self::Both => "both deferred",
        }
    }
}

/// The two prices that crossed, and where TQ's one lives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceConflict {
    /// The price the index/junk pass would have written at the index station.
    pub ours: f64,
    /// The NPC price on the **other** side that it crossed.
    pub theirs: f64,
    pub station_id: u64,
}

/// One type whose index/junk seed lost a side (or both) to the NPC catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct Deferred {
    pub type_id: u32,
    pub name: String,
    pub bucket: BucketFlags,
    pub side: DeferredSide,
    /// Our ask against the NPC bid that beat it. `None` under `"whole-type"`, which does not
    /// look at prices at all.
    pub ask_conflict: Option<PriceConflict>,
    /// Our bid against the NPC ask that beat it.
    pub bid_conflict: Option<PriceConflict>,
}

impl Deferred {
    pub fn bucket_label(&self) -> &'static str {
        self.bucket.label()
    }

    /// `"ask 30,180.00 < NPC bid 5,000,000.00 @ 60012345"`, or the whole-type equivalent.
    pub fn detail(&self) -> String {
        let mut parts = Vec::new();
        if let Some(conflict) = self.ask_conflict {
            parts.push(format!(
                "ask {} < NPC bid {} @ {}",
                crate::isk(conflict.ours),
                crate::isk(conflict.theirs),
                conflict.station_id
            ));
        }
        if let Some(conflict) = self.bid_conflict {
            parts.push(format!(
                "bid {} > NPC ask {} @ {}",
                crate::isk(conflict.ours),
                crate::isk(conflict.theirs),
                conflict.station_id
            ));
        }
        if parts.is_empty() {
            "the NPC catalog holds this type".to_string()
        } else {
            parts.join("; ")
        }
    }
}

/// The verdict on one type's two sides, and the prices behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SideVerdict {
    pub sides: SeedSides,
    pub ask_conflict: Option<PriceConflict>,
    pub bid_conflict: Option<PriceConflict>,
}

/// **The rule**, in one place: which of our two prices for a type would form a haul loop
/// against the NPC catalog.
///
/// `ours` is the pricing that will actually write this row — `[index_seed]` for A∪B,
/// `[junk_seed]` for bucket D — and `cost` is its manifest price, so `ours.ask(cost)` and
/// `ours.bid(cost)` are exactly the numbers [`crate::build::plan_seed_rows`] computes from the
/// same two inputs. Nothing here re-derives a price.
///
/// The comparisons are **strictly** the untaxed crossing, `our_ask < npc_bid` and
/// `our_bid > npc_ask`, not the 8%-tax-adjusted one audit check 4 gates on. A loop worth 1%
/// is not worth seeding a row for, so the marginal ones are dropped as well; erring the other
/// way would leave a violation the audit reports but does not fail on, which is a worse place
/// to be than one missing Jita row.
pub fn conflicting_sides(ours: SeedPricing, cost: f64, quote: NpcQuote) -> SideVerdict {
    let our_ask = ours.ask(cost);
    let our_bid = ours.bid(cost);
    let ask_conflict = quote
        .bid
        .filter(|npc| our_ask < npc.price)
        .map(|npc| PriceConflict {
            ours: our_ask,
            theirs: npc.price,
            station_id: npc.station_id,
        });
    let bid_conflict = quote
        .ask
        .filter(|npc| our_bid > npc.price)
        .map(|npc| PriceConflict {
            ours: our_bid,
            theirs: npc.price,
            station_id: npc.station_id,
        });
    SideVerdict {
        sides: SeedSides {
            ask: ask_conflict.is_none(),
            bid: bid_conflict.is_none(),
        },
        ask_conflict,
        bid_conflict,
    }
}

/// Which of the deferral states a resolved plan was in — so a report can tell "nothing
/// collided" apart from "nothing was compared".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Deferral {
    /// `[index_seed] defer_to_npc_catalog = "off"`: both mechanisms may own a type.
    Disabled,
    /// No NPC catalog was imported, so there is nothing to defer to.
    #[default]
    NoCatalog,
    /// Active against a catalog of this many types, in this mode.
    Active {
        mode: NpcDeferralMode,
        catalog_types: usize,
    },
}

impl Deferral {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Active { .. })
    }

    /// The mode a run actually applied, or `None` when it deferred nothing.
    pub fn mode(self) -> Option<NpcDeferralMode> {
        match self {
            Self::Active { mode, .. } => Some(mode),
            _ => None,
        }
    }

    /// One line for a report header.
    pub fn label(self) -> String {
        match self {
            Self::Disabled => {
                "disabled ([index_seed] defer_to_npc_catalog = \"off\"): the index/junk pass \
                 may seed both sides of types the NPC catalog also seeds"
                    .to_string()
            }
            Self::NoCatalog => {
                "no NPC catalog imported, so nothing is deferred (--skip-overlay or a \
                 price-only command)"
                    .to_string()
            }
            Self::Active {
                mode: NpcDeferralMode::WholeType,
                catalog_types,
            } => format!(
                "\"whole-type\" against {} NPC-catalog types: a type TQ seeds anywhere, on \
                 either side, gets no index/junk row",
                crate::count(catalog_types)
            ),
            Self::Active {
                mode: _,
                catalog_types,
            } => format!(
                "\"conflicting-prices\" against {} NPC-catalog types: we drop our ask where it \
                 is below TQ's best bid, and our bid where it is above TQ's best ask — each \
                 side on its own",
                crate::count(catalog_types)
            ),
        }
    }
}

/// One type that will not be seeded, with everything a report needs to explain why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedType {
    pub type_id: u32,
    pub name: String,
    pub bucket: BucketFlags,
    pub reason: DropReason,
    /// Canonical role/profile when this drop came from the role-based planner. Legacy
    /// bucket-only planning leaves these empty.
    pub role: Option<MarketRole>,
    pub profile: Option<PricingProfile>,
    /// Leaf type IDs the cost model could not price, which is what blocked this type.
    /// Empty for [`DropReason::UncapturedJunk`] (bucket D has no cost ladder at all) and
    /// for any caller that resolved without a cost report.
    pub blocking_leaves: Vec<u32>,
}

impl DroppedType {
    /// `A`, `A+B`, `B` or `D`.
    pub fn bucket_label(&self) -> &'static str {
        self.bucket.label()
    }
}

/// One type that will be seeded, at the price the manifest resolved for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeedableType {
    pub type_id: u32,
    /// Finite and > 0 — see [`SeedPlan::verify_prices`].
    pub price: f64,
    pub source: PriceSource,
    pub bucket: BucketFlags,
    pub role: MarketRole,
    pub profile: PricingProfile,
    /// Which of the two rows this type actually gets. Both unless one side was deferred to
    /// the NPC catalog; never neither, which is a drop instead.
    pub sides: SeedSides,
}

impl SeedableType {
    pub fn bucket_label(&self) -> &'static str {
        self.bucket.label()
    }
}

/// The finished answer: what gets seeded, and what was dropped instead.
#[derive(Debug, Clone, Default)]
pub struct SeedPlan {
    /// Keyed by type ID, so a type appears exactly once however many buckets claim it.
    pub seedable: BTreeMap<u32, SeedableType>,
    /// Index drops first, then junk drops, then NPC-catalog deferrals; ascending by type ID
    /// within each group.
    pub dropped: Vec<DroppedType>,
    /// Every deferral, both-sided and one-sided, ascending by type ID. The `Both` entries are
    /// the same types [`DropReason::DeferredToNpcCatalog`] names in [`SeedPlan::dropped`];
    /// the `Ask` and `Bid` entries are types that ARE seeded, on their other side.
    pub deferrals: Vec<Deferred>,
    /// Whether the NPC-catalog deferral ran, and against what.
    pub deferral: Deferral,
    /// Rare rows whose anchor was not an exact market reference, keyed for audit reporting.
    pub rare_fallbacks: BTreeMap<u32, RareFallbackProvenance>,
    /// V2's independently resolved side authorities and item policy. Empty in V1 mode.
    pub v2_sides: BTreeMap<u32, V2SidePrices>,
}

#[derive(Debug, Clone)]
pub struct V2SidePrices {
    pub policy: ResolvedItemPolicy,
    pub sell: Option<(f64, PriceSource)>,
    pub buy: Option<(f64, PriceSource)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RareFallbackProvenance {
    SiblingFamily {
        variation_parent_type_id: u32,
        sibling_type_id: u32,
        sibling_name: String,
        sibling_reference: f64,
        sibling_external_source: PriceSource,
        eligible_referenced_siblings: usize,
    },
    FundedCost {
        funded_cost: f64,
        path_summary: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RareFallbackAnchor {
    pub price: f64,
    pub source: PriceSource,
    pub provenance: RareFallbackProvenance,
}

pub type RareFallbackBook = BTreeMap<u32, RareFallbackAnchor>;
pub type ProductionIntermediatePriceBook = BTreeMap<u32, f64>;

const MAX_REQUIRED_RARE_REFERENCE_ANOMALIES: usize = 25;

/// Canonical assignments whose price must come from an external market reference. These
/// types share the ordinary manifest capture lifecycle; this function only adds the missing
/// policy-required membership to that lifecycle.
pub fn required_external_market_references(
    classification: &Classification,
    static_data: &StaticData,
    lp_rewards: &BTreeSet<u32>,
    bridge_group_ids: &BTreeSet<u32>,
    bridge_type_ids: &BTreeSet<u32>,
) -> Result<BTreeMap<u32, MarketAssignment>> {
    required_external_market_references_with_progression(
        classification,
        static_data,
        lp_rewards,
        bridge_group_ids,
        bridge_type_ids,
        &BTreeSet::new(),
        &BTreeSet::new(),
    )
}

fn required_external_market_references_with_progression(
    classification: &Classification,
    static_data: &StaticData,
    lp_rewards: &BTreeSet<u32>,
    bridge_group_ids: &BTreeSet<u32>,
    bridge_type_ids: &BTreeSet<u32>,
    progression_npc_group_ids: &BTreeSet<u32>,
    progression_npc_type_ids: &BTreeSet<u32>,
) -> Result<BTreeMap<u32, MarketAssignment>> {
    let mut required = BTreeMap::new();
    for item in static_data.marketable_item_types() {
        let assignment = classification.market_assignment_checked_with_supply_ladder(
            static_data,
            item.type_id,
            false,
            lp_rewards.contains(&item.type_id),
            bridge_group_ids,
            bridge_type_ids,
            progression_npc_group_ids,
            progression_npc_type_ids,
            false,
        )?;
        if let Some(assignment) =
            assignment.filter(|assignment| assignment.role == MarketRole::Rare)
        {
            required.insert(item.type_id, assignment);
        }
    }
    Ok(required)
}

pub fn required_external_market_references_from_config(
    classification: &Classification,
    static_data: &StaticData,
    config: &SeederConfig,
) -> Result<BTreeMap<u32, MarketAssignment>> {
    let lp_rewards =
        load_expanded_lp_reward_type_ids(&config.market_roles.lp_offer_catalog_path, static_data)?;
    let bridge_groups = config
        .market_roles
        .bridge_group_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let bridge_types = config
        .market_roles
        .bridge_type_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let progression_npc_groups = config
        .market_roles
        .progression_npc_group_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let progression_npc_types = config
        .market_roles
        .progression_npc_type_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut required = BTreeMap::new();
    for item in static_data.marketable_item_types() {
        let assignment = classification.market_assignment_checked_with_supply_ladder(
            static_data,
            item.type_id,
            false,
            lp_rewards.contains(&item.type_id),
            &bridge_groups,
            &bridge_types,
            &progression_npc_groups,
            &progression_npc_types,
            config.market_roles.moon_mining_available,
        )?;
        if let Some(assignment) =
            assignment.filter(|assignment| assignment.role == MarketRole::Rare)
        {
            required.insert(item.type_id, assignment);
        }
    }
    Ok(required)
}

/// V2 Rare source membership comes from resolved item policy, never legacy role tables.
pub fn required_external_market_references_v2(
    resolved: &BTreeMap<u32, Resolution>,
) -> Result<BTreeMap<u32, MarketAssignment>> {
    let mut required = BTreeMap::new();
    for (&type_id, decision) in resolved {
        let Some(policy) = &decision.policy else {
            continue;
        };
        if policy
            .sell
            .as_ref()
            .is_some_and(|side| side.source == PolicySource::RareReference)
            || policy
                .buy
                .as_ref()
                .is_some_and(|side| side.source == PolicySource::RareReference)
        {
            required.insert(type_id, diagnostic_assignment(policy)?);
        }
    }
    Ok(required)
}

/// Required Rare liquidity may only spend an approved captured market price. In particular,
/// a local `base-price` manifest entry is not a substitute.
pub fn check_required_external_market_references(
    required: &BTreeMap<u32, MarketAssignment>,
    static_data: &StaticData,
    manifest: &PriceManifest,
    policy: CapturePolicy,
    attempted_sources: &str,
) -> Result<()> {
    let missing = required
        .iter()
        .filter(|(type_id, _)| manifest.market_capture(**type_id, policy).is_none())
        .map(|(type_id, assignment)| {
            format!(
                "{} {} [role {}, profile {}; attempted sources: {}]",
                type_id,
                type_name(static_data, *type_id),
                assignment.role.key(),
                assignment.profile.key(),
                attempted_sources
            )
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    bail!(
        "{} required canonical Rare Buy-only types have no approved external market reference. Static basePrice and derived substitutes are not permitted. Unresolved:\n  {}",
        missing.len(),
        missing.join("\n  ")
    )
}

/// Refresh exhausts exact sources and strict family fallbacks. Missing RareT2 is deferred to
/// the build's funded-production resolver; every other unresolved Rare profile fails here.
pub fn check_refresh_rare_reference_coverage(
    required: &BTreeMap<u32, MarketAssignment>,
    static_data: &StaticData,
    manifest: &PriceManifest,
    policy: CapturePolicy,
    sibling_fallbacks: &RareFallbackBook,
    attempted_sources: &str,
) -> Result<()> {
    let missing = required
        .iter()
        .filter(|(type_id, assignment)| {
            let required_gameplay = static_data
                .item_type(**type_id)
                .is_some_and(|item| {
                    rare_liquidity_requirement(item)
                        == RareLiquidityRequirement::RequiredGameplay
                });
            required_gameplay
                && manifest.market_capture(**type_id, policy).is_none()
                && assignment.profile != PricingProfile::RareT2
                && !sibling_fallbacks.contains_key(type_id)
        })
        .map(|(type_id, assignment)| {
            format!(
                "{} {} [role {}, profile {}; attempted sources: {}; no same-profile referenced variationParentTypeID sibling]",
                type_id,
                type_name(static_data, *type_id),
                assignment.role.key(),
                assignment.profile.key(),
                attempted_sources
            )
        })
        .collect::<Vec<_>>();
    if missing.len() <= MAX_REQUIRED_RARE_REFERENCE_ANOMALIES {
        return Ok(());
    }
    bail!(
        "{} RequiredRareReferenceUnavailable anomalies exceed the systemic-failure threshold {}. Static basePrice, fuzzy names, same-group matches, cross-profile siblings, and derived prices are forbidden. Unresolved:\n  {}",
        missing.len(),
        MAX_REQUIRED_RARE_REFERENCE_ANOMALIES,
        missing.join("\n  ")
    )
}

fn rare_reference(
    type_id: u32,
    manifest: &PriceManifest,
    overlay: Option<&crate::overlay::OverlayImport>,
    policy: CapturePolicy,
) -> Option<(f64, PriceSource)> {
    overlay
        .and_then(|book| book.reference_book.reference_price(type_id))
        .map(|reference| (reference.price, PriceSource::CcpSnapshotJitaSplit))
        .or_else(|| {
            manifest
                .market_capture(type_id, policy)
                .map(|capture| (capture.price, capture.source))
        })
}

fn skillbook_reference(
    type_id: u32,
    base_price: Option<f64>,
    manifest: &PriceManifest,
    overlay: Option<&crate::overlay::OverlayImport>,
    policy: CapturePolicy,
) -> (f64, PriceSource) {
    overlay
        .and_then(|book| book.npc_min_sell(type_id))
        .map(|price| (price, PriceSource::NpcReference))
        .or_else(|| {
            overlay
                .and_then(|book| book.reference_book.reference_price(type_id))
                .map(|reference| (reference.price, PriceSource::CcpSnapshotJitaSplit))
        })
        .or_else(|| {
            manifest
                .market_capture(type_id, policy)
                .map(|capture| (capture.price, capture.source))
        })
        .or_else(|| {
            base_price
                .filter(|price| price.is_finite() && *price > 0.0)
                .map(|price| (price, PriceSource::BasePrice))
        })
        .unwrap_or((
            SKILLBOOK_FIXED_FALLBACK_ISK,
            PriceSource::SkillbookFixedFallback,
        ))
}

/// Resolves only strict same-family, same-profile loot/LP siblings. Exact references are
/// deliberately excluded from the output because they always win in canonical planning.
pub fn strict_sibling_family_fallbacks(
    required: &BTreeMap<u32, MarketAssignment>,
    static_data: &StaticData,
    families: &VariantFamilies,
    manifest: &PriceManifest,
    overlay: Option<&crate::overlay::OverlayImport>,
    policy: CapturePolicy,
) -> RareFallbackBook {
    let allowed = |profile| {
        matches!(
            profile,
            PricingProfile::RareFactionLp
                | PricingProfile::RareOfficer
                | PricingProfile::RareDeadspaceDed
        )
    };
    let mut fallbacks = RareFallbackBook::new();
    for (type_id, assignment) in required {
        if !allowed(assignment.profile)
            || static_data.item_type(*type_id).is_none_or(|item| {
                rare_liquidity_requirement(item) != RareLiquidityRequirement::RequiredGameplay
            })
            || rare_reference(*type_id, manifest, overlay, policy).is_some()
        {
            continue;
        }
        let Some(parent) = families.parent(*type_id) else {
            continue;
        };
        let mut siblings = required
            .iter()
            .filter(|(candidate_id, candidate)| {
                **candidate_id != *type_id
                    && candidate.profile == assignment.profile
                    && families.parent(**candidate_id) == Some(parent)
                    && static_data
                        .item_type(**candidate_id)
                        .is_some_and(|item| item.is_marketable())
            })
            .filter_map(|(candidate_id, _)| {
                rare_reference(*candidate_id, manifest, overlay, policy)
                    .map(|(price, source)| (*candidate_id, price, source))
            })
            .collect::<Vec<_>>();
        siblings.sort_by(|left, right| left.1.total_cmp(&right.1).then(left.0.cmp(&right.0)));
        let Some((sibling_type_id, sibling_reference, sibling_external_source)) =
            siblings.first().copied()
        else {
            continue;
        };
        fallbacks.insert(
            *type_id,
            RareFallbackAnchor {
                price: sibling_reference,
                source: PriceSource::SiblingFamilyFallback,
                provenance: RareFallbackProvenance::SiblingFamily {
                    variation_parent_type_id: parent,
                    sibling_type_id,
                    sibling_name: type_name(static_data, sibling_type_id).to_string(),
                    sibling_reference,
                    sibling_external_source,
                    eligible_referenced_siblings: siblings.len(),
                },
            },
        );
    }
    fallbacks
}

impl SeedPlan {
    /// Resolves the canonical solo-economy book. Snapshot data is reference authority only;
    /// no overlay row or collision deferral participates in this plan.
    pub fn resolve_canonical(
        classification: &Classification,
        static_data: &StaticData,
        cost: &CostReport,
        manifest: &PriceManifest,
        overlay: Option<&crate::overlay::OverlayImport>,
        config: &SeederConfig,
    ) -> Result<Self> {
        Self::resolve_canonical_inner(
            classification,
            static_data,
            cost,
            manifest,
            overlay,
            config,
            &RareFallbackBook::new(),
            &ProductionIntermediatePriceBook::new(),
            true,
            None,
        )
    }

    pub fn resolve_canonical_partial(
        classification: &Classification,
        static_data: &StaticData,
        cost: &CostReport,
        manifest: &PriceManifest,
        overlay: Option<&crate::overlay::OverlayImport>,
        config: &SeederConfig,
        fallbacks: &RareFallbackBook,
    ) -> Result<Self> {
        Self::resolve_canonical_inner(
            classification,
            static_data,
            cost,
            manifest,
            overlay,
            config,
            fallbacks,
            &ProductionIntermediatePriceBook::new(),
            false,
            None,
        )
    }

    pub fn resolve_canonical_with_fallbacks(
        classification: &Classification,
        static_data: &StaticData,
        cost: &CostReport,
        manifest: &PriceManifest,
        overlay: Option<&crate::overlay::OverlayImport>,
        config: &SeederConfig,
        fallbacks: &RareFallbackBook,
    ) -> Result<Self> {
        Self::resolve_canonical_inner(
            classification,
            static_data,
            cost,
            manifest,
            overlay,
            config,
            fallbacks,
            &ProductionIntermediatePriceBook::new(),
            true,
            None,
        )
    }

    pub fn resolve_canonical_with_supply_ladder(
        classification: &Classification,
        static_data: &StaticData,
        cost: &CostReport,
        manifest: &PriceManifest,
        overlay: Option<&crate::overlay::OverlayImport>,
        config: &SeederConfig,
        fallbacks: &RareFallbackBook,
        production_intermediates: &ProductionIntermediatePriceBook,
    ) -> Result<Self> {
        Self::resolve_canonical_inner(
            classification,
            static_data,
            cost,
            manifest,
            overlay,
            config,
            fallbacks,
            production_intermediates,
            true,
            None,
        )
    }

    pub fn resolve_v2_partial(
        classification: &Classification,
        static_data: &StaticData,
        cost: &CostReport,
        manifest: &PriceManifest,
        overlay: Option<&crate::overlay::OverlayImport>,
        config: &SeederConfig,
        fallbacks: &RareFallbackBook,
        resolved_policy: &BTreeMap<u32, Resolution>,
    ) -> Result<Self> {
        Self::resolve_canonical_inner(
            classification,
            static_data,
            cost,
            manifest,
            overlay,
            config,
            fallbacks,
            &ProductionIntermediatePriceBook::new(),
            false,
            Some(resolved_policy),
        )
    }

    pub fn resolve_v2_final(
        classification: &Classification,
        static_data: &StaticData,
        cost: &CostReport,
        manifest: &PriceManifest,
        overlay: Option<&crate::overlay::OverlayImport>,
        config: &SeederConfig,
        fallbacks: &RareFallbackBook,
        production_intermediates: &ProductionIntermediatePriceBook,
        resolved_policy: &BTreeMap<u32, Resolution>,
    ) -> Result<Self> {
        Self::resolve_canonical_inner(
            classification,
            static_data,
            cost,
            manifest,
            overlay,
            config,
            fallbacks,
            production_intermediates,
            true,
            Some(resolved_policy),
        )
    }

    fn resolve_canonical_inner(
        classification: &Classification,
        static_data: &StaticData,
        cost: &CostReport,
        manifest: &PriceManifest,
        overlay: Option<&crate::overlay::OverlayImport>,
        config: &SeederConfig,
        fallbacks: &RareFallbackBook,
        production_intermediates: &ProductionIntermediatePriceBook,
        enforce_required_references: bool,
        resolved_policy: Option<&BTreeMap<u32, Resolution>>,
    ) -> Result<Self> {
        let lp_path = config
            .policy
            .as_ref()
            .map(|v2| &v2.lp_offer_catalog_path)
            .unwrap_or(&config.market_roles.lp_offer_catalog_path);
        let lp_rewards = load_expanded_lp_reward_type_ids(lp_path, static_data)?;
        let bridge_groups = config
            .market_roles
            .bridge_group_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let bridge_types = config
            .market_roles
            .bridge_type_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let progression_npc_groups = config
            .market_roles
            .progression_npc_group_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let progression_npc_types = config
            .market_roles
            .progression_npc_type_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let policy = CapturePolicy::from_config(config);
        let needs_tq = resolved_policy.is_some_and(|items| {
            items.values().any(|resolution| {
                resolution.policy.as_ref().is_some_and(|item| {
                    item.sell.as_ref().is_some_and(|s| {
                        matches!(
                            s.source,
                            PolicySource::TqSnapshot
                                | PolicySource::TqSnapshotSellFallback
                                | PolicySource::TqSnapshotBuyFallback
                                | PolicySource::TqAveragePrice
                        )
                    }) || item.buy.as_ref().is_some_and(|s| {
                        matches!(
                            s.source,
                            PolicySource::TqSnapshot
                                | PolicySource::TqSnapshotSellFallback
                                | PolicySource::TqSnapshotBuyFallback
                                | PolicySource::TqAveragePrice
                        )
                    })
                })
            })
        });
        let tq_book = if needs_tq {
            let path = config
                .policy
                .as_ref()
                .and_then(|p| p.tq_snapshot_path.as_ref())
                .ok_or_else(|| anyhow::anyhow!("tq_snapshot requires [policy] tq_snapshot_path"))?;
            Some(crate::tq_snapshot::TqSnapshot::load(path)?)
        } else {
            None
        };
        let mut plan = Self::default();
        let mut side_owners: BTreeMap<(u32, bool), MarketRole> = BTreeMap::new();

        for item in static_data.marketable_item_types() {
            let type_id = item.type_id;
            let bpo_npc_eligible = overlay.is_some_and(|book| book.is_npc_catalog_type(type_id));
            let v2_item = resolved_policy
                .and_then(|book| book.get(&type_id))
                .and_then(|resolution| resolution.policy.as_ref());
            let assignment = if resolved_policy.is_some() {
                let Some(policy) = v2_item else { continue };
                diagnostic_assignment(policy)?
            } else {
                let Some(assignment) = classification
                    .market_assignment_checked_with_supply_ladder(
                        static_data,
                        type_id,
                        bpo_npc_eligible,
                        lp_rewards.contains(&type_id),
                        &bridge_groups,
                        &bridge_types,
                        &progression_npc_groups,
                        &progression_npc_types,
                        config.market_roles.moon_mining_available,
                    )?
                else {
                    continue;
                };
                assignment
            };
            let bucket = classification.flags(type_id);
            let mut sides = SeedSides {
                ask: assignment.sides.sell,
                bid: assignment.sides.buy,
            };
            if resolved_policy.is_none()
                && assignment.profile == PricingProfile::PlanetaryIndustry
                && !config.market_roles.planetary_industry_buy_enabled
            {
                sides.bid = false;
            }

            if assignment.role == MarketRole::Rare
                && rare_liquidity_requirement(item) == RareLiquidityRequirement::OptionalBlueprint
            {
                plan.dropped.push(DroppedType {
                    type_id,
                    name: item.name.clone(),
                    bucket,
                    reason: DropReason::OptionalRareBlueprint,
                    role: Some(assignment.role),
                    profile: Some(assignment.profile),
                    blocking_leaves: Vec::new(),
                });
                continue;
            }

            let v2_prices = if let Some(item_policy) = v2_item {
                let sell = item_policy
                    .sell
                    .as_ref()
                    .map(|side| {
                        resolve_v2_source(
                            side.source,
                            crate::tq_snapshot::Side::Sell,
                            tq_book.as_ref(),
                            type_id,
                            item.base_price,
                            bucket,
                            manifest,
                            overlay,
                            classification,
                            static_data,
                            policy,
                            fallbacks,
                            production_intermediates,
                        )
                    })
                    .transpose()?;
                let buy = item_policy
                    .buy
                    .as_ref()
                    .map(|side| {
                        resolve_v2_source(
                            side.source,
                            crate::tq_snapshot::Side::Buy,
                            tq_book.as_ref(),
                            type_id,
                            item.base_price,
                            bucket,
                            manifest,
                            overlay,
                            classification,
                            static_data,
                            policy,
                            fallbacks,
                            production_intermediates,
                        )
                    })
                    .transpose()?;
                Some(V2SidePrices {
                    policy: item_policy.clone(),
                    sell: sell.flatten(),
                    buy: buy.flatten(),
                })
            } else {
                None
            };
            let resolved = if let Some(side_prices) = &v2_prices {
                side_prices.sell.or(side_prices.buy)
            } else {
                match assignment.role {
                    MarketRole::Core => match manifest.seed_price(type_id, bucket, policy) {
                        SeedPrice::Priced { price, source } => Some((price, source)),
                        SeedPrice::Blocked | SeedPrice::UnseedableJunk => None,
                    },
                    MarketRole::Bridge => manifest
                        .entry(type_id)
                        .and_then(|entry| entry.market_capture_record(policy))
                        .map(|capture| (capture.price, capture.source)),
                    MarketRole::ProgressionNpc => overlay
                        .and_then(|book| book.npc_min_sell(type_id))
                        .map(|price| (price, PriceSource::NpcReference)),
                    MarketRole::ProductionIntermediate => production_intermediates
                        .get(&type_id)
                        .copied()
                        .map(|price| (price, PriceSource::FundedProductionIntermediate)),
                    MarketRole::Rare
                        if classification
                            .is_special_reward_placeholder_hull(static_data, type_id) =>
                    {
                        rare_reference(type_id, manifest, overlay, policy)
                    }
                    MarketRole::Rare => {
                        rare_reference(type_id, manifest, overlay, policy).or_else(|| {
                            fallbacks
                                .get(&type_id)
                                .map(|fallback| (fallback.price, fallback.source))
                        })
                    }
                    MarketRole::Bpo => bpo_reference(
                        type_id,
                        item.base_price,
                        overlay,
                        classification.is_published_reaction_formula(static_data, type_id),
                    ),
                    MarketRole::Skillbook => Some(skillbook_reference(
                        type_id,
                        item.base_price,
                        manifest,
                        overlay,
                        policy,
                    )),
                    MarketRole::CommandCenter => overlay
                        .and_then(|book| book.npc_min_sell(type_id))
                        .map(|price| (price, PriceSource::CcpSnapshotJitaSplit))
                        .or_else(|| {
                            item.base_price
                                .filter(|price| price.is_finite() && *price > 0.0)
                                .map(|price| (price, PriceSource::BasePrice))
                        }),
                }
            };

            let Some((price, source)) = resolved else {
                let reason = if assignment.role == MarketRole::Core {
                    if bucket.junk_candidate {
                        DropReason::UncapturedJunk
                    } else {
                        DropReason::UnpricedIndexType
                    }
                } else if assignment.role == MarketRole::Rare {
                    match rare_liquidity_requirement(item) {
                        RareLiquidityRequirement::RequiredGameplay => {
                            DropReason::RequiredRareReferenceUnavailable
                        }
                        RareLiquidityRequirement::OptionalCosmetic => {
                            DropReason::OptionalRareUnreferenced
                        }
                        RareLiquidityRequirement::OptionalBlueprint => {
                            DropReason::OptionalRareBlueprint
                        }
                    }
                } else {
                    DropReason::UnpricedRoleReference
                };
                plan.dropped.push(DroppedType {
                    type_id,
                    name: item.name.clone(),
                    bucket,
                    reason,
                    role: Some(assignment.role),
                    profile: Some(assignment.profile),
                    blocking_leaves: if reason == DropReason::UnpricedIndexType {
                        blocking_leaves(cost, type_id)
                    } else {
                        Vec::new()
                    },
                });
                continue;
            };

            for (is_buy, enabled) in [(false, sides.ask), (true, sides.bid)] {
                if !enabled {
                    continue;
                }
                if let Some(owner) = side_owners.insert((type_id, is_buy), assignment.role) {
                    bail!(
                        "type {type_id} side {} is owned by both {} and {}; canonical seed side ownership must be unique before SQLite writing",
                        if is_buy { "buy" } else { "sell" },
                        owner.key(),
                        assignment.role.key()
                    );
                }
            }
            if plan
                .seedable
                .insert(
                    type_id,
                    SeedableType {
                        type_id,
                        price,
                        source,
                        bucket,
                        role: assignment.role,
                        profile: assignment.profile,
                        sides,
                    },
                )
                .is_some()
            {
                bail!("type {type_id} received two canonical market assignments");
            }
            if let Some(side_prices) = v2_prices {
                plan.v2_sides.insert(type_id, side_prices);
            }
            if source == PriceSource::SiblingFamilyFallback
                || source == PriceSource::FundedCostFallback
            {
                let fallback = fallbacks.get(&type_id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "type {type_id} carries fallback source {} without provenance",
                        source.key()
                    )
                })?;
                plan.rare_fallbacks
                    .insert(type_id, fallback.provenance.clone());
            }
        }

        plan.verify_prices()?;
        if enforce_required_references {
            plan.check_required_role_references()?;
        }
        plan.check_drop_budget(config)?;
        Ok(plan)
    }

    /// Resolves the seeded universe against the manifest.
    ///
    /// `cost` supplies the blame for index drops (`missing_leaves`). It may be an empty
    /// [`CostReport`] — a caller that consumes the manifest read-only still gets the right
    /// seedable set and drop list, just without leaf-level blame.
    ///
    /// `npc_catalog` is the bucket-C book this pass stands down in front of; pass
    /// [`NpcCatalog::none`] when no overlay was imported. `pricing` is the two multiplier sets
    /// the seed writer will use, because under the default `"conflicting-prices"` mode the
    /// deferral compares **our computed prices** against TQ's and cannot be decided without
    /// them.
    ///
    /// ### Why this cannot leave a seeder-involved check-4 violation
    ///
    /// Audit check 4 flags `best_bid_anywhere > best_ask_anywhere` for one type across two
    /// stations. Write `A` for a surviving ask of ours and `B` for a surviving bid; by
    /// [`conflicting_sides`], `A >= npc_bid` for every NPC bid on that type and
    /// `B <= npc_ask` for every NPC ask. The check has four ways to pick its pair:
    ///
    /// 1. buy at our `A`, sell into an NPC bid — needs `npc_bid > A`, excluded;
    /// 2. buy at an NPC ask, sell into our `B` — needs `B > npc_ask`, excluded;
    /// 3. both legs ours, which is one station (Jita 4-4) and not check 4's business — and
    ///    `build::verify_seed_rows` proves `bid <= ask` there anyway;
    /// 4. both legs TQ's, which is npc-to-npc and not seeder-involved.
    ///
    /// The [`BestPrice::elsewhere`](crate::audit::BestPrice) fallback picks a *different*
    /// station on one leg, which only ever swaps one of those legs for another price of the
    /// same origin, so it lands back in the same four cases. Both quantifiers are over the
    /// catalog's best price on the opposite side, which is what [`NpcCatalog`] stores.
    ///
    /// Fails when the price invariant is violated, or when more than
    /// `[index_seed] max_unpriced_drops` A∪B types were dropped.
    pub fn resolve(
        classification: &Classification,
        static_data: &StaticData,
        cost: &CostReport,
        manifest: &PriceManifest,
        npc_catalog: &NpcCatalog,
        pricing: SeedPricingPair,
        config: &SeederConfig,
    ) -> Result<Self> {
        let mut plan = Self::default();
        let mut index_dropped: Vec<DroppedType> = Vec::new();
        let mut junk_dropped: Vec<DroppedType> = Vec::new();
        let mut deferred_both: Vec<DroppedType> = Vec::new();
        // Rule A lives here rather than at each call site: whether an `adjusted_price`-only
        // record counts as a captured market price decides bucket-D seedability, so it has to
        // be the same answer for every bucket in a run.
        let policy = CapturePolicy::from_config(config);
        plan.deferral = match (
            config.index_seed.defer_to_npc_catalog,
            npc_catalog.imported(),
        ) {
            (NpcDeferralMode::Off, _) => Deferral::Disabled,
            (_, false) => Deferral::NoCatalog,
            (mode, true) => Deferral::Active {
                mode,
                catalog_types: npc_catalog.len(),
            },
        };
        let mode = plan.deferral.mode();

        // `seeded_types` is A ∪ B ∪ D with each type counted once, and already excludes
        // bucket D entirely when `[junk_seed] enabled = false`.
        for type_id in classification.seeded_types() {
            let bucket = classification.flags(type_id);
            // "whole-type" is decided BEFORE the price is looked at, on purpose: it does not
            // consult a price at all, and asking the manifest first would file an
            // unpriceable-but-deferred type under the wrong reason and charge it to the drop
            // budget. "conflicting-prices" cannot do that — it has nothing to compare until
            // the manifest has answered — so under it an unpriceable type is reported as
            // unpriced, which is what it is: it was never going to be seeded either way.
            if mode == Some(NpcDeferralMode::WholeType) && npc_catalog.contains(type_id) {
                let name = type_name(static_data, type_id).to_string();
                plan.deferrals.push(Deferred {
                    type_id,
                    name: name.clone(),
                    bucket,
                    side: DeferredSide::Both,
                    ask_conflict: None,
                    bid_conflict: None,
                });
                deferred_both.push(DroppedType {
                    type_id,
                    name,
                    bucket,
                    reason: DropReason::DeferredToNpcCatalog,
                    role: None,
                    profile: None,
                    blocking_leaves: Vec::new(),
                });
                continue;
            }
            // Every bucket rule lives in `PriceManifest::seed_price`; this loop only
            // decides what to do with its answer.
            match manifest.seed_price(type_id, bucket, policy) {
                SeedPrice::Priced { price, source } => {
                    let verdict = match mode {
                        Some(NpcDeferralMode::ConflictingPrices) => conflicting_sides(
                            pricing.for_bucket(bucket),
                            price,
                            npc_catalog.quote(type_id),
                        ),
                        _ => SideVerdict {
                            sides: SeedSides::BOTH,
                            ask_conflict: None,
                            bid_conflict: None,
                        },
                    };
                    if let Some(side) = verdict.sides.deferred_side() {
                        plan.deferrals.push(Deferred {
                            type_id,
                            name: type_name(static_data, type_id).to_string(),
                            bucket,
                            side,
                            ask_conflict: verdict.ask_conflict,
                            bid_conflict: verdict.bid_conflict,
                        });
                    }
                    if verdict.sides.is_empty() {
                        // Both prices cross: there is no row left to write, so this is a drop
                        // and lands in the same bucket the whole-type mode uses.
                        deferred_both.push(DroppedType {
                            type_id,
                            name: type_name(static_data, type_id).to_string(),
                            bucket,
                            reason: DropReason::DeferredToNpcCatalog,
                            role: None,
                            profile: None,
                            blocking_leaves: Vec::new(),
                        });
                        continue;
                    }
                    plan.seedable.insert(
                        type_id,
                        SeedableType {
                            type_id,
                            price,
                            source,
                            bucket,
                            role: MarketRole::Core,
                            profile: PricingProfile::CoreGeneral,
                            sides: verdict.sides,
                        },
                    );
                }
                SeedPrice::Blocked => index_dropped.push(DroppedType {
                    type_id,
                    name: type_name(static_data, type_id).to_string(),
                    bucket,
                    reason: DropReason::UnpricedIndexType,
                    role: None,
                    profile: None,
                    blocking_leaves: blocking_leaves(cost, type_id),
                }),
                SeedPrice::UnseedableJunk => junk_dropped.push(DroppedType {
                    type_id,
                    name: type_name(static_data, type_id).to_string(),
                    bucket,
                    reason: DropReason::UncapturedJunk,
                    role: None,
                    profile: None,
                    blocking_leaves: Vec::new(),
                }),
            }
        }

        plan.dropped = index_dropped;
        plan.dropped.extend(junk_dropped);
        plan.dropped.extend(deferred_both);

        plan.verify_prices()?;
        plan.check_drop_budget(config)?;
        Ok(plan)
    }

    /// The invariant the seed writer depends on: no seedable type may carry a zero,
    /// negative, NaN or infinite price. Violations name the type rather than asserting.
    pub fn verify_prices(&self) -> Result<()> {
        for entry in self.seedable.values() {
            if !entry.price.is_finite() || entry.price <= 0.0 {
                bail!(
                    "type {} is seedable but its manifest price is {} (source {}). Every \
                     seedable type must carry a finite manifest price greater than 0 — a \
                     zero or NaN price would seed a free or unmarshalable order. Fix or \
                     remove the entry in the price manifest.",
                    entry.type_id,
                    entry.price,
                    entry.source.key()
                );
            }
        }
        Ok(())
    }

    /// Canonical non-Core roles are policy-required supply or liquidity. Unlike Core price
    /// gaps, they have no tolerated drop budget: losing one is a hard planning failure.
    fn check_required_role_references(&self) -> Result<()> {
        const SAMPLE: usize = 10;
        let affected = self
            .dropped_with(DropReason::UnpricedRoleReference)
            .collect::<Vec<_>>();
        if affected.is_empty() {
            // Continue below: Rare anomalies have their own deliberately small review budget.
        } else {
            let mut sample = affected
                .iter()
                .take(SAMPLE)
                .map(|entry| {
                    format!(
                        "{} {} [role {}, profile {}]",
                        entry.type_id,
                        entry.name,
                        entry.role.map(MarketRole::key).unwrap_or("unknown"),
                        entry.profile.map(PricingProfile::key).unwrap_or("unknown")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            if affected.len() > SAMPLE {
                sample.push_str(&format!(", … and {} more", affected.len() - SAMPLE));
            }
            bail!(
                "{} required canonical non-Rare role types have no approved reference price. This is a hard failure outside [index_seed] max_unpriced_drops; required supply may not disappear silently. Affected: {sample}",
                affected.len()
            )
        }

        let rare = self
            .dropped_with(DropReason::RequiredRareReferenceUnavailable)
            .collect::<Vec<_>>();
        if rare.len() > MAX_REQUIRED_RARE_REFERENCE_ANOMALIES {
            bail!(
                "{} RequiredRareReferenceUnavailable anomalies exceed the systemic-failure threshold {}. No price was fabricated. Affected:\n  {}",
                rare.len(),
                MAX_REQUIRED_RARE_REFERENCE_ANOMALIES,
                rare.iter()
                    .map(|entry| format!(
                        "{} {} [profile {}]",
                        entry.type_id,
                        entry.name,
                        entry.profile.map(PricingProfile::key).unwrap_or("unknown")
                    ))
                    .collect::<Vec<_>>()
                    .join("\n  ")
            );
        }
        Ok(())
    }

    /// Fails when the number of dropped A∪B index types exceeds
    /// `[index_seed] max_unpriced_drops`.
    fn check_drop_budget(&self, config: &SeederConfig) -> Result<()> {
        let budget = config.index_seed.max_unpriced_drops;
        let dropped = self.index_drop_count();
        if (dropped as i64) <= budget {
            return Ok(());
        }
        const NAMED: usize = 10;
        let mut sample = self
            .dropped_with(DropReason::UnpricedIndexType)
            .take(NAMED)
            .map(|entry| format!("{} {}", entry.type_id, entry.name))
            .collect::<Vec<_>>()
            .join(", ");
        if dropped > NAMED {
            sample.push_str(&format!(", … and {} more", dropped - NAMED));
        }
        bail!(
            "{dropped} A∪B index types have no price in the manifest, which exceeds \
             [index_seed] max_unpriced_drops = {budget}. Unpriceable types are dropped \
             rather than seeded at a guessed price, but this many usually means the price \
             manifest is missing, stale or truncated. Run `market-seederv3 refresh-prices` \
             to capture what is missing, or raise max_unpriced_drops deliberately once you \
             have confirmed every drop is permanent. Dropped: {sample}."
        )
    }

    pub fn seedable_count(&self) -> usize {
        self.seedable.len()
    }

    /// Seedable types that will get a `seed_stock` row. Differs from
    /// [`SeedPlan::seedable_count`] only by the ask-deferred types.
    pub fn seedable_ask_count(&self) -> usize {
        self.seedable
            .values()
            .filter(|entry| entry.sides.ask)
            .count()
    }

    /// Seedable types that will get a `seed_buy_orders` row.
    pub fn seedable_bid_count(&self) -> usize {
        self.seedable
            .values()
            .filter(|entry| entry.sides.bid)
            .count()
    }

    /// Seedable A∪B types — the index seed rows.
    pub fn seedable_index_count(&self) -> usize {
        self.seedable
            .values()
            .filter(|entry| entry.bucket.manufacturable_t1 || entry.bucket.material)
            .count()
    }

    /// Seedable bucket-D types — the junk seed rows.
    pub fn seedable_junk_count(&self) -> usize {
        self.seedable_count() - self.seedable_index_count()
    }

    pub fn dropped_with(&self, reason: DropReason) -> impl Iterator<Item = &DroppedType> {
        self.dropped
            .iter()
            .filter(move |drop| drop.reason == reason)
    }

    pub fn index_drop_count(&self) -> usize {
        self.dropped_with(DropReason::UnpricedIndexType).count()
    }

    pub fn junk_drop_count(&self) -> usize {
        self.dropped_with(DropReason::UncapturedJunk).count()
    }

    pub fn unpriced_role_drop_count(&self) -> usize {
        self.dropped_with(DropReason::UnpricedRoleReference).count()
    }

    pub fn required_rare_anomaly_count(&self) -> usize {
        self.dropped_with(DropReason::RequiredRareReferenceUnavailable)
            .count()
    }

    pub fn optional_rare_drop_count(&self) -> usize {
        self.dropped_with(DropReason::OptionalRareUnreferenced)
            .count()
            + self.dropped_with(DropReason::OptionalRareBlueprint).count()
    }

    /// Types that lost **both** sides to the NPC catalog, i.e. are not seeded at all.
    pub fn deferred_count(&self) -> usize {
        self.dropped_with(DropReason::DeferredToNpcCatalog).count()
    }

    /// Every deferral, whichever side it took.
    pub fn deferral_count(&self) -> usize {
        self.deferrals.len()
    }

    pub fn deferrals_with(&self, side: DeferredSide) -> impl Iterator<Item = &Deferred> {
        self.deferrals
            .iter()
            .filter(move |entry| entry.side == side)
    }

    pub fn deferred_side_count(&self, side: DeferredSide) -> usize {
        self.deferrals_with(side).count()
    }

    /// The seedable set broken down by bucket label, in `BucketFlags::label` order. The
    /// parts sum to [`SeedPlan::seedable_count`].
    pub fn seedable_composition(&self) -> Vec<(&'static str, usize)> {
        self.composition(self.seedable.values().map(SeedableType::bucket_label))
    }

    pub fn seedable_role_composition(&self) -> Vec<(&'static str, usize)> {
        let mut counts = BTreeMap::<&'static str, usize>::new();
        for entry in self.seedable.values() {
            *counts.entry(entry.role.key()).or_insert(0) += 1;
        }
        [
            "core",
            "bpo",
            "bridge",
            "rare",
            "skillbook",
            "command-center",
        ]
        .into_iter()
        .map(|key| (key, counts.get(key).copied().unwrap_or(0)))
        .collect()
    }

    pub fn seedable_profile_composition(&self) -> Vec<(&'static str, usize)> {
        let mut counts = BTreeMap::<&'static str, usize>::new();
        for entry in self.seedable.values() {
            *counts.entry(entry.profile.key()).or_insert(0) += 1;
        }
        counts.into_iter().collect()
    }

    /// The deferred set broken down the same way, so a reader can see whether the NPC catalog
    /// took ships (A), materials (B) or junk (D) off the index seed.
    pub fn deferred_composition(&self) -> Vec<(&'static str, usize)> {
        self.composition(
            self.dropped_with(DropReason::DeferredToNpcCatalog)
                .map(DroppedType::bucket_label),
        )
    }

    fn composition<'a>(&self, labels: impl Iterator<Item = &'a str>) -> Vec<(&'static str, usize)> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for label in labels {
            *counts.entry(label).or_insert(0) += 1;
        }
        ["A", "A+B", "B", "D"]
            .into_iter()
            .map(|label| (label, counts.get(label).copied().unwrap_or(0)))
            .collect()
    }
}

fn diagnostic_assignment(policy: &ResolvedItemPolicy) -> Result<MarketAssignment> {
    let key = policy.profile_id.as_deref().unwrap_or("core_general");
    let profile = match key {
        "compressed_ore" => PricingProfile::CompressedOre,
        "raw_ore" => PricingProfile::RawOre,
        "minerals" => PricingProfile::Minerals,
        "gas_ice_salvage" => PricingProfile::GasIceSalvage,
        "t1_module_ammo_rig" => PricingProfile::T1ModuleAmmoRig,
        "destroyer" => PricingProfile::Destroyer,
        "cruiser" => PricingProfile::Cruiser,
        "battlecruiser" => PricingProfile::Battlecruiser,
        "battleship" => PricingProfile::Battleship,
        "capital_structure_component" => PricingProfile::CapitalStructureComponent,
        "orca_freighter_structure" => PricingProfile::OrcaFreighterStructure,
        "bpo" => PricingProfile::Bpo,
        "reaction_formula" => PricingProfile::ReactionFormula,
        "bridge" => PricingProfile::Bridge,
        "planetary_industry" => PricingProfile::PlanetaryIndustry,
        "progression_npc" => PricingProfile::ProgressionNpc,
        "production_intermediate" => PricingProfile::ProductionIntermediate,
        "rare_t2" => PricingProfile::RareT2,
        "rare_faction_lp" => PricingProfile::RareFactionLp,
        "rare_deadspace_ded" => PricingProfile::RareDeadspaceDed,
        "rare_officer" => PricingProfile::RareOfficer,
        "skillbook" => PricingProfile::Skillbook,
        "command_center" => PricingProfile::CommandCenter,
        _ => PricingProfile::CoreGeneral,
    };
    let source = policy
        .sell
        .as_ref()
        .or(policy.buy.as_ref())
        .ok_or_else(|| anyhow::anyhow!("seeded policy has no source"))?
        .source;
    let role = match source {
        PolicySource::TqSnapshot
        | PolicySource::TqSnapshotSellFallback
        | PolicySource::TqSnapshotBuyFallback
        | PolicySource::TqAveragePrice => MarketRole::Core,
        PolicySource::FundedCost => MarketRole::ProductionIntermediate,
        PolicySource::NpcAcquisition
        | PolicySource::T1VariantSell
        | PolicySource::T1VariantBuy
        | PolicySource::ManualFixed
        | PolicySource::SiblingFamily => MarketRole::Core,
        PolicySource::CoreManifestCost => MarketRole::Core,
        PolicySource::CapturedMarket => MarketRole::Bridge,
        PolicySource::NpcMinSell => MarketRole::ProgressionNpc,
        PolicySource::BpoNpcOrBase => MarketRole::Bpo,
        PolicySource::RareReference => MarketRole::Rare,
        PolicySource::SkillbookLadder => MarketRole::Skillbook,
        PolicySource::CommandCenterLadder => MarketRole::CommandCenter,
    };
    Ok(MarketAssignment {
        role,
        profile,
        sides: crate::classify::MarketSidePolicy {
            sell: policy.sides.has_sell(),
            buy: policy.sides.has_buy(),
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn resolve_v2_source(
    source: PolicySource,
    side: crate::tq_snapshot::Side,
    tq_book: Option<&crate::tq_snapshot::TqSnapshot>,
    type_id: u32,
    base_price: Option<f64>,
    bucket: BucketFlags,
    manifest: &PriceManifest,
    overlay: Option<&crate::overlay::OverlayImport>,
    classification: &Classification,
    static_data: &StaticData,
    capture: CapturePolicy,
    fallbacks: &RareFallbackBook,
    production_intermediates: &ProductionIntermediatePriceBook,
) -> Result<Option<(f64, PriceSource)>> {
    let item = static_data
        .item_type(type_id)
        .ok_or_else(|| anyhow::anyhow!("source resolution missing type {type_id}"))?;
    let result = match source {
        PolicySource::NpcAcquisition
        | PolicySource::T1VariantSell
        | PolicySource::T1VariantBuy
        | PolicySource::ManualFixed
        | PolicySource::SiblingFamily => {
            bail!("type {type_id}: this source requires the Workbench GENERAL_TQ quote context");
        }
        PolicySource::TqSnapshot
        | PolicySource::TqSnapshotSellFallback
        | PolicySource::TqSnapshotBuyFallback
        | PolicySource::TqAveragePrice => tq_book
            .and_then(|book| book.policy_reference(type_id, side, source))
            .map(|price| (price, PriceSource::TqSnapshot)),
        PolicySource::FundedCost => production_intermediates
            .get(&type_id)
            .copied()
            .map(|price| (price, PriceSource::FundedProductionIntermediate)),
        PolicySource::CoreManifestCost => match manifest.seed_price(type_id, bucket, capture) {
            SeedPrice::Priced { price, source } => Some((price, source)),
            SeedPrice::Blocked | SeedPrice::UnseedableJunk => None,
        },
        PolicySource::CapturedMarket => manifest
            .entry(type_id)
            .and_then(|entry| entry.market_capture_record(capture))
            .map(|record| (record.price, record.source)),
        PolicySource::NpcMinSell => overlay
            .and_then(|book| book.npc_min_sell(type_id))
            .map(|price| (price, PriceSource::NpcReference)),
        PolicySource::BpoNpcOrBase => {
            let reaction = classification.is_published_reaction_formula(static_data, type_id);
            let npc_eligible = overlay.is_some_and(|book| book.is_npc_catalog_type(type_id));
            if !reaction
                && !(npc_eligible && classification.is_approved_reusable_bpo(static_data, type_id))
            {
                bail!("type {type_id}: bpo_npc_or_base source is ineligible");
            }
            bpo_reference(type_id, base_price, overlay, reaction)
        }
        PolicySource::RareReference => {
            let exact = rare_reference(type_id, manifest, overlay, capture);
            if classification.is_special_reward_placeholder_hull(static_data, type_id) {
                exact
            } else {
                exact.or_else(|| {
                    fallbacks
                        .get(&type_id)
                        .map(|anchor| (anchor.price, anchor.source))
                })
            }
        }
        PolicySource::SkillbookLadder => {
            if item.category_id != Some(crate::classify::SKILL_CATEGORY_ID) {
                bail!("type {type_id}: skillbook_ladder source is ineligible");
            }
            Some(skillbook_reference(
                type_id, base_price, manifest, overlay, capture,
            ))
        }
        PolicySource::CommandCenterLadder => {
            if item.group_id != Some(crate::classify::COMMAND_CENTER_GROUP_ID) {
                bail!("type {type_id}: command_center_ladder source is ineligible");
            }
            overlay
                .and_then(|book| book.npc_min_sell(type_id))
                .map(|price| (price, PriceSource::CcpSnapshotJitaSplit))
                .or_else(|| {
                    base_price
                        .filter(|p| p.is_finite() && *p > 0.0)
                        .map(|price| (price, PriceSource::BasePrice))
                })
        }
    };
    Ok(result)
}

/// Resolve a reusable-recipe reference. Ordinary manufacturing BPOs still require independent
/// NPC-catalog proof. A published Reaction Formula may use static basePrice because its static
/// reaction activity is itself authoritative eligibility proof; that exception is not available
/// to arbitrary manufacturing blueprints.
pub(crate) fn bpo_reference(
    type_id: u32,
    base_price: Option<f64>,
    overlay: Option<&crate::overlay::OverlayImport>,
    published_reaction_formula: bool,
) -> Option<(f64, PriceSource)> {
    if let Some(book) = overlay.filter(|book| book.is_npc_catalog_type(type_id)) {
        if let Some(price) = book.npc_min_sell(type_id) {
            return Some((price, PriceSource::CcpSnapshotJitaSplit));
        }
        return base_price
            .filter(|price| price.is_finite() && *price > 0.0)
            .map(|price| (price, PriceSource::BasePrice));
    }
    if published_reaction_formula {
        return base_price
            .filter(|price| price.is_finite() && *price > 0.0)
            .map(|price| (price, PriceSource::BasePrice));
    }
    None
}

fn blocking_leaves(cost: &CostReport, type_id: u32) -> Vec<u32> {
    cost.outcomes
        .get(&type_id)
        .map(|outcome| outcome.missing_leaves().iter().copied().collect())
        .unwrap_or_default()
}

fn type_name(static_data: &StaticData, type_id: u32) -> &str {
    static_data
        .item_type(type_id)
        .map(|item| item.name.as_str())
        .unwrap_or("<unknown type>")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::classify::COMMODITY_CATEGORY_ID;
    use crate::config::{JunkSeedConfig, PricingModel};
    use crate::cost::{CapturedLeafPrices, CostInputs, CostMap};
    use crate::manifest::{LocalPrice, PriceEntry, local_prices_from_report};
    use crate::staticdata::{
        BlueprintActivities, BlueprintActivity, BlueprintDefinition, DogmaProjection,
        ItemTypeRecord, MaterialEntry, ProductEntry,
    };

    const CATEGORY_CELESTIAL: u32 = 2;
    const CATEGORY_MATERIAL: u32 = 4;
    const CATEGORY_SHIP: u32 = 6;
    const CATEGORY_MODULE: u32 = 7;

    /// The four permanently unpriceable A∪B types this change exists for, and the leaves
    /// that block each one.
    const UNPRICEABLE: &[(u32, &[u32])] = &[
        (3581, &[3583, 3584]),
        (36902, &[34556, 34557, 34558, 34559, 34560]),
        (47028, &[47020, 47021]),
        (49787, &[49787]),
    ];

    fn item(type_id: u32, category_id: u32, name: &str, base_price: Option<f64>) -> ItemTypeRecord {
        ItemTypeRecord {
            type_id,
            group_id: Some(1),
            category_id: Some(category_id),
            group_name: Some("Test Group".to_string()),
            name: name.to_string(),
            mass: None,
            volume: None,
            capacity: None,
            portion_size: None,
            race_id: None,
            base_price,
            market_group_id: Some(1),
            icon_id: None,
            sound_id: None,
            graphic_id: None,
            radius: None,
            published: true,
        }
    }

    /// An untraded event/exclusive input: no basePrice, and outside the seed universe (no
    /// market group), so it blocks its consumers without being classified itself.
    fn untraded_input(type_id: u32, name: &str) -> ItemTypeRecord {
        let mut record = item(type_id, CATEGORY_MATERIAL, name, None);
        record.market_group_id = None;
        record
    }

    fn manufacturing(
        blueprint_type_id: u32,
        product_type_id: u32,
        per_run: i64,
        materials: &[(u32, i64)],
    ) -> BlueprintDefinition {
        BlueprintDefinition {
            blueprint_type_id,
            blueprint_name: format!("Blueprint {blueprint_type_id}"),
            product_type_id: 0,
            product_name: String::new(),
            max_production_limit: None,
            published: true,
            activities: BlueprintActivities {
                manufacturing: Some(BlueprintActivity {
                    materials: materials
                        .iter()
                        .map(|(type_id, quantity)| MaterialEntry {
                            type_id: *type_id,
                            quantity: *quantity,
                        })
                        .collect(),
                    products: vec![ProductEntry {
                        type_id: product_type_id,
                        quantity: per_run,
                    }],
                    time: Some(100),
                }),
                ..BlueprintActivities::default()
            },
        }
    }

    /// A miniature seed universe with the shapes that matter:
    ///
    /// * 34 Tritanium — bucket B leaf at its basePrice;
    /// * 587 Rifter — bucket A, cost-recursive off Tritanium;
    /// * 4051 fuel block — A∩B, counted once;
    /// * the four [`UNPRICEABLE`] index types;
    /// * 5001 / 5002 — bucket D, one captured and one not.
    struct Fixture {
        data: StaticData,
        classification: Classification,
    }

    impl Fixture {
        fn new() -> Self {
            let mut item_types = vec![
                item(34, CATEGORY_MATERIAL, "Tritanium", Some(2.0)),
                item(587, CATEGORY_SHIP, "Rifter", Some(300_000.0)),
                item(4051, CATEGORY_MATERIAL, "Nitrogen Fuel Block", None),
                item(
                    3581,
                    CATEGORY_MODULE,
                    "Purloined Sansha Data Analyzer",
                    None,
                ),
                item(36902, CATEGORY_MATERIAL, "Coalesced Element", None),
                item(
                    47028,
                    CATEGORY_MODULE,
                    "Neural Lace 'Blackglass' Net Intrusion 920-40",
                    None,
                ),
                item(
                    5001,
                    CATEGORY_MODULE,
                    "Captured Named Module",
                    Some(1_000.0),
                ),
                item(
                    5002,
                    CATEGORY_MODULE,
                    "Uncaptured Named Module",
                    Some(1_000.0),
                ),
            ];
            // 49787 is bucket B: category 2 with a harvestable-gas group name. It carries no
            // basePrice and nothing produces it here, so it blocks on itself — the same
            // outcome the real Triglavian gas reaches through the cost map's cycle guard.
            let mut gas = item(49787, CATEGORY_CELESTIAL, "Hiemal Tricarboxyl Vapor", None);
            gas.group_name = Some("Harvestable Cloud".to_string());
            item_types.push(gas);

            for (type_id, leaves) in UNPRICEABLE {
                for leaf in *leaves {
                    if *leaf == *type_id {
                        continue;
                    }
                    item_types.push(untraded_input(*leaf, &format!("Untraded Input {leaf}")));
                }
            }

            let mut blueprints = vec![
                manufacturing(5871, 587, 1, &[(34, 22_400)]),
                manufacturing(4312, 4051, 40, &[(34, 100)]),
            ];
            for (type_id, leaves) in UNPRICEABLE {
                if leaves.len() == 1 && leaves[0] == *type_id {
                    // Blocked by itself: a price-less leaf with no producer at all.
                    continue;
                }
                blueprints.push(manufacturing(
                    type_id + 1,
                    *type_id,
                    1,
                    &leaves.iter().map(|leaf| (*leaf, 1_i64)).collect::<Vec<_>>(),
                ));
            }

            let data = fixture(item_types, blueprints);
            let classification = Classification::compute(&data, &JunkSeedConfig::default())
                .expect("the fixture classifies");
            Self {
                data,
                classification,
            }
        }

        /// The A∪B cost report, resolved with no captures — exactly what `refresh-prices`
        /// feeds the plan.
        fn cost_report(&self) -> CostReport {
            let captures = CapturedLeafPrices::new();
            let no_twins = crate::cost::OverridePrices::new();
            let no_floors = crate::cost::OverridePrices::new();
            let mut map = CostMap::new(CostInputs {
                data: &self.data,
                producers: &self.classification.producers,
                invention_lineage: &self.classification.invention_lineage,
                captures: &captures,
                model: PricingModel::Recursive,
                leaf_source: crate::config::LeafPriceSource::default(),
                twins: &no_twins,
                floors: &no_floors,
            });
            CostReport::build(&mut map, self.classification.union_ab().iter().copied())
        }

        /// The manifest a healthy `refresh-prices` would leave behind: a local entry for
        /// every A∪B type the cost map priced, plus the one bucket-D capture.
        fn manifest(&self, report: &CostReport) -> PriceManifest {
            let mut manifest =
                PriceManifest::empty(Some(3_396_210), "recursive-v1", "2026-08-10T00:00:00Z");
            for (type_id, local) in local_prices_from_report(report) {
                manifest.prices.insert(type_id, PriceEntry::local(local));
            }
            manifest.prices.insert(
                5001,
                PriceEntry::capture(2_500.0, PriceSource::CcpEsiAverage, "2026-08-01T00:00:00Z"),
            );
            // A stale *local* entry for the uncaptured junk candidate. §2.4 must ignore it:
            // if bucket D had any fallback at all, 5002 would be seeded off this.
            manifest.prices.insert(
                5002,
                PriceEntry::local(LocalPrice {
                    price: 1_000.0,
                    source: PriceSource::BasePrice,
                }),
            );
            manifest
        }
    }

    fn fixture(
        item_types: Vec<ItemTypeRecord>,
        blueprints: Vec<BlueprintDefinition>,
    ) -> StaticData {
        let mut item_types = item_types;
        item_types.sort_by_key(|item| item.type_id);
        let item_type_index = item_types
            .iter()
            .enumerate()
            .map(|(index, item)| (item.type_id, index))
            .collect::<HashMap<_, _>>();
        StaticData {
            dir: PathBuf::from("."),
            stations: Vec::new(),
            station_index: HashMap::new(),
            solar_systems: Vec::new(),
            solar_system_index: HashMap::new(),
            item_types,
            item_type_index,
            blueprints,
            dogma: HashMap::new(),
            compressed_type_ids: BTreeSet::new(),
        }
    }

    fn config(max_unpriced_drops: i64) -> SeederConfig {
        let mut config = SeederConfig::default();
        config.index_seed.max_unpriced_drops = max_unpriced_drops;
        config.validate().expect("the test config is valid");
        config
    }

    const REQUIRED_T2: u32 = 91_001;
    const REQUIRED_OFFICER: u32 = 91_002;
    const REQUIRED_DEADSPACE: u32 = 91_003;
    const OPTIONAL_COMMODITY: u32 = 91_004;
    const OUT_OF_SCOPE_T3: u32 = 91_005;
    const FACTION_TARGET: u32 = 91_006;
    const FACTION_HIGH: u32 = 91_007;
    const FACTION_LOW: u32 = 91_008;
    const DEADSPACE_TARGET: u32 = 91_009;
    const DEADSPACE_SIBLING: u32 = 91_010;
    const OFFICER_SIBLING: u32 = 91_011;

    fn rare_reference_fixture() -> (StaticData, Classification) {
        let mut data = fixture(
            vec![
                item(
                    REQUIRED_T2,
                    CATEGORY_MODULE,
                    "Required Tech II Module",
                    Some(8_000.0),
                ),
                item(
                    REQUIRED_OFFICER,
                    CATEGORY_MODULE,
                    "Required Officer Module",
                    Some(9_000.0),
                ),
                item(
                    REQUIRED_DEADSPACE,
                    CATEGORY_MODULE,
                    "Required Deadspace Module",
                    Some(10_000.0),
                ),
                item(
                    OPTIONAL_COMMODITY,
                    COMMODITY_CATEGORY_ID,
                    "Optional Commodity",
                    Some(1.0),
                ),
                item(
                    OUT_OF_SCOPE_T3,
                    CATEGORY_MODULE,
                    "Out-of-scope Tech III Module",
                    Some(1.0),
                ),
                item(
                    FACTION_TARGET,
                    CATEGORY_MODULE,
                    "Faction Family Target",
                    Some(1.0),
                ),
                item(
                    FACTION_HIGH,
                    CATEGORY_MODULE,
                    "Faction Family High",
                    Some(1.0),
                ),
                item(
                    FACTION_LOW,
                    CATEGORY_MODULE,
                    "Faction Family Low",
                    Some(1.0),
                ),
                item(
                    DEADSPACE_TARGET,
                    CATEGORY_MODULE,
                    "Deadspace Family Target",
                    Some(1.0),
                ),
                item(
                    DEADSPACE_SIBLING,
                    CATEGORY_MODULE,
                    "Deadspace Family Sibling",
                    Some(1.0),
                ),
                item(
                    OFFICER_SIBLING,
                    CATEGORY_MODULE,
                    "Officer Family Sibling",
                    Some(1.0),
                ),
            ],
            Vec::new(),
        );
        data.dogma.insert(
            REQUIRED_T2,
            DogmaProjection {
                tech_level: Some(2.0),
                meta_group_id: Some(2),
            },
        );
        for type_id in [FACTION_TARGET, FACTION_HIGH, FACTION_LOW] {
            data.dogma.insert(
                type_id,
                DogmaProjection {
                    tech_level: None,
                    meta_group_id: Some(4),
                },
            );
        }
        for type_id in [DEADSPACE_TARGET, DEADSPACE_SIBLING] {
            data.dogma.insert(
                type_id,
                DogmaProjection {
                    tech_level: None,
                    meta_group_id: Some(6),
                },
            );
        }
        data.dogma.insert(
            OFFICER_SIBLING,
            DogmaProjection {
                tech_level: None,
                meta_group_id: Some(5),
            },
        );
        data.dogma.insert(
            REQUIRED_OFFICER,
            DogmaProjection {
                tech_level: None,
                meta_group_id: Some(5),
            },
        );
        data.dogma.insert(
            REQUIRED_DEADSPACE,
            DogmaProjection {
                tech_level: None,
                meta_group_id: Some(6),
            },
        );
        data.dogma.insert(
            OUT_OF_SCOPE_T3,
            DogmaProjection {
                tech_level: Some(3.0),
                meta_group_id: None,
            },
        );
        let classification = Classification::compute(&data, &JunkSeedConfig::default())
            .expect("the Rare reference fixture classifies");
        (data, classification)
    }

    fn required_rare_fixture() -> (StaticData, BTreeMap<u32, MarketAssignment>) {
        let (data, classification) = rare_reference_fixture();
        let required = required_external_market_references(
            &classification,
            &data,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("required reference scope resolves");
        (data, required)
    }

    fn unresolved_rare_policy_plan(category_id: u32) -> SeedPlan {
        let type_id = 99_001;
        let mut data = fixture(
            vec![item(
                type_id,
                category_id,
                "Unreferenced Rare policy fixture",
                Some(1.0),
            )],
            Vec::new(),
        );
        data.dogma.insert(
            type_id,
            DogmaProjection {
                tech_level: None,
                meta_group_id: Some(4),
            },
        );
        let classification = Classification::compute(&data, &JunkSeedConfig::default())
            .expect("the policy fixture classifies");
        SeedPlan::resolve_canonical_with_fallbacks(
            &classification,
            &data,
            &CostReport::default(),
            &PriceManifest::empty(None, "test", "t"),
            None,
            &SeederConfig::default(),
            &RareFallbackBook::new(),
        )
        .expect("an isolated unresolved Rare policy fixture must not block planning")
    }

    #[test]
    fn unresolved_skin_is_non_blocking_and_receives_no_synthetic_side() {
        let plan = unresolved_rare_policy_plan(crate::classify::SKIN_CATEGORY_ID);
        assert!(plan.seedable.is_empty());
        assert_eq!(plan.dropped.len(), 1);
        assert_eq!(plan.dropped[0].reason, DropReason::OptionalRareUnreferenced);
    }

    #[test]
    fn unresolved_apparel_is_non_blocking_and_receives_no_synthetic_side() {
        let plan = unresolved_rare_policy_plan(crate::classify::APPAREL_CATEGORY_ID);
        assert!(plan.seedable.is_empty());
        assert_eq!(plan.dropped.len(), 1);
        assert_eq!(plan.dropped[0].reason, DropReason::OptionalRareUnreferenced);
    }

    #[test]
    fn unresolved_faction_blueprint_is_not_promoted_to_rare_buy_liquidity() {
        let plan = unresolved_rare_policy_plan(crate::classify::BLUEPRINT_CATEGORY_ID);
        assert!(plan.seedable.is_empty());
        assert_eq!(plan.dropped.len(), 1);
        assert_eq!(plan.dropped[0].role, Some(MarketRole::Rare));
        assert_eq!(plan.dropped[0].reason, DropReason::OptionalRareBlueprint);
    }

    #[test]
    fn unresolved_required_gameplay_rare_is_an_explicit_unpriced_non_blocking_anomaly() {
        let plan = unresolved_rare_policy_plan(CATEGORY_MODULE);
        assert!(plan.seedable.is_empty());
        assert_eq!(plan.required_rare_anomaly_count(), 1);
        assert_eq!(
            plan.dropped[0].reason,
            DropReason::RequiredRareReferenceUnavailable
        );
        assert_ne!(plan.dropped[0].reason, DropReason::OptionalRareUnreferenced);
    }

    #[test]
    fn required_rare_t2_absent_from_a_primary_book_remains_in_capture_scope() {
        let (_, required) = required_rare_fixture();
        let primary_book_types = BTreeSet::<u32>::new();
        assert!(!primary_book_types.contains(&REQUIRED_T2));
        assert_eq!(required[&REQUIRED_T2].profile, PricingProfile::RareT2);
        assert_eq!(required[&REQUIRED_T2].role, MarketRole::Rare);
    }

    #[test]
    fn required_officer_and_deadspace_absent_from_a_primary_book_remain_in_capture_scope() {
        let (_, required) = required_rare_fixture();
        let primary_book_types = BTreeSet::<u32>::new();
        assert!(!primary_book_types.contains(&REQUIRED_OFFICER));
        assert!(!primary_book_types.contains(&REQUIRED_DEADSPACE));
        assert_eq!(
            required[&REQUIRED_OFFICER].profile,
            PricingProfile::RareOfficer
        );
        assert_eq!(
            required[&REQUIRED_DEADSPACE].profile,
            PricingProfile::RareDeadspaceDed
        );
    }

    #[test]
    fn captured_reference_feeds_the_existing_rare_t2_buy_only_profile() {
        let (data, required) = required_rare_fixture();
        let assignment = required[&REQUIRED_T2];
        let mut manifest = PriceManifest::empty(None, "test", "t");
        manifest.prices.insert(
            REQUIRED_T2,
            PriceEntry::capture(1_000.0, PriceSource::CcpEsiAverage, "t"),
        );
        let (price, source) =
            rare_reference(REQUIRED_T2, &manifest, None, CapturePolicy::default())
                .expect("the approved capture prices the required Rare type");
        let mut plan = SeedPlan::default();
        plan.seedable.insert(
            REQUIRED_T2,
            SeedableType {
                type_id: REQUIRED_T2,
                price,
                source,
                bucket: BucketFlags::default(),
                role: assignment.role,
                profile: assignment.profile,
                sides: SeedSides {
                    ask: assignment.sides.sell,
                    bid: assignment.sides.buy,
                },
            },
        );
        let pricing = crate::build::SeedPricingProfiles::from_config(&SeederConfig::default());
        let rows = crate::build::plan_canonical_seed_rows(&plan, &data, &pricing, 10);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].writes_ask() && rows[0].writes_bid());
        assert_eq!(
            rows[0].bid, 700.0,
            "RareT2 keeps the reviewed x0.70 Buy target"
        );
    }

    #[test]
    fn required_rare_missing_after_every_approved_capture_source_hard_fails() {
        let (data, required) = required_rare_fixture();
        let manifest = PriceManifest::empty(None, "test", "t");
        let error = check_required_external_market_references(
            &required,
            &data,
            &manifest,
            CapturePolicy::default(),
            "Jita snapshot; CCP ESI",
        )
        .expect_err("missing required Rare references must fail closed");
        let message = format!("{error:#}");
        assert!(message.contains(&REQUIRED_T2.to_string()), "{message}");
        assert!(message.contains("Required Tech II Module"), "{message}");
        assert!(message.contains("rare_t2"), "{message}");
        assert!(message.contains("Jita snapshot; CCP ESI"), "{message}");
    }

    #[test]
    fn static_base_price_cannot_satisfy_a_required_rare_reference() {
        let (data, required) = required_rare_fixture();
        assert!(
            data.item_type(REQUIRED_T2)
                .and_then(|item| item.base_price)
                .is_some()
        );
        let mut manifest = PriceManifest::empty(None, "test", "t");
        manifest.prices.insert(
            REQUIRED_T2,
            PriceEntry::local(LocalPrice {
                price: 8_000.0,
                source: PriceSource::BasePrice,
            }),
        );
        assert!(rare_reference(REQUIRED_T2, &manifest, None, CapturePolicy::default()).is_none());
        check_required_external_market_references(
            &required,
            &data,
            &manifest,
            CapturePolicy::default(),
            "approved captures",
        )
        .expect_err("a local basePrice entry is not an external Rare reference");
    }

    #[test]
    fn optional_and_out_of_scope_types_are_not_promoted_to_required_capture_scope() {
        let (_, required) = required_rare_fixture();
        assert!(!required.contains_key(&OPTIONAL_COMMODITY));
        assert!(!required.contains_key(&OUT_OF_SCOPE_T3));
    }

    fn captured_manifest(entries: &[(u32, f64)]) -> PriceManifest {
        let mut manifest = PriceManifest::empty(None, "test", "t");
        for (type_id, price) in entries {
            manifest.prices.insert(
                *type_id,
                PriceEntry::capture(*price, PriceSource::CcpEsiAverage, "t"),
            );
        }
        manifest
    }

    #[test]
    fn exact_approved_reference_always_wins_over_a_family_fallback() {
        let (data, required) = required_rare_fixture();
        let manifest = captured_manifest(&[(FACTION_TARGET, 900.0), (FACTION_LOW, 100.0)]);
        let families = VariantFamilies::fixture(&[(FACTION_TARGET, 10), (FACTION_LOW, 10)]);
        let fallbacks = strict_sibling_family_fallbacks(
            &required,
            &data,
            &families,
            &manifest,
            None,
            CapturePolicy::default(),
        );
        assert!(!fallbacks.contains_key(&FACTION_TARGET));
        assert_eq!(
            rare_reference(FACTION_TARGET, &manifest, None, CapturePolicy::default()),
            Some((900.0, PriceSource::CcpEsiAverage))
        );
    }

    #[test]
    fn faction_fallback_is_same_family_same_profile_and_uses_minimum_reference() {
        let (data, required) = required_rare_fixture();
        let manifest = captured_manifest(&[(FACTION_HIGH, 500.0), (FACTION_LOW, 200.0)]);
        let families = VariantFamilies::fixture(&[
            (FACTION_TARGET, 10),
            (FACTION_HIGH, 10),
            (FACTION_LOW, 10),
        ]);
        let fallbacks = strict_sibling_family_fallbacks(
            &required,
            &data,
            &families,
            &manifest,
            None,
            CapturePolicy::default(),
        );
        let fallback = &fallbacks[&FACTION_TARGET];
        assert_eq!(fallback.price, 200.0);
        assert!(matches!(
            fallback.provenance,
            RareFallbackProvenance::SiblingFamily {
                sibling_type_id: FACTION_LOW,
                eligible_referenced_siblings: 2,
                ..
            }
        ));
    }

    #[test]
    fn required_gameplay_sibling_fallback_produces_normal_buy_only_liquidity() {
        let (data, classification) = rare_reference_fixture();
        let required = required_external_market_references(
            &classification,
            &data,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("required reference scope resolves");
        let manifest = captured_manifest(&[(FACTION_LOW, 100.0)]);
        let families = VariantFamilies::fixture(&[(FACTION_TARGET, 10), (FACTION_LOW, 10)]);
        let fallbacks = strict_sibling_family_fallbacks(
            &required,
            &data,
            &families,
            &manifest,
            None,
            CapturePolicy::default(),
        );
        let plan = SeedPlan::resolve_canonical_with_fallbacks(
            &classification,
            &data,
            &CostReport::default(),
            &manifest,
            None,
            &SeederConfig::default(),
            &fallbacks,
        )
        .expect("the approved sibling fallback resolves normal Rare liquidity");
        let seeded = &plan.seedable[&FACTION_TARGET];
        assert_eq!(seeded.source, PriceSource::SiblingFamilyFallback);
        assert_eq!(seeded.sides, SeedSides::BUY_ONLY);
        let rows = crate::build::plan_canonical_seed_rows(
            &plan,
            &data,
            &crate::build::SeedPricingProfiles::from_config(&SeederConfig::default()),
            10,
        );
        let row = rows
            .iter()
            .find(|row| row.type_id == FACTION_TARGET)
            .unwrap();
        assert!(!row.writes_ask() && row.writes_bid());
        assert_eq!(row.bid, 65.0);
    }

    #[test]
    fn faction_never_borrows_from_officer_deadspace_or_t2_in_the_same_family() {
        let (data, required) = required_rare_fixture();
        let manifest = captured_manifest(&[
            (REQUIRED_OFFICER, 10.0),
            (DEADSPACE_SIBLING, 20.0),
            (REQUIRED_T2, 30.0),
        ]);
        let families = VariantFamilies::fixture(&[
            (FACTION_TARGET, 10),
            (REQUIRED_OFFICER, 10),
            (DEADSPACE_SIBLING, 10),
            (REQUIRED_T2, 10),
        ]);
        let fallbacks = strict_sibling_family_fallbacks(
            &required,
            &data,
            &families,
            &manifest,
            None,
            CapturePolicy::default(),
        );
        assert!(!fallbacks.contains_key(&FACTION_TARGET));
    }

    #[test]
    fn officer_and_deadspace_each_stay_inside_their_own_profile_family() {
        let (data, required) = required_rare_fixture();
        let manifest = captured_manifest(&[(OFFICER_SIBLING, 300.0), (DEADSPACE_SIBLING, 400.0)]);
        let families = VariantFamilies::fixture(&[
            (REQUIRED_OFFICER, 20),
            (OFFICER_SIBLING, 20),
            (DEADSPACE_TARGET, 30),
            (DEADSPACE_SIBLING, 30),
        ]);
        let fallbacks = strict_sibling_family_fallbacks(
            &required,
            &data,
            &families,
            &manifest,
            None,
            CapturePolicy::default(),
        );
        assert_eq!(fallbacks[&REQUIRED_OFFICER].price, 300.0);
        assert_eq!(fallbacks[&DEADSPACE_TARGET].price, 400.0);
    }

    #[test]
    fn same_group_and_similar_name_without_shared_parent_are_never_siblings() {
        let (data, required) = required_rare_fixture();
        let manifest = captured_manifest(&[(FACTION_LOW, 100.0)]);
        let families = VariantFamilies::fixture(&[(FACTION_TARGET, 10), (FACTION_LOW, 11)]);
        let fallbacks = strict_sibling_family_fallbacks(
            &required,
            &data,
            &families,
            &manifest,
            None,
            CapturePolicy::default(),
        );
        assert_eq!(
            data.item_type(FACTION_TARGET).unwrap().group_id,
            data.item_type(FACTION_LOW).unwrap().group_id
        );
        assert!(!fallbacks.contains_key(&FACTION_TARGET));
    }

    #[test]
    fn no_referenced_valid_sibling_is_an_explicit_anomaly_and_base_price_cannot_rescue_it() {
        let (data, required) = required_rare_fixture();
        let manifest = PriceManifest::empty(None, "test", "t");
        let families = VariantFamilies::fixture(&[(FACTION_TARGET, 10), (FACTION_LOW, 10)]);
        let fallbacks = strict_sibling_family_fallbacks(
            &required,
            &data,
            &families,
            &manifest,
            None,
            CapturePolicy::default(),
        );
        assert!(!fallbacks.contains_key(&FACTION_TARGET));
        check_refresh_rare_reference_coverage(
            &required,
            &data,
            &manifest,
            CapturePolicy::default(),
            &fallbacks,
            "exact captures",
        )
        .expect("one isolated required Rare anomaly is reviewable");
    }

    const RENS: u64 = 60_004_588;
    const HEK: u64 = 60_005_686;

    /// The fixture's own seed prices, at the shipped multipliers (ask x3 over a 100 ISK floor,
    /// bid x1, no floor). Named because every deferral test below is an inequality against one
    /// of them, and a bare 134,400 in an assertion says nothing about where it came from.
    const RIFTER_ASK: f64 = 134_400.0;
    const RIFTER_BID: f64 = 44_800.0;
    const TRITANIUM_ASK: f64 = 100.0;
    const TRITANIUM_BID: f64 = 2.0;

    fn pricing() -> SeedPricingPair {
        SeedPricingPair::from_config(&SeederConfig::default())
    }

    /// An overlay import holding exactly the named rows, so the catalog is derived from a real
    /// [`crate::overlay::OverlayImport`] rather than a hand-written quote table: what counts as
    /// "the NPC catalog prices it" has to be the same question the build asks.
    fn overlay_with(rows: &[(u64, u32, bool, f64)]) -> crate::overlay::OverlayImport {
        let mut import = crate::overlay::OverlayImport::default();
        for (station_id, type_id, is_buy, price) in rows.iter().copied() {
            let liquidity = crate::overlay::SeedLiquidity {
                station_id,
                solar_system_id: 30_002_510,
                constellation_id: 20_000_369,
                region_id: 10_000_030,
                type_id,
                price_cents: crate::overlay::price_to_cents(price),
                quantity: 7,
            };
            if is_buy {
                import.buy.insert((station_id, type_id), liquidity);
            } else {
                import.sell.insert((station_id, type_id), liquidity);
            }
        }
        import
    }

    /// Resolves against an overlay built from `rows`, at the default `"conflicting-prices"`.
    fn resolve_against(
        fixture: &Fixture,
        report: &CostReport,
        manifest: &PriceManifest,
        rows: &[(u64, u32, bool, f64)],
    ) -> SeedPlan {
        SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            report,
            manifest,
            &NpcCatalog::from_overlay(&overlay_with(rows)),
            pricing(),
            &config(25),
        )
        .expect("resolves")
    }

    fn sides_of(plan: &SeedPlan, type_id: u32) -> Option<SeedSides> {
        plan.seedable.get(&type_id).map(|entry| entry.sides)
    }

    /// The catalog is the best price on each side across every station — cheapest ask, dearest
    /// bid — because those are the two a player would actually use, and they are what the
    /// deferral must be measured against.
    #[test]
    fn the_catalog_keeps_the_cheapest_ask_and_the_dearest_bid_across_stations() {
        let catalog = NpcCatalog::from_overlay(&overlay_with(&[
            (RENS, 587, false, 9_000.0),
            (HEK, 587, false, 4_000.0),
            (RENS, 587, true, 1_500.0),
            (HEK, 587, true, 7_250.0),
            (RENS, 34, true, 3.0),
        ]));

        let rifter = catalog.quote(587);
        assert_eq!(
            rifter.ask.map(|price| (price.price, price.station_id)),
            Some((4_000.0, HEK))
        );
        assert_eq!(
            rifter.bid.map(|price| (price.price, price.station_id)),
            Some((7_250.0, HEK))
        );

        // One side only is a real state, and the other side stays absent rather than becoming
        // a zero — a 0.00 NPC ask would defer every bid in the game.
        let tritanium = catalog.quote(34);
        assert!(tritanium.ask.is_none());
        assert_eq!(tritanium.bid.map(|price| price.price), Some(3.0));

        // A type the catalog has never heard of quotes nothing at all.
        assert!(catalog.quote(99_999).is_empty());
        assert!(!catalog.contains(99_999));
        assert_eq!(catalog.len(), 2);
    }

    /// **The default rule, side by side.** Our ask is dropped only when TQ bids above it; our
    /// bid only when TQ sells below it. Each side is decided on its own, so a type can lose
    /// one and keep the other.
    #[test]
    fn only_the_side_whose_price_crosses_tqs_is_deferred() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let manifest = fixture.manifest(&report);

        // --- ask-only conflict. TQ buys Tritanium at 500, well over our 100 ISK floored ask:
        // buy from us, haul, sell into the buyback. TQ sells it nowhere, so our 2.00 bid has
        // nothing to cross and stays.
        let plan = resolve_against(&fixture, &report, &manifest, &[(RENS, 34, true, 500.0)]);
        assert_eq!(
            sides_of(&plan, 34),
            Some(SeedSides {
                ask: false,
                bid: true
            }),
            "the ask crossed TQ's bid; the bid did not cross anything"
        );
        assert_eq!(plan.deferred_side_count(DeferredSide::Ask), 1);
        assert_eq!(plan.deferred_side_count(DeferredSide::Bid), 0);
        assert_eq!(
            plan.deferred_count(),
            0,
            "the type is still seeded, on its bid"
        );
        let deferred = plan
            .deferrals_with(DeferredSide::Ask)
            .next()
            .expect("Tritanium's ask is deferred");
        assert_eq!(
            (deferred.type_id, deferred.name.as_str()),
            (34, "Tritanium")
        );
        assert_eq!(
            deferred.ask_conflict.map(|conflict| (
                conflict.ours,
                conflict.theirs,
                conflict.station_id
            )),
            Some((TRITANIUM_ASK, 500.0, RENS)),
            "the deferral names both prices and where TQ's one lives"
        );
        assert!(deferred.bid_conflict.is_none());
        assert_eq!(plan.seedable_ask_count() + 1, plan.seedable_bid_count());

        // --- bid-only conflict. TQ sells a Rifter for 1,000 and buys none: buy from TQ, haul,
        // sell into our 44,800 bid. Our 134,400 ask has no TQ bid to cross.
        let plan = resolve_against(&fixture, &report, &manifest, &[(RENS, 587, false, 1_000.0)]);
        assert_eq!(
            sides_of(&plan, 587),
            Some(SeedSides {
                ask: true,
                bid: false
            })
        );
        assert_eq!(plan.deferred_side_count(DeferredSide::Bid), 1);
        assert_eq!(plan.deferred_side_count(DeferredSide::Ask), 0);
        let deferred = plan
            .deferrals_with(DeferredSide::Bid)
            .next()
            .expect("the Rifter's bid is deferred");
        assert_eq!(
            deferred
                .bid_conflict
                .map(|conflict| (conflict.ours, conflict.theirs)),
            Some((RIFTER_BID, 1_000.0))
        );
        assert!(deferred.ask_conflict.is_none());
        assert_eq!(deferred.bucket_label(), "A");

        // --- neither conflicts, and this is the case the whole change exists for. TQ holds
        // the Rifter on BOTH sides, at prices that sit inside our spread: TQ sells at 200,000
        // (above our 44,800 bid) and buys at 50,000 (below our 134,400 ask). Under the old
        // all-or-nothing rule this type lost both rows for nothing.
        let plan = resolve_against(
            &fixture,
            &report,
            &manifest,
            &[(RENS, 587, false, 200_000.0), (RENS, 587, true, 50_000.0)],
        );
        assert_eq!(sides_of(&plan, 587), Some(SeedSides::BOTH));
        assert_eq!(
            plan.deferral_count(),
            0,
            "a type the catalog merely holds keeps both sides"
        );
        assert!(
            plan.deferral.is_active(),
            "…and it was genuinely compared, not skipped"
        );

        // --- both conflict: TQ bids 1,000,000 and asks 1.00, which crosses us in both
        // directions. Nothing is left to write, so the type is dropped outright.
        let plan = resolve_against(
            &fixture,
            &report,
            &manifest,
            &[(RENS, 587, true, 1_000_000.0), (HEK, 587, false, 1.0)],
        );
        assert!(!plan.seedable.contains_key(&587));
        assert_eq!(plan.deferred_side_count(DeferredSide::Both), 1);
        assert_eq!(plan.deferred_count(), 1);
        assert_eq!(
            plan.dropped_with(DropReason::DeferredToNpcCatalog)
                .map(|drop| drop.type_id)
                .collect::<Vec<_>>(),
            vec![587],
            "a both-sided deferral is still a drop, under the reason it always had"
        );
        let deferred = plan
            .deferrals_with(DeferredSide::Both)
            .next()
            .expect("the Rifter loses both sides");
        assert_eq!(
            deferred
                .ask_conflict
                .map(|conflict| (conflict.ours, conflict.theirs)),
            Some((RIFTER_ASK, 1_000_000.0))
        );
        assert_eq!(
            deferred.bid_conflict.map(|conflict| (
                conflict.ours,
                conflict.theirs,
                conflict.station_id
            )),
            Some((RIFTER_BID, 1.0, HEK))
        );
        assert!(deferred.detail().contains("ask") && deferred.detail().contains("bid"));
    }

    /// A bucket-D type is priced by `[junk_seed]`, and the deferral must use *that* pricing:
    /// asking the index multipliers about a junk row would compare a price nobody writes.
    #[test]
    fn a_junk_types_deferral_is_decided_at_the_junk_multipliers() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let manifest = fixture.manifest(&report);
        // 5001 captures at 2,500, so at x3/x1 its ask is 7,500 and its bid 2,500.
        let mut config = config(25);
        config.junk_seed.sell_multiplier = 10.0;
        let plan = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            // 9,000 is above the x3 ask (7,500) and below the x10 one (25,000).
            &NpcCatalog::from_overlay(&overlay_with(&[(RENS, 5001, true, 9_000.0)])),
            SeedPricingPair::from_config(&config),
            &config,
        )
        .expect("resolves");
        assert_eq!(
            sides_of(&plan, 5001),
            Some(SeedSides::BOTH),
            "at x10 the junk ask is 25,000 and TQ's 9,000 buyback cannot cross it"
        );

        // The same catalog against the shipped x3 junk ask does cross, and the junk pass is
        // not exempt from the rule.
        let plan = resolve_against(&fixture, &report, &manifest, &[(RENS, 5001, true, 9_000.0)]);
        assert_eq!(
            sides_of(&plan, 5001),
            Some(SeedSides {
                ask: false,
                bid: true
            })
        );
        assert_eq!(
            plan.deferrals_with(DeferredSide::Ask)
                .next()
                .expect("the junk ask is deferred")
                .bucket_label(),
            "D"
        );
        assert_eq!(
            plan.junk_drop_count(),
            1,
            "5002 is still an uncaptured-junk drop; a deferral does not absorb it"
        );
    }

    /// **The boundary.** The comparison is the strict, untaxed crossing, not the 8%-tax one
    /// audit check 4 gates on. A loop that pays before tax and not after is still a loop we
    /// refuse to seed a leg of.
    #[test]
    fn a_loop_that_dies_to_the_tax_is_still_deferred() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let manifest = fixture.manifest(&report);

        // Our Tritanium ask is 100.00. A 105.00 NPC buyback is a 5% gross margin and a LOSS
        // after the 8% transaction tax (105 x 0.92 = 96.60), so the audit would report it and
        // not gate on it. It is still deferred.
        let plan = resolve_against(&fixture, &report, &manifest, &[(RENS, 34, true, 105.0)]);
        assert!(
            105.0 * crate::audit::TAX_KEPT_FRACTION < TRITANIUM_ASK,
            "the tax kills this loop"
        );
        assert_eq!(
            sides_of(&plan, 34),
            Some(SeedSides {
                ask: false,
                bid: true
            }),
            "the marginal loop is dropped too — being conservative is the whole point"
        );

        // Exactly equal is NOT a crossing: `our_ask < npc_bid` is strict, and a zero-margin
        // haul is not an exploit.
        let plan = resolve_against(
            &fixture,
            &report,
            &manifest,
            &[(RENS, 34, true, TRITANIUM_ASK)],
        );
        assert_eq!(sides_of(&plan, 34), Some(SeedSides::BOTH));

        // …and the mirror on the bid side: our bid equal to TQ's ask is kept, one cent over
        // TQ's ask is not.
        for (npc_ask, expected_bid) in [
            (TRITANIUM_BID, true),
            (TRITANIUM_BID + 0.01, true),
            (TRITANIUM_BID - 0.01, false),
        ] {
            let plan = resolve_against(&fixture, &report, &manifest, &[(RENS, 34, false, npc_ask)]);
            assert_eq!(
                sides_of(&plan, 34).map(|sides| sides.bid),
                Some(expected_bid),
                "our {TRITANIUM_BID} bid against a {npc_ask} NPC ask"
            );
        }
    }

    /// `"whole-type"` is the original all-or-nothing rule and must stay exactly that: the
    /// prices are never consulted, so a type the catalog merely touches loses both sides.
    #[test]
    fn whole_type_still_drops_both_sides_whatever_the_prices_are() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let manifest = fixture.manifest(&report);
        let mut whole = config(25);
        whole.index_seed.defer_to_npc_catalog = NpcDeferralMode::WholeType;

        // Prices that cross nothing at all — 200,000 sell / 50,000 buy sit inside our spread.
        let catalog = NpcCatalog::from_overlay(&overlay_with(&[
            (RENS, 587, false, 200_000.0),
            (RENS, 587, true, 50_000.0),
        ]));
        let plan = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &catalog,
            pricing(),
            &whole,
        )
        .expect("resolves");
        assert!(!plan.seedable.contains_key(&587));
        assert_eq!(plan.deferred_count(), 1);
        assert_eq!(plan.deferred_side_count(DeferredSide::Both), 1);
        assert_eq!(plan.deferred_side_count(DeferredSide::Ask), 0);
        assert_eq!(plan.deferred_side_count(DeferredSide::Bid), 0);
        assert_eq!(
            plan.deferred_composition(),
            vec![("A", 1), ("A+B", 0), ("B", 0), ("D", 0)]
        );
        let deferred = plan.deferrals.first().expect("the Rifter is deferred");
        assert!(
            deferred.ask_conflict.is_none() && deferred.bid_conflict.is_none(),
            "whole-type never looked at a price, so it has none to report"
        );
        assert!(deferred.detail().contains("holds this type"));
        assert_eq!(
            plan.deferral,
            Deferral::Active {
                mode: NpcDeferralMode::WholeType,
                catalog_types: 1
            },
            "the plan records the mode it ran in, not just that it ran"
        );
        assert!(plan.deferral.label().contains("whole-type"));
        // Everything else is untouched.
        assert!(plan.seedable.contains_key(&34));
        assert!(plan.seedable.contains_key(&4051));
        assert_eq!(plan.index_drop_count(), 4);
    }

    /// The three ways nothing is deferred, and they report differently on purpose: nothing
    /// collided, nothing was compared, and nothing was asked are three different facts.
    #[test]
    fn nothing_is_deferred_without_a_catalog_or_with_the_knob_off() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let manifest = fixture.manifest(&report);
        // Prices that cross us on every side, so only the knob can be keeping them.
        let catalog = NpcCatalog::from_overlay(&overlay_with(&[
            (RENS, 587, false, 1.0),
            (RENS, 587, true, 9_999_999.0),
            (RENS, 34, true, 9_999.0),
            (RENS, 5001, false, 0.5),
        ]));

        // --skip-overlay: no catalog was imported, so there is nothing to defer to.
        let plan = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &NpcCatalog::none(),
            pricing(),
            &config(25),
        )
        .expect("resolves");
        assert_eq!(plan.deferral_count(), 0);
        assert_eq!(plan.deferral, Deferral::NoCatalog);
        assert_eq!(plan.deferral.mode(), None);
        for type_id in [587, 34, 5001] {
            assert_eq!(sides_of(&plan, type_id), Some(SeedSides::BOTH));
        }

        // …and the knob at "off", which is a different state again.
        let mut off = config(25);
        off.index_seed.defer_to_npc_catalog = NpcDeferralMode::Off;
        let plan = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &catalog,
            pricing(),
            &off,
        )
        .expect("resolves");
        assert_eq!(plan.deferral_count(), 0);
        assert_eq!(plan.deferral, Deferral::Disabled);
        for type_id in [587, 34, 5001] {
            assert_eq!(
                sides_of(&plan, type_id),
                Some(SeedSides::BOTH),
                "type {type_id} keeps both sides with the knob off, crossing prices and all"
            );
        }
        assert_ne!(plan.deferral.label(), Deferral::NoCatalog.label());
    }

    /// A deferred type is not an unpriced one, so it must not spend the budget that exists to
    /// catch a missing manifest — even when it happens to be unpriceable as well.
    ///
    /// The "even when" half is `"whole-type"`'s alone: it decides before it asks for a price,
    /// so it can absorb an unpriceable type. `"conflicting-prices"` has nothing to compare
    /// until the manifest has answered, so an unpriceable type there is reported as unpriced —
    /// which is what it is, and it was not going to be seeded either way.
    #[test]
    fn a_deferred_type_never_counts_against_max_unpriced_drops() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let manifest = fixture.manifest(&report);

        // Two priced types crossed on both sides, so both lose everything.
        let crossed = [
            (RENS, 587, true, 9_999_999.0),
            (RENS, 587, false, 1.0),
            (RENS, 4051, true, 9_999_999.0),
            (RENS, 4051, false, 1.0),
        ];
        let plan = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &NpcCatalog::from_overlay(&overlay_with(&crossed)),
            pricing(),
            &config(4),
        )
        .expect("deferrals are outside the unpriced budget");
        assert_eq!(plan.deferred_count(), 2, "587 and 4051 lose both sides");
        assert!(!DropReason::DeferredToNpcCatalog.counts_against_budget());
        assert_eq!(
            plan.index_drop_count(),
            UNPRICEABLE.len(),
            "the unpriceable four are unpriced drops here: there was no price to compare"
        );

        // Add the four unpriceable types to the catalog and switch to "whole-type", which
        // decides before it looks at a price: now they are absorbed as deferrals and a budget
        // of zero still passes.
        let everything = UNPRICEABLE
            .iter()
            .map(|(type_id, _)| (RENS, *type_id, false, 1.0))
            .chain(crossed)
            .collect::<Vec<_>>();
        let mut whole = config(0);
        whole.index_seed.defer_to_npc_catalog = NpcDeferralMode::WholeType;
        let plan = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &NpcCatalog::from_overlay(&overlay_with(&everything)),
            pricing(),
            &whole,
        )
        .expect("whole-type absorbs them, even at a budget of zero");
        assert_eq!(plan.index_drop_count(), 0);
        assert_eq!(plan.deferred_count(), UNPRICEABLE.len() + 2);
        assert!(
            plan.dropped_with(DropReason::DeferredToNpcCatalog)
                .any(|drop| drop.type_id == 3581),
            "a type that is both unpriceable and in the catalog is reported once, as deferred"
        );
        assert_eq!(
            plan.seedable_count() + plan.dropped.len(),
            fixture.classification.seeded_types().len(),
            "every type still lands in exactly one outcome"
        );
    }

    /// The plan's sides have to survive into the rows the writer sees, at the prices the
    /// deferral was decided against — otherwise the whole argument is about numbers nobody
    /// writes.
    #[test]
    fn a_deferred_side_reaches_the_row_set_as_a_row_that_is_not_written() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let manifest = fixture.manifest(&report);
        let plan = resolve_against(&fixture, &report, &manifest, &[(RENS, 34, true, 500.0)]);

        let rows = crate::build::plan_seed_rows(&plan, &fixture.data, pricing(), 1_000, 1_000);
        let tritanium = rows
            .iter()
            .find(|row| row.type_id == 34)
            .expect("Tritanium is still a planned row");
        assert!(!tritanium.writes_ask() && tritanium.writes_bid());
        assert_eq!(tritanium.row_count(), 1);
        assert_eq!(
            tritanium.ask, TRITANIUM_ASK,
            "the deferred price is still computed, and it is the one the deferral judged"
        );
        assert_eq!(tritanium.bid, TRITANIUM_BID);

        let (planned_sell, planned_buy) = crate::build::planned_side_counts(&rows);
        assert_eq!(planned_sell + 1, planned_buy);
        assert_eq!(planned_buy, rows.len());
        crate::build::verify_seed_rows(&rows, plan.seedable_count(), 1_000, 1_000)
            .expect("a one-sided row is a legal plan");
    }

    #[test]
    fn the_four_permanently_unpriceable_types_are_dropped_and_named_not_fatal() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let manifest = fixture.manifest(&report);

        let plan = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &NpcCatalog::none(),
            pricing(),
            &config(25),
        )
        .expect("four permanent drops are under the default budget of 25");

        assert_eq!(plan.index_drop_count(), 4);
        let dropped = plan
            .dropped_with(DropReason::UnpricedIndexType)
            .map(|drop| (drop.type_id, drop.blocking_leaves.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            dropped,
            UNPRICEABLE
                .iter()
                .map(|(type_id, leaves)| (*type_id, leaves.to_vec()))
                .collect::<Vec<_>>(),
            "every drop names the leaves that blocked it"
        );

        // Named, bucketed, and out of the seedable set.
        for drop in plan.dropped_with(DropReason::UnpricedIndexType) {
            assert!(
                !drop.name.is_empty() && drop.name != "<unknown type>",
                "{drop:?}"
            );
            assert!(drop.bucket.is_classified(), "{drop:?}");
            assert!(!plan.seedable.contains_key(&drop.type_id));
        }
        assert_eq!(
            plan.dropped_with(DropReason::UnpricedIndexType)
                .find(|drop| drop.type_id == 49787)
                .map(DroppedType::bucket_label),
            Some("B"),
            "the Triglavian gas is a bucket-B type blocked by itself"
        );

        // …and the types that do have prices are still seeded.
        assert!(plan.seedable.contains_key(&587));
        assert!(plan.seedable.contains_key(&34));
        assert!(plan.seedable.contains_key(&4051));
        assert_eq!(plan.seedable[&34].source, PriceSource::BasePrice);
        assert_eq!(plan.seedable[&587].price, 44_800.0);
    }

    #[test]
    fn a_drop_budget_below_the_real_drop_count_fails_naming_the_count_and_the_knob() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let manifest = fixture.manifest(&report);

        let error = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &NpcCatalog::none(),
            pricing(),
            &config(3),
        )
        .expect_err("four drops must exceed a budget of three");
        let message = format!("{error:#}");
        assert!(message.contains('4'), "the count must be named: {message}");
        assert!(
            message.contains("max_unpriced_drops = 3"),
            "the knob and its value must be named: {message}"
        );
        assert!(
            message.contains("refresh-prices"),
            "the operator must be told the remedy: {message}"
        );
        assert!(
            message.contains("Purloined Sansha Data Analyzer"),
            "the first drops must be named: {message}"
        );

        // The systemic case the knob exists for. This fixture holds only 7 A∪B types, so
        // the budget is scaled to it: 6 tolerates the 4 permanent drops, and a missing
        // manifest — which drops all 7 — still fails loudly instead of seeding almost
        // nothing. On real data the same contrast is 4 drops against 3,017.
        assert_eq!(fixture.classification.union_ab().len(), 7);
        SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &NpcCatalog::none(),
            pricing(),
            &config(6),
        )
        .expect("the four permanent drops still fit under a budget of six");

        let empty = PriceManifest::empty(Some(3_396_210), "recursive-v1", "t");
        let error = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &empty,
            &NpcCatalog::none(),
            pricing(),
            &config(6),
        )
        .expect_err("an empty manifest must fail loudly, not seed almost nothing");
        assert!(format!("{error:#}").contains('7'), "{error:#}");
    }

    #[test]
    fn uncaptured_junk_is_dropped_routinely_and_never_counts_against_the_index_budget() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let mut manifest = fixture.manifest(&report);
        // Price the four permanent drops so the only remaining drop is the junk one, and
        // set the budget to zero: a D drop must still not fail the run.
        for (type_id, _) in UNPRICEABLE {
            manifest.prices.insert(
                *type_id,
                PriceEntry::capture(9.0, PriceSource::CcpEsiAverage, "2026-08-01T00:00:00Z"),
            );
        }

        let plan = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &NpcCatalog::none(),
            pricing(),
            &config(0),
        )
        .expect("junk drops are routine and outside the budget");

        assert_eq!(plan.index_drop_count(), 0);
        assert_eq!(plan.junk_drop_count(), 1);
        let junk = plan
            .dropped_with(DropReason::UncapturedJunk)
            .next()
            .expect("5002 is dropped");
        assert_eq!(junk.type_id, 5002);
        assert_eq!(junk.name, "Uncaptured Named Module");
        assert_eq!(junk.bucket_label(), "D");
        assert!(junk.blocking_leaves.is_empty());
        assert!(!DropReason::UncapturedJunk.counts_against_budget());

        // 5002 has both a basePrice and a stale local manifest entry, and is still dropped:
        // bucket D is captured-price-or-nothing.
        assert!(!plan.seedable.contains_key(&5002));
        assert!(plan.seedable.contains_key(&5001));
        assert_eq!(plan.seedable[&5001].source, PriceSource::CcpEsiAverage);
        assert_eq!(plan.seedable_junk_count(), 1);
    }

    #[test]
    fn a_zero_or_nan_manifest_price_is_an_error_naming_the_type() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();

        for bad in [0.0_f64, f64::NAN, -1.0, f64::INFINITY] {
            let mut manifest = fixture.manifest(&report);
            manifest.prices.insert(
                587,
                PriceEntry::local(LocalPrice {
                    price: bad,
                    source: PriceSource::CostRecursive,
                }),
            );
            let error = SeedPlan::resolve(
                &fixture.classification,
                &fixture.data,
                &report,
                &manifest,
                &NpcCatalog::none(),
                pricing(),
                &config(25),
            )
            .expect_err("a degenerate price must not reach the seed writer");
            let message = format!("{error:#}");
            assert!(
                message.contains("587"),
                "{bad} must name the type: {message}"
            );
        }

        // A captured zero on the bucket-D side is caught by the same invariant.
        let mut manifest = fixture.manifest(&report);
        manifest.prices.insert(
            5001,
            PriceEntry::capture(0.0, PriceSource::CcpEsiAverage, "2026-08-01T00:00:00Z"),
        );
        let error = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &NpcCatalog::none(),
            pricing(),
            &config(25),
        )
        .expect_err("a captured zero must not seed a free item");
        assert!(format!("{error:#}").contains("5001"));
    }

    #[test]
    fn every_unpriced_required_non_core_role_is_a_hard_canonical_failure() {
        for (role, profile) in [
            (MarketRole::Bpo, PricingProfile::Bpo),
            (MarketRole::Bridge, PricingProfile::Bridge),
            (MarketRole::ProgressionNpc, PricingProfile::ProgressionNpc),
            (MarketRole::Rare, PricingProfile::RareT2),
            (MarketRole::Skillbook, PricingProfile::Skillbook),
            (MarketRole::CommandCenter, PricingProfile::CommandCenter),
        ] {
            let plan = SeedPlan {
                dropped: vec![DroppedType {
                    type_id: 90_000 + role as u32,
                    name: format!("Missing {}", role.key()),
                    bucket: BucketFlags::default(),
                    reason: DropReason::UnpricedRoleReference,
                    role: Some(role),
                    profile: Some(profile),
                    blocking_leaves: Vec::new(),
                }],
                ..SeedPlan::default()
            };
            let error = plan
                .check_required_role_references()
                .expect_err("required non-Core supply/liquidity may not disappear");
            let message = format!("{error:#}");
            assert!(message.contains(role.key()), "{message}");
            assert!(message.contains(profile.key()), "{message}");
            assert!(message.contains("max_unpriced_drops"), "{message}");
        }
    }

    #[test]
    fn progression_npc_uses_exact_npc_sell_is_sell_only_and_missing_proof_fails() {
        const TYPE_ID: u32 = 56_201;
        const GARBAGE: u32 = 41;
        let mut core = item(
            TYPE_ID,
            CATEGORY_MATERIAL,
            "Astrahus Upwell Quantum Core",
            Some(1.0),
        );
        core.group_id = Some(4_086);
        let garbage = item(GARBAGE, COMMODITY_CATEGORY_ID, "Garbage", Some(999.0));
        let data = fixture(vec![core, garbage], Vec::new());
        let classification = Classification::compute(&data, &JunkSeedConfig::default())
            .expect("progression NPC fixture classifies");
        let config = SeederConfig::default();
        let mut overlay = crate::overlay::OverlayImport::default();
        overlay.npc_min_sell_cents.insert(TYPE_ID, 60_000_000_000);
        overlay.npc_min_sell_cents.insert(GARBAGE, 1_400);

        let plan = SeedPlan::resolve_canonical_with_fallbacks(
            &classification,
            &data,
            &CostReport::default(),
            &PriceManifest::empty(None, "test", "t"),
            Some(&overlay),
            &config,
            &RareFallbackBook::new(),
        )
        .expect("exact NPC proof supplies required progression goods");
        let seeded = &plan.seedable[&TYPE_ID];
        assert_eq!(seeded.role, MarketRole::ProgressionNpc);
        assert_eq!(seeded.profile, PricingProfile::ProgressionNpc);
        assert_eq!(seeded.source, PriceSource::NpcReference);
        assert_eq!(seeded.price, 600_000_000.0);
        assert_eq!(seeded.sides, SeedSides::SELL_ONLY);
        let garbage = &plan.seedable[&GARBAGE];
        assert_eq!(garbage.price, 14.0);
        assert_eq!(garbage.source, PriceSource::NpcReference);
        assert_eq!(garbage.sides, SeedSides::SELL_ONLY);

        let rows = crate::build::plan_canonical_seed_rows(
            &plan,
            &data,
            &crate::build::SeedPricingProfiles::from_config(&config),
            10,
        );
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(row.writes_ask());
            assert!(!row.writes_bid());
        }
        assert_eq!(
            rows.iter().find(|row| row.type_id == TYPE_ID).unwrap().ask,
            600_000_000.0
        );
        assert_eq!(
            rows.iter().find(|row| row.type_id == GARBAGE).unwrap().ask,
            14.0,
            "ProgressionNpc has no BPO-style 1,000 ISK floor"
        );

        let mut unproven_overlay = crate::overlay::OverlayImport::default();
        unproven_overlay
            .reference_book
            .min_ask_cents
            .insert(TYPE_ID, 70_000_000_000);
        unproven_overlay
            .reference_book
            .min_ask_cents
            .insert(GARBAGE, 2_000);
        let manifest = captured_manifest(&[(TYPE_ID, 700_000_000.0), (GARBAGE, 20.0)]);
        let error = SeedPlan::resolve_canonical_with_fallbacks(
            &classification,
            &data,
            &CostReport::default(),
            &manifest,
            Some(&unproven_overlay),
            &config,
            &RareFallbackBook::new(),
        )
        .expect_err("Jita, manifest, and static prices cannot replace exact NPC proof");
        let message = format!("{error:#}");
        assert!(message.contains("progression-npc"), "{message}");
        assert!(message.contains("no approved reference price"), "{message}");
    }

    #[test]
    fn bpo_static_fallback_requires_independent_npc_catalog_proof() {
        const TYPE_ID: u32 = 90_100;
        let mut overlay = crate::overlay::OverlayImport::default();

        assert_eq!(
            bpo_reference(TYPE_ID, Some(42_000.0), Some(&overlay), false),
            None
        );

        overlay.npc_catalog_type_ids.insert(TYPE_ID);
        assert_eq!(
            bpo_reference(TYPE_ID, Some(42_000.0), Some(&overlay), false),
            Some((42_000.0, PriceSource::BasePrice))
        );

        overlay.npc_min_sell_cents.insert(TYPE_ID, 1_234_500);
        assert_eq!(
            bpo_reference(TYPE_ID, Some(42_000.0), Some(&overlay), false),
            Some((12_345.0, PriceSource::CcpSnapshotJitaSplit))
        );

        let no_catalog = crate::overlay::OverlayImport::default();
        assert_eq!(
            bpo_reference(TYPE_ID, Some(42_000.0), Some(&no_catalog), true),
            Some((42_000.0, PriceSource::BasePrice)),
            "published Reaction Formulas alone may use static basePrice"
        );
    }

    #[test]
    fn npc_skillbook_reference_wins_over_every_lower_pricing_rung() {
        const TYPE_ID: u32 = 90_200;
        let mut overlay = crate::overlay::OverlayImport::default();
        overlay.npc_min_sell_cents.insert(TYPE_ID, 12_345_600);
        overlay
            .reference_book
            .min_ask_cents
            .insert(TYPE_ID, 99_000_000);
        let manifest = captured_manifest(&[(TYPE_ID, 777_000.0)]);
        assert_eq!(
            skillbook_reference(
                TYPE_ID,
                Some(42_000.0),
                &manifest,
                Some(&overlay),
                CapturePolicy::default(),
            ),
            (123_456.0, PriceSource::NpcReference)
        );
    }

    #[test]
    fn exact_external_skillbook_reference_is_used_without_an_npc_sell() {
        const TYPE_ID: u32 = 90_201;
        let mut overlay = crate::overlay::OverlayImport::default();
        overlay
            .reference_book
            .min_ask_cents
            .insert(TYPE_ID, 77_700_000);
        assert_eq!(
            skillbook_reference(
                TYPE_ID,
                Some(42_000.0),
                &PriceManifest::empty(None, "test", "t"),
                Some(&overlay),
                CapturePolicy::default(),
            ),
            (777_000.0, PriceSource::CcpSnapshotJitaSplit)
        );
    }

    #[test]
    fn static_skillbook_base_price_is_used_without_exact_market_references() {
        assert_eq!(
            skillbook_reference(
                90_202,
                Some(42_000.0),
                &PriceManifest::empty(None, "test", "t"),
                None,
                CapturePolicy::default(),
            ),
            (42_000.0, PriceSource::BasePrice)
        );
    }

    #[test]
    fn missing_skillbook_references_use_exactly_five_million_isk() {
        assert_eq!(
            skillbook_reference(
                90_203,
                None,
                &PriceManifest::empty(None, "test", "t"),
                None,
                CapturePolicy::default(),
            ),
            (5_000_000.0, PriceSource::SkillbookFixedFallback)
        );
    }

    #[test]
    fn fixed_fallback_skillbook_is_sell_only_has_no_buy_and_does_not_block() {
        const TYPE_ID: u32 = 90_204;
        let data = fixture(
            vec![item(
                TYPE_ID,
                crate::classify::SKILL_CATEGORY_ID,
                "Fallback Skillbook",
                None,
            )],
            Vec::new(),
        );
        let classification = Classification::compute(&data, &JunkSeedConfig::default())
            .expect("the Skillbook fixture classifies");
        let config = SeederConfig::default();
        let plan = SeedPlan::resolve_canonical_with_fallbacks(
            &classification,
            &data,
            &CostReport::default(),
            &PriceManifest::empty(None, "test", "t"),
            None,
            &config,
            &RareFallbackBook::new(),
        )
        .expect("missing Skillbook references are covered by approved fixed supply");
        assert!(plan.dropped.is_empty());
        let seeded = &plan.seedable[&TYPE_ID];
        assert_eq!(seeded.price, 5_000_000.0);
        assert_eq!(seeded.source, PriceSource::SkillbookFixedFallback);
        assert_eq!(seeded.sides, SeedSides::SELL_ONLY);
        let rows = crate::build::plan_canonical_seed_rows(
            &plan,
            &data,
            &crate::build::SeedPricingProfiles::from_config(&config),
            10,
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].writes_ask());
        assert!(!rows[0].writes_bid());
        assert_eq!(rows[0].ask, 5_000_000.0);
    }

    #[test]
    fn canonical_resolution_uses_lp_blueprint_product_identity_and_exact_reference() {
        const PRODUCT: u32 = 72_811;
        const BLUEPRINT: u32 = 72_923;
        let mut blueprint_item = item(BLUEPRINT, 9, "Cyclone Fleet Issue Blueprint", None);
        blueprint_item.market_group_id = None;
        let data = fixture(
            vec![
                item(PRODUCT, CATEGORY_SHIP, "Cyclone Fleet Issue", None),
                blueprint_item,
                item(34, CATEGORY_MATERIAL, "Tritanium", Some(2.0)),
            ],
            vec![manufacturing(BLUEPRINT, PRODUCT, 1, &[(34, 100)])],
        );
        let classification = Classification::compute(&data, &JunkSeedConfig::default())
            .expect("LP product fixture classifies");
        let path = std::env::temp_dir().join(format!(
            "market-seederv3-lp-product-{}.json",
            std::process::id()
        ));
        fs::write(
            &path,
            format!(r#"{{"offersByCorpID":{{"1":[{{"type_id":{BLUEPRINT}}}]}}}}"#),
        )
        .expect("write LP fixture");
        let mut config = SeederConfig::default();
        config.market_roles.lp_offer_catalog_path = path.clone();
        let manifest = captured_manifest(&[(PRODUCT, 100_000_000.0)]);
        let plan = SeedPlan::resolve_canonical_with_fallbacks(
            &classification,
            &data,
            &CostReport::default(),
            &manifest,
            None,
            &config,
            &RareFallbackBook::new(),
        )
        .expect("LP BPC product resolves from its exact external reference");
        let _ = fs::remove_file(path);

        let seeded = &plan.seedable[&PRODUCT];
        assert_eq!(seeded.role, MarketRole::Rare);
        assert_eq!(seeded.profile, PricingProfile::RareFactionLp);
        assert_eq!(seeded.price, 100_000_000.0);
        assert_eq!(seeded.source, PriceSource::CcpEsiAverage);
        assert_eq!(seeded.sides, SeedSides::BUY_ONLY);
    }

    #[test]
    fn placeholder_reward_hulls_require_exact_external_references_and_never_sell() {
        let cases = [
            (3_756, 33_327, "Gnosis", 80_000_000.0),
            (42_685, 44_106, "Sunesis", 20_000_000.0),
            (47_466, 47_716, "Praxis", 150_000_000.0),
            (77_114, 79_214, "Metamorphosis", 60_000_000.0),
        ];
        let mut items = vec![item(34, CATEGORY_MATERIAL, "Tritanium", Some(3.78))];
        let mut blueprints = Vec::new();
        for (product, blueprint_id, name, _) in cases {
            items.push(item(product, CATEGORY_SHIP, name, None));
            let mut blueprint_item = item(blueprint_id, 9, "Reward Blueprint", None);
            blueprint_item.market_group_id = None;
            items.push(blueprint_item);
            blueprints.push(manufacturing(blueprint_id, product, 1, &[(34, 1)]));
        }
        let data = fixture(items, blueprints);
        let classification = Classification::compute(&data, &JunkSeedConfig::default())
            .expect("placeholder reward fixture classifies");
        let manifest = captured_manifest(
            &cases
                .iter()
                .map(|(product, _, _, reference)| (*product, *reference))
                .collect::<Vec<_>>(),
        );
        let config = SeederConfig::default();
        let plan = SeedPlan::resolve_canonical_with_fallbacks(
            &classification,
            &data,
            &CostReport::default(),
            &manifest,
            None,
            &config,
            &RareFallbackBook::new(),
        )
        .expect("exact reward references resolve");
        for (product, _, _, reference) in cases {
            let seeded = &plan.seedable[&product];
            assert_eq!(seeded.role, MarketRole::Rare);
            assert_eq!(seeded.profile, PricingProfile::RareFactionLp);
            assert_eq!(seeded.sides, SeedSides::BUY_ONLY);
            assert_eq!(seeded.price, reference);
            assert_ne!(seeded.price, 3.78);
        }

        let mut forbidden_fallbacks = RareFallbackBook::new();
        forbidden_fallbacks.insert(
            cases[0].0,
            RareFallbackAnchor {
                price: 999_000_000.0,
                source: PriceSource::SiblingFamilyFallback,
                provenance: RareFallbackProvenance::SiblingFamily {
                    variation_parent_type_id: 1,
                    sibling_type_id: 2,
                    sibling_name: "Forbidden fuzzy reward fallback".to_string(),
                    sibling_reference: 999_000_000.0,
                    sibling_external_source: PriceSource::CcpEsiAverage,
                    eligible_referenced_siblings: 1,
                },
            },
        );
        let missing = SeedPlan::resolve_canonical_with_fallbacks(
            &classification,
            &data,
            &CostReport::default(),
            &PriceManifest::empty(None, "test", "t"),
            None,
            &config,
            &forbidden_fallbacks,
        )
        .expect("missing exact reward references remain visible, unseeded anomalies");
        assert!(missing.seedable.is_empty());
        let reward_drops = missing
            .dropped
            .iter()
            .filter(|drop| cases.iter().any(|(type_id, ..)| *type_id == drop.type_id))
            .collect::<Vec<_>>();
        assert_eq!(reward_drops.len(), cases.len());
        assert!(reward_drops.iter().all(|drop| {
            drop.reason == DropReason::RequiredRareReferenceUnavailable
                && drop.role == Some(MarketRole::Rare)
                && drop.profile == Some(PricingProfile::RareFactionLp)
        }));
    }

    #[test]
    fn authoritative_moon_asteroid_group_is_canonical_bridge_sell_only() {
        const TYPE_ID: u32 = 90_205;
        let mut moon = item(TYPE_ID, 25, "Moon Resource", Some(1_000.0));
        moon.group_id = Some(1920);
        moon.group_name = Some("Common Moon Asteroids".to_string());
        let data = fixture(vec![moon], Vec::new());
        let classification = Classification::compute(&data, &JunkSeedConfig::default())
            .expect("the moon resource fixture classifies");
        let manifest = captured_manifest(&[(TYPE_ID, 1_000.0)]);
        let mut config = SeederConfig::default();
        config.market_roles.moon_mining_available = false;
        let plan = SeedPlan::resolve_canonical_with_fallbacks(
            &classification,
            &data,
            &CostReport::default(),
            &manifest,
            None,
            &config,
            &RareFallbackBook::new(),
        )
        .expect("Moon-unavailable supply resolves through Bridge policy");
        let seeded = &plan.seedable[&TYPE_ID];
        assert_eq!(seeded.role, MarketRole::Bridge);
        assert_eq!(seeded.profile, PricingProfile::Bridge);
        assert_eq!(seeded.sides, SeedSides::SELL_ONLY);
        let rows = crate::build::plan_canonical_seed_rows(
            &plan,
            &data,
            &crate::build::SeedPricingProfiles::from_config(&config),
            10,
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].writes_ask());
        assert!(!rows[0].writes_bid());
    }

    #[test]
    fn exact_loot_inputs_are_bridge_sell_only_without_granting_blueprint_access() {
        const SPATIAL: u32 = 33_195;
        const SPATIAL_BLUEPRINT: u32 = 33_196;
        const SIGNALLER: u32 = 57_450;
        let spatial = item(
            SPATIAL,
            COMMODITY_CATEGORY_ID,
            "Spatial Attunement Unit",
            None,
        );
        let signaller = item(
            SIGNALLER,
            COMMODITY_CATEGORY_ID,
            "Electro-Neural Signaller",
            None,
        );
        let mut blueprint = item(
            SPATIAL_BLUEPRINT,
            9,
            "Spatial Attunement Unit Blueprint",
            Some(5_000_000.0),
        );
        blueprint.market_group_id = None;
        let data = fixture(vec![spatial, signaller, blueprint], Vec::new());
        let classification = Classification::compute(&data, &JunkSeedConfig::default())
            .expect("loot-input fixture classifies");
        let manifest = captured_manifest(&[(SPATIAL, 14_000.0), (SIGNALLER, 133_148_260.21)]);
        let config = SeederConfig::default();
        let plan = SeedPlan::resolve_canonical_with_fallbacks(
            &classification,
            &data,
            &CostReport::default(),
            &manifest,
            None,
            &config,
            &RareFallbackBook::new(),
        )
        .expect("exact captured references fund both Bridge inputs");

        for type_id in [SPATIAL, SIGNALLER] {
            let seeded = &plan.seedable[&type_id];
            assert_eq!(seeded.role, MarketRole::Bridge);
            assert_eq!(seeded.profile, PricingProfile::Bridge);
            assert_eq!(seeded.sides, SeedSides::SELL_ONLY);
            assert!(seeded.source.is_market_price());
        }
        assert!(!plan.seedable.contains_key(&SPATIAL_BLUEPRINT));
    }

    #[test]
    fn planetary_industry_uses_exact_bridge_reference_and_narrow_buy_toggle() {
        const WATER: u32 = 3_648;
        let mut water = item(WATER, 43, "Water", None);
        water.group_id = Some(1_034);
        let data = fixture(vec![water], Vec::new());
        let classification = Classification::compute(&data, &JunkSeedConfig::default())
            .expect("PI fixture classifies");
        let manifest = captured_manifest(&[(WATER, 100.0)]);

        for (enabled, expected_sides) in [(true, SeedSides::BOTH), (false, SeedSides::SELL_ONLY)] {
            let mut config = SeederConfig::default();
            config.market_roles.planetary_industry_buy_enabled = enabled;
            let plan = SeedPlan::resolve_canonical_with_fallbacks(
                &classification,
                &data,
                &CostReport::default(),
                &manifest,
                None,
                &config,
                &RareFallbackBook::new(),
            )
            .expect("exact PI reference resolves");
            let seeded = &plan.seedable[&WATER];
            assert_eq!(seeded.role, MarketRole::Bridge);
            assert_eq!(seeded.profile, PricingProfile::PlanetaryIndustry);
            assert_eq!(seeded.sides, expected_sides);
            assert_eq!(seeded.price, 100.0);
            assert!(seeded.source.is_market_price());
            let rows = crate::build::plan_canonical_seed_rows(
                &plan,
                &data,
                &crate::build::SeedPricingProfiles::from_config(&config),
                10,
            );
            assert_eq!(rows[0].ask, 102.0);
            assert_eq!(rows[0].bid, 65.0);
            assert!(rows[0].ask > rows[0].bid);
            assert_eq!(rows[0].writes_bid(), enabled);
        }
    }

    #[test]
    fn funded_production_intermediate_anchor_is_buy_sell_and_profile_priced() {
        const TYPE_ID: u32 = 11_539;
        let mut intermediate = item(
            TYPE_ID,
            COMMODITY_CATEGORY_ID,
            "Nanoelectrical Microprocessor",
            None,
        );
        intermediate.group_id = Some(334);
        let data = fixture(vec![intermediate], Vec::new());
        let classification = Classification::compute(&data, &JunkSeedConfig::default())
            .expect("intermediate fixture classifies");
        let config = SeederConfig::default();
        let prices = ProductionIntermediatePriceBook::from([(TYPE_ID, 800.0)]);
        let plan = SeedPlan::resolve_canonical_with_supply_ladder(
            &classification,
            &data,
            &CostReport::default(),
            &PriceManifest::empty(None, "test", "2026-08-29T00:00:00Z"),
            None,
            &config,
            &RareFallbackBook::new(),
            &prices,
        )
        .expect("funded intermediate anchor resolves");
        let seeded = &plan.seedable[&TYPE_ID];
        assert_eq!(seeded.role, MarketRole::ProductionIntermediate);
        assert_eq!(seeded.profile, PricingProfile::ProductionIntermediate);
        assert_eq!(seeded.sides, SeedSides::BOTH);
        assert_eq!(seeded.source, PriceSource::FundedProductionIntermediate);
        let rows = crate::build::plan_canonical_seed_rows(
            &plan,
            &data,
            &crate::build::SeedPricingProfiles::from_config(&config),
            10,
        );
        assert_eq!(rows[0].ask, 1_000.0);
        assert_eq!(rows[0].bid, 760.0);
        assert!(rows[0].writes_bid());
    }

    #[test]
    fn the_seedable_set_is_exactly_a_union_b_union_d_minus_the_drops_each_type_once() {
        let fixture = Fixture::new();
        let report = fixture.cost_report();
        let manifest = fixture.manifest(&report);
        let plan = SeedPlan::resolve(
            &fixture.classification,
            &fixture.data,
            &report,
            &manifest,
            &NpcCatalog::none(),
            pricing(),
            &config(25),
        )
        .expect("resolves");

        let seeded = fixture.classification.seeded_types();
        let dropped = plan
            .dropped
            .iter()
            .map(|drop| drop.type_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            dropped.len(),
            plan.dropped.len(),
            "no type is dropped twice"
        );
        let expected = seeded
            .iter()
            .copied()
            .filter(|type_id| !dropped.contains(type_id))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            plan.seedable.keys().copied().collect::<BTreeSet<_>>(),
            expected
        );
        assert_eq!(plan.seedable_count() + plan.dropped.len(), seeded.len());

        // 4051 is in both A and B, and appears exactly once.
        assert!(fixture.classification.overlap_ab().contains(&4051));
        assert_eq!(plan.seedable[&4051].bucket_label(), "A+B");
        assert_eq!(
            plan.seedable_composition()
                .iter()
                .map(|(_, n)| n)
                .sum::<usize>(),
            plan.seedable_count()
        );
        assert_eq!(
            plan.seedable_index_count() + plan.seedable_junk_count(),
            plan.seedable_count()
        );

        // Junk seeding off means bucket D contributes nothing at all — not even drops.
        let off = Classification::compute(
            &fixture.data,
            &JunkSeedConfig {
                enabled: false,
                ..JunkSeedConfig::default()
            },
        )
        .expect("classifies");
        let plan = SeedPlan::resolve(
            &off,
            &fixture.data,
            &report,
            &manifest,
            &NpcCatalog::none(),
            pricing(),
            &config(25),
        )
        .expect("resolves");
        assert_eq!(plan.junk_drop_count(), 0);
        assert_eq!(plan.seedable_junk_count(), 0);
        assert!(!plan.seedable.contains_key(&5001));
    }
}
