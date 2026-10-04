//! The cross-bucket pump audit — plan §8.3, and the §9 checklist item that gates a real seed.
//!
//! The seed both buys and sells almost everything at Jita 4-4, at `ask = 3 x cost` and
//! `bid = 1 x cost`. That is safe **only while every way of turning one seeded type into
//! another loses money**. Two price bases coexist (SDE `basePrice` for leaves, captured TQ
//! market prices for everything else), and every seam between them is a place where a loop
//! can come out positive. Two were found by hand:
//!
//! * floored **bids** put Tritanium's bid 50x above its cost, making buy-Venture → reprocess
//!   → sell-minerals a 3.52x pump (fixed in piece 5 by making the floor ask-only);
//! * *Batch Compressed Mercoxit III-Grade* prices at 9.55B/unit because its blueprint eats
//!   500 ore units at an SDE `basePrice` of 19.1M each.
//!
//! This module finds the rest mechanically. It **reports and never fixes**: no pricing rule
//! lives here, and nothing it computes is written anywhere.
//!
//! ### What a price means here
//!
//! Only seeded types have prices, and the two sides are not interchangeable:
//!
//! * a type with a `seed_stock` row can be **bought** at its ask;
//! * a type with a `seed_buy_orders` row can be **sold** at its bid;
//! * a type with neither has no ask and no bid. It contributes **zero revenue** as a
//!   reprocessing output, and makes a recipe **unbuyable** as a manufacturing input — such a
//!   recipe is not exploitable at all, so it is skipped and counted rather than flagged.
//!
//! v3 writes both sides for every seedable type, so in practice "seeded" is one set; the
//! asymmetry is still modelled explicitly because it is what makes an unpriced input a skip
//! instead of a free lunch.
//!
//! ### The four checks
//!
//! | check | the loop | violation |
//! |-------|----------|-----------|
//! | 1 reprocess   | buy 1 unit at ask, refine it, sell the output into seed bids | `revenue > ask` |
//! | 2 manufacture | use a fully funded recipe with guaranteed blueprint/BPC access, build 1 unit, sell it into its own seed bid | `bid > funded recurring cost` |
//! | 3 compression | buy one side, sell the other, both directions | `revenue > outlay` |
//! | 4 cross-station | buy at the cheapest seeded ask anywhere, haul, sell into the best bid elsewhere | `best bid > best ask` |
//!
//! ### Prices are the best price anywhere, not the Jita row
//!
//! Checks 1-3 predate the NPC overlay, when "the seed" meant one station and a type had one
//! ask and one bid. Now that TQ's NPC catalog seeds ~5,000 stations, every check prices a
//! type at the **best ask and best bid across all seeded stations** — a player will always
//! use the better price, so anything else under-reports — and every violation names the
//! station each of its prices came from.
//!
//! Check 4 exists because the first three would otherwise come back clean over a book they
//! only half understand: they all look for a *transformation*, and hauling is not one. With
//! rows at thousands of stations, "buy here, sell there" needs no skill, blueprint or refinery
//! and is the cheapest exploit in the game to run.
//!
//! ### Yield, and why the default is the *perfect* refine
//!
//! `--yield` defaults to **1.0**. A 100% recovery is the worst case for us: a loop that does
//! not pay at perfect refine cannot be exploited at any real skill level, so 1.0 is the only
//! honest gate. The realistic 0.906 pass is printed beside it so the report shows both.
//! Yield enters check 1 only — check 2 never refines anything, and in check 3 both sides
//! refine at the same rate, so it cancels out of the ratio.
//!
//! ### The 8% tax
//!
//! Selling into a seed bid pays the 8% transaction tax (`marketProxyService.js:2951`), so a
//! loop only really prints ISK when `revenue x 0.92 > outlay`. Every violation is reported
//! with and without it, and **the tax-surviving count at yield 1.0 is the build gate**:
//! non-zero means exit 1.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, bail};
use console::style;
use rusqlite::{Connection, OpenFlags};

use crate::build::{SeedPricingProfiles, SeedRow, SeedStation, plan_canonical_seed_rows};
use crate::classify::{Classification, ProducerIndex, load_lp_reward_type_ids};
use crate::config::SeederConfig;
use crate::cost::{CostInputs, CostMap, CostReport, OverridePrices};
use crate::funded::{FundedChoiceKind, FundedResolver, IndustrySemantics, ProductionCatalog};
use crate::manifest::{CapturePolicy, PriceManifest, read_sde_build};
use crate::seedplan::{
    SeedPlan, required_external_market_references_from_config, strict_sibling_family_fallbacks,
};
use crate::staticdata::{ReprocessingStatic, StaticData, VariantFamilies, resolve_static_data_dir};

/// The transaction tax a seed-bid fill pays (`doc/JITA_INDEX_SEED_PLAN.md` §5).
pub const SEED_BID_TAX: f64 = 0.08;
/// What the seller actually keeps out of a seed-bid sale.
pub const TAX_KEPT_FRACTION: f64 = 1.0 - SEED_BID_TAX;
/// Perfect refine — the default and the only yield the build gate is evaluated at.
pub const PERFECT_YIELD: f64 = 1.0;
/// A realistic station refine (50% base, max skills, no rigs), printed as a second pass.
pub const REALISTIC_YIELD: f64 = 0.906;

// ------------------------------------------------------------------------------- the book

/// Who wrote a seed row: us, or TQ.
///
/// This is the difference between a loop we can fix and a loop we inherited. Every price in
/// the book carries it, so a violation can say which of its legs — if any — is a price the
/// seeder computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceOrigin {
    /// An index or junk row: `sell_multiplier x manifest cost` / `buy_multiplier x cost`, at
    /// the index station. Ours, and movable by a pricing rule.
    Seeder,
    /// A row imported verbatim from the TQ snapshot by [`crate::overlay`] — TQ's own NPC
    /// price at TQ's own station.
    NpcOverlay,
}

impl PriceOrigin {
    pub fn key(self) -> &'static str {
        match self {
            Self::Seeder => "seeder",
            Self::NpcOverlay => "npc-overlay",
        }
    }
}

/// One price, the station it is at, and who put it there.
#[derive(Debug, Clone, PartialEq)]
pub struct StationPrice {
    pub price: f64,
    pub station_id: u64,
    pub station_name: String,
    pub origin: PriceOrigin,
}

/// The best price for one side of one type across every seeded station, plus the best price
/// available at some **other** station.
///
/// The second field is what makes check 4 exact. If a type's best ask and best bid happen to
/// be at the same station, the cross-station question is not "is there an opportunity" but
/// "is there one between two *different* stations", and answering it needs the runner-up from
/// elsewhere. Without it a station that both buys high and sells low would mask a genuine
/// haul against a third station.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BestPrice {
    pub best: Option<StationPrice>,
    /// Best price at a station other than `best`'s.
    pub elsewhere: Option<StationPrice>,
}

impl BestPrice {
    /// One candidate ask: cheapest wins.
    pub fn absorb_ask(&mut self, candidate: StationPrice) {
        self.absorb(candidate, true);
    }

    /// One candidate bid: dearest wins.
    pub fn absorb_bid(&mut self, candidate: StationPrice) {
        self.absorb(candidate, false);
    }

    fn absorb(&mut self, candidate: StationPrice, minimise: bool) {
        let better = |left: f64, right: f64| {
            if minimise { left < right } else { left > right }
        };
        let Some(best) = self.best.clone() else {
            self.best = Some(candidate);
            return;
        };
        if better(candidate.price, best.price) {
            // The old best was the best over *all* stations, so it is exactly the best over
            // "every station but the new winner's" — unless it came from that same station,
            // in which case `elsewhere` is already right and must not move.
            if candidate.station_id != best.station_id {
                self.elsewhere = Some(best);
            }
            self.best = Some(candidate);
        } else if candidate.station_id != best.station_id {
            let improves = self
                .elsewhere
                .as_ref()
                .is_none_or(|current| better(candidate.price, current.price));
            if improves {
                self.elsewhere = Some(candidate);
            }
        }
    }

    pub fn price(&self) -> Option<f64> {
        self.best.as_ref().map(|entry| entry.price)
    }

    pub fn station_id(&self) -> Option<u64> {
        self.best.as_ref().map(|entry| entry.station_id)
    }

    /// Who wrote the best price on this side, when there is one.
    pub fn origin(&self) -> Option<PriceOrigin> {
        self.best.as_ref().map(|entry| entry.origin)
    }
}

/// Both seed prices for one seeded type, at the best station for each side.
#[derive(Debug, Clone, PartialEq)]
pub struct SeedSide {
    pub type_id: u32,
    pub name: String,
    /// `A`, `A+B`, `B`, `D`, or `C` for a type only the NPC overlay seeds.
    pub bucket: &'static str,
    /// The manifest price the index/junk sides were derived from; 0 for overlay-only types,
    /// which carry TQ's price and no cost model at all.
    pub cost: f64,
    /// Cheapest ask anywhere. Empty when nothing sells this type.
    pub ask: BestPrice,
    /// Highest bid anywhere, before the 8% tax. Empty when nothing buys this type.
    pub bid: BestPrice,
}

/// Which side of the book a price came from, so a report can say where to find it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSide {
    Ask,
    Bid,
}

/// The seeded universe as a price book: the audit's whole view of the economy.
///
/// **A price is the best price available anywhere.** Before the NPC overlay landed this was
/// trivially the Jita 4-4 row, and the checks could assume it; now the same type can be sold
/// at Jita 4-4 by the index seed and bought by an NPC in Rens, and a player will always take
/// the better of the two. Every check therefore prices a type at its best ask and best bid
/// across all seeded stations, and reports which station each came from.
///
/// [`SeedBook::ask`] and [`SeedBook::bid`] return `None` for a side nothing seeds, which is
/// the distinction the checks are built on — an absent price is never silently a zero price.
#[derive(Debug, Clone, Default)]
pub struct SeedBook {
    entries: BTreeMap<u32, SeedSide>,
    /// The index seed's own station, used only to keep the report readable: prices from
    /// anywhere else are tagged with their station, prices from here are not.
    index_station_id: Option<u64>,
    /// Distinct stations contributing at least one price.
    stations: BTreeSet<u64>,
}

impl SeedBook {
    /// Built from the same [`SeedRow`] set the build writes, so the audited prices are the
    /// prices — not a re-derivation that could drift from them. A side the build will not
    /// write (deferred to the NPC catalog) gets no price here either: an absent price is the
    /// whole distinction the checks are built on.
    pub fn from_rows(rows: &[SeedRow], station: &SeedStation) -> Self {
        let mut book = Self {
            index_station_id: Some(station.station_id),
            ..Self::default()
        };
        for row in rows {
            book.insert(SeedSide {
                type_id: row.type_id,
                name: row.name.clone(),
                bucket: row.bucket,
                cost: row.cost,
                ask: one_price(row.writes_ask().then_some(row.ask), station),
                bid: one_price(row.writes_bid().then_some(row.bid), station),
            });
        }
        book.stations.insert(station.station_id);
        book
    }

    /// Builds the best-price view of one identical canonical book replicated at every hub.
    pub fn from_rows_at_stations(rows: &[SeedRow], stations: &[SeedStation]) -> Self {
        let mut book = Self {
            index_station_id: stations.first().map(|station| station.station_id),
            ..Self::default()
        };
        for row in rows {
            let entry = book.entries.entry(row.type_id).or_insert_with(|| SeedSide {
                type_id: row.type_id,
                name: row.name.clone(),
                bucket: row.bucket,
                cost: row.cost,
                ask: BestPrice::default(),
                bid: BestPrice::default(),
            });
            for station in stations {
                if row.writes_ask() {
                    entry.ask.absorb_ask(station_price(row.ask, station));
                }
                if row.writes_bid() {
                    entry.bid.absorb_bid(station_price(row.bid, station));
                }
            }
        }
        for station in stations {
            book.stations.insert(station.station_id);
        }
        book
    }

    /// Use the actual persisted candidate prices for all economic checks. Planned rows
    /// supply only diagnostic names and role labels; their computed prices are discarded.
    pub fn from_candidate_db(
        path: &Path,
        rows: &[SeedRow],
        stations: &[SeedStation],
        data: &StaticData,
    ) -> Result<Self> {
        let mut book = Self::from_rows_at_stations(rows, stations);
        for side in book.entries.values_mut() {
            side.ask = BestPrice::default();
            side.bid = BestPrice::default();
        }
        book.stations.clear();
        let approved = stations
            .iter()
            .map(|s| s.station_id)
            .collect::<BTreeSet<_>>();
        let uri = format!(
            "file:{}?mode=ro&immutable=1",
            path.to_string_lossy().replace('\\', "/")
        );
        let connection = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        for (table, is_ask) in [("seed_stock", true), ("seed_buy_orders", false)] {
            let mut query = connection.prepare(&format!(
                "SELECT station_id, type_id, price FROM {table} ORDER BY station_id, type_id"
            ))?;
            let persisted = query.query_map([], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })?;
            for persisted_row in persisted {
                let (station_id, type_id, price) = persisted_row?;
                if !approved.contains(&station_id) {
                    bail!("candidate {table} contains non-canonical station {station_id}");
                }
                if !price.is_finite() || price <= 0.0 {
                    bail!(
                        "candidate {table} has invalid price for type {type_id} at station {station_id}"
                    );
                }
                let station_name = data
                    .station(station_id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("candidate station {station_id} is absent from static data")
                    })?
                    .station_name
                    .clone();
                let name = data
                    .item_type(type_id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("candidate type {type_id} is absent from static data")
                    })?
                    .name
                    .clone();
                let side = book.entries.entry(type_id).or_insert_with(|| SeedSide {
                    type_id,
                    name,
                    bucket: "C",
                    cost: 0.0,
                    ask: BestPrice::default(),
                    bid: BestPrice::default(),
                });
                let candidate = StationPrice {
                    price,
                    station_id,
                    station_name,
                    origin: PriceOrigin::Seeder,
                };
                if is_ask {
                    side.ask.absorb_ask(candidate);
                } else {
                    side.bid.absorb_bid(candidate);
                }
                book.stations.insert(station_id);
            }
        }
        if book.stations != approved {
            bail!("candidate station set does not match configured canonical stations");
        }
        book.entries
            .retain(|_, side| side.ask.best.is_some() || side.bid.best.is_some());
        Ok(book)
    }

    /// Folds the NPC overlay into the book **exactly as the build writes it**: the overlay
    /// went in first, then the index pass `INSERT OR REPLACE`d its own station, so an overlay
    /// row at the index station on a side the index pass also seeds no longer exists in the
    /// database and must not exist here either. Every other overlay row does — including the
    /// one at the index station on a side the index pass **deferred**, which is precisely the
    /// row the deferral left standing.
    pub fn merge_overlay(
        &mut self,
        import: &crate::overlay::OverlayImport,
        static_data: &StaticData,
        index_station_id: u64,
    ) {
        let planned = |side: PriceSide| -> BTreeSet<u32> {
            self.entries
                .values()
                .filter(|entry| match side {
                    PriceSide::Ask => entry.ask.best.is_some(),
                    PriceSide::Bid => entry.bid.best.is_some(),
                })
                .map(|entry| entry.type_id)
                .collect()
        };
        let (planned_ask, planned_bid) = (planned(PriceSide::Ask), planned(PriceSide::Bid));
        let mut names: BTreeMap<u64, String> = BTreeMap::new();
        let mut station_name = |station_id: u64, data: &StaticData| -> String {
            names
                .entry(station_id)
                .or_insert_with(|| {
                    data.station(station_id)
                        .map(|station| station.station_name.clone())
                        .unwrap_or_else(|| format!("station {station_id}"))
                })
                .clone()
        };

        for (side, book, planned) in [
            (PriceSide::Ask, &import.sell, &planned_ask),
            (PriceSide::Bid, &import.buy, &planned_bid),
        ] {
            for ((station_id, type_id), row) in book {
                if *station_id == index_station_id && planned.contains(type_id) {
                    // Replaced by the index pass. Counting it here would audit a price that
                    // is not in the database.
                    continue;
                }
                let candidate = StationPrice {
                    price: row.price(),
                    station_id: *station_id,
                    station_name: station_name(*station_id, static_data),
                    origin: PriceOrigin::NpcOverlay,
                };
                let entry = self.entries.entry(*type_id).or_insert_with(|| SeedSide {
                    type_id: *type_id,
                    name: static_data
                        .item_type(*type_id)
                        .map(|item| item.name.clone())
                        .unwrap_or_else(|| format!("type {type_id}")),
                    // Plan §2.3's bucket: seeded by TQ's NPCs, priced by nothing of ours.
                    bucket: "C",
                    cost: 0.0,
                    ask: BestPrice::default(),
                    bid: BestPrice::default(),
                });
                match side {
                    PriceSide::Ask => entry.ask.absorb_ask(candidate),
                    PriceSide::Bid => entry.bid.absorb_bid(candidate),
                }
                self.stations.insert(*station_id);
            }
        }
    }

    pub fn insert(&mut self, side: SeedSide) {
        for price in [
            side.ask.best.as_ref(),
            side.ask.elsewhere.as_ref(),
            side.bid.best.as_ref(),
            side.bid.elsewhere.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            self.stations.insert(price.station_id);
        }
        self.entries.insert(side.type_id, side);
    }

    pub fn get(&self, type_id: u32) -> Option<&SeedSide> {
        self.entries.get(&type_id)
    }

    pub fn station_count(&self) -> usize {
        self.stations.len()
    }

    /// What one unit costs to buy from the seed at the cheapest station that sells it, or
    /// `None` when no `seed_stock` row anywhere holds it.
    pub fn ask(&self, type_id: u32) -> Option<f64> {
        self.entries.get(&type_id).and_then(|side| side.ask.price())
    }

    /// What the best seed bid anywhere pays for one unit, or `None` when nothing buys it.
    pub fn bid(&self, type_id: u32) -> Option<f64> {
        self.entries.get(&type_id).and_then(|side| side.bid.price())
    }

    /// The cheapest ask anywhere, with its station and its origin — what a check needs to
    /// know both what a leg costs and whose price it is.
    pub fn ask_price(&self, type_id: u32) -> Option<&StationPrice> {
        self.entries
            .get(&type_id)
            .and_then(|side| side.ask.best.as_ref())
    }

    /// The dearest bid anywhere, with its station and its origin.
    pub fn bid_price(&self, type_id: u32) -> Option<&StationPrice> {
        self.entries
            .get(&type_id)
            .and_then(|side| side.bid.best.as_ref())
    }

    /// `" @ Rens VI - Moon 8 - …"` when the price is not at the index station, `""` when it
    /// is. Keeps a 40-item material list readable while still saying where every price lives.
    pub fn qualify(&self, type_id: u32, side: PriceSide) -> String {
        let Some(entry) = self.entries.get(&type_id) else {
            return String::new();
        };
        let best = match side {
            PriceSide::Ask => entry.ask.best.as_ref(),
            PriceSide::Bid => entry.bid.best.as_ref(),
        };
        match best {
            Some(price) if Some(price.station_id) != self.index_station_id => {
                format!(" @ {}", price.station_name)
            }
            _ => String::new(),
        }
    }

    /// The revenue an unseeded output contributes: nothing. Separate from [`SeedBook::bid`]
    /// so a caller has to opt into the zero rather than get it by accident.
    pub fn bid_or_zero(&self, type_id: u32) -> f64 {
        self.bid(type_id).unwrap_or(0.0)
    }

    pub fn name(&self, type_id: u32) -> String {
        self.entries
            .get(&type_id)
            .map(|side| side.name.clone())
            .unwrap_or_else(|| format!("type {type_id}"))
    }

    pub fn iter(&self) -> impl Iterator<Item = &SeedSide> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Seeded types per bucket label, in report order.
    pub fn composition(&self) -> Vec<(&'static str, usize)> {
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        for side in self.entries.values() {
            *counts.entry(side.bucket).or_insert(0) += 1;
        }
        ["A", "A+B", "B", "C", "D"]
            .into_iter()
            .map(|label| (label, counts.get(label).copied().unwrap_or(0)))
            .collect()
    }
}

/// Independent Market v1 policy gate for reward supply. This deliberately derives both
/// predicates from the authoritative LP catalog and raw blueprint definitions instead of
/// trusting the classifier's final role.
pub fn check_reward_sell_invariants(
    book: &SeedBook,
    data: &StaticData,
    direct_lp_rewards: &BTreeSet<u32>,
) -> Result<()> {
    let mut violations = BTreeSet::<String>::new();

    for blueprint in &data.blueprints {
        if !blueprint.published || !direct_lp_rewards.contains(&blueprint.blueprint_type_id) {
            continue;
        }
        let Some(manufacturing) = blueprint.activities.manufacturing.as_ref() else {
            continue;
        };
        for product in &manufacturing.products {
            if data
                .item_type(product.type_id)
                .is_some_and(|item| item.is_marketable())
                && book.ask(product.type_id).is_some()
            {
                violations.insert(format!(
                    "LP-offered blueprint {} manufactures type {} ({}) with a canonical Sell",
                    blueprint.blueprint_type_id,
                    product.type_id,
                    data.item_type(product.type_id)
                        .map(|item| item.name.as_str())
                        .unwrap_or("<unknown>")
                ));
            }
        }
    }

    let invention_lineage = data
        .blueprints
        .iter()
        .filter_map(|blueprint| blueprint.activities.invention.as_ref())
        .flat_map(|invention| invention.products.iter().map(|product| product.type_id))
        .collect::<BTreeSet<_>>();
    for blueprint in &data.blueprints {
        let Some(manufacturing) = blueprint.activities.manufacturing.as_ref() else {
            continue;
        };
        if !blueprint.published
            || invention_lineage.contains(&blueprint.blueprint_type_id)
            || manufacturing.materials.len() != 1
            || manufacturing.materials[0].type_id != 34
            || manufacturing.materials[0].quantity != 1
            || data
                .item_type(blueprint.blueprint_type_id)
                .is_none_or(|item| item.is_marketable())
            || book.ask(blueprint.blueprint_type_id).is_some()
        {
            continue;
        }
        for product in &manufacturing.products {
            let placeholder_hull = product.quantity == 1
                && data
                    .item_type(product.type_id)
                    .is_some_and(|item| item.is_marketable() && item.category_id == Some(6));
            if placeholder_hull && book.ask(product.type_id).is_some() {
                violations.insert(format!(
                    "special placeholder blueprint {} manufactures type {} ({}) from 1 Tritanium with a canonical Sell",
                    blueprint.blueprint_type_id,
                    product.type_id,
                    data.item_type(product.type_id)
                        .map(|item| item.name.as_str())
                        .unwrap_or("<unknown>")
                ));
            }
        }
    }

    if violations.is_empty() {
        return Ok(());
    }
    bail!(
        "{} independent reward-supply invariant violation(s):\n  {}",
        violations.len(),
        violations.into_iter().collect::<Vec<_>>().join("\n  ")
    )
}

/// Independent Market v1 gate for gameplay-essential NPC supply. Membership is derived
/// directly from the configured authoritative allowlists, and price authority comes only
/// from the captured NPC Sell catalog; the classifier and final assigned role are not trusted.
pub fn check_progression_npc_supply_invariants(
    book: &SeedBook,
    data: &StaticData,
    overlay: Option<&crate::overlay::OverlayImport>,
    group_ids: &[u32],
    type_ids: &[u32],
) -> Result<()> {
    let configured_groups = group_ids.iter().copied().collect::<BTreeSet<_>>();
    let mut required = type_ids.iter().copied().collect::<BTreeSet<_>>();
    required.extend(data.marketable_item_types().filter_map(|item| {
        item.group_id
            .filter(|group_id| configured_groups.contains(group_id))
            .map(|_| item.type_id)
    }));

    let mut violations = Vec::new();
    for type_id in required {
        let name = data
            .item_type(type_id)
            .map(|item| item.name.as_str())
            .unwrap_or("<unknown>");
        if data
            .item_type(type_id)
            .is_none_or(|item| !item.is_marketable())
        {
            violations.push(format!(
                "configured progression NPC type {type_id} ({name}) is absent or not marketable"
            ));
            continue;
        }
        let Some(reference) = overlay.and_then(|prices| prices.npc_min_sell(type_id)) else {
            violations.push(format!(
                "progression NPC type {type_id} ({name}) has no exact NPC Sell reference"
            ));
            continue;
        };
        match book.ask(type_id) {
            Some(ask) if (ask - reference).abs() < 0.005 => {}
            Some(ask) => violations.push(format!(
                "progression NPC type {type_id} ({name}) canonical Sell {ask:.2} differs from NPC reference {reference:.2}"
            )),
            None => violations.push(format!(
                "progression NPC type {type_id} ({name}) has no canonical Sell at NPC reference {reference:.2}"
            )),
        }
        if let Some(bid) = book.bid(type_id) {
            violations.push(format!(
                "progression NPC type {type_id} ({name}) has forbidden canonical Buy {bid:.2}"
            ));
        }
    }

    if violations.is_empty() {
        return Ok(());
    }
    bail!(
        "{} independent progression-NPC supply invariant violation(s):\n  {}",
        violations.len(),
        violations.join("\n  ")
    )
}

/// One price at one station: what an index/junk seed row is before any overlay merges in.
/// `None` for a side the index pass does not write, which is an absent price, not a zero one.
fn one_price(price: Option<f64>, station: &SeedStation) -> BestPrice {
    BestPrice {
        best: price.map(|price| station_price(price, station)),
        elsewhere: None,
    }
}

fn station_price(price: f64, station: &SeedStation) -> StationPrice {
    StationPrice {
        price,
        station_id: station.station_id,
        station_name: station.station_name.clone(),
        origin: PriceOrigin::Seeder,
    }
}

// ----------------------------------------------------------------------------- the checks

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Check {
    Reprocess,
    Manufacture,
    Compression,
    CrossStation,
}

impl Check {
    pub fn number(self) -> u8 {
        match self {
            Self::Reprocess => 1,
            Self::Manufacture => 2,
            Self::Compression => 3,
            Self::CrossStation => 4,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Reprocess => "reprocess",
            Self::Manufacture => "manufacture",
            Self::Compression => "compression",
            Self::CrossStation => "cross-station",
        }
    }

    /// One line saying what loop the check walks.
    pub fn headline(self) -> &'static str {
        match self {
            Self::Reprocess => "buy 1 unit at its ask, refine it, sell the output into seed bids",
            Self::Manufacture => {
                "use an accessible fully funded production path, build 1 unit, sell it into its own seed bid"
            }
            Self::Compression => {
                "buy one side of a compressed/uncompressed pair and sell the other, both ways"
            }
            Self::CrossStation => {
                "buy 1 unit at the cheapest seeded ask anywhere, haul it, sell it into the \
                 highest seeded bid at another station"
            }
        }
    }
}

// Skip reasons. Static strings so the report can group them without allocating per type.
const SKIP_NO_REPROCESSING_ROW: &str = "no reprocessing row";
const SKIP_NOT_REPROCESSABLE: &str = "neither refinable nor recyclable";
const SKIP_REFINES_INTO_NOTHING: &str = "refines into nothing";
const SKIP_BAD_PORTION_SIZE: &str = "portion size <= 0 (bad data)";
const SKIP_NO_PRODUCER: &str = "no blueprint produces it";
const SKIP_UNFUNDED_PRODUCTION: &str =
    "no production path has guaranteed blueprint/BPC and recurring-input access";
const SKIP_PAIR_UNSEEDED: &str = "one side of the pair is not seeded";
const SKIP_NO_RATIO_BASIS: &str = "no blueprint and no comparable refine yields";
const SKIP_RATIO_DISAGREES: &str = "refine yields disagree on the ratio";
const SKIP_NOT_BUYABLE: &str = "nothing sells it (no ask anywhere)";
const SKIP_NOT_SELLABLE: &str = "nothing buys it (no bid anywhere)";
const SKIP_ONE_SIDED: &str = "seeded on one side only";
const SKIP_SINGLE_STATION: &str = "only one station holds both sides";

/// Whether a loop touches a price **we** set.
///
/// The whole point of the distinction is that only one of these two is ours to fix. A loop
/// with a seeder-priced leg exists because our computed price met some other price; a loop
/// with no seeder-priced leg at all is TQ's own book, imported verbatim by decision 2's
/// TQ-authentic placement, and no pricing rule of ours can close it without discarding NPC
/// rows CCP ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ownership {
    /// At least one leg is an index or junk row — a price the seeder computed.
    SeederInvolved,
    /// Every leg is an NPC overlay row. Inherited from the TQ snapshot.
    NpcToNpc,
}

impl Ownership {
    /// Ours if **any** leg is ours.
    pub fn of(origins: impl IntoIterator<Item = PriceOrigin>) -> Self {
        if origins
            .into_iter()
            .any(|origin| origin == PriceOrigin::Seeder)
        {
            Self::SeederInvolved
        } else {
            Self::NpcToNpc
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::SeederInvolved => "seeder-involved",
            Self::NpcToNpc => "npc-to-npc",
        }
    }

    pub fn is_seeder_involved(self) -> bool {
        matches!(self, Self::SeederInvolved)
    }
}

/// One loop that returns more than it costs.
///
/// `type_id`/`name` always name the type the loop **sells** — the thing whose bid is paying
/// out — because that is the price a fix would have to move.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub check: Check,
    pub type_id: u32,
    pub name: String,
    pub bucket: &'static str,
    /// Whether any leg of this loop is a price the seeder computed.
    pub ownership: Ownership,
    /// The loop in words, with the quantities that make it work.
    pub loop_text: String,
    /// ISK spent at seed asks.
    pub outlay: f64,
    /// ISK the seed bids pay back, before tax.
    pub revenue: f64,
}

impl Violation {
    /// `revenue / outlay`. Callers only build a [`Violation`] with a positive outlay, so this
    /// is finite and > 1.
    pub fn ratio(&self) -> f64 {
        self.revenue / self.outlay
    }

    /// What the loop actually pays after the 8% seed-bid transaction tax.
    pub fn after_tax(&self) -> f64 {
        self.revenue * TAX_KEPT_FRACTION
    }

    /// The question that matters: is this still a money pump once the tax is paid?
    pub fn survives_tax(&self) -> bool {
        self.after_tax() > self.outlay
    }

    /// Whether this loop fails the build.
    ///
    /// Everything that survives the tax gates, with exactly one exception: a **check 4**
    /// npc-to-npc haul is TQ's own fixed NPC spread across two of TQ's own stations, which
    /// `[import] gate_on_npc_arbitrage` (default false) decides about. The exception is
    /// deliberately narrow — an npc-to-npc *transformation* (checks 1-3) would mean TQ's
    /// prices disagree with the game's own reprocessing or manufacturing mechanics, which is
    /// a different claim entirely and still gates.
    pub fn gates(&self, gate_on_npc_arbitrage: bool) -> bool {
        if !self.survives_tax() {
            return false;
        }
        self.ownership.is_seeder_involved()
            || self.check != Check::CrossStation
            || gate_on_npc_arbitrage
    }
}

/// What one check found: how much it looked at, what it could not look at and why, and every
/// loop that came out positive.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckOutcome {
    pub check: Check,
    /// Candidates actually evaluated.
    pub checked: usize,
    /// Reason → how many candidates it excluded. Sums to [`CheckOutcome::skipped_total`].
    pub skipped: BTreeMap<&'static str, usize>,
    /// Sorted by ratio, worst first.
    pub violations: Vec<Violation>,
    /// Free-text observations that are not skips — printed under the counts.
    pub notes: Vec<String>,
}

impl CheckOutcome {
    pub fn new(check: Check) -> Self {
        Self {
            check,
            checked: 0,
            skipped: BTreeMap::new(),
            violations: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn skip(&mut self, reason: &'static str) {
        *self.skipped.entry(reason).or_insert(0) += 1;
    }

    /// Records a loop **only if it is actually positive**, so callers can hand every
    /// evaluated candidate here without pre-filtering. A non-positive outlay is refused: a
    /// free purchase would divide by zero and is bad data, not an exploit.
    ///
    /// `ownership` is computed from the origins of every price the loop actually used, which
    /// only the caller knows.
    fn consider(
        &mut self,
        type_id: u32,
        name: String,
        bucket: &'static str,
        ownership: Ownership,
        outlay: f64,
        revenue: f64,
        loop_text: impl FnOnce() -> String,
    ) {
        if !(outlay.is_finite() && outlay > 0.0) || !revenue.is_finite() || revenue <= outlay {
            return;
        }
        self.violations.push(Violation {
            check: self.check,
            type_id,
            name,
            bucket,
            ownership,
            loop_text: loop_text(),
            outlay,
            revenue,
        });
    }

    fn finish(mut self) -> Self {
        self.violations.sort_by(|left, right| {
            right
                .ratio()
                .partial_cmp(&left.ratio())
                .unwrap_or(Ordering::Equal)
                .then(left.type_id.cmp(&right.type_id))
        });
        self
    }

    pub fn skipped_total(&self) -> usize {
        self.skipped.values().sum()
    }

    pub fn violation_count(&self) -> usize {
        self.violations.len()
    }

    /// Violations that still print ISK after the 8% tax.
    pub fn surviving_tax(&self) -> usize {
        self.violations
            .iter()
            .filter(|violation| violation.survives_tax())
            .count()
    }

    pub fn violations_owned(&self, ownership: Ownership) -> impl Iterator<Item = &Violation> {
        self.violations
            .iter()
            .filter(move |violation| violation.ownership == ownership)
    }

    pub fn violation_count_owned(&self, ownership: Ownership) -> usize {
        self.violations_owned(ownership).count()
    }

    /// Tax-surviving violations on one side of the ownership split.
    pub fn surviving_tax_owned(&self, ownership: Ownership) -> usize {
        self.violations_owned(ownership)
            .filter(|violation| violation.survives_tax())
            .count()
    }

    /// Tax-surviving violations that actually fail the build under this gate setting.
    pub fn gating(&self, gate_on_npc_arbitrage: bool) -> usize {
        self.violations
            .iter()
            .filter(|violation| violation.gates(gate_on_npc_arbitrage))
            .count()
    }

    pub fn worst_ratio(&self) -> Option<f64> {
        self.violations.first().map(Violation::ratio)
    }
}

/// Check 1 — reprocess pump.
///
/// `revenue_per_unit = yield * Σ(quantity * bid(material)) / portion_size`, flagged when it
/// beats the type's own ask. Outputs that are not seeded pay nothing and are counted in a
/// note: they make the loop *less* profitable, so including them can only under-report.
pub fn check_reprocess(
    book: &SeedBook,
    reprocessing: &ReprocessingStatic,
    refine_yield: f64,
) -> CheckOutcome {
    let mut outcome = CheckOutcome::new(Check::Reprocess);
    let mut with_unseeded_outputs = 0usize;

    for side in book.iter() {
        let Some(ask_price) = side.ask.best.as_ref() else {
            // Nothing sells it, so the loop cannot even be started.
            outcome.skip(SKIP_NOT_BUYABLE);
            continue;
        };
        let Some(entry) = reprocessing.get(side.type_id) else {
            outcome.skip(SKIP_NO_REPROCESSING_ROW);
            continue;
        };
        if !entry.is_reprocessable() {
            outcome.skip(SKIP_NOT_REPROCESSABLE);
            continue;
        }
        if entry.materials.is_empty() {
            outcome.skip(SKIP_REFINES_INTO_NOTHING);
            continue;
        }
        if entry.portion_size <= 0 {
            outcome.skip(SKIP_BAD_PORTION_SIZE);
            continue;
        }

        outcome.checked += 1;
        let portion = entry.portion_size as f64;
        let mut revenue = 0.0;
        let mut unseeded_output = false;
        // Every price the loop touches: the ask it buys at, and each output bid it sells into.
        let mut origins = vec![ask_price.origin];
        // (material, units per refined unit, ISK per refined unit)
        let mut parts: Vec<(u32, f64, f64)> = Vec::with_capacity(entry.materials.len());
        for material in &entry.materials {
            let per_unit = material.quantity as f64 / portion;
            match book.bid_price(material.type_id) {
                Some(bid) => {
                    let value = per_unit * bid.price * refine_yield;
                    revenue += value;
                    origins.push(bid.origin);
                    parts.push((material.type_id, per_unit * refine_yield, value));
                }
                None => unseeded_output = true,
            }
        }
        if unseeded_output {
            with_unseeded_outputs += 1;
        }

        let ask = ask_price.price;
        let ask_station = ask_price.station_name.clone();
        let name = side.name.clone();
        let ownership = Ownership::of(origins);
        outcome.consider(
            side.type_id,
            name.clone(),
            side.bucket,
            ownership,
            ask,
            revenue,
            || {
                format!(
                    "buy 1 x {} at ask {} at {} -> refine at {} -> {} -> seed bids pay {}",
                    name,
                    crate::isk(ask),
                    ask_station,
                    percent(refine_yield),
                    describe_parts(book, &parts, PriceSide::Bid),
                    crate::isk(revenue)
                )
            },
        );
    }

    if with_unseeded_outputs > 0 {
        outcome.notes.push(format!(
            "{} of the checked types refine into at least one unseeded output, which pays 0 \
             and can only make the loop look worse than it is",
            count(with_unseeded_outputs)
        ));
    }
    outcome.finish()
}

/// Check 2 — manufacture pump.
///
/// The funded recurring unit cost includes guaranteed recipe access, invention where required,
/// material efficiency, facility/rig semantics and recursive build-vs-buy choices. It is flagged
/// only when the product's own seed **bid** beats that executable funded path.
#[derive(Debug)]
struct RecipeAccessEvidence {
    blueprint_type_id: u32,
    accessible: bool,
    reason: String,
}

/// Independent access proof built from raw static definitions and the canonical Sell book.
/// This deliberately does not consult `FundedResolver`: the audit must reject an unavailable
/// BPC even if pricing code were ever to regress and fabricate a cost for it.
fn recipe_access_evidence(
    book: &SeedBook,
    data: &StaticData,
) -> BTreeMap<u32, Vec<RecipeAccessEvidence>> {
    let mut invention_sources =
        BTreeMap::<u32, Vec<&crate::staticdata::BlueprintDefinition>>::new();
    for source in &data.blueprints {
        let Some(invention) = source.activities.invention.as_ref() else {
            continue;
        };
        for product in &invention.products {
            invention_sources
                .entry(product.type_id)
                .or_default()
                .push(source);
        }
    }

    let mut by_product = BTreeMap::<u32, Vec<RecipeAccessEvidence>>::new();
    for blueprint in &data.blueprints {
        for (activity, definition) in [
            ("manufacturing", blueprint.activities.manufacturing.as_ref()),
            ("reaction", blueprint.activities.reaction.as_ref()),
        ] {
            let Some(definition) = definition else {
                continue;
            };
            for product in &definition.products {
                let (accessible, reason) = if !blueprint.published {
                    (false, "blueprint definition is unpublished".to_string())
                } else if product.quantity <= 0 || definition.materials.is_empty() {
                    (
                        false,
                        "static recipe has no usable materials/output".to_string(),
                    )
                } else if activity == "manufacturing"
                    && invention_sources.contains_key(&blueprint.blueprint_type_id)
                {
                    let sources = &invention_sources[&blueprint.blueprint_type_id];
                    let supported = sources.iter().find(|source| {
                        let Some(invention) = source.activities.invention.as_ref() else {
                            return false;
                        };
                        let probability = invention
                            .products
                            .iter()
                            .map(|entry| entry.probability.unwrap_or(1.0).clamp(0.0, 1.0))
                            .sum::<f64>()
                            / invention.products.len().max(1) as f64;
                        source.published
                            && book.ask(source.blueprint_type_id).is_some()
                            && probability > 0.0
                            && !invention.materials.is_empty()
                            && invention
                                .materials
                                .iter()
                                .all(|material| book.ask(material.type_id).is_some())
                    });
                    match supported {
                        Some(source) => (
                            true,
                            format!(
                                "supported invention from guaranteed source BPO {} with every \
                                 consumable on canonical Sell",
                                source.blueprint_type_id
                            ),
                        ),
                        None => (
                            false,
                            "invented BPC has no published, guaranteed source-BPO lineage with \
                             every invention consumable on canonical Sell"
                                .to_string(),
                        ),
                    }
                } else if book.ask(blueprint.blueprint_type_id).is_some() {
                    (
                        true,
                        format!("reusable {activity} blueprint/formula has canonical Sell"),
                    )
                } else {
                    (
                        false,
                        format!(
                            "reusable {activity} blueprint/formula has no canonical Sell and is \
                             not a supported invented BPC"
                        ),
                    )
                };
                by_product
                    .entry(product.type_id)
                    .or_default()
                    .push(RecipeAccessEvidence {
                        blueprint_type_id: blueprint.blueprint_type_id,
                        accessible,
                        reason,
                    });
            }
        }
    }
    by_product
}

pub fn check_manufacture(
    book: &SeedBook,
    data: &StaticData,
    semantics: &IndustrySemantics,
) -> CheckOutcome {
    let mut outcome = CheckOutcome::new(Check::Manufacture);
    let catalog = ProductionCatalog::from_static(data);
    let access_by_product = recipe_access_evidence(book, data);
    let guaranteed_sells = book
        .iter()
        .filter_map(|side| side.ask.best.as_ref().map(|ask| (side.type_id, ask.price)))
        .collect::<BTreeMap<_, _>>();
    let mut resolver = FundedResolver::new(data, &catalog, semantics, &guaranteed_sells);
    let mut rejected = Vec::new();

    for side in book.iter() {
        let Some(bid_price) = side.bid.best.as_ref() else {
            outcome.skip(SKIP_NOT_SELLABLE);
            continue;
        };
        let Some(access_evidence) = access_by_product.get(&side.type_id) else {
            outcome.skip(SKIP_NO_PRODUCER);
            continue;
        };
        if !access_evidence.iter().any(|evidence| evidence.accessible) {
            outcome.skip(SKIP_UNFUNDED_PRODUCTION);
            for evidence in access_evidence {
                rejected.push(format!(
                    "rejected {} {} via blueprint {}: {}",
                    side.type_id, side.name, evidence.blueprint_type_id, evidence.reason
                ));
            }
            continue;
        }
        if !catalog.has_recipe(side.type_id) {
            outcome.skip(SKIP_UNFUNDED_PRODUCTION);
            rejected.push(format!(
                "rejected {} {}: accessible static evidence was not a supported published recipe",
                side.type_id, side.name
            ));
            continue;
        }
        let path = match resolver.production_cost(side.type_id) {
            Ok(path) => path,
            Err(unfunded) => {
                outcome.skip(SKIP_UNFUNDED_PRODUCTION);
                rejected.push(format!(
                    "rejected {} {}: {}",
                    unfunded.type_id, unfunded.name, unfunded.reason
                ));
                continue;
            }
        };

        outcome.checked += 1;
        let name = side.name.clone();
        let bid = bid_price.price;
        let bid_station = bid_price.station_name.clone();
        let input_cost = path.unit_cost;
        let activity = path.activity.key();
        let blueprint = path.blueprint_type_id;
        let mut origins = vec![bid_price.origin];
        for choice in &path.choices {
            if choice.kind == FundedChoiceKind::Buy {
                if let Some(ask) = book.ask_price(choice.type_id) {
                    origins.push(ask.origin);
                }
            }
        }
        let ownership = Ownership::of(origins);
        let buys = path
            .choices
            .iter()
            .filter(|choice| choice.kind == FundedChoiceKind::Buy)
            .count();
        let builds = path
            .choices
            .iter()
            .filter(|choice| choice.kind == FundedChoiceKind::Build)
            .count();
        outcome.consider(
            side.type_id,
            name.clone(),
            side.bucket,
            ownership,
            input_cost,
            bid,
            || {
                format!(
                    "funded {} path costs {} per unit (blueprint {}; {} guaranteed-Sell \
                     choices, {} recursive Build choices) -> manufacture 1 x {} -> the best \
                     seed bid, at {}, pays {}",
                    activity,
                    crate::isk(input_cost),
                    blueprint,
                    buys,
                    builds,
                    name,
                    bid_station,
                    crate::isk(bid)
                )
            },
        );
    }

    if !rejected.is_empty() {
        outcome.notes.push(format!(
            "{} static production outputs were rejected because no fully funded path proved \
             blueprint/BPC access and every recurring input; rejected-route diagnostics follow",
            count(rejected.len())
        ));
        outcome.notes.extend(rejected);
    }
    outcome.finish()
}

/// Where a compression ratio came from. Reported per pair, because the two bases can and do
/// disagree, and a guess would be worse than a skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RatioBasis {
    /// The compression blueprint itself: `material quantity / per-run output`.
    Blueprint(u32),
    /// Both sides refine into the same materials, so their per-unit yields fix the ratio.
    RefineYield,
}

impl RatioBasis {
    pub fn describe(self) -> String {
        match self {
            Self::Blueprint(blueprint) => format!("blueprint {blueprint}"),
            Self::RefineYield => "refine yields".to_string(),
        }
    }
}

/// How many source units one compressed unit is worth, and where that number came from.
///
/// The blueprint is preferred because it is the mechanism a player would actually use. The
/// refine-yield basis is the fallback for the 200 of 212 pairs with no compression blueprint
/// in the SDE: both sides refine into the same materials, so the ratio of their per-unit
/// yields *is* the compression ratio. Returns the skip reason when neither basis exists.
pub fn compression_ratio(
    source: u32,
    compressed: u32,
    reprocessing: &ReprocessingStatic,
    producers: &ProducerIndex,
) -> Result<(f64, RatioBasis), &'static str> {
    if let Some(producer) = producers.get(compressed) {
        if producer.per_run > 0 {
            if let Some(material) = producer
                .materials
                .iter()
                .find(|material| material.type_id == source)
            {
                let ratio = material.quantity as f64 / producer.per_run as f64;
                if ratio.is_finite() && ratio > 0.0 {
                    return Ok((ratio, RatioBasis::Blueprint(producer.blueprint_type_id)));
                }
            }
        }
    }

    let source_yield = per_unit_yield(reprocessing, source).ok_or(SKIP_NO_RATIO_BASIS)?;
    let compressed_yield = per_unit_yield(reprocessing, compressed).ok_or(SKIP_NO_RATIO_BASIS)?;
    let mut resolved: Option<f64> = None;
    for (material, source_per_unit) in &source_yield {
        let Some(compressed_per_unit) = compressed_yield.get(material) else {
            continue;
        };
        if *source_per_unit <= 0.0 {
            continue;
        }
        let ratio = compressed_per_unit / source_per_unit;
        if !(ratio.is_finite() && ratio > 0.0) {
            continue;
        }
        match resolved {
            None => resolved = Some(ratio),
            // Every shared material must agree, or the pair has no single ratio and the
            // honest answer is to skip it.
            Some(first) if (ratio - first).abs() > first.abs() * 1e-6 => {
                return Err(SKIP_RATIO_DISAGREES);
            }
            Some(_) => {}
        }
    }
    resolved
        .map(|ratio| (ratio, RatioBasis::RefineYield))
        .ok_or(SKIP_NO_RATIO_BASIS)
}

/// Materials per **one unit** of a type, or `None` when it has no usable refine row.
fn per_unit_yield(reprocessing: &ReprocessingStatic, type_id: u32) -> Option<BTreeMap<u32, f64>> {
    let entry = reprocessing.get(type_id)?;
    if entry.materials.is_empty() || entry.portion_size <= 0 {
        return None;
    }
    let portion = entry.portion_size as f64;
    Some(
        entry
            .materials
            .iter()
            .map(|material| (material.type_id, material.quantity as f64 / portion))
            .collect(),
    )
}

/// Check 3 — compression pump.
///
/// Both directions of every compressed/uncompressed pair where both sides are seeded: buy
/// the source and sell the compressed product, and buy the compressed product and sell the
/// source back. Yield does not enter: no refining happens in either direction, and the
/// refine-yield *basis* is a ratio of two yields, so a common factor cancels.
pub fn check_compression(
    book: &SeedBook,
    reprocessing: &ReprocessingStatic,
    producers: &ProducerIndex,
) -> CheckOutcome {
    let mut outcome = CheckOutcome::new(Check::Compression);
    let mut by_basis: BTreeMap<&'static str, usize> = BTreeMap::new();

    for (source, compressed) in &reprocessing.compressed_by_source {
        let (source, compressed) = (*source, *compressed);
        let (Some(source_side), Some(compressed_side)) = (book.get(source), book.get(compressed))
        else {
            outcome.skip(SKIP_PAIR_UNSEEDED);
            continue;
        };
        // Both directions need both sides of both types.
        let (Some(source_ask), Some(source_bid), Some(compressed_ask), Some(compressed_bid)) = (
            source_side.ask.best.as_ref(),
            source_side.bid.best.as_ref(),
            compressed_side.ask.best.as_ref(),
            compressed_side.bid.best.as_ref(),
        ) else {
            outcome.skip(SKIP_ONE_SIDED);
            continue;
        };
        let (ratio, basis) = match compression_ratio(source, compressed, reprocessing, producers) {
            Ok(resolved) => resolved,
            Err(reason) => {
                outcome.skip(reason);
                continue;
            }
        };

        outcome.checked += 1;
        *by_basis
            .entry(match basis {
                RatioBasis::Blueprint(_) => "blueprint",
                RatioBasis::RefineYield => "refine yields",
            })
            .or_insert(0) += 1;

        // Forward: buy `ratio` source units, end up holding one compressed unit, sell it.
        let compress_outlay = ratio * source_ask.price;
        let compressed_name = compressed_side.name.clone();
        let source_name = source_side.name.clone();
        let basis_text = basis.describe();
        let source_ask_station = source_ask.station_name.clone();
        let compressed_bid_station = compressed_bid.station_name.clone();
        outcome.consider(
            compressed,
            compressed_name.clone(),
            compressed_side.bucket,
            Ownership::of([source_ask.origin, compressed_bid.origin]),
            compress_outlay,
            compressed_bid.price,
            || {
                format!(
                    "buy {} x {} at ask {} each at {} = {} -> compress to 1 x {} (ratio from \
                     {}) -> its seed bid at {} pays {}",
                    trim_float(ratio),
                    source_name,
                    crate::isk(source_ask.price),
                    source_ask_station,
                    crate::isk(compress_outlay),
                    compressed_name,
                    basis_text,
                    compressed_bid_station,
                    crate::isk(compressed_bid.price)
                )
            },
        );

        // Reverse: buy one compressed unit and sell the source units it stands for.
        let expand_revenue = ratio * source_bid.price;
        let compressed_name = compressed_side.name.clone();
        let source_name = source_side.name.clone();
        let basis_text = basis.describe();
        let compressed_ask_station = compressed_ask.station_name.clone();
        let source_bid_station = source_bid.station_name.clone();
        outcome.consider(
            source,
            source_name.clone(),
            source_side.bucket,
            Ownership::of([compressed_ask.origin, source_bid.origin]),
            compressed_ask.price,
            expand_revenue,
            || {
                format!(
                    "buy 1 x {} at ask {} at {} -> unpack to {} x {} (ratio from {}) -> their \
                     seed bids at {} pay {} each = {}",
                    compressed_name,
                    crate::isk(compressed_ask.price),
                    compressed_ask_station,
                    trim_float(ratio),
                    source_name,
                    basis_text,
                    source_bid_station,
                    crate::isk(source_bid.price),
                    crate::isk(expand_revenue)
                )
            },
        );
    }

    for (basis, pairs) in by_basis {
        outcome.notes.push(format!(
            "{} pairs took their ratio from {basis}",
            count(pairs)
        ));
    }
    outcome.finish()
}

/// Check 4 — cross-station arbitrage (plan §8 item 7, generalised).
///
/// The first three checks all ask "can one seeded type be turned into another for a profit".
/// None of them asks the simplest question of all: **can one type be bought at one station
/// and sold at another?** While the seed lived at Jita 4-4 alone that question had no content.
/// With the NPC overlay writing rows at ~5,000 stations it does, and a clean run of checks 1-3
/// over an overlay-shaped book would be false confidence: no transformation is needed, no
/// skill, no blueprint — buy, undock, dock, sell.
///
/// So: for every type seeded anywhere, compare the **lowest ask across all seeded stations**
/// against the **highest bid across all seeded stations**. `best_bid > best_ask` is free ISK
/// at the rate a player can haul.
///
/// **Same-station pairs are not this check's business.** A bid above an ask at one station is
/// the build's own `bid <= ask` invariant (`build::verify_seed_rows`), and for our own rows it
/// cannot happen; where the overlay carries one it is TQ's price, and it is counted in a note
/// rather than double-reported here. When the best ask and the best bid *are* at one station,
/// the check falls back to the best price available elsewhere on each side
/// ([`BestPrice::elsewhere`]) so a station that both buys high and sells low cannot mask a
/// genuine haul against a third one.
///
/// ### Who owns the loop decides what it means
///
/// Every violation is tagged [`Ownership`], and the two kinds are genuinely different findings:
///
/// * **seeder-involved** — one leg is our computed price. Our price met a fixed NPC price and
///   lost. This is a bug in the seed and it gates the build.
/// * **npc-to-npc** — both legs are TQ's own NPC rows: TQ sells a trade good in one station
///   for less than TQ buys it back in another. Nothing of ours produced it, and no multiplier
///   of ours can close it; the only way to remove it is to discard or reprice NPC rows, which
///   would undo plan decision 2 (TQ-authentic placement) and invent a catalog CCP does not
///   ship. Reported in full, in its own section, and gated only when `[import]
///   gate_on_npc_arbitrage` says so.
pub fn check_cross_station(book: &SeedBook) -> CheckOutcome {
    let mut outcome = CheckOutcome::new(Check::CrossStation);
    let mut same_station_crossings: Vec<(u32, String, f64, f64, String)> = Vec::new();

    for side in book.iter() {
        let (Some(best_ask), Some(best_bid)) = (side.ask.best.as_ref(), side.bid.best.as_ref())
        else {
            outcome.skip(SKIP_ONE_SIDED);
            continue;
        };

        // The best pair whose two legs are at different stations.
        let (buy_at, sell_to) = if best_ask.station_id != best_bid.station_id {
            (Some(best_ask), Some(best_bid))
        } else {
            if best_bid.price > best_ask.price {
                same_station_crossings.push((
                    side.type_id,
                    side.name.clone(),
                    best_ask.price,
                    best_bid.price,
                    best_ask.station_name.clone(),
                ));
            }
            // Both legs at one station: take whichever substitution leaves more on the table.
            let hold_bid = side
                .ask
                .elsewhere
                .as_ref()
                .map(|ask| (best_bid.price - ask.price, ask, best_bid));
            let hold_ask = side
                .bid
                .elsewhere
                .as_ref()
                .map(|bid| (bid.price - best_ask.price, best_ask, bid));
            match [hold_bid, hold_ask]
                .into_iter()
                .flatten()
                .max_by(|left, right| left.0.total_cmp(&right.0))
            {
                Some((_, ask, bid)) => (Some(ask), Some(bid)),
                None => {
                    outcome.skip(SKIP_SINGLE_STATION);
                    continue;
                }
            }
        };
        let (Some(buy_at), Some(sell_to)) = (buy_at, sell_to) else {
            outcome.skip(SKIP_SINGLE_STATION);
            continue;
        };

        outcome.checked += 1;
        let name = side.name.clone();
        let ask = buy_at.price;
        let bid = sell_to.price;
        let ask_station = buy_at.station_name.clone();
        let ask_station_id = buy_at.station_id;
        let ask_origin = buy_at.origin;
        let bid_station = sell_to.station_name.clone();
        let bid_station_id = sell_to.station_id;
        let bid_origin = sell_to.origin;
        outcome.consider(
            side.type_id,
            name.clone(),
            side.bucket,
            Ownership::of([ask_origin, bid_origin]),
            ask,
            bid,
            || {
                format!(
                    "buy 1 x {} for {} at {} ({}, {}) -> haul it -> sell into the seed bid at \
                     {} ({}, {}) for {}: margin {} per unit",
                    name,
                    crate::isk(ask),
                    ask_station,
                    ask_station_id,
                    ask_origin.key(),
                    bid_station,
                    bid_station_id,
                    bid_origin.key(),
                    crate::isk(bid),
                    crate::isk(bid - ask)
                )
            },
        );
    }

    if !same_station_crossings.is_empty() {
        same_station_crossings
            .sort_by(|left, right| (right.3 - right.2).total_cmp(&(left.3 - left.2)));
        let worst = same_station_crossings
            .iter()
            .take(5)
            .map(|(type_id, name, ask, bid, station)| {
                format!(
                    "{type_id} {name} bids {} against its own {} ask at {station}",
                    crate::isk(*bid),
                    crate::isk(*ask)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        outcome.notes.push(format!(
            "{} types cross their own book at ONE station (bid above ask at the same place). \
             Those are not cross-station hauls and are not counted above; none can be ours, \
             so they are TQ prices the overlay imported. Two things make that true: the build \
             proves its own two rows uncrossed, and where per-side deferral leaves one side \
             ours and the other TQ's at the index station, the deferral measured TQ's best \
             prices INCLUDING that station's. Worst: {worst}",
            count(same_station_crossings.len())
        ));
    }
    outcome.finish()
}

// ------------------------------------------------------------------------------ the report

/// All four checks at one yield.
#[derive(Debug, Clone)]
pub struct AuditReport {
    pub refine_yield: f64,
    pub reprocess: CheckOutcome,
    pub manufacture: CheckOutcome,
    pub compression: CheckOutcome,
    pub cross_station: CheckOutcome,
}

impl AuditReport {
    pub fn run(
        book: &SeedBook,
        reprocessing: &ReprocessingStatic,
        producers: &ProducerIndex,
        data: &StaticData,
        semantics: &IndustrySemantics,
        refine_yield: f64,
    ) -> Self {
        Self {
            refine_yield,
            reprocess: check_reprocess(book, reprocessing, refine_yield),
            manufacture: check_manufacture(book, data, semantics),
            compression: check_compression(book, reprocessing, producers),
            cross_station: check_cross_station(book),
        }
    }

    pub fn checks(&self) -> [&CheckOutcome; 4] {
        [
            &self.reprocess,
            &self.manufacture,
            &self.compression,
            &self.cross_station,
        ]
    }

    pub fn violations(&self) -> impl Iterator<Item = &Violation> {
        self.checks()
            .into_iter()
            .flat_map(|outcome| outcome.violations.iter())
    }

    pub fn violation_count(&self) -> usize {
        self.checks()
            .into_iter()
            .map(CheckOutcome::violation_count)
            .sum()
    }

    /// Loops that still pay after the 8% transaction tax, whoever owns them.
    pub fn surviving_tax(&self) -> usize {
        self.checks()
            .into_iter()
            .map(CheckOutcome::surviving_tax)
            .sum()
    }

    pub fn surviving_tax_owned(&self, ownership: Ownership) -> usize {
        self.checks()
            .into_iter()
            .map(|outcome| outcome.surviving_tax_owned(ownership))
            .sum()
    }

    pub fn violation_count_owned(&self, ownership: Ownership) -> usize {
        self.checks()
            .into_iter()
            .map(|outcome| outcome.violation_count_owned(ownership))
            .sum()
    }

    /// **The build gate.** Tax-surviving loops that fail the build, which is everything
    /// except the check-4 npc-to-npc hauls when `[import] gate_on_npc_arbitrage` is off.
    pub fn gating(&self, gate_on_npc_arbitrage: bool) -> usize {
        self.checks()
            .into_iter()
            .map(|outcome| outcome.gating(gate_on_npc_arbitrage))
            .sum()
    }

    /// Tax-surviving check-4 hauls between two NPC stations — inherited from the TQ snapshot,
    /// and the one class the gate can be told to forgive.
    pub fn inherited_npc_hauls(&self) -> usize {
        self.cross_station.surviving_tax_owned(Ownership::NpcToNpc)
    }

    /// The worst loops on one side of the ownership split, by ratio.
    pub fn worst_owned(&self, ownership: Ownership, limit: usize) -> Vec<&Violation> {
        let mut all = self
            .violations()
            .filter(|violation| violation.ownership == ownership)
            .collect::<Vec<_>>();
        all.sort_by(|left, right| {
            right
                .ratio()
                .partial_cmp(&left.ratio())
                .unwrap_or(Ordering::Equal)
                .then(left.check.cmp(&right.check))
                .then(left.type_id.cmp(&right.type_id))
        });
        all.truncate(limit);
        all
    }

    /// Worst offenders across all three checks, by ratio.
    pub fn worst(&self, limit: usize) -> Vec<&Violation> {
        let mut all = self.violations().collect::<Vec<_>>();
        all.sort_by(|left, right| {
            right
                .ratio()
                .partial_cmp(&left.ratio())
                .unwrap_or(Ordering::Equal)
                .then(left.check.cmp(&right.check))
                .then(left.type_id.cmp(&right.type_id))
        });
        all.truncate(limit);
        all
    }

    /// `(check, type_id)` for every violation — used to diff two yields.
    pub fn violation_keys(&self) -> BTreeSet<(Check, u32)> {
        self.violations()
            .map(|violation| (violation.check, violation.type_id))
            .collect()
    }
}

// -------------------------------------------------------------------------- the subcommand

/// `audit [--yield F] [--top N] [--allow-violations] [--skip-overlay]`.
///
/// Reconstructs the canonical five-hub book exactly as `build` writes it. The cached snapshot
/// supplies reference authority but contributes no rows. It reads no database: a build is a
/// pure function of config, static data, price manifest and snapshot, and the audit runs that
/// same function. Returns `Ok(true)` when the existing Phase 1 checks pass.
pub fn run_audit(
    config_path: &Path,
    refine_yield: f64,
    top: usize,
    allow_violations: bool,
    skip_overlay: bool,
    candidate_db: Option<&Path>,
) -> Result<bool> {
    run_audit_with_stations(
        config_path,
        refine_yield,
        top,
        allow_violations,
        skip_overlay,
        candidate_db,
        None,
    )
}
/// Workbench passes a previously validated NPC station set for a distributed candidate.
/// The economic algorithms and thresholds below are shared with the canonical audit.
pub(crate) fn run_audit_with_stations(
    config_path: &Path,
    refine_yield: f64,
    top: usize,
    allow_violations: bool,
    skip_overlay: bool,
    candidate_db: Option<&Path>,
    candidate_station_ids: Option<&[u64]>,
) -> Result<bool> {
    anyhow::ensure!(
        candidate_station_ids.is_none() || candidate_db.is_some(),
        "explicit audit stations require a persisted candidate"
    );
    anyhow::ensure!(
        candidate_station_ids.is_none_or(|ids| !ids.is_empty()),
        "empty candidate audit station set"
    );
    if !(refine_yield.is_finite() && refine_yield > 0.0 && refine_yield <= 1.0) {
        bail!(
            "--yield {refine_yield} is not a refine fraction. It must be greater than 0 and at \
             most 1.0 (1.0 = perfect refine, which is the worst case for the seed and the only \
             yield the build gate is evaluated at)."
        );
    }

    let config = SeederConfig::load(config_path)?;
    if skip_overlay {
        bail!(
            "--skip-overlay is not supported by the canonical five-hub audit because snapshot references are required even though snapshot rows are never written"
        );
    }
    let (static_data_dir, static_data_dir_source) =
        resolve_static_data_dir(config.input.static_data_dir.as_deref());

    println!("{} market-seederv3 audit", style("[config]").cyan().bold());
    crate::print_path("config file", config_path);
    println!(
        "  {:<24}: {} (from {})",
        "static data dir",
        crate::display_path(&static_data_dir),
        static_data_dir_source.label()
    );
    crate::print_path("price manifest", &config.price_manifest.path);
    println!(
        "  {:<24}: {} identical hubs, quantity {} per enabled side",
        "canonical market",
        config.canonical_market.station_ids.len(),
        config.canonical_market.quantity
    );
    println!(
        "  {:<24}: {} independent Sell/Buy definitions; legacy cost pricing {}",
        "pricing profiles",
        config.pricing_profiles.definitions.len(),
        config.index_seed.pricing.mode_key()
    );
    println!(
        "  {:<24}: {} (plan §11: which rung a leaf tries first)",
        "leaf price source",
        config.index_seed.leaf_price_source.key()
    );
    println!(
        "  {:<24}: {}",
        "pricing rules",
        config.index_seed.pricing_rules().join(", ")
    );
    println!(
        "  {:<24}: {} primary, {} second pass; gate evaluated at {}",
        "refine yield",
        percent(refine_yield),
        percent(REALISTIC_YIELD),
        percent(PERFECT_YIELD)
    );

    println!();
    println!(
        "{} this command reports; it never changes a price. A violation is a decision for a \
         human,",
        style("[scope]").yellow().bold()
    );
    println!("  not something the audit is allowed to paper over (plan §8.3, §11).");

    // ------------------------------------------------------------------ resolve the plan
    println!();
    let static_data = StaticData::load(&static_data_dir)?;
    let reprocessing = ReprocessingStatic::load(&static_data_dir)?;
    let classification = Classification::compute(&static_data, &config.junk_seed)?;

    // Parse the snapshot before planning, exactly as build does, for BPO and rare references.
    // Its station rows are never merged into the audited canonical book.
    let overlay = {
        match crate::snapshot::find_cached_snapshot(&config.source.download_dir)? {
            Some(cached) => {
                println!(
                    "{} reading price authority from {} ({})",
                    style("[snapshot reference]").cyan().bold(),
                    crate::display_path(&cached.path),
                    cached.size_label()
                );
                Some(crate::overlay::import_overlay(
                    &cached.path,
                    &static_data,
                    config.import.order_filter,
                    config.import.npc_order_duration_threshold_days,
                    &BTreeSet::new(),
                    config.price_manifest.reference_solar_system_id,
                    false,
                )?)
            }
            None => bail!(
                "no market-order snapshot is cached in {}, so the audit cannot resolve canonical \
                 BPO and rare reference prices.\n\n  Run: market-seederv3 snapshot-info \
                 --download --reuse-download",
                crate::display_path(&config.source.download_dir)
            ),
        }
    };
    let sde = read_sde_build(&static_data_dir);
    let price_manifest = PriceManifest::load(&config.price_manifest.path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no price manifest at {}. The audit prices the seed exactly as the build does and \
             never invents a price, so there is nothing to audit without it. Run \
             `market-seederv3 refresh-prices` first.",
            config.price_manifest.path.to_string_lossy()
        )
    })?;
    if price_manifest.sde_build != sde.build {
        println!(
            "{} price manifest sdeBuild {} != static data build {}; the audited prices are not \
             the prices this static data would produce.",
            style("[warn]").yellow().bold(),
            price_manifest
                .sde_build
                .map(|build| build.to_string())
                .unwrap_or_else(|| "null".to_string()),
            sde.build
                .map(|build| build.to_string())
                .unwrap_or_else(|| "null".to_string())
        );
    }
    // A stale legacy rule-C manifest can already have raised the underlying price. That
    // cannot be repaired safely after the fact, so canonical audit rejects it.
    let configured_rules = config.index_seed.pricing_rules();
    if price_manifest.pricing_rules != configured_rules {
        bail!(
            "the manifest was written with pricingRules [{}] but this config asks for [{}]. \
             Market v1 requires a refreshed manifest so the profile-aware reprocessing floor \
             raises Sell only rather than inheriting a cost-mutating legacy floor.",
            if price_manifest.pricing_rules.is_empty() {
                "none recorded".to_string()
            } else {
                price_manifest.pricing_rules.join(", ")
            },
            configured_rules.join(", ")
        );
    }

    // The audit prices nothing itself: every seed price comes from the manifest, which the
    // refresh already wrote with rules A/B/C applied. This cost map exists only to name the
    // leaves blocking a dropped type, so it runs with no overrides.
    let captures = price_manifest.captured_leaf_prices(CapturePolicy::from_config(&config));
    let no_twins = OverridePrices::new();
    let no_floors = OverridePrices::new();
    let mut cost_map = CostMap::new(CostInputs {
        data: &static_data,
        producers: &classification.producers,
        invention_lineage: &classification.invention_lineage,
        captures: &captures,
        model: config.index_seed.pricing,
        leaf_source: config.index_seed.leaf_price_source,
        twins: &no_twins,
        floors: &no_floors,
    });
    let cost_report = CostReport::build(&mut cost_map, classification.seeded_types().into_iter());
    // Same independent profile table the build uses.
    let pricing = SeedPricingProfiles::from_config(&config);
    let verified_build = sde.build.ok_or_else(|| {
        anyhow::anyhow!(
            "generated static data has no verified SDE build; Rare variant-family metadata cannot be used"
        )
    })?;
    let families = VariantFamilies::load_for_generated_data(&static_data_dir, verified_build)?;
    println!(
        "  raw Rare family metadata: {} (verified build {})",
        families.types_path.display(),
        families.build
    );
    let required_rare =
        required_external_market_references_from_config(&classification, &static_data, &config)?;
    let sibling_fallbacks = strict_sibling_family_fallbacks(
        &required_rare,
        &static_data,
        &families,
        &price_manifest,
        overlay.as_ref(),
        CapturePolicy::from_config(&config),
    );
    let partial_plan = SeedPlan::resolve_canonical_partial(
        &classification,
        &static_data,
        &cost_report,
        &price_manifest,
        overlay.as_ref(),
        &config,
        &sibling_fallbacks,
    )?;
    let industry_semantics =
        crate::funded::IndustrySemantics::load(&static_data_dir, &static_data)?;
    let funded_fallback_report = crate::funded::resolve_rare_t2_fallbacks(
        &partial_plan,
        &static_data,
        &industry_semantics,
        &config,
    );
    crate::funded::print_rare_t2_funded_resolution(&funded_fallback_report, &config);
    let mut rare_fallbacks = sibling_fallbacks;
    rare_fallbacks.extend(funded_fallback_report.fallbacks);
    let production_intermediate_report = crate::funded::resolve_production_intermediates(
        &partial_plan,
        &static_data,
        &industry_semantics,
        &config,
    )?;
    crate::funded::print_production_intermediate_resolution(
        &production_intermediate_report,
        &config,
    );
    let plan = SeedPlan::resolve_canonical_with_supply_ladder(
        &classification,
        &static_data,
        &cost_report,
        &price_manifest,
        overlay.as_ref(),
        &config,
        &rare_fallbacks,
        &production_intermediate_report.prices,
    )?;
    println!();
    println!(
        "{} {} seedable types with exclusive canonical role/side ownership",
        style("[seed plan]").cyan().bold(),
        count(plan.seedable_count())
    );
    crate::build::print_deferral_report(&plan);

    let mut rows = plan_canonical_seed_rows(
        &plan,
        &static_data,
        &pricing,
        config.canonical_market.quantity,
    );
    crate::build::print_skillbook_supply_report(&plan, &rows);
    let safety_report = crate::funded::apply_market_safety(
        &mut rows,
        &static_data,
        &reprocessing,
        &industry_semantics,
        &config,
    )?;
    crate::build::print_rare_fallback_report(
        &plan,
        &rows,
        &config,
        &funded_fallback_report.unfunded,
    );
    crate::funded::print_market_safety_report(&safety_report);

    let stations = candidate_station_ids
        .unwrap_or(&config.canonical_market.station_ids)
        .iter()
        .map(|station_id| {
            static_data
                .station(*station_id)
                .map(SeedStation::from)
                .ok_or_else(|| {
                    anyhow::anyhow!("canonical station {station_id} is absent from static data")
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let station = stations
        .first()
        .expect("five canonical stations are validated");
    let book = if let Some(candidate_path) = candidate_db {
        println!(
            "{} reading persisted candidate book (read-only): {}",
            style("[candidate]").cyan().bold(),
            crate::display_path(candidate_path)
        );
        SeedBook::from_candidate_db(candidate_path, &rows, &stations, &static_data)?
    } else {
        SeedBook::from_rows_at_stations(&rows, &stations)
    };
    let direct_lp_rewards = load_lp_reward_type_ids(&config.market_roles.lp_offer_catalog_path)?;
    check_reward_sell_invariants(&book, &static_data, &direct_lp_rewards)?;
    check_progression_npc_supply_invariants(
        &book,
        &static_data,
        overlay.as_ref(),
        &config.market_roles.progression_npc_group_ids,
        &config.market_roles.progression_npc_type_ids,
    )?;

    println!();
    println!(
        "{} {} seeded types priced ({}) across {} stations",
        style("[book]").cyan().bold(),
        count(book.len()),
        book.composition()
            .iter()
            .map(|(label, size)| format!("{label} {}", count(*size)))
            .collect::<Vec<_>>()
            .join(" | "),
        count(book.station_count())
    );
    println!(
        "  {:<24}: the best ask and the best bid ANYWHERE, because a player will always use \
         the better price",
        "every price is"
    );
    println!(
        "  {:<24}: prices at {} are unmarked; anything else is tagged \" @ station\"",
        "in the report", station.station_name
    );
    println!(
        "  {:<24}: {} types, {} compressed/uncompressed pairs",
        "reprocessing table",
        count(reprocessing.len()),
        count(reprocessing.compressed_by_source.len())
    );
    println!(
        "  {:<24}: {} product types",
        "producer index",
        count(classification.producers.len())
    );

    // --------------------------------------------------------------------- run the checks
    let primary = AuditReport::run(
        &book,
        &reprocessing,
        &classification.producers,
        &static_data,
        &industry_semantics,
        refine_yield,
    );
    let gate = if (refine_yield - PERFECT_YIELD).abs() < f64::EPSILON {
        primary.clone()
    } else {
        AuditReport::run(
            &book,
            &reprocessing,
            &classification.producers,
            &static_data,
            &industry_semantics,
            PERFECT_YIELD,
        )
    };
    let realistic = if (refine_yield - REALISTIC_YIELD).abs() < f64::EPSILON {
        primary.clone()
    } else {
        AuditReport::run(
            &book,
            &reprocessing,
            &classification.producers,
            &static_data,
            &industry_semantics,
            REALISTIC_YIELD,
        )
    };

    let gate_on_npc_arbitrage = config.import.gate_on_npc_arbitrage;
    for outcome in primary.checks() {
        print_check(outcome, refine_yield, gate_on_npc_arbitrage);
    }

    print_worst_table(&primary, top);
    print_yield_comparison(&primary, &realistic);

    // ------------------------------------------------------------------------- the verdict
    println!();
    let gating = gate.gating(gate_on_npc_arbitrage);
    let inherited = gate.inherited_npc_hauls();
    let surviving = gate.surviving_tax();
    let passed = gating == 0;
    // Printed on every run, pass or fail: the residue must never be invisible.
    println!(
        "{} {} loops evaluated at {} yield; {} positive, {} surviving the {}% tax — {} \
         seeder-involved, {} npc-to-npc.",
        style("[gate]").cyan().bold(),
        count(
            gate.checks()
                .into_iter()
                .map(|outcome| outcome.checked)
                .sum()
        ),
        percent(PERFECT_YIELD),
        count(gate.violation_count()),
        count(surviving),
        (SEED_BID_TAX * 100.0).round(),
        count(gate.surviving_tax_owned(Ownership::SeederInvolved)),
        count(gate.surviving_tax_owned(Ownership::NpcToNpc))
    );
    println!(
        "  {} of those gate the existing audit; {} legacy npc-to-npc classifications remain \
         (canonical builds write no snapshot rows).",
        count(gating),
        count(inherited)
    );

    if passed {
        println!(
            "{} CLEAN under the existing Phase 1 checks — no measured loop surviving the {}% \
             transaction tax gates this audit.",
            style("[verdict]").green().bold(),
            (SEED_BID_TAX * 100.0).round()
        );
        if inherited > 0 {
            println!(
                "  {} inherited TQ-to-TQ haul loop(s) remain in the book and are listed under \
                 check 4 above. They are TQ's own NPC prices at TQ's own stations (plan \
                 decision 2); set [import] gate_on_npc_arbitrage = true to fail the build on \
                 them instead.",
                count(inherited)
            );
            if let Some(worst) = gate
                .worst_owned(Ownership::NpcToNpc, 1)
                .first()
                .filter(|violation| violation.check == Check::CrossStation)
            {
                println!(
                    "  worst inherited: x{:.3} on {} ({}) — {}",
                    worst.ratio(),
                    worst.name,
                    worst.type_id,
                    worst.loop_text
                );
            }
        }
    } else {
        println!(
            "{} EXPLOITABLE — {} positive loops at {} yield, {} still profitable after the {}% \
             transaction tax, {} of which gate this build.",
            style("[verdict]").red().bold(),
            count(gate.violation_count()),
            percent(PERFECT_YIELD),
            count(surviving),
            (SEED_BID_TAX * 100.0).round(),
            count(gating)
        );
        // Name a loop the build is actually failing on, not merely the worst one on paper.
        let mut gating_loops = gate
            .violations()
            .filter(|violation| violation.gates(gate_on_npc_arbitrage))
            .collect::<Vec<_>>();
        gating_loops.sort_by(|left, right| {
            right
                .ratio()
                .partial_cmp(&left.ratio())
                .unwrap_or(Ordering::Equal)
                .then(left.type_id.cmp(&right.type_id))
        });
        if let Some(violation) = gating_loops.first() {
            println!(
                "  worst gating: x{:.3} on {} ({}, {}) — {}",
                violation.ratio(),
                violation.name,
                violation.type_id,
                violation.ownership.key(),
                violation.loop_text
            );
        }
        println!(
            "  the audit reports and never repairs: fix these in the pricing rules (plan §11) \
             and re-run."
        );
    }
    println!(
        "{} Phase 2 funded-production, ore progression, and final-Buy reprocessing safety \
         were applied to the reconstructed plan. Production deployment remains blocked on \
         independent review.",
        style("[phase boundary]").yellow().bold()
    );

    if !passed && allow_violations {
        println!(
            "{} --allow-violations: exiting 0 despite {} gating loops. Never use this on a \
             real seed build.",
            style("[gate]").yellow().bold(),
            count(gating)
        );
    }
    Ok(passed || allow_violations)
}

fn print_check(outcome: &CheckOutcome, refine_yield: f64, gate_on_npc_arbitrage: bool) {
    println!();
    println!(
        "{} {} pump — {}{}",
        style(format!("[check {}]", outcome.check.number()))
            .cyan()
            .bold(),
        outcome.check.key(),
        outcome.check.headline(),
        if outcome.check == Check::Reprocess {
            format!(" (yield {})", percent(refine_yield))
        } else {
            String::new()
        }
    );
    println!("  {:<24}: {}", "checked", count(outcome.checked));
    println!(
        "  {:<24}: {}{}",
        "skipped",
        count(outcome.skipped_total()),
        if outcome.skipped.is_empty() {
            String::new()
        } else {
            format!(
                "  ({})",
                outcome
                    .skipped
                    .iter()
                    .map(|(reason, size)| format!("{reason}: {}", count(*size)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    let violations = outcome.violation_count();
    let surviving = outcome.surviving_tax();
    let line = format!(
        "{}  ({} survive the {}% tax)",
        count(violations),
        count(surviving),
        (SEED_BID_TAX * 100.0).round()
    );
    println!(
        "  {:<24}: {}",
        "violations",
        if violations == 0 {
            style(line).green()
        } else {
            style(line).red().bold()
        }
    );
    // The split is printed for every check, always, so the inherited residue can never become
    // invisible by being reported only when someone asks for it.
    println!(
        "  {:<24}: {} seeder-involved ({} survive the tax) | {} npc-to-npc ({} survive)",
        "  by ownership",
        count(outcome.violation_count_owned(Ownership::SeederInvolved)),
        count(outcome.surviving_tax_owned(Ownership::SeederInvolved)),
        count(outcome.violation_count_owned(Ownership::NpcToNpc)),
        count(outcome.surviving_tax_owned(Ownership::NpcToNpc))
    );
    for note in &outcome.notes {
        println!("  note: {note}");
    }

    for violation in outcome.violations_owned(Ownership::SeederInvolved) {
        print_violation(violation, gate_on_npc_arbitrage);
    }

    let inherited = outcome
        .violations_owned(Ownership::NpcToNpc)
        .collect::<Vec<_>>();
    if inherited.is_empty() {
        return;
    }
    println!();
    println!(
        "  {} {} loop(s) with NO seeder-priced leg — both sides are NPC rows imported verbatim \
         from the TQ snapshot.",
        style("[inherited from TQ]").yellow().bold(),
        count(inherited.len())
    );
    println!(
        "      TQ's own fixed NPC prices; plan decision 2 keeps the catalog TQ-authentic, and \
         no pricing rule of ours"
    );
    println!(
        "      can close these without discarding NPC rows CCP ships. {}",
        if outcome.check == Check::CrossStation {
            if gate_on_npc_arbitrage {
                "GATING ([import] gate_on_npc_arbitrage = true).".to_string()
            } else {
                "Reported, not gating ([import] gate_on_npc_arbitrage = false).".to_string()
            }
        } else {
            "A transformation loop gates whoever owns it.".to_string()
        }
    );
    for violation in inherited {
        print_violation(violation, gate_on_npc_arbitrage);
    }
}

fn print_violation(violation: &Violation, gate_on_npc_arbitrage: bool) {
    println!();
    println!(
        "  x{:<8.3} {:<7} {} [{}] {}",
        violation.ratio(),
        violation.type_id,
        violation.name,
        violation.bucket,
        violation.ownership.key()
    );
    println!("      {}", violation.loop_text);
    println!(
        "      outlay {} -> revenue {} ({} after the {}% tax) -> {}",
        crate::isk(violation.outlay),
        crate::isk(violation.revenue),
        crate::isk(violation.after_tax()),
        (SEED_BID_TAX * 100.0).round(),
        if !violation.survives_tax() {
            style("dies to the tax".to_string()).yellow()
        } else if violation.gates(gate_on_npc_arbitrage) {
            style("SURVIVES THE TAX — GATES THE BUILD".to_string())
                .red()
                .bold()
        } else {
            style("survives the tax, inherited from TQ, not gating".to_string()).yellow()
        }
    );
}

fn print_worst_table(report: &AuditReport, top: usize) {
    println!();
    println!(
        "{} worst {} offenders by ratio, all checks, yield {}",
        style("[top]").cyan().bold(),
        top,
        percent(report.refine_yield)
    );
    let worst = report.worst(top);
    if worst.is_empty() {
        println!("  none — no loop returns more than it costs.");
        return;
    }
    println!(
        "  {:>4}  {:>9}  {:<12}  {:>7}  {:<40}  {:<5}  {:<15}  {:>18}  {:>18}  {}",
        "rank", "ratio", "check", "type", "name", "bkt", "owner", "outlay", "revenue", "after tax?"
    );
    for (rank, violation) in worst.iter().enumerate() {
        println!(
            "  {:>4}  {:>9}  {:<12}  {:>7}  {:<40}  {:<5}  {:<15}  {:>18}  {:>18}  {}",
            rank + 1,
            format!("x{:.3}", violation.ratio()),
            violation.check.key(),
            violation.type_id,
            truncate(&violation.name, 40),
            violation.bucket,
            violation.ownership.key(),
            crate::isk(violation.outlay),
            crate::isk(violation.revenue),
            if violation.survives_tax() {
                "SURVIVES"
            } else {
                "dies"
            }
        );
    }
}

fn print_yield_comparison(primary: &AuditReport, realistic: &AuditReport) {
    println!();
    println!(
        "{} {} (primary) vs {} (realistic station refine)",
        style("[yield]").cyan().bold(),
        percent(primary.refine_yield),
        percent(realistic.refine_yield)
    );
    if (primary.refine_yield - realistic.refine_yield).abs() < f64::EPSILON {
        println!("  same yield — nothing to compare.");
        return;
    }
    for (left, right) in primary.checks().into_iter().zip(realistic.checks()) {
        if left.check == Check::Reprocess {
            println!(
                "  check {} {:<12}: {} -> {} violations, {} -> {} survive the tax",
                left.check.number(),
                left.check.key(),
                count(left.violation_count()),
                count(right.violation_count()),
                count(left.surviving_tax()),
                count(right.surviving_tax())
            );
        } else {
            println!(
                "  check {} {:<12}: {} violations, unchanged — yield does not enter this loop",
                left.check.number(),
                left.check.key(),
                count(left.violation_count())
            );
        }
    }
    let gone = primary
        .violation_keys()
        .difference(&realistic.violation_keys())
        .copied()
        .collect::<Vec<_>>();
    if gone.is_empty() {
        println!("  the same loops violate at both yields.");
        return;
    }
    let named = gone
        .iter()
        .take(12)
        .map(|(_, type_id)| {
            primary
                .violations()
                .find(|violation| violation.type_id == *type_id)
                .map(|violation| format!("{} {}", violation.type_id, violation.name))
                .unwrap_or_else(|| type_id.to_string())
        })
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "  violating at {} but not at {}: {}{}",
        percent(primary.refine_yield),
        percent(realistic.refine_yield),
        named,
        if gone.len() > 12 {
            format!(", … and {} more", gone.len() - 12)
        } else {
            String::new()
        }
    );
}

// ---------------------------------------------------------------------------- formatting

/// The three biggest contributors of a loop's material list, with the tail summarised.
///
/// `side` says which price each part was taken at, so the station can be named when it is not
/// the index station. Unmarked parts are at the index station; the report header says so once
/// rather than repeating it forty times per line.
fn describe_parts(book: &SeedBook, parts: &[(u32, f64, f64)], side: PriceSide) -> String {
    if parts.is_empty() {
        return "nothing seeded".to_string();
    }
    let mut sorted = parts.to_vec();
    sorted.sort_by(|left, right| {
        right
            .2
            .partial_cmp(&left.2)
            .unwrap_or(Ordering::Equal)
            .then(left.0.cmp(&right.0))
    });
    const NAMED: usize = 3;
    let mut text = sorted
        .iter()
        .take(NAMED)
        .map(|(type_id, quantity, value)| {
            format!(
                "{} x {} ({}){}",
                trim_float(*quantity),
                book.name(*type_id),
                crate::isk(*value),
                book.qualify(*type_id, side)
            )
        })
        .collect::<Vec<_>>()
        .join(" + ");
    if sorted.len() > NAMED {
        let rest: f64 = sorted.iter().skip(NAMED).map(|(_, _, value)| value).sum();
        text.push_str(&format!(
            " + {} more ({})",
            sorted.len() - NAMED,
            crate::isk(rest)
        ));
    }
    text
}

fn producer_run_label(per_run: i64) -> String {
    count(per_run.max(0) as usize)
}

/// `100.0%` / `90.6%`.
fn percent(fraction: f64) -> String {
    format!("{:.1}%", fraction * 100.0)
}

/// Quantities that are usually whole but can be fractional (0.25 x Tritanium per unit).
fn trim_float(value: f64) -> String {
    if value.is_finite() && (value - value.round()).abs() < 1e-9 {
        count(value.round().max(0.0) as usize)
    } else {
        format!("{value:.4}")
    }
}

/// Thousands-grouped integer (the crate-wide helper, kept under the short local name the
/// report lines are written against).
fn count(value: usize) -> String {
    crate::count(value)
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut shortened = text
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    shortened.push('…');
    shortened
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;
    use crate::staticdata::{
        BlueprintActivities, BlueprintActivity, BlueprintDefinition, DogmaProjection,
        InventionActivity, InventionProductEntry, ItemTypeRecord, MaterialEntry, ProductEntry,
        ReprocessingMaterial, ReprocessingType,
    };

    const TRITANIUM: u32 = 34;
    const PYERITE: u32 = 35;

    const JITA_4_4: u64 = 60_003_760;
    const RENS: u64 = 60_004_588;
    const AMARR: u64 = 60_008_494;

    /// A price at a station, owned by whoever seeds that station in these fixtures: Jita 4-4
    /// is the index station, so a price there is ours; everywhere else is the NPC overlay.
    /// That is the real shape of the book — the index/junk pass writes one station and the
    /// overlay writes the other ~5,000 — and [`npc_at`] is there for the exception (an overlay
    /// row at Jita 4-4 for a type the index pass does not seed).
    fn at(station_id: u64, price: f64) -> StationPrice {
        priced(
            station_id,
            price,
            if station_id == JITA_4_4 {
                PriceOrigin::Seeder
            } else {
                PriceOrigin::NpcOverlay
            },
        )
    }

    fn npc_at(station_id: u64, price: f64) -> StationPrice {
        priced(station_id, price, PriceOrigin::NpcOverlay)
    }

    fn priced(station_id: u64, price: f64, origin: PriceOrigin) -> StationPrice {
        StationPrice {
            price,
            station_id,
            station_name: match station_id {
                JITA_4_4 => "Jita IV - Moon 4 - Caldari Navy Assembly Plant",
                RENS => "Rens VI - Moon 8 - Brutor Tribe Treasury",
                AMARR => "Amarr VIII (Oris) - Emperor Family Academy",
                _ => "somewhere else",
            }
            .to_string(),
            origin,
        }
    }

    /// Both sides at the index station, at the real 3x/1x pricing minus the floor so the
    /// fixtures stay readable.
    fn side(type_id: u32, name: &str, cost: f64) -> SeedSide {
        SeedSide {
            type_id,
            name: name.to_string(),
            bucket: "B",
            cost,
            ask: BestPrice {
                best: Some(at(JITA_4_4, cost * 3.0)),
                elsewhere: None,
            },
            bid: BestPrice {
                best: Some(at(JITA_4_4, cost)),
                elsewhere: None,
            },
        }
    }

    /// One type priced at named stations: `(station, ask)` and `(station, bid)` candidates,
    /// absorbed exactly as the real book absorbs them.
    fn side_at(type_id: u32, name: &str, asks: &[(u64, f64)], bids: &[(u64, f64)]) -> SeedSide {
        let mut ask = BestPrice::default();
        for (station_id, price) in asks {
            ask.absorb_ask(at(*station_id, *price));
        }
        let mut bid = BestPrice::default();
        for (station_id, price) in bids {
            bid.absorb_bid(at(*station_id, *price));
        }
        SeedSide {
            type_id,
            name: name.to_string(),
            bucket: "C",
            cost: 0.0,
            ask,
            bid,
        }
    }

    /// One price at the index station.
    fn one_at(price: f64) -> BestPrice {
        BestPrice {
            best: Some(at(JITA_4_4, price)),
            elsewhere: None,
        }
    }

    fn seed_book(sides: impl IntoIterator<Item = SeedSide>) -> SeedBook {
        let mut book = SeedBook {
            index_station_id: Some(JITA_4_4),
            ..SeedBook::default()
        };
        for side in sides {
            book.insert(side);
        }
        book
    }

    fn refines(type_id: u32, portion_size: i64, materials: &[(u32, i64)]) -> ReprocessingType {
        ReprocessingType {
            type_id,
            name: format!("type {type_id}"),
            portion_size,
            is_refinable: true,
            is_recyclable: true,
            family: "test".to_string(),
            materials: materials
                .iter()
                .map(|(type_id, quantity)| ReprocessingMaterial {
                    type_id: *type_id,
                    quantity: *quantity,
                })
                .collect(),
        }
    }

    fn repro_table(types: Vec<ReprocessingType>, pairs: &[(u32, u32)]) -> ReprocessingStatic {
        ReprocessingStatic::new(types, pairs.iter().copied().collect())
    }

    fn item(type_id: u32) -> ItemTypeRecord {
        ItemTypeRecord {
            type_id,
            group_id: Some(1),
            category_id: Some(7),
            group_name: Some("Test".to_string()),
            name: format!("type {type_id}"),
            mass: None,
            volume: None,
            capacity: None,
            portion_size: Some(1),
            race_id: None,
            base_price: Some(1.0),
            market_group_id: Some(1),
            icon_id: None,
            sound_id: None,
            graphic_id: None,
            radius: None,
            published: true,
        }
    }

    fn static_data(blueprints: Vec<BlueprintDefinition>) -> StaticData {
        let type_ids = blueprints
            .iter()
            .flat_map(|blueprint| {
                let activities = [
                    blueprint.activities.manufacturing.as_ref(),
                    blueprint.activities.reaction.as_ref(),
                ];
                activities
                    .into_iter()
                    .flatten()
                    .flat_map(|activity| {
                        activity
                            .materials
                            .iter()
                            .map(|entry| entry.type_id)
                            .chain(activity.products.iter().map(|entry| entry.type_id))
                    })
                    .chain(std::iter::once(blueprint.blueprint_type_id))
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>();
        let item_types = type_ids.into_iter().map(item).collect::<Vec<_>>();
        let item_type_index = item_types
            .iter()
            .enumerate()
            .map(|(index, item)| (item.type_id, index))
            .collect();
        StaticData {
            dir: PathBuf::from("."),
            stations: Vec::new(),
            station_index: HashMap::new(),
            solar_systems: Vec::new(),
            solar_system_index: HashMap::new(),
            item_types,
            item_type_index,
            blueprints,
            dogma: HashMap::<u32, DogmaProjection>::new(),
            compressed_type_ids: BTreeSet::new(),
        }
    }

    /// A [`ProducerIndex`] built the real way, from blueprint rows, so compression tests
    /// exercise the same static collision rules as production.
    fn producers(blueprints: Vec<BlueprintDefinition>) -> ProducerIndex {
        ProducerIndex::build(&static_data(blueprints))
    }

    fn test_semantics() -> IndustrySemantics {
        IndustrySemantics::fixture(1.0)
    }

    fn ask_only(type_id: u32, name: &str, ask: f64) -> SeedSide {
        SeedSide {
            type_id,
            name: name.to_string(),
            bucket: "B",
            cost: ask,
            ask: one_at(ask),
            bid: BestPrice::default(),
        }
    }

    fn blueprint(
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

    #[test]
    fn independent_reward_gate_rejects_lp_blueprint_product_sell() {
        let blueprint_type_id = 72_923;
        let product_type_id = 72_811;
        let mut data = static_data(vec![blueprint(
            blueprint_type_id,
            product_type_id,
            1,
            &[(TRITANIUM, 100)],
        )]);
        data.item_types
            .iter_mut()
            .find(|item| item.type_id == product_type_id)
            .expect("fixture product")
            .category_id = Some(6);
        let book = seed_book([ask_only(product_type_id, "Cyclone Fleet Issue", 100.0)]);

        let error =
            check_reward_sell_invariants(&book, &data, &BTreeSet::from([blueprint_type_id]))
                .expect_err("an LP-blueprint product Sell must fail independently of its role");
        assert!(error.to_string().contains("LP-offered blueprint 72923"));
        assert!(error.to_string().contains("type 72811"));
    }

    #[test]
    fn independent_progression_npc_gate_requires_exact_sell_and_forbids_buy() {
        const CORE: u32 = 56_201;
        let mut data = static_data(vec![blueprint(90_001, 90_002, 1, &[(CORE, 1)])]);
        let core = data
            .item_types
            .iter_mut()
            .find(|item| item.type_id == CORE)
            .expect("fixture core");
        core.group_id = Some(4_086);
        core.name = "Astrahus Upwell Quantum Core".to_string();
        let mut overlay = crate::overlay::OverlayImport::default();
        overlay.npc_min_sell_cents.insert(CORE, 60_000_000_000);
        let groups = [4_086];

        let valid = seed_book([ask_only(
            CORE,
            "Astrahus Upwell Quantum Core",
            600_000_000.0,
        )]);
        check_progression_npc_supply_invariants(&valid, &data, Some(&overlay), &groups, &[])
            .expect("exact NPC Sell-only supply passes");

        let wrong = seed_book([ask_only(CORE, "Astrahus Upwell Quantum Core", 1.0)]);
        assert!(
            format!(
                "{:#}",
                check_progression_npc_supply_invariants(
                    &wrong,
                    &data,
                    Some(&overlay),
                    &groups,
                    &[],
                )
                .expect_err("wrong reference must fail")
            )
            .contains("differs from NPC reference")
        );

        let mut forbidden_buy = ask_only(CORE, "Astrahus Upwell Quantum Core", 600_000_000.0);
        forbidden_buy.bid = one_at(1.0);
        assert!(
            format!(
                "{:#}",
                check_progression_npc_supply_invariants(
                    &seed_book([forbidden_buy]),
                    &data,
                    Some(&overlay),
                    &groups,
                    &[],
                )
                .expect_err("Buy liquidity must fail")
            )
            .contains("forbidden canonical Buy")
        );

        assert!(
            format!(
                "{:#}",
                check_progression_npc_supply_invariants(&valid, &data, None, &groups, &[])
                    .expect_err("missing NPC proof must fail")
            )
            .contains("no exact NPC Sell reference")
        );
    }

    #[test]
    fn independent_reward_gate_rejects_placeholder_hull_sell() {
        let blueprint_type_id = 33_327;
        let product_type_id = 3_756;
        let mut data = static_data(vec![blueprint(
            blueprint_type_id,
            product_type_id,
            1,
            &[(TRITANIUM, 1)],
        )]);
        data.item_types
            .iter_mut()
            .find(|item| item.type_id == product_type_id)
            .expect("fixture product")
            .category_id = Some(6);
        data.item_types
            .iter_mut()
            .find(|item| item.type_id == blueprint_type_id)
            .expect("fixture blueprint")
            .market_group_id = None;
        let book = seed_book([ask_only(product_type_id, "Gnosis", 4.38)]);

        let error = check_reward_sell_invariants(&book, &data, &BTreeSet::new())
            .expect_err("a placeholder reward-hull Sell must fail independently of its role");
        assert!(
            error
                .to_string()
                .contains("special placeholder blueprint 33327")
        );
        assert!(error.to_string().contains("type 3756"));
    }

    fn unpublished_blueprint(
        blueprint_type_id: u32,
        product_type_id: u32,
        materials: &[(u32, i64)],
    ) -> BlueprintDefinition {
        let mut definition = blueprint(blueprint_type_id, product_type_id, 1, materials);
        definition.published = false;
        definition
    }

    fn invention_source(
        source_blueprint_type_id: u32,
        output_blueprint_type_id: u32,
        materials: &[(u32, i64)],
    ) -> BlueprintDefinition {
        BlueprintDefinition {
            blueprint_type_id: source_blueprint_type_id,
            blueprint_name: format!("Blueprint {source_blueprint_type_id}"),
            product_type_id: 0,
            product_name: String::new(),
            max_production_limit: None,
            published: true,
            activities: BlueprintActivities {
                invention: Some(InventionActivity {
                    materials: materials
                        .iter()
                        .map(|(type_id, quantity)| MaterialEntry {
                            type_id: *type_id,
                            quantity: *quantity,
                        })
                        .collect(),
                    products: vec![InventionProductEntry {
                        type_id: output_blueprint_type_id,
                        quantity: 1,
                        probability: Some(1.0),
                    }],
                    skills: Vec::new(),
                    time: Some(100),
                }),
                ..BlueprintActivities::default()
            },
        }
    }

    #[test]
    fn a_reprocess_pump_is_detected_with_the_right_ratio() {
        // 900 refines into 100 Tritanium per unit: 200 ISK of minerals out of a 30 ISK ask.
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(900, "Pumped Ore", 10.0),
        ]);
        let reprocessing = repro_table(vec![refines(900, 100, &[(TRITANIUM, 10_000)])], &[]);

        let outcome = check_reprocess(&book, &reprocessing, PERFECT_YIELD);

        assert_eq!(outcome.checked, 1, "Tritanium has no refine row of its own");
        assert_eq!(
            *outcome.skipped.get(SKIP_NO_REPROCESSING_ROW).unwrap_or(&0),
            1,
            "and it is counted as skipped with the reason, not quietly ignored"
        );
        assert_eq!(outcome.violation_count(), 1);
        let violation = &outcome.violations[0];
        assert_eq!(violation.type_id, 900);
        assert_eq!(violation.check, Check::Reprocess);
        // 10,000 Tritanium / portion 100 = 100 per unit x bid 2.00 = 200.00 against a 30.00 ask.
        assert_eq!(violation.outlay, 30.0);
        assert_eq!(violation.revenue, 200.0);
        assert!((violation.ratio() - 200.0 / 30.0).abs() < 1e-12);
        assert!(violation.loop_text.contains("Tritanium"));
    }

    #[test]
    fn a_safe_item_is_not_flagged_and_an_unseeded_output_pays_nothing() {
        // The cost model's own shape: a type built from 22,400 Tritanium costs 44,800, asks
        // 134,400 and refines back into exactly 44,800 — a third of the ask.
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(901, "Honest Hull", 44_800.0),
        ]);
        let reprocessing = repro_table(
            vec![refines(901, 1, &[(TRITANIUM, 22_400), (7777, 5)])],
            &[],
        );

        let outcome = check_reprocess(&book, &reprocessing, PERFECT_YIELD);

        assert_eq!(
            outcome.violation_count(),
            0,
            "revenue is a third of the ask"
        );
        assert_eq!(outcome.checked, 1);
        assert!(
            outcome.notes.iter().any(|note| note.contains("unseeded")),
            "the unseeded output 7777 must be reported, not silently valued: {:?}",
            outcome.notes
        );
    }

    #[test]
    fn a_manufacture_pump_is_detected_and_a_sane_recipe_is_not() {
        // 902 bids 1,000 but builds out of 100 Tritanium bought at 6.00 = 600. Free money.
        // 903 stays below the ME10 funded cost and is not a profit.
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(902, "Overbid Widget", 1_000.0),
            side(903, "Fair Widget", 500.0),
            ask_only(9021, "Overbid Widget Blueprint", 1_000.0),
            ask_only(9031, "Fair Widget Blueprint", 1_000.0),
        ]);
        let data = static_data(vec![
            blueprint(9021, 902, 1, &[(TRITANIUM, 100)]),
            blueprint(9031, 903, 1, &[(TRITANIUM, 100)]),
        ]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.checked, 2);
        assert_eq!(*outcome.skipped.get(SKIP_NO_PRODUCER).unwrap_or(&0), 1);
        assert_eq!(outcome.violation_count(), 1);
        let violation = &outcome.violations[0];
        assert_eq!(violation.type_id, 902);
        assert_eq!(violation.outlay, 540.0);
        assert_eq!(violation.revenue, 1_000.0);
        assert!((violation.ratio() - 1_000.0 / 540.0).abs() < 1e-12);
    }

    #[test]
    fn per_run_output_divides_the_input_cost() {
        // 100 units per run out of 100 Tritanium: 6.00 of inputs per unit, not 600.
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(904, "Missile", 5.0),
            ask_only(9041, "Missile Blueprint", 1_000.0),
        ]);
        let data = static_data(vec![blueprint(9041, 904, 100, &[(TRITANIUM, 100)])]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.violation_count(), 0, "bid 5.00 < 6.00 of inputs");

        // Nudge the bid past the per-unit input cost and it must flag.
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(904, "Missile", 7.0),
            ask_only(9041, "Missile Blueprint", 1_000.0),
        ]);
        let outcome = check_manufacture(&book, &data, &test_semantics());
        assert_eq!(outcome.violation_count(), 1);
        assert_eq!(outcome.violations[0].outlay, 5.4);
    }

    #[test]
    fn an_unbuyable_material_is_skipped_not_flagged() {
        // 905 bids 1,000,000 against inputs that would cost nothing if an absent ask were
        // read as 0 — the exact bug this skip exists to prevent.
        let book = seed_book([side(905, "Needs An Unseeded Part", 1_000_000.0)]);
        let data = static_data(vec![blueprint(9051, 905, 1, &[(7777, 1)])]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.checked, 0);
        assert_eq!(outcome.violation_count(), 0);
        assert_eq!(
            *outcome.skipped.get(SKIP_UNFUNDED_PRODUCTION).unwrap_or(&0),
            1,
            "the recipe must be counted as skipped, with the reason"
        );
        assert_eq!(outcome.skipped_total(), 1);
    }

    #[test]
    fn seeded_materials_and_output_bid_do_not_replace_unavailable_bpo_access() {
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(9_100, "Unavailable-BPO Product", 1_000_000.0),
        ]);
        let data = static_data(vec![blueprint(9_101, 9_100, 1, &[(TRITANIUM, 1)])]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.checked, 0);
        assert_eq!(outcome.violation_count(), 0);
        assert_eq!(
            *outcome.skipped.get(SKIP_UNFUNDED_PRODUCTION).unwrap_or(&0),
            1
        );
        assert!(outcome.notes.iter().any(|note| note.contains("9101")));
    }

    #[test]
    fn published_definition_without_canonical_blueprint_sell_is_not_access() {
        let definition = blueprint(9_111, 9_110, 1, &[(TRITANIUM, 1)]);
        assert!(definition.published);
        let data = static_data(vec![definition]);
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(9_110, "Published But Inaccessible", 1_000_000.0),
        ]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.violation_count(), 0);
        assert_eq!(
            *outcome.skipped.get(SKIP_UNFUNDED_PRODUCTION).unwrap_or(&0),
            1
        );
    }

    #[test]
    fn unpublished_blueprint_is_not_accessible_even_if_a_sell_row_exists() {
        let data = static_data(vec![unpublished_blueprint(9_121, 9_120, &[(TRITANIUM, 1)])]);
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(9_120, "Unpublished Product", 1_000_000.0),
            ask_only(9_121, "Unpublished Blueprint", 1.0),
        ]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.violation_count(), 0);
        assert_eq!(*outcome.skipped.get(SKIP_NO_PRODUCER).unwrap_or(&0), 1);
        assert_eq!(
            *outcome.skipped.get(SKIP_UNFUNDED_PRODUCTION).unwrap_or(&0),
            1
        );
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.contains("unpublished"))
        );
    }

    #[test]
    fn accessible_reusable_bpo_and_seeded_materials_report_a_profitable_loop() {
        let data = static_data(vec![blueprint(9_131, 9_130, 1, &[(TRITANIUM, 100)])]);
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(9_130, "Funded Product", 1_000.0),
            ask_only(9_131, "Funded Product Blueprint", 1_000.0),
        ]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.violation_count(), 1);
        assert!(outcome.violations[0].loop_text.contains("blueprint 9131"));
    }

    #[test]
    fn supported_invention_with_guaranteed_inputs_may_report_a_loop() {
        const SOURCE_BPO: u32 = 9_141;
        const INVENTED_BPC: u32 = 9_142;
        const PRODUCT: u32 = 9_143;
        const DATACORE: u32 = 9_144;
        let data = static_data(vec![
            invention_source(SOURCE_BPO, INVENTED_BPC, &[(DATACORE, 1)]),
            blueprint(INVENTED_BPC, PRODUCT, 1, &[(TRITANIUM, 10)]),
        ]);
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            ask_only(DATACORE, "Datacore", 10.0),
            ask_only(SOURCE_BPO, "Source BPO", 1_000.0),
            side(PRODUCT, "Invented Product", 1_000.0),
        ]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.violation_count(), 1);
        assert!(
            outcome.violations[0]
                .loop_text
                .contains("invention+manufacturing")
        );
    }

    #[test]
    fn unsupported_invented_bpc_does_not_qualify() {
        let data = static_data(vec![blueprint(9_152, 9_150, 1, &[(TRITANIUM, 1)])]);
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(9_150, "Unsupported BPC Product", 1_000_000.0),
        ]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.violation_count(), 0);
        assert_eq!(
            *outcome.skipped.get(SKIP_UNFUNDED_PRODUCTION).unwrap_or(&0),
            1
        );
    }

    #[test]
    fn inaccessible_producer_does_not_mask_accessible_producer_for_same_output() {
        const PRODUCT: u32 = 9_160;
        let data = static_data(vec![
            blueprint(9_161, PRODUCT, 1, &[(TRITANIUM, 1)]),
            blueprint(9_162, PRODUCT, 1, &[(TRITANIUM, 100)]),
        ]);
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            side(PRODUCT, "Alternative-Producer Product", 1_000.0),
            ask_only(9_162, "Accessible Blueprint", 1_000.0),
        ]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.violation_count(), 1);
        assert!(outcome.violations[0].loop_text.contains("blueprint 9162"));
    }

    #[test]
    fn laelaps_style_one_tritanium_recipe_without_blueprint_access_is_rejected() {
        let data = static_data(vec![blueprint(60_767, 60_764, 1, &[(TRITANIUM, 1)])]);
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 100.0 / 3.0),
            side(60_764, "Laelaps", 285_935_000.0),
        ]);

        let outcome = check_manufacture(&book, &data, &test_semantics());

        assert_eq!(outcome.checked, 0);
        assert_eq!(outcome.violation_count(), 0);
        assert!(outcome.notes.iter().any(|note| note.contains("60767")));
    }

    #[test]
    fn tax_survival_is_computed_at_the_boundary() {
        // revenue x 0.92 must be strictly greater than the outlay to survive.
        let just_over = Violation {
            check: Check::Reprocess,
            type_id: 1,
            name: "Just Over".to_string(),
            bucket: "B",
            ownership: Ownership::SeederInvolved,
            loop_text: String::new(),
            outlay: 100.0,
            revenue: 108.7,
        };
        let exactly_at = Violation {
            revenue: 100.0 / TAX_KEPT_FRACTION,
            ..just_over.clone()
        };
        let just_under = Violation {
            revenue: 108.6,
            ..just_over.clone()
        };

        assert!(just_over.survives_tax(), "{}", just_over.after_tax());
        assert!(
            !exactly_at.survives_tax(),
            "breaking even is not printing ISK: {}",
            exactly_at.after_tax()
        );
        assert!(!just_under.survives_tax(), "{}", just_under.after_tax());
        // …and all three are still violations before tax.
        for violation in [&just_over, &exactly_at, &just_under] {
            assert!(violation.ratio() > 1.0);
        }

        // The same boundary through a whole check: ask 100, refine into 108.70 of Tritanium.
        let book = seed_book([
            SeedSide {
                type_id: TRITANIUM,
                name: "Tritanium".to_string(),
                bucket: "B",
                cost: 1.0,
                ask: one_at(3.0),
                bid: one_at(1.0),
            },
            SeedSide {
                type_id: 906,
                name: "Boundary Ore".to_string(),
                bucket: "B",
                cost: 100.0,
                ask: one_at(100.0),
                bid: one_at(100.0),
            },
        ]);
        let outcome = check_reprocess(
            &book,
            &repro_table(vec![refines(906, 10, &[(TRITANIUM, 1_087)])], &[]),
            PERFECT_YIELD,
        );
        assert_eq!(outcome.violation_count(), 1);
        assert_eq!(outcome.surviving_tax(), 1);

        let outcome = check_reprocess(
            &book,
            &repro_table(vec![refines(906, 10, &[(TRITANIUM, 1_040)])], &[]),
            PERFECT_YIELD,
        );
        assert_eq!(outcome.violation_count(), 1, "104.00 > the 100.00 ask");
        assert_eq!(
            outcome.surviving_tax(),
            0,
            "but 104.00 x 0.92 = 95.68 does not clear it"
        );
    }

    #[test]
    fn yield_scaling_changes_the_verdict_where_expected() {
        // Refines into 195.00 of Tritanium against a 180.00 ask: a pump at 100% recovery,
        // dead at 90.6% (176.67).
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            SeedSide {
                type_id: 907,
                name: "Marginal Ore".to_string(),
                bucket: "B",
                cost: 60.0,
                ask: one_at(180.0),
                bid: one_at(60.0),
            },
        ]);
        let reprocessing = repro_table(vec![refines(907, 2, &[(TRITANIUM, 195)])], &[]);

        let perfect = check_reprocess(&book, &reprocessing, PERFECT_YIELD);
        assert_eq!(perfect.violation_count(), 1);
        assert_eq!(perfect.violations[0].revenue, 195.0);

        let realistic = check_reprocess(&book, &reprocessing, REALISTIC_YIELD);
        assert_eq!(
            realistic.violation_count(),
            0,
            "195.00 x 0.906 = {} is under the 180.00 ask",
            195.0 * REALISTIC_YIELD
        );

        // …and a loop far enough over the line survives both, so the yield knob is not just
        // switching everything off.
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            SeedSide {
                type_id: 908,
                name: "Blatant Ore".to_string(),
                bucket: "B",
                cost: 10.0,
                ask: one_at(30.0),
                bid: one_at(10.0),
            },
        ]);
        let reprocessing = repro_table(vec![refines(908, 1, &[(TRITANIUM, 100)])], &[]);
        assert_eq!(
            check_reprocess(&book, &reprocessing, PERFECT_YIELD).violation_count(),
            1
        );
        assert_eq!(
            check_reprocess(&book, &reprocessing, REALISTIC_YIELD).violation_count(),
            1
        );
    }

    #[test]
    fn compression_uses_the_blueprint_ratio_when_one_exists() {
        // 100 raw units make 1 compressed unit, and the compressed bid pays more than the
        // 100 raw asks cost.
        let book = seed_book([
            SeedSide {
                type_id: 910,
                name: "Raw Ore".to_string(),
                bucket: "B",
                cost: 1.0,
                ask: one_at(3.0),
                bid: one_at(1.0),
            },
            SeedSide {
                type_id: 911,
                name: "Compressed Ore".to_string(),
                bucket: "B",
                cost: 400.0,
                ask: one_at(1_200.0),
                bid: one_at(400.0),
            },
        ]);
        let producers = producers(vec![blueprint(9111, 911, 1, &[(910, 100)])]);
        let reprocessing = repro_table(Vec::new(), &[(910, 911)]);

        let (ratio, basis) = compression_ratio(910, 911, &reprocessing, &producers)
            .expect("the blueprint fixes the ratio");
        assert_eq!(ratio, 100.0);
        assert_eq!(basis, RatioBasis::Blueprint(9111));

        let outcome = check_compression(&book, &reprocessing, &producers);
        assert_eq!(outcome.checked, 1);
        assert_eq!(outcome.violation_count(), 1);
        let violation = &outcome.violations[0];
        assert_eq!(
            violation.type_id, 911,
            "the compressed side is the one sold"
        );
        assert_eq!(violation.outlay, 300.0);
        assert_eq!(violation.revenue, 400.0);
        assert!(violation.loop_text.contains("blueprint 9111"));
    }

    #[test]
    fn compression_falls_back_to_refine_yields_and_checks_the_reverse_direction() {
        // No blueprint: 1 compressed unit refines into 100x what a raw unit does, so it is
        // worth 100 raw units. Here the *raw* bids overpay, so the reverse leg is the pump.
        let book = seed_book([
            SeedSide {
                type_id: 912,
                name: "Raw Ore".to_string(),
                bucket: "B",
                cost: 10.0,
                ask: one_at(30.0),
                bid: one_at(10.0),
            },
            SeedSide {
                type_id: 913,
                name: "Compressed Ore".to_string(),
                bucket: "B",
                cost: 100.0,
                ask: one_at(300.0),
                bid: one_at(100.0),
            },
        ]);
        let reprocessing = repro_table(
            vec![
                refines(912, 100, &[(TRITANIUM, 100), (PYERITE, 50)]),
                refines(913, 1, &[(TRITANIUM, 100), (PYERITE, 50)]),
            ],
            &[(912, 913)],
        );
        let producers = producers(Vec::new());

        let (ratio, basis) = compression_ratio(912, 913, &reprocessing, &producers)
            .expect("both sides refine into the same materials");
        assert_eq!(ratio, 100.0);
        assert_eq!(basis, RatioBasis::RefineYield);

        let outcome = check_compression(&book, &reprocessing, &producers);
        assert_eq!(outcome.checked, 1);
        assert_eq!(outcome.violation_count(), 1);
        let violation = &outcome.violations[0];
        assert_eq!(violation.type_id, 912, "the raw side is the one sold");
        assert_eq!(violation.outlay, 300.0, "one compressed unit at its ask");
        assert_eq!(violation.revenue, 1_000.0, "100 raw units at their bids");
        assert!(violation.loop_text.contains("refine yields"));
    }

    #[test]
    fn a_pair_with_no_usable_basis_is_skipped_not_guessed() {
        let book = seed_book([side(914, "Raw", 10.0), side(915, "Compressed", 10.0)]);
        let producers = producers(Vec::new());

        // Nothing to go on at all.
        let outcome = check_compression(&book, &repro_table(Vec::new(), &[(914, 915)]), &producers);
        assert_eq!(outcome.checked, 0);
        assert_eq!(outcome.violation_count(), 0);
        assert_eq!(*outcome.skipped.get(SKIP_NO_RATIO_BASIS).unwrap_or(&0), 1);

        // Refine rows that share no material: still no ratio.
        let disjoint = repro_table(
            vec![
                refines(914, 1, &[(TRITANIUM, 10)]),
                refines(915, 1, &[(PYERITE, 10)]),
            ],
            &[(914, 915)],
        );
        assert_eq!(
            *check_compression(&book, &disjoint, &producers)
                .skipped
                .get(SKIP_NO_RATIO_BASIS)
                .unwrap_or(&0),
            1
        );

        // Shared materials that imply two different ratios: a guess would be worse.
        let inconsistent = repro_table(
            vec![
                refines(914, 1, &[(TRITANIUM, 10), (PYERITE, 10)]),
                refines(915, 1, &[(TRITANIUM, 100), (PYERITE, 50)]),
            ],
            &[(914, 915)],
        );
        assert_eq!(
            *check_compression(&book, &inconsistent, &producers)
                .skipped
                .get(SKIP_RATIO_DISAGREES)
                .unwrap_or(&0),
            1
        );

        // And an unseeded side is its own skip.
        let half_seeded = seed_book([side(914, "Raw", 10.0)]);
        let outcome = check_compression(
            &half_seeded,
            &repro_table(vec![refines(914, 1, &[(TRITANIUM, 10)])], &[(914, 915)]),
            &producers,
        );
        assert_eq!(*outcome.skipped.get(SKIP_PAIR_UNSEEDED).unwrap_or(&0), 1);
    }

    #[test]
    fn the_report_gates_on_tax_survival_and_ranks_by_ratio() {
        let book = seed_book([
            side(TRITANIUM, "Tritanium", 2.0),
            // x6.67 before tax, x6.13 after — survives.
            side(920, "Blatant", 10.0),
            // x1.04 before tax, x0.96 after — a violation that dies to the tax.
            SeedSide {
                type_id: 921,
                name: "Marginal".to_string(),
                bucket: "B",
                cost: 100.0,
                ask: one_at(100.0),
                bid: one_at(100.0),
            },
        ]);
        let reprocessing = repro_table(
            vec![
                refines(920, 1, &[(TRITANIUM, 100)]),
                refines(921, 1, &[(TRITANIUM, 52)]),
            ],
            &[],
        );
        let report = AuditReport::run(
            &book,
            &reprocessing,
            &producers(Vec::new()),
            &static_data(Vec::new()),
            &test_semantics(),
            PERFECT_YIELD,
        );

        assert_eq!(report.violation_count(), 2);
        assert_eq!(report.surviving_tax(), 1);
        let worst = report.worst(20);
        assert_eq!(worst.len(), 2);
        assert!(
            worst[0].ratio() > worst[1].ratio(),
            "the top table must be sorted by ratio"
        );
        assert_eq!(worst[0].type_id, 920);
        assert_eq!(report.worst(1).len(), 1);
    }

    /// Check 4's whole reason to exist: nothing is transformed, so checks 1-3 cannot see it.
    #[test]
    fn check_four_finds_a_haul_from_the_cheapest_ask_to_the_dearest_bid() {
        let book = seed_book([
            // We sell it at Jita for 1,000; an NPC in Rens buys it for 4,000.
            side_at(
                950,
                "Hauled Commodity",
                &[(JITA_4_4, 1_000.0)],
                &[(RENS, 4_000.0)],
            ),
            // Safe: bought dearer than anyone will pay for it.
            side_at(
                951,
                "Honest Commodity",
                &[(JITA_4_4, 1_000.0)],
                &[(RENS, 900.0)],
            ),
        ]);

        let outcome = check_cross_station(&book);

        assert_eq!(outcome.checked, 2);
        assert_eq!(outcome.violation_count(), 1);
        let violation = &outcome.violations[0];
        assert_eq!(violation.type_id, 950);
        assert_eq!(violation.check, Check::CrossStation);
        assert_eq!(violation.outlay, 1_000.0);
        assert_eq!(violation.revenue, 4_000.0);
        assert_eq!(violation.ratio(), 4.0);
        assert!(violation.survives_tax(), "4,000 x 0.92 = 3,680 > 1,000");
        // Both stations, both prices and the margin are in the text a human reads.
        assert!(
            violation.loop_text.contains("Jita IV - Moon 4"),
            "{}",
            violation.loop_text
        );
        assert!(
            violation.loop_text.contains("Rens VI - Moon 8"),
            "{}",
            violation.loop_text
        );
        assert!(
            violation.loop_text.contains("3,000.00"),
            "margin: {}",
            violation.loop_text
        );

        // A margin the tax eats is still reported, and still marked as dying to it.
        let thin = seed_book([side_at(
            952,
            "Thin Margin",
            &[(JITA_4_4, 1_000.0)],
            &[(RENS, 1_050.0)],
        )]);
        let outcome = check_cross_station(&thin);
        assert_eq!(outcome.violation_count(), 1);
        assert!(
            !outcome.violations[0].survives_tax(),
            "1,050 x 0.92 = 966 < 1,000, so it is a violation that does not gate the build"
        );
        assert_eq!(outcome.surviving_tax(), 0);
    }

    /// The scope line: check 4 is about hauling between two stations. A bid over an ask at
    /// ONE station is the build's own `bid <= ask` invariant and checks 1-3's territory, so it
    /// is counted in a note instead of being reported twice.
    #[test]
    fn check_four_ignores_a_same_station_crossing_but_still_says_it_saw_one() {
        let crossed_at_one_station = seed_book([side_at(
            960,
            "TQ Crossed Trade Good",
            &[(RENS, 100.0)],
            &[(RENS, 900.0)],
        )]);

        let outcome = check_cross_station(&crossed_at_one_station);

        assert_eq!(
            outcome.violation_count(),
            0,
            "one station is not a cross-station haul"
        );
        assert_eq!(
            *outcome.skipped.get(SKIP_SINGLE_STATION).unwrap_or(&0),
            1,
            "and with nowhere else to buy or sell there is nothing for this check to compare"
        );
        assert_eq!(outcome.notes.len(), 1);
        assert!(outcome.notes[0].contains("960"), "{}", outcome.notes[0]);
        assert!(
            outcome.notes[0].contains("ONE station"),
            "{}",
            outcome.notes[0]
        );

        // The same crossing, plus a third station that sells it cheaper still: now there IS a
        // haul, and it must be measured against that third station rather than dismissed.
        let with_a_third_station = seed_book([side_at(
            960,
            "TQ Crossed Trade Good",
            &[(RENS, 100.0), (AMARR, 120.0)],
            &[(RENS, 900.0)],
        )]);
        let outcome = check_cross_station(&with_a_third_station);
        assert_eq!(outcome.violation_count(), 1);
        assert_eq!(
            outcome.violations[0].outlay, 120.0,
            "the best ask is at Rens, where the bid also is, so the haul starts at Amarr"
        );
        assert_eq!(outcome.violations[0].revenue, 900.0);

        // A one-sided type cannot be hauled at all.
        let sell_only = seed_book([side_at(961, "Skillbook", &[(RENS, 100.0)], &[])]);
        let outcome = check_cross_station(&sell_only);
        assert_eq!(outcome.checked, 0);
        assert_eq!(*outcome.skipped.get(SKIP_ONE_SIDED).unwrap_or(&0), 1);
    }

    /// **Fix 2.** A haul loop with one of our prices on it is a bug in the seed; a haul loop
    /// between two NPC stations is TQ's own spread, imported verbatim. Check 4 has to say
    /// which it is looking at, because only the first one is ours to fix.
    #[test]
    fn check_four_says_whether_a_haul_touches_a_price_we_set() {
        let book = seed_book([
            // Ours sells at Jita, an NPC buys higher at Rens — the class-A shape.
            side_at(
                950,
                "Our Ask vs NPC Bid",
                &[(JITA_4_4, 1_000.0)],
                &[(RENS, 4_000.0)],
            ),
            // An NPC sells at Rens, our Jita bid pays more — the class-B shape.
            side_at(
                951,
                "NPC Ask vs Our Bid",
                &[(RENS, 100.0)],
                &[(JITA_4_4, 800.0)],
            ),
            // Neither leg is ours: TQ sells it at Amarr and buys it back dearer at Rens.
            side_at(952, "TQ Trade Good", &[(AMARR, 100.0)], &[(RENS, 202.0)]),
        ]);

        let outcome = check_cross_station(&book);

        assert_eq!(outcome.violation_count(), 3);
        let owned = outcome
            .violations
            .iter()
            .map(|violation| (violation.type_id, violation.ownership))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(owned[&950], Ownership::SeederInvolved, "our ask is a leg");
        assert_eq!(owned[&951], Ownership::SeederInvolved, "our bid is a leg");
        assert_eq!(owned[&952], Ownership::NpcToNpc, "neither leg is ours");
        assert_eq!(outcome.violation_count_owned(Ownership::SeederInvolved), 2);
        assert_eq!(outcome.violation_count_owned(Ownership::NpcToNpc), 1);
        assert_eq!(outcome.surviving_tax(), 3);
        assert_eq!(outcome.surviving_tax_owned(Ownership::NpcToNpc), 1);

        // The loop text names the owner of each leg, so the split can be read without
        // cross-referencing a station list.
        let tq = outcome
            .violations
            .iter()
            .find(|violation| violation.type_id == 952)
            .expect("the TQ loop is reported, never hidden");
        assert!(tq.loop_text.contains("npc-overlay"), "{}", tq.loop_text);
        assert!(!tq.loop_text.contains("seeder"), "{}", tq.loop_text);

        // The gate: npc-to-npc hauls are forgiven by default and gate when asked to.
        assert_eq!(
            outcome.gating(false),
            2,
            "the two loops with one of our prices on them fail the build"
        );
        assert_eq!(
            outcome.gating(true),
            3,
            "[import] gate_on_npc_arbitrage = true makes the inherited one fail too"
        );
        assert!(!tq.gates(false));
        assert!(tq.gates(true));
        for type_id in [950, 951] {
            let ours = outcome
                .violations
                .iter()
                .find(|violation| violation.type_id == type_id)
                .expect("ours is reported");
            assert!(ours.gates(false), "{type_id} gates whatever the knob says");
            assert!(ours.gates(true));
        }

        // A loop that dies to the tax gates under neither setting, whoever owns it.
        let thin = seed_book([side_at(
            953,
            "Thin TQ Spread",
            &[(AMARR, 1_000.0)],
            &[(RENS, 1_050.0)],
        )]);
        let outcome = check_cross_station(&thin);
        assert_eq!(outcome.violations[0].ownership, Ownership::NpcToNpc);
        assert_eq!(outcome.gating(true), 0);
    }

    /// The exemption is check 4's alone. A *transformation* loop between two NPC stations
    /// means TQ's prices disagree with the game's own reprocessing mechanics, which is a
    /// different claim from a haul and still fails the build.
    #[test]
    fn the_npc_exemption_does_not_leak_into_the_transformation_checks() {
        // Both legs are NPC rows: TQ sells the ore at Amarr, TQ buys the mineral at Rens.
        let book = seed_book([
            side_at(TRITANIUM, "Tritanium", &[(RENS, 4.0)], &[(RENS, 2.0)]),
            side_at(971, "TQ Ore", &[(AMARR, 30.0)], &[(AMARR, 10.0)]),
        ]);
        let reprocessing = repro_table(vec![refines(971, 1, &[(TRITANIUM, 100)])], &[]);

        let outcome = check_reprocess(&book, &reprocessing, PERFECT_YIELD);

        assert_eq!(outcome.violation_count(), 1);
        assert_eq!(outcome.violations[0].ownership, Ownership::NpcToNpc);
        assert!(outcome.violations[0].survives_tax());
        assert_eq!(
            outcome.gating(false),
            1,
            "an npc-to-npc reprocess pump gates the build; only check 4 is exempt"
        );
        assert_eq!(outcome.gating(true), 1);

        // And the same loop with one of our prices on it is seeder-involved.
        let mut ours = side_at(TRITANIUM, "Tritanium", &[(RENS, 4.0)], &[(RENS, 2.0)]);
        ours.bid.absorb_bid(at(JITA_4_4, 50.0));
        let book = seed_book([
            ours,
            side_at(971, "TQ Ore", &[(AMARR, 30.0)], &[(AMARR, 10.0)]),
        ]);
        let outcome = check_reprocess(&book, &reprocessing, PERFECT_YIELD);
        assert_eq!(outcome.violations[0].ownership, Ownership::SeederInvolved);
    }

    /// The whole-report gate, which is what the exit code is.
    #[test]
    fn the_report_gate_forgives_only_inherited_hauls_and_still_counts_them() {
        let book = seed_book([
            side_at(952, "TQ Trade Good", &[(AMARR, 100.0)], &[(RENS, 202.0)]),
            side_at(953, "Another TQ Spread", &[(AMARR, 10.0)], &[(RENS, 30.0)]),
        ]);
        let report = AuditReport::run(
            &book,
            &repro_table(Vec::new(), &[]),
            &producers(Vec::new()),
            &static_data(Vec::new()),
            &test_semantics(),
            PERFECT_YIELD,
        );

        assert_eq!(report.violation_count(), 2);
        assert_eq!(report.surviving_tax(), 2, "both still pay after the tax");
        assert_eq!(report.violation_count_owned(Ownership::SeederInvolved), 0);
        assert_eq!(report.inherited_npc_hauls(), 2);
        assert_eq!(
            report.gating(false),
            0,
            "nothing of ours is involved, so nothing gates"
        );
        assert_eq!(report.gating(true), 2);
        // The residue is still visible in the numbers a verdict prints.
        assert_eq!(report.surviving_tax_owned(Ownership::NpcToNpc), 2);
        assert_eq!(report.worst_owned(Ownership::NpcToNpc, 1)[0].type_id, 953);

        // One seeder-involved loop and the gate closes whatever the knob says.
        let mut with_ours = book.clone();
        with_ours.insert(side_at(
            954,
            "Our Ask vs NPC Bid",
            &[(JITA_4_4, 1_000.0)],
            &[(RENS, 4_000.0)],
        ));
        let report = AuditReport::run(
            &with_ours,
            &repro_table(Vec::new(), &[]),
            &producers(Vec::new()),
            &static_data(Vec::new()),
            &test_semantics(),
            PERFECT_YIELD,
        );
        assert_eq!(report.gating(false), 1);
        assert_eq!(report.gating(true), 3);
        assert_eq!(
            report.inherited_npc_hauls(),
            2,
            "the inherited count is unchanged"
        );
    }

    /// Jita 4-4 and Rens, a Venture and a skillbook — the smallest world a merge test needs.
    fn two_station_data() -> StaticData {
        use crate::staticdata::{SolarSystemRecord, StationRecord};

        let station_record = |station_id: u64, name: &str| StationRecord {
            station_id,
            solar_system_id: 30_000_142,
            constellation_id: 20_000_020,
            region_id: 10_000_002,
            region_name: "The Forge".to_string(),
            station_name: name.to_string(),
            security: 0.9,
        };
        let item = |type_id: u32, name: &str| ItemTypeRecord {
            type_id,
            group_id: Some(1),
            category_id: Some(9),
            group_name: Some("Test".to_string()),
            name: name.to_string(),
            mass: None,
            volume: None,
            capacity: None,
            portion_size: Some(1),
            race_id: None,
            base_price: None,
            market_group_id: Some(1),
            icon_id: None,
            sound_id: None,
            graphic_id: None,
            radius: None,
            published: true,
        };
        let stations = vec![
            station_record(JITA_4_4, "Jita IV - Moon 4 - Caldari Navy Assembly Plant"),
            station_record(RENS, "Rens VI - Moon 8 - Brutor Tribe Treasury"),
        ];
        let item_types = vec![item(32_880, "Venture"), item(980, "Skillbook")];
        StaticData {
            dir: PathBuf::from("."),
            station_index: stations
                .iter()
                .enumerate()
                .map(|(index, station)| (station.station_id, index))
                .collect(),
            stations,
            solar_systems: Vec::<SolarSystemRecord>::new(),
            solar_system_index: HashMap::new(),
            item_type_index: item_types
                .iter()
                .enumerate()
                .map(|(index, item)| (item.type_id, index))
                .collect(),
            item_types,
            blueprints: Vec::new(),
            dogma: HashMap::<u32, DogmaProjection>::new(),
            compressed_type_ids: BTreeSet::new(),
        }
    }

    /// The merge has to reproduce the database, not the snapshot: the index pass's
    /// `INSERT OR REPLACE` deleted the overlay's rows at the index station for every type it
    /// seeds, so those prices are not in the book either.
    #[test]
    fn merging_the_overlay_drops_exactly_the_rows_the_index_pass_replaced() {
        let data = two_station_data();

        let mut import = crate::overlay::OverlayImport::default();
        let mut row = |station_id: u64, type_id: u32, is_buy: bool, price_cents: i64| {
            let liquidity = crate::overlay::SeedLiquidity {
                station_id,
                solar_system_id: 30_000_142,
                constellation_id: 20_000_020,
                region_id: 10_000_002,
                type_id,
                price_cents,
                quantity: 5,
            };
            if is_buy {
                import.buy.insert((station_id, type_id), liquidity);
            } else {
                import.sell.insert((station_id, type_id), liquidity);
            }
        };
        // TQ's Venture prices at Jita 4-4 (replaced by us) and at Rens (kept), plus a
        // skillbook only TQ seeds.
        // Prices are integer cents: 10_000 cents = 100.00 ISK.
        row(JITA_4_4, 32_880, false, 10_000);
        row(JITA_4_4, 32_880, true, 9_000);
        row(RENS, 32_880, false, 60_000_000);
        row(RENS, 32_880, true, 45_000_000);
        row(RENS, 980, false, 2_000_000);

        let station = crate::build::test_station();
        let rows = vec![SeedRow {
            type_id: 32_880,
            name: "Venture".to_string(),
            bucket: "A",
            junk: false,
            role: crate::classify::MarketRole::Core,
            profile: crate::classify::PricingProfile::CoreGeneral,
            cost: 252_906.5,
            ask: 758_719.5,
            bid: 252_906.5,
            sides: crate::seedplan::SeedSides::BOTH,
            quantity: 2_147_483_647,
        }];
        let mut book = SeedBook::from_rows(&rows, &station);
        book.merge_overlay(&import, &data, station.station_id);

        let venture = book.get(32_880).expect("the Venture is seeded");
        assert_eq!(
            venture.ask.price(),
            Some(600_000.0),
            "TQ's 100.00 ask at Jita 4-4 no longer exists — the index pass replaced that row — \
             so the cheapest ask anywhere is the 600,000.00 at Rens, not 100.00"
        );
        assert_eq!(venture.ask.station_id(), Some(RENS));
        assert_eq!(
            venture.ask.elsewhere.as_ref().map(|price| price.price),
            Some(758_719.5),
            "and our own Jita ask is the runner-up"
        );
        assert_eq!(
            venture.bid.price(),
            Some(450_000.0),
            "TQ's 90.00 Jita bid is gone too; the best bid is our 252,906.50 or TQ's 450,000.00"
        );
        assert_eq!(venture.bid.station_id(), Some(RENS));

        // The overlay-only type joins the book as bucket C, sell side only.
        let skillbook = book
            .get(980)
            .expect("the skillbook comes in from the overlay");
        assert_eq!(skillbook.bucket, "C");
        assert_eq!(skillbook.name, "Skillbook");
        assert_eq!(skillbook.ask.price(), Some(20_000.0));
        assert_eq!(skillbook.bid.price(), None, "nothing buys it back");
        assert_eq!(book.station_count(), 2);
        assert_eq!(
            book.composition(),
            vec![("A", 1), ("A+B", 0), ("B", 0), ("C", 1), ("D", 0)]
        );
        // Report qualification: the index station is unmarked, everything else is named.
        assert_eq!(
            book.qualify(980, PriceSide::Ask),
            " @ Rens VI - Moon 8 - Brutor Tribe Treasury"
        );
    }

    /// The other half of that rule, once a deferral can take one side: the index pass replaces
    /// only the side it **writes**. A deferred ask leaves TQ's ask at Jita 4-4 in the database,
    /// so it has to be in the book too — otherwise the audit prices a station that does not
    /// exist and check 4 gets a free pass on the leg the deferral deliberately left standing.
    #[test]
    fn a_deferred_side_leaves_tqs_row_at_the_index_station_in_the_book() {
        let data = two_station_data();
        let mut import = crate::overlay::OverlayImport::default();
        let mut row = |station_id: u64, type_id: u32, is_buy: bool, price_cents: i64| {
            let liquidity = crate::overlay::SeedLiquidity {
                station_id,
                solar_system_id: 30_000_142,
                constellation_id: 20_000_020,
                region_id: 10_000_002,
                type_id,
                price_cents,
                quantity: 5,
            };
            if is_buy {
                import.buy.insert((station_id, type_id), liquidity);
            } else {
                import.sell.insert((station_id, type_id), liquidity);
            }
        };
        // TQ holds the Venture on both sides at Jita 4-4: sells at 100.00, buys at 90.00.
        row(JITA_4_4, 32_880, false, 10_000);
        row(JITA_4_4, 32_880, true, 9_000);

        let station = crate::build::test_station();
        let rows = vec![SeedRow {
            type_id: 32_880,
            name: "Venture".to_string(),
            bucket: "A",
            junk: false,
            role: crate::classify::MarketRole::Core,
            profile: crate::classify::PricingProfile::CoreGeneral,
            cost: 252_906.5,
            ask: 758_719.5,
            bid: 252_906.5,
            // The ask was deferred; only the bid is ours.
            sides: crate::seedplan::SeedSides {
                ask: false,
                bid: true,
            },
            quantity: 2_147_483_647,
        }];
        let mut book = SeedBook::from_rows(&rows, &station);
        let venture = book.get(32_880).expect("the Venture is in the book");
        assert_eq!(
            venture.ask.price(),
            None,
            "a deferred side is an ABSENT price, not a zero one"
        );
        assert_eq!(venture.bid.price(), Some(252_906.5));

        book.merge_overlay(&import, &data, station.station_id);
        let venture = book.get(32_880).expect("still there");
        assert_eq!(
            venture.ask.price(),
            Some(100.0),
            "TQ's Jita ask survives, because our ask was never written over it"
        );
        assert_eq!(venture.ask.origin(), Some(PriceOrigin::NpcOverlay));
        assert_eq!(
            venture.bid.price(),
            Some(252_906.5),
            "TQ's 90.00 Jita bid IS replaced — that is the side the index pass wrote"
        );
        assert_eq!(venture.bid.origin(), Some(PriceOrigin::Seeder));

        // …and this is exactly the same-station pair `build::verify_seed_tables` refuses to
        // let a build commit: our 252,906.50 bid against TQ's 100.00 ask would be free ISK
        // with no haul at all. It only cannot happen because the deferral measures TQ's best
        // ask over every station INCLUDING this one, so a surviving bid of ours is under it.
        assert!(
            venture.bid.price() > venture.ask.price(),
            "the fixture deliberately builds the crossing the real rule makes impossible"
        );
    }

    /// The runner-up bookkeeping check 4 leans on, exercised directly.
    #[test]
    fn the_best_price_keeps_a_runner_up_from_a_different_station() {
        let mut asks = BestPrice::default();
        asks.absorb_ask(at(JITA_4_4, 100.0));
        asks.absorb_ask(at(JITA_4_4, 90.0)); // cheaper, same station: no runner-up yet
        assert_eq!(asks.price(), Some(90.0));
        assert!(asks.elsewhere.is_none());

        asks.absorb_ask(at(RENS, 120.0)); // dearer, elsewhere: becomes the runner-up
        assert_eq!(asks.price(), Some(90.0));
        assert_eq!(asks.elsewhere.as_ref().map(|p| p.price), Some(120.0));

        asks.absorb_ask(at(AMARR, 110.0)); // dearer still, but better than the runner-up
        assert_eq!(asks.elsewhere.as_ref().map(|p| p.price), Some(110.0));

        asks.absorb_ask(at(AMARR, 50.0)); // new best from elsewhere: the old best steps down
        assert_eq!(asks.price(), Some(50.0));
        assert_eq!(asks.station_id(), Some(AMARR));
        assert_eq!(asks.elsewhere.as_ref().map(|p| p.price), Some(90.0));

        // Bids mirror it exactly, dearest wins.
        let mut bids = BestPrice::default();
        for (station, price) in [
            (JITA_4_4, 10.0),
            (JITA_4_4, 20.0),
            (RENS, 15.0),
            (AMARR, 5.0),
        ] {
            bids.absorb_bid(at(station, price));
        }
        assert_eq!(bids.price(), Some(20.0));
        assert_eq!(bids.station_id(), Some(JITA_4_4));
        assert_eq!(bids.elsewhere.as_ref().map(|p| p.price), Some(15.0));
    }

    /// Checks 1-3 price a type at the best seeded price anywhere, not at the Jita row.
    #[test]
    fn the_earlier_checks_use_the_best_price_anywhere_and_name_the_station() {
        // Tritanium: our Jita bid is 2.00, but some NPC in Rens pays 50.00 for it. Refining
        // an ore bought at Jita for 30.00 into 100 Tritanium is only a pump at the Rens bid.
        let mut tritanium = side(TRITANIUM, "Tritanium", 2.0);
        tritanium.bid.absorb_bid(at(RENS, 50.0));
        let book = seed_book([tritanium, side(970, "Ore", 10.0)]);
        let reprocessing = repro_table(vec![refines(970, 100, &[(TRITANIUM, 100)])], &[]);

        let outcome = check_reprocess(&book, &reprocessing, PERFECT_YIELD);

        assert_eq!(outcome.violation_count(), 1);
        let violation = &outcome.violations[0];
        assert_eq!(
            violation.revenue, 50.0,
            "1 Tritanium per unit, valued at the best bid anywhere (50.00), not the 2.00 at Jita"
        );
        assert_eq!(violation.outlay, 30.0);
        assert!(
            violation.loop_text.contains("@ Rens VI - Moon 8"),
            "the report must say where that bid is: {}",
            violation.loop_text
        );
        // The ask side is at the index station, so it is left unmarked.
        assert!(
            violation.loop_text.contains("at Jita IV - Moon 4"),
            "{}",
            violation.loop_text
        );
    }

    #[test]
    fn the_book_never_turns_an_absent_price_into_a_zero() {
        let book = seed_book([side(930, "Seeded", 10.0)]);
        assert_eq!(book.ask(930), Some(30.0));
        assert_eq!(book.bid(930), Some(10.0));
        assert_eq!(book.ask(931), None);
        assert_eq!(book.bid(931), None);
        assert_eq!(book.bid_or_zero(931), 0.0);
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn the_book_is_built_from_the_rows_the_build_would_write() {
        let rows = vec![SeedRow {
            type_id: 940,
            name: "Row Type".to_string(),
            bucket: "A",
            junk: false,
            role: crate::classify::MarketRole::Core,
            profile: crate::classify::PricingProfile::CoreGeneral,
            cost: 12.5,
            ask: 100.0,
            bid: 12.5,
            sides: crate::seedplan::SeedSides::BOTH,
            quantity: 2_147_483_647,
        }];
        let station = crate::build::test_station();
        let book = SeedBook::from_rows(&rows, &station);
        let side = book.get(940).expect("the row is in the book");
        assert_eq!(
            side.ask.price(),
            Some(100.0),
            "the floored ask is carried through as-is"
        );
        assert_eq!(side.bid.price(), Some(12.5));
        assert_eq!(side.bucket, "A");
        // Every index row is at the index station, and nothing is "elsewhere" until the
        // overlay merges in.
        assert_eq!(side.ask.station_id(), Some(station.station_id));
        assert!(side.ask.elsewhere.is_none());
        assert_eq!(book.station_count(), 1);
        assert_eq!(
            book.composition(),
            vec![("A", 1), ("A+B", 0), ("B", 0), ("C", 0), ("D", 0)]
        );
    }

    #[test]
    fn canonical_book_retains_best_and_elsewhere_prices_across_all_five_hubs() {
        let rows = vec![SeedRow {
            type_id: 941,
            name: "Canonical Type".to_string(),
            bucket: "A",
            junk: false,
            role: crate::classify::MarketRole::Core,
            profile: crate::classify::PricingProfile::CoreGeneral,
            cost: 50.0,
            ask: 60.0,
            bid: 40.0,
            sides: crate::seedplan::SeedSides::BOTH,
            quantity: 2_147_483_647,
        }];
        let stations = crate::config::CANONICAL_HUB_STATION_IDS
            .into_iter()
            .enumerate()
            .map(|(index, station_id)| SeedStation {
                station_id,
                solar_system_id: 30_000_000 + index as u32,
                constellation_id: 20_000_000 + index as u32,
                region_id: 10_000_000 + index as u32,
                station_name: format!("Canonical Hub {index}"),
                region_name: format!("Region {index}"),
            })
            .collect::<Vec<_>>();

        let book = SeedBook::from_rows_at_stations(&rows, &stations);
        let side = book.get(941).expect("canonical type is retained");
        assert_eq!(book.station_count(), 5);
        assert_eq!(side.ask.price(), Some(60.0));
        assert_eq!(side.bid.price(), Some(40.0));
        assert_ne!(
            side.ask.best.as_ref().unwrap().station_id,
            side.ask
                .elsewhere
                .as_ref()
                .expect("second canonical ask")
                .station_id
        );
        assert_ne!(
            side.bid.best.as_ref().unwrap().station_id,
            side.bid
                .elsewhere
                .as_ref()
                .expect("second canonical bid")
                .station_id
        );
    }
}
