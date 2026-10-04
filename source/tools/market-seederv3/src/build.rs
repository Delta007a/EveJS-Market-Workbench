//! The SQLite build pass — plan §5/§6, ported from market-seederv2's database machinery.
//!
//! What this piece writes, and what it deliberately does not:
//!
//! | table                  | what the build writes                           |
//! |------------------------|-------------------------------------------------|
//! | `regions` / `solar_systems` / `stations` / `market_types` | full static tables, v2's `write_static_tables` |
//! | `seed_stock` / `seed_buy_orders` | one canonical synthetic book replicated identically to the five approved hubs |
//! | `region_summaries` / `system_seed_summaries` | rebuilt from the seed tables, **after both passes** |
//! | `price_history`        | `[history] days` of synthetic chart per seeded type ([`crate::history`]) |
//! | `manifest`             | `manifest_json` for the daemon, plus flat build-metadata rows |
//! | `market_orders` / `market_order_events` | **never written** |
//!
//! The build **never downloads**: it uses whatever `snapshot-info --download` last cached and
//! stops with instructions when the cache is empty. Snapshot NPC orders remain reference
//! authority for approved roles (notably BPOs), but are never written as universe-wide rows.
//! The canonical writer uses plain `INSERT`; role/side ownership and duplicate rows are
//! rejected before SQLite is opened, so output no longer depends on pass order or replacement.
//!
//! ### Pricing, and the reason the floor is ask-only
//!
//! ```text
//! ask = max(price_floor, round2(sell_multiplier * cost))
//! bid =                  round2(buy_multiplier  * cost)     // floor NEVER applies
//! ```
//!
//! The plan text (§3.3) originally put `max(floor, …)` on both sides. That is a money pump,
//! measured: Tritanium costs 2 ISK, so a floored bid would pay **100 ISK for it — 50x cost**.
//! Buy a Venture at its 403,200 ask, reprocess it, sell the minerals into floored bids and
//! you get 1,417,500 ISK back: a **3.52x** loop that runs forever at int32-max quantities.
//! Bids must track cost exactly; only the ask carries a floor, and the floor's job is to stop
//! the seed selling items for fractions of an ISK, not to set a price of ISK.
//!
//! Both sides are additionally clamped to [`MIN_SEED_PRICE`] so no row can ever be written at
//! 0.00 — a zero ask hands out free items. With the default 100 ISK floor that clamp never
//! binds on the ask side; it exists for a deliberately floorless configuration.
//!
//! ### The invariants
//!
//! [`verify_seed_rows`] runs over the planned rows and [`verify_seed_tables`] re-runs the same
//! questions in SQL **inside the write transaction, before it commits**, so a bug in the
//! writer cannot leave a bad row behind: a violation aborts the transaction and the staged
//! database is discarded without ever touching the live one.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, bail};
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use market_common::{
    MANIFEST_KEY, MARKET_SCHEMA_VERSION, MarketManifest, RUNTIME_INDEX_DROP_SQL, RUNTIME_INDEX_SQL,
    SCHEMA_SQL, now_rfc3339, round_isk,
};
use rusqlite::{Connection, OpenFlags, params};

use crate::audit::{
    SeedBook, check_progression_npc_supply_invariants, check_reward_sell_invariants,
};
use crate::classify::{
    BucketFlags, Classification, MOON_RESOURCE_GROUP_IDS, MarketRole, PricingProfile,
    load_expanded_lp_reward_type_ids, load_lp_reward_type_ids,
};
use crate::config::{PricingProfilesConfig, SeederConfig};
use crate::cost::{CostInputs, CostMap, CostReport, OverridePrices};
use crate::funded::{
    IndustrySemantics, apply_market_safety, apply_market_safety_with_source_contract,
    print_market_safety_report, print_production_intermediate_resolution,
    print_rare_t2_funded_resolution, resolve_production_intermediates,
    resolve_production_intermediates_v2, resolve_rare_t2_fallbacks, resolve_rare_t2_fallbacks_v2,
};
use crate::history::{HistoryPlan, HistorySource};
use crate::manifest::{CapturePolicy, PriceManifest, PriceSource, read_sde_build};
use crate::ore::{
    OreFamilyNormalization, print_ore_family_normalization_report, verify_ore_family_monotonicity,
};
use crate::policy::PolicyDocument;
use crate::policy::Source as PolicySource;
use crate::policy_facts::FactInputs;
use crate::policy_resolve::{CatalogItem, Resolution};
use crate::seedplan::{
    DeferredSide, DropReason, RareFallbackProvenance, SeedPlan, SeedSides,
    required_external_market_references_from_config, required_external_market_references_v2,
    strict_sibling_family_fallbacks,
};
use crate::staticdata::{
    ReprocessingStatic, StaticData, StationRecord, VariantFamilies, resolve_static_data_dir,
};

/// No seed row may be written below this price. A 0.00 ask is free items; a 0.00 bid is a
/// row the client renders as a broken order. Prices are stored to 2 decimals, so this is the
/// smallest representable non-zero price.
pub const MIN_SEED_PRICE: f64 = 0.01;

/// `price_version` for every row this seeder writes. The daemon bumps it when it reprices.
pub const SEED_PRICE_VERSION: i64 = 1;

/// `manifest.selection_mode` — what a v3-built database is.
pub const SELECTION_MODE: &str = "canonical_five_hub_seed";

/// Plan §9's anchor: the Venture's EIV is the server's own industry parity fixture
/// (`server/tests/industryManufacturingParity.test.js:667`), so its seeded ask and bid are
/// the one pair a human can check by hand.
pub const ANCHOR_VENTURE: u32 = 32_880;
/// Plan's cheapest anchor and the reason the floor is ask-only: cost 2 ISK.
pub const ANCHOR_TRITANIUM: u32 = 34;

// ---------------------------------------------------------------------------- pricing

/// The two multipliers and the ask floor for one bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeedPricing {
    pub sell_multiplier: f64,
    pub buy_multiplier: f64,
    /// Applies to the **ask only**. See the module docs for the Tritanium pump this
    /// asymmetry prevents.
    pub price_floor: f64,
}

impl SeedPricing {
    /// `max(price_floor, round2(sell_multiplier * cost))`, never below [`MIN_SEED_PRICE`].
    pub fn ask(self, cost: f64) -> f64 {
        let priced = round_isk(self.sell_multiplier * cost);
        let floored = if priced < self.price_floor {
            round_isk(self.price_floor)
        } else {
            priced
        };
        floored.max(MIN_SEED_PRICE)
    }

    /// `round2(buy_multiplier * cost)`, clamped only to the hard [`MIN_SEED_PRICE`] minimum.
    ///
    /// **`price_floor` is not applied here and must never be.** A floored bid pays more than
    /// cost for every cheap material in the game, which is a reprocessing money pump.
    pub fn bid(self, cost: f64) -> f64 {
        round_isk(self.buy_multiplier * cost).max(MIN_SEED_PRICE)
    }

    /// True when the ask came out at the floor rather than at `multiplier x cost`.
    pub fn ask_is_floored(self, cost: f64) -> bool {
        round_isk(self.sell_multiplier * cost) < self.price_floor
    }
}

/// The two pricing sets a run uses, and the rule for choosing between them.
///
/// One value rather than two loose arguments because the seed plan needs it as well now:
/// `[index_seed] defer_to_npc_catalog = "conflicting-prices"` compares our computed ask and
/// bid against the NPC catalog's, so [`crate::seedplan::SeedPlan::resolve`] has to be able to
/// compute exactly the prices [`plan_seed_rows`] will write, from exactly the same inputs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeedPricingPair {
    /// `[index_seed]` — buckets A, A+B and B.
    pub index: SeedPricing,
    /// `[junk_seed]` — bucket D. It has no floor knob of its own and shares the index one.
    pub junk: SeedPricing,
}

impl SeedPricingPair {
    pub fn from_config(config: &SeederConfig) -> Self {
        Self {
            index: SeedPricing {
                sell_multiplier: config.index_seed.sell_multiplier,
                buy_multiplier: config.index_seed.buy_multiplier,
                price_floor: config.index_seed.price_floor,
            },
            junk: SeedPricing {
                sell_multiplier: config.junk_seed.sell_multiplier,
                buy_multiplier: config.junk_seed.buy_multiplier,
                price_floor: config.index_seed.price_floor,
            },
        }
    }

    /// The one predicate that decides which multipliers a type gets: anything outside A∪B is
    /// junk. `crate::cost` splits on the same test.
    pub fn is_junk(bucket: BucketFlags) -> bool {
        !(bucket.manufacturable_t1 || bucket.material)
    }

    pub fn for_bucket(self, bucket: BucketFlags) -> SeedPricing {
        if Self::is_junk(bucket) {
            self.junk
        } else {
            self.index
        }
    }
}

/// Profile-indexed pricing for the canonical plan. Sell and Buy are calculated independently;
/// later safety ceilings can lower `bid` without changing `ask` or the reference cost.
#[derive(Debug, Clone)]
pub struct SeedPricingProfiles {
    definitions: PricingProfilesConfig,
}

impl SeedPricingProfiles {
    pub fn from_config(config: &SeederConfig) -> Self {
        Self {
            definitions: config.pricing_profiles.clone(),
        }
    }

    pub fn for_profile(&self, profile: PricingProfile) -> SeedPricing {
        let configured = self
            .definitions
            .get(profile.key())
            .expect("SeederConfig::validate requires every canonical pricing profile");
        SeedPricing {
            sell_multiplier: configured.sell_multiplier,
            buy_multiplier: configured.buy_multiplier,
            price_floor: configured.sell_floor,
        }
    }
}

/// One seedable type resolved into the rows it will occupy — normally two, but one when the
/// NPC catalog took the other side (see [`SeedSides`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SeedRow {
    pub type_id: u32,
    pub name: String,
    /// `A`, `A+B`, `B` or `D`.
    pub bucket: &'static str,
    /// True for bucket D, which carries its own `[junk_seed]` multipliers and quantity.
    pub junk: bool,
    pub role: MarketRole,
    pub profile: PricingProfile,
    /// The manifest price this row was derived from.
    pub cost: f64,
    pub ask: f64,
    pub bid: f64,
    /// Which of `ask` and `bid` is actually written. Both prices are computed either way —
    /// the deferred one is what the deferral was decided against, and it is worth keeping
    /// visible in a report — but only the enabled sides reach the database.
    pub sides: SeedSides,
    /// Written to both `quantity` and `initial_quantity`.
    pub quantity: i64,
}

impl SeedRow {
    pub fn writes_ask(&self) -> bool {
        self.sides.ask
    }

    pub fn writes_bid(&self) -> bool {
        self.sides.bid
    }

    /// How many seed rows this type contributes: 2, or 1 when a side was deferred.
    pub fn row_count(&self) -> usize {
        usize::from(self.sides.ask) + usize::from(self.sides.bid)
    }
}

/// Turns the seed plan into the row set, applying each type's own bucket pricing.
pub fn plan_seed_rows(
    plan: &SeedPlan,
    static_data: &StaticData,
    pricing: SeedPricingPair,
    index_quantity: i64,
    junk_quantity: i64,
) -> Vec<SeedRow> {
    plan.seedable
        .values()
        .map(|entry| {
            let is_junk = SeedPricingPair::is_junk(entry.bucket);
            // The same call the deferral decision made, on the same cost, so the two can
            // never answer differently about what this row's ask and bid are.
            let priced = pricing.for_bucket(entry.bucket);
            let quantity = if is_junk {
                junk_quantity
            } else {
                index_quantity
            };
            SeedRow {
                type_id: entry.type_id,
                name: static_data
                    .item_type(entry.type_id)
                    .map(|item| item.name.clone())
                    .unwrap_or_else(|| "<unknown type>".to_string()),
                bucket: entry.bucket_label(),
                junk: is_junk,
                role: entry.role,
                profile: entry.profile,
                cost: entry.price,
                ask: priced.ask(entry.price),
                bid: priced.bid(entry.price),
                sides: entry.sides,
                quantity,
            }
        })
        .collect()
}

/// Canonical profile-aware row planning. One reference price feeds two independent profile
/// targets; role side ownership decides which targets reach SQLite.
pub fn plan_canonical_seed_rows(
    plan: &SeedPlan,
    static_data: &StaticData,
    pricing: &SeedPricingProfiles,
    quantity: i64,
) -> Vec<SeedRow> {
    plan.seedable
        .values()
        .map(|entry| {
            let target = pricing.for_profile(entry.profile);
            SeedRow {
                type_id: entry.type_id,
                name: static_data
                    .item_type(entry.type_id)
                    .map(|item| item.name.clone())
                    .unwrap_or_else(|| "<unknown type>".to_string()),
                bucket: entry.bucket_label(),
                junk: false,
                role: entry.role,
                profile: entry.profile,
                cost: entry.price,
                ask: target.ask(entry.price),
                bid: target.bid(entry.price),
                sides: entry.sides,
                quantity,
            }
        })
        .collect()
}

/// V2 row planning consumes each resolved side separately. Item policy has no station field;
/// fixed canonical distribution still replicates these rows after this step.
pub fn plan_v2_canonical_seed_rows(
    plan: &SeedPlan,
    static_data: &StaticData,
    quantity: i64,
) -> Result<Vec<SeedRow>> {
    let mut rows = Vec::with_capacity(plan.seedable.len());
    for entry in plan.seedable.values() {
        let side = plan.v2_sides.get(&entry.type_id).ok_or_else(|| {
            anyhow::anyhow!("type {} has no resolved V2 side prices", entry.type_id)
        })?;
        let sell = side.policy.sell.as_ref();
        let buy = side.policy.buy.as_ref();
        let ask = if let Some(sell) = sell {
            let reference = side
                .sell
                .ok_or_else(|| {
                    anyhow::anyhow!("type {} has no eligible Sell source", entry.type_id)
                })?
                .0;
            SeedPricing {
                sell_multiplier: sell.multiplier,
                buy_multiplier: 1.0,
                price_floor: sell.floor.unwrap_or(0.0),
            }
            .ask(reference)
        } else {
            0.0
        };
        let bid = if let Some(buy) = buy {
            let reference = side
                .buy
                .ok_or_else(|| {
                    anyhow::anyhow!("type {} has no eligible Buy source", entry.type_id)
                })?
                .0;
            SeedPricing {
                sell_multiplier: 1.0,
                buy_multiplier: buy.multiplier,
                price_floor: 0.0,
            }
            .bid(reference)
        } else {
            0.0
        };
        rows.push(SeedRow {
            type_id: entry.type_id,
            name: static_data
                .item_type(entry.type_id)
                .map(|item| item.name.clone())
                .unwrap_or_else(|| "<unknown type>".into()),
            bucket: entry.bucket_label(),
            junk: false,
            role: entry.role,
            profile: entry.profile,
            cost: entry.price,
            ask,
            bid,
            sides: entry.sides,
            quantity,
        });
    }
    Ok(rows)
}

fn resolve_v2_catalog(
    policy: &PolicyDocument,
    config: &SeederConfig,
    data: &StaticData,
    classification: &Classification,
    overlay: &crate::overlay::OverlayImport,
) -> Result<BTreeMap<u32, Resolution>> {
    let policy_config = config
        .policy
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("V2 catalog resolution requires [policy]"))?;
    let direct_lp = load_lp_reward_type_ids(&policy_config.lp_offer_catalog_path)?;
    let expanded_lp = load_expanded_lp_reward_type_ids(&policy_config.lp_offer_catalog_path, data)?;
    let facts = FactInputs {
        data,
        classification,
        direct_lp_rewards: &direct_lp,
        expanded_lp_rewards: &expanded_lp,
        npc_catalog_types: &overlay.npc_catalog_type_ids,
    };
    let mut catalog = Vec::new();
    for item in data.marketable_item_types() {
        catalog.push(CatalogItem {
            type_id: item.type_id,
            group_id: item.group_id,
            category_id: item.category_id,
            facts: facts.for_type(item.type_id),
        });
    }
    policy.validate_reachability(&catalog)?;
    let bridge_groups = config
        .market_roles
        .bridge_group_ids
        .iter()
        .copied()
        .collect();
    let bridge_types = config
        .market_roles
        .bridge_type_ids
        .iter()
        .copied()
        .collect();
    let progression_groups = config
        .market_roles
        .progression_npc_group_ids
        .iter()
        .copied()
        .collect();
    let progression_types = config
        .market_roles
        .progression_npc_type_ids
        .iter()
        .copied()
        .collect();
    let mut resolved = BTreeMap::new();
    for item in catalog {
        let decision =
            policy.resolve(item.type_id, item.group_id, item.category_id, &item.facts)?;
        // Differential migration gate only. V2 seed planning below consumes `decision`,
        // never this historical assignment.
        let old = if !policy_config.allow_intentional_policy_delta {
            classification.market_assignment_checked_with_supply_ladder(
                data,
                item.type_id,
                overlay.is_npc_catalog_type(item.type_id),
                expanded_lp.contains(&item.type_id),
                &bridge_groups,
                &bridge_types,
                &progression_groups,
                &progression_types,
                config.market_roles.moon_mining_available,
            )?
        } else {
            None
        };
        let old_profile = old.as_ref().map(|assignment| assignment.profile.key());
        let new_profile = decision
            .policy
            .as_ref()
            .and_then(|p| p.profile_id.as_deref());
        if !policy_config.allow_intentional_policy_delta && old_profile != new_profile {
            bail!(
                "V1/V2 dry policy mismatch type {} ({}): V1 profile {:?}, V2 {:?}, winner {:?}",
                item.type_id,
                data.item_type(item.type_id)
                    .map(|x| x.name.as_str())
                    .unwrap_or("?"),
                old_profile,
                new_profile,
                decision.winner_rule_id
            );
        }
        if let (Some(old), Some(new)) = (old, decision.policy.as_ref()) {
            if old.sides.sell != new.sides.has_sell() || old.sides.buy != new.sides.has_buy() {
                bail!(
                    "V1/V2 dry side mismatch type {}: V1 {:?}, V2 {:?}",
                    item.type_id,
                    old.sides,
                    new.sides
                );
            }
        }
        resolved.insert(item.type_id, decision);
    }
    Ok(resolved)
}

/// How many `seed_stock` and `seed_buy_orders` rows a planned row set writes. Not
/// `2 x rows.len()` any more: a type whose ask or bid was deferred writes one row, not two.
pub fn planned_side_counts(rows: &[SeedRow]) -> (usize, usize) {
    (
        rows.iter().filter(|row| row.writes_ask()).count(),
        rows.iter().filter(|row| row.writes_bid()).count(),
    )
}

/// Independent dry gate over persisted V1 prices and quantities at one verified canonical hub.
/// The full five-hub candidate comparison is repeated after the isolated SQLite write.
fn verify_v2_rows_against_baseline(rows: &[SeedRow], baseline_path: &Path) -> Result<()> {
    let db = Connection::open_with_flags(baseline_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening V1 baseline read-only {}", baseline_path.display()))?;
    db.pragma_update(None, "query_only", "ON")?;
    let mut expected = BTreeMap::<(u32, bool), (u64, i64, i64)>::new();
    for (table, is_buy) in [("seed_stock", false), ("seed_buy_orders", true)] {
        let sql = format!(
            "SELECT type_id, price, quantity, initial_quantity FROM {table} WHERE station_id = 60003760"
        );
        let mut statement = db.prepare(&sql)?;
        let cursor = statement.query_map([], |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        for result in cursor {
            let (type_id, price, quantity, initial_quantity) = result?;
            if expected
                .insert(
                    (type_id, is_buy),
                    (price.to_bits(), quantity, initial_quantity),
                )
                .is_some()
            {
                bail!("duplicate V1 baseline type {type_id} side {is_buy}");
            }
        }
    }
    let mut actual = BTreeMap::new();
    for row in rows {
        if row.writes_ask() {
            actual.insert(
                (row.type_id, false),
                (row.ask.to_bits(), row.quantity, row.quantity),
            );
        }
        if row.writes_bid() {
            actual.insert(
                (row.type_id, true),
                (row.bid.to_bits(), row.quantity, row.quantity),
            );
        }
    }
    if expected != actual {
        let keys = expected
            .keys()
            .chain(actual.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        let mismatches = keys
            .into_iter()
            .filter_map(|key| {
                let old = expected.get(&key);
                let new = actual.get(&key);
                (old != new).then(|| {
                    format!(
                        "type {} {} V1 {:?} V2 {:?}",
                        key.0,
                        if key.1 { "Buy" } else { "Sell" },
                        old.map(|x| (f64::from_bits(x.0), x.1, x.2)),
                        new.map(|x| (f64::from_bits(x.0), x.1, x.2))
                    )
                })
            })
            .take(12)
            .collect::<Vec<_>>();
        bail!(
            "V2 dry persisted-book parity failed: V1 {} sides, V2 {} sides; first mismatches:\n{}",
            expected.len(),
            actual.len(),
            mismatches.join("\n")
        );
    }
    println!(
        "  [V2 dry parity] {} stored V1 station/side/price/quantity rows match exactly",
        actual.len()
    );
    Ok(())
}

fn write_v2_dry_plan(
    path: &Path,
    config: &SeederConfig,
    plan: &SeedPlan,
    rows: &[SeedRow],
    resolved: &BTreeMap<u32, Resolution>,
    safety: &crate::funded::MarketSafetyReport,
) -> Result<()> {
    let policy_config = config
        .policy
        .as_ref()
        .expect("V2 dry plan needs policy config");
    for forbidden in [
        &config.output.database_path,
        &policy_config.path,
        policy_config
            .parity_baseline_path
            .as_ref()
            .unwrap_or(&config.output.database_path),
    ] {
        if path == forbidden {
            bail!(
                "V2 dry-plan JSON output cannot overwrite {}",
                forbidden.display()
            );
        }
    }
    let production = safety
        .production
        .iter()
        .map(|d| d.type_id)
        .collect::<BTreeSet<_>>();
    let ore = safety
        .ore
        .iter()
        .map(|d| d.type_id)
        .collect::<BTreeSet<_>>();
    let mut profile_usage = BTreeMap::<String, usize>::new();
    for row in rows {
        let id = plan.v2_sides[&row.type_id]
            .policy
            .profile_id
            .as_deref()
            .unwrap_or("inline");
        *profile_usage.entry(id.to_string()).or_default() += 1;
    }
    let entries = rows.iter().map(|row| {
        let side = &plan.v2_sides[&row.type_id];
        let mut obligations = Vec::new();
        if row.writes_bid() { obligations.push("final_buy_economic_safety"); }
        if side.policy.buy.as_ref().is_some_and(|s| s.source == PolicySource::FundedCost) {
            obligations.push("guaranteed_funded_source");
        }
        if production.contains(&row.type_id) { obligations.push("production_ceiling"); }
        if ore.contains(&row.type_id) { obligations.push("ore_refining_ceiling"); }
        if row.writes_ask() && row.writes_bid() { obligations.push("same_station_spread"); }
        serde_json::json!({
            "type_id": row.type_id, "name": row.name,
            "resolution": resolved[&row.type_id],
            "sell_reference": side.sell.map(|(price, source)| serde_json::json!({"price":price,"authority":source.key()})),
            "buy_reference": side.buy.map(|(price, source)| serde_json::json!({"price":price,"authority":source.key()})),
            "final_sell_price": row.writes_ask().then_some(row.ask),
            "final_buy_price": row.writes_bid().then_some(row.bid),
            "quantity_per_side_per_hub": row.quantity,
            "safety_obligations": obligations,
        })
    }).collect::<Vec<_>>();
    let drops = plan
        .dropped
        .iter()
        .map(|drop| {
            serde_json::json!({
                "type_id": drop.type_id, "name": drop.name, "reason": format!("{:?}", drop.reason),
                "resolution": resolved.get(&drop.type_id),
            })
        })
        .collect::<Vec<_>>();
    let matched = resolved
        .values()
        .filter(|x| x.winner_rule_id.is_some())
        .count();
    let explicit_excluded = resolved
        .values()
        .filter(|x| x.exclusion_reason.as_deref() == Some("explicit_unseeded"))
        .count();
    let (sell, buy) = planned_side_counts(rows);
    let policy_bytes = fs::read(&policy_config.path)?;
    let output = serde_json::json!({
        "format_version": crate::policy::FORMAT_VERSION,
        "policy_path": policy_config.path.display().to_string(),
        "policy_sha256": crate::policy_preview::sha256_hex(&policy_bytes),
        "catalog_types_considered": resolved.len(),
        "matched_types": matched,
        "finalized_seeded_types": rows.len(),
        "explicit_excluded_types": explicit_excluded,
        "sell_count_per_hub": sell, "buy_count_per_hub": buy,
        "unresolved_or_dropped_types": drops.len(),
        "ambiguous_rules": 0, "validation_failures": 0,
        "profile_usage": profile_usage,
        "distribution": {"station_ids": config.canonical_market.station_ids,
            "quantity_per_enabled_side": config.canonical_market.quantity},
        "entries": entries, "drops": drops,
    });
    let mut json = serde_json::to_string_pretty(&output)?;
    json.push('\n');
    fs::write(path, json)
        .with_context(|| format!("writing finalized V2 dry plan {}", path.display()))?;
    Ok(())
}

/// The five invariants that must hold before anything is committed. Each failure names the
/// offending type, because "the build failed" without a type ID is a two-hour bisect.
pub fn verify_seed_rows(
    rows: &[SeedRow],
    seedable_count: usize,
    index_quantity: i64,
    junk_quantity: i64,
) -> Result<()> {
    verify_seed_rows_with_crossing_mode(rows, seedable_count, index_quantity, junk_quantity, false)
}

/// TQ/custom quoting permits crossed quotes as warnings; structural checks are shared.
/// Legacy callers retain the original economic gate above.
pub fn verify_general_tq_seed_rows(
    rows: &[SeedRow],
    seedable_count: usize,
    index_quantity: i64,
    junk_quantity: i64,
) -> Result<()> {
    verify_seed_rows_with_crossing_mode(rows, seedable_count, index_quantity, junk_quantity, true)
}

fn verify_seed_rows_with_crossing_mode(
    rows: &[SeedRow],
    seedable_count: usize,
    index_quantity: i64,
    junk_quantity: i64,
    allow_crossings: bool,
) -> Result<()> {
    // 5. no type appears twice. Checked first: a duplicate makes every
    //    count below lie, so naming it is more useful than the count mismatch it causes.
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    for row in rows {
        if !seen.insert(row.type_id) {
            bail!(
                "type {} ({}) appears twice in the seed row set. Each type may hold exactly \
                one role/profile and one owner per enabled side. Canonical inserts must never \
                 rely on a database collision to choose a winner.",
                row.type_id,
                row.name
            );
        }
    }

    for row in rows {
        // 1. every row's price is finite and > 0.
        for (side, enabled, price) in [
            ("ask", row.writes_ask(), row.ask),
            ("bid", row.writes_bid(), row.bid),
        ] {
            if !enabled {
                continue;
            }
            if !price.is_finite() || price <= 0.0 {
                bail!(
                    "type {} ({}) resolved to a {} of {} from cost {}. Every seed price must \
                     be finite and greater than 0: a zero ask hands out free items and a NaN \
                     cannot be marshalled to the client.",
                    row.type_id,
                    row.name,
                    side,
                    price,
                    row.cost
                );
            }
        }

        // 2. bid <= ask for every type.
        if !allow_crossings && row.writes_ask() && row.writes_bid() && row.bid > row.ask {
            bail!(
                "type {} ({}) would bid {} against its own ask of {} (cost {}, bucket {}). A \
                 bid above the ask at the same station is an instant arbitrage loop: buy the \
                 seed's stock and sell it straight back for a profit, forever. Check the \
                 [{}] sell_multiplier / buy_multiplier pair.",
                row.type_id,
                row.name,
                row.bid,
                row.ask,
                row.cost,
                row.bucket,
                if row.junk { "junk_seed" } else { "index_seed" }
            );
        }

        // 3. quantity == initial_quantity == the configured quantity. The equality of the two
        //    columns is structural in the INSERT (`?7, ?7`) and re-checked in SQL by
        //    `verify_seed_tables`; what this arm catches is a row planned at the wrong value.
        let expected = if row.junk {
            junk_quantity
        } else {
            index_quantity
        };
        if row.quantity != expected {
            bail!(
                "type {} ({}) was planned at quantity {} but [{}] quantity is {}.",
                row.type_id,
                row.name,
                row.quantity,
                if row.junk { "junk_seed" } else { "index_seed" },
                expected
            );
        }
    }

    // 4. one planned row per seedable type, and every one of them writes at least one side.
    //    A type with neither side is not a seed row at all — the plan drops it as a
    //    both-sides deferral — so one here would silently plan a no-op.
    if rows.len() != seedable_count {
        bail!(
            "the seed plan holds {} seedable types but {} rows were prepared. Every seedable \
             type must produce exactly one planned row.",
            seedable_count,
            rows.len(),
        );
    }
    if let Some(row) = rows.iter().find(|row| row.sides.is_empty()) {
        bail!(
            "type {} ({}) is planned with neither an ask nor a bid. A type that loses both \
             sides to the NPC catalog is dropped from the plan, not carried into it as a row \
             that writes nothing.",
            row.type_id,
            row.name
        );
    }

    Ok(())
}

/// `total_ask_quantity` in both summary tables is a `SUM(quantity)` over every station
/// holding the type, stored in an i64 column. This proves the sum cannot overflow at the
/// configured quantity, and returns how many stations it would take before it could.
pub fn summary_sum_headroom(stations_per_type: u64, quantity: i64) -> Result<i64> {
    let total = i128::from(stations_per_type) * i128::from(quantity);
    if total > i128::from(i64::MAX) {
        bail!(
            "{stations_per_type} stations x quantity {quantity} = {total}, which overflows the \
             i64 SUM(quantity) behind total_ask_quantity."
        );
    }
    Ok((i128::from(i64::MAX) / i128::from(quantity.max(1))) as i64)
}

// ------------------------------------------------------------------------- seed writing

/// The station geometry every seed row copies. Denormalised into the seed tables so the
/// daemon's per-region and per-system book queries never join.
#[derive(Debug, Clone, PartialEq)]
pub struct SeedStation {
    pub station_id: u64,
    pub solar_system_id: u32,
    pub constellation_id: u32,
    pub region_id: u32,
    pub station_name: String,
    pub region_name: String,
}

/// Jita 4-4, for tests anywhere in the crate that need somewhere to put a row. The audit's
/// book is keyed by station now, so a `SeedRow` on its own is no longer a complete fixture.
#[cfg(test)]
pub fn test_station() -> SeedStation {
    SeedStation {
        station_id: 60_003_760,
        solar_system_id: 30_000_142,
        constellation_id: 20_000_020,
        region_id: 10_000_002,
        station_name: "Jita IV - Moon 4 - Caldari Navy Assembly Plant".to_string(),
        region_name: "The Forge".to_string(),
    }
}

impl From<&StationRecord> for SeedStation {
    fn from(station: &StationRecord) -> Self {
        Self {
            station_id: station.station_id,
            solar_system_id: station.solar_system_id,
            constellation_id: station.constellation_id,
            region_id: station.region_id,
            station_name: station.station_name.clone(),
            region_name: station.region_name.clone(),
        }
    }
}

/// How many rows each seed table must hold once every pass has run.
///
/// With the NPC overlay in the picture the row count is no longer "one per planned type":
/// the overlay writes first at ~5,000 stations, and the index pass then `INSERT OR
/// REPLACE`s over whatever it finds at its own station. So the total is
/// `overlay + planned - (overlay rows the index pass replaced)`, and the caller — which is
/// the only thing that knows both sets — computes it with [`SeedTableCounts::after_overlay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedTableCounts {
    pub sell_rows: i64,
    pub buy_rows: i64,
}

impl SeedTableCounts {
    /// No overlay: one row per planned side, and nothing else.
    pub fn index_only(planned: &[SeedRow]) -> Self {
        let (planned_sell, planned_buy) = planned_side_counts(planned);
        Self {
            sell_rows: planned_sell as i64,
            buy_rows: planned_buy as i64,
        }
    }

    pub fn canonical(planned: &[SeedRow], station_count: usize) -> Self {
        let (planned_sell, planned_buy) = planned_side_counts(planned);
        Self {
            sell_rows: (planned_sell * station_count) as i64,
            buy_rows: (planned_buy * station_count) as i64,
        }
    }

    /// The overlay wrote `overlay_sell` / `overlay_buy` rows, of which `replaced_sell` /
    /// `replaced_buy` sat at the index station on a side the index pass also seeds and are
    /// therefore overwritten rather than added.
    pub fn after_overlay(
        planned: &[SeedRow],
        overlay_sell: usize,
        overlay_buy: usize,
        replaced_sell: usize,
        replaced_buy: usize,
    ) -> Self {
        let (planned_sell, planned_buy) = planned_side_counts(planned);
        Self {
            sell_rows: (overlay_sell + planned_sell - replaced_sell) as i64,
            buy_rows: (overlay_buy + planned_buy - replaced_buy) as i64,
        }
    }
}

/// How many overlay rows the index pass will overwrite rather than add: the ones sitting at
/// the index station on a **side** the index seed also prices. `(sell, buy)`.
///
/// Per side, not per type: a type whose ask was deferred to the NPC catalog leaves TQ's own
/// ask at the index station standing, so that row is added rather than replaced, and counting
/// it as replaced would make the expected row count one short.
///
/// This is the measured size of the collision the pass ordering exists to resolve — TQ selling
/// the same skillbook-adjacent types at Jita 4-4 that our own index seed prices — so the build
/// prints it.
pub fn overlay_rows_replaced_at(
    import: &crate::overlay::OverlayImport,
    station_id: u64,
    planned: &[SeedRow],
) -> (usize, usize) {
    let count = |book: &BTreeMap<(u64, u32), crate::overlay::SeedLiquidity>,
                 writes: fn(&SeedRow) -> bool| {
        planned
            .iter()
            .filter(|row| writes(row))
            .map(|row| row.type_id)
            .collect::<BTreeSet<_>>()
            .iter()
            .filter(|type_id| book.contains_key(&(station_id, **type_id)))
            .count()
    };
    (
        count(&import.sell, SeedRow::writes_ask),
        count(&import.buy, SeedRow::writes_bid),
    )
}

/// Writes the planned sides of each planned type — normally one `seed_stock` row and one
/// `seed_buy_orders` row, one row where the NPC catalog took the other side — in a single
/// transaction, verifying the tables in SQL before committing.
///
/// `INSERT OR REPLACE` mirrors v2's `write_custom_station` (`main.rs:1425-1508`): the seed
/// tables are keyed `(station_id, type_id)`, the NPC overlay has already written its rows,
/// and the index seed must win at the one station it owns **on the sides it writes**. A
/// deferred side is simply not written, which leaves TQ's own row there untouched — that is
/// the point of deferring it. That ordering is asserted in SQL by [`verify_seed_tables`]
/// before this transaction commits.
pub fn write_seed_rows(
    connection: &mut Connection,
    station: &SeedStation,
    rows: &[SeedRow],
    updated_at: &str,
    expected: SeedTableCounts,
    progress: Option<&ProgressBar>,
) -> Result<(u64, u64)> {
    let transaction = connection.transaction()?;
    {
        let mut sell = transaction.prepare(
            "INSERT OR REPLACE INTO seed_stock (
               station_id, solar_system_id, constellation_id, region_id,
               type_id, price, quantity, initial_quantity, price_version, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)",
        )?;
        let mut buy = transaction.prepare(
            "INSERT OR REPLACE INTO seed_buy_orders (
               station_id, solar_system_id, constellation_id, region_id,
               type_id, price, quantity, initial_quantity, price_version, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)",
        )?;
        for row in rows {
            if row.writes_ask() {
                sell.execute(params![
                    station.station_id,
                    station.solar_system_id,
                    station.constellation_id,
                    station.region_id,
                    row.type_id,
                    row.ask,
                    row.quantity,
                    SEED_PRICE_VERSION,
                    updated_at,
                ])?;
            }
            if row.writes_bid() {
                buy.execute(params![
                    station.station_id,
                    station.solar_system_id,
                    station.constellation_id,
                    station.region_id,
                    row.type_id,
                    row.bid,
                    row.quantity,
                    SEED_PRICE_VERSION,
                    updated_at,
                ])?;
            }
            if let Some(progress) = progress {
                progress.inc(row.row_count() as u64);
            }
        }
    }

    // The same invariants, asked of the database rather than of the plan, while the
    // transaction can still be thrown away.
    verify_seed_tables(&transaction, rows, station.station_id, expected)?;
    transaction.commit()?;
    let (planned_sell, planned_buy) = planned_side_counts(rows);
    Ok((planned_sell as u64, planned_buy as u64))
}

/// Writes one already-verified canonical book to every approved hub in one transaction.
/// Plain `INSERT` is intentional: any collision is an error, never a replacement policy.
pub fn write_canonical_seed_rows(
    connection: &mut Connection,
    stations: &[SeedStation],
    rows: &[SeedRow],
    updated_at: &str,
    progress: Option<&ProgressBar>,
) -> Result<(u64, u64)> {
    let expected = SeedTableCounts::canonical(rows, stations.len());
    let transaction = connection.transaction()?;
    {
        let mut sell = transaction.prepare(
            "INSERT INTO seed_stock (
               station_id, solar_system_id, constellation_id, region_id,
               type_id, price, quantity, initial_quantity, price_version, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)",
        )?;
        let mut buy = transaction.prepare(
            "INSERT INTO seed_buy_orders (
               station_id, solar_system_id, constellation_id, region_id,
               type_id, price, quantity, initial_quantity, price_version, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)",
        )?;
        for station in stations {
            for row in rows {
                if row.writes_ask() {
                    sell.execute(params![
                        station.station_id,
                        station.solar_system_id,
                        station.constellation_id,
                        station.region_id,
                        row.type_id,
                        row.ask,
                        row.quantity,
                        SEED_PRICE_VERSION,
                        updated_at,
                    ])?;
                }
                if row.writes_bid() {
                    buy.execute(params![
                        station.station_id,
                        station.solar_system_id,
                        station.constellation_id,
                        station.region_id,
                        row.type_id,
                        row.bid,
                        row.quantity,
                        SEED_PRICE_VERSION,
                        updated_at,
                    ])?;
                }
                if let Some(progress) = progress {
                    progress.inc(row.row_count() as u64);
                }
            }
        }
    }
    verify_canonical_seed_tables(&transaction, stations, rows, expected)?;
    transaction.commit()?;
    Ok((expected.sell_rows as u64, expected.buy_rows as u64))
}

fn verify_canonical_seed_tables(
    connection: &Connection,
    stations: &[SeedStation],
    planned: &[SeedRow],
    expected: SeedTableCounts,
) -> Result<()> {
    let station_ids = stations
        .iter()
        .map(|station| station.station_id)
        .collect::<BTreeSet<_>>();
    if station_ids.len() != stations.len() {
        bail!("canonical station list contains a duplicate station ID");
    }
    for (table, expected_total, writes, price) in [
        (
            "seed_stock",
            expected.sell_rows,
            SeedRow::writes_ask as fn(&SeedRow) -> bool,
            (|row: &SeedRow| row.ask) as fn(&SeedRow) -> f64,
        ),
        (
            "seed_buy_orders",
            expected.buy_rows,
            SeedRow::writes_bid as fn(&SeedRow) -> bool,
            (|row: &SeedRow| row.bid) as fn(&SeedRow) -> f64,
        ),
    ] {
        let total: i64 =
            connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
        if total != expected_total {
            bail!("{table} holds {total} rows, expected {expected_total} canonical rows");
        }
        let expected_per_station = planned.iter().filter(|row| writes(row)).count() as i64;
        for station in stations {
            let count: i64 = connection.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE station_id = ?1"),
                params![station.station_id],
                |row| row.get(0),
            )?;
            if count != expected_per_station {
                bail!(
                    "{table} holds {count} rows at station {}, expected {expected_per_station}",
                    station.station_id
                );
            }
            for planned_row in planned.iter().filter(|row| writes(row)) {
                let stored: Option<(f64, i64, i64)> = connection
                    .query_row(
                        &format!(
                            "SELECT price, quantity, initial_quantity FROM {table} WHERE station_id = ?1 AND type_id = ?2"
                        ),
                        params![station.station_id, planned_row.type_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .ok();
                let Some((stored_price, quantity, initial_quantity)) = stored else {
                    bail!(
                        "{table} is missing type {} at canonical station {}",
                        planned_row.type_id,
                        station.station_id
                    );
                };
                if stored_price != price(planned_row)
                    || quantity != planned_row.quantity
                    || initial_quantity != planned_row.quantity
                {
                    bail!(
                        "{table} type {} differs at canonical station {}",
                        planned_row.type_id,
                        station.station_id
                    );
                }
            }
        }
        let outside: i64 = connection.query_row(
            &format!(
                "SELECT COUNT(*) FROM {table} WHERE station_id NOT IN ({})",
                station_ids
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            [],
            |row| row.get(0),
        )?;
        if outside != 0 {
            bail!("{table} contains {outside} rows outside the five canonical hubs");
        }
    }
    Ok(())
}

/// Re-asks [`verify_seed_rows`]'s questions in SQL, inside the open transaction, plus the one
/// question only SQL can answer: **did the index pass actually win at its own station?**
///
/// The crossed-pair test is scoped to the index station on purpose. Overlay rows are TQ's
/// prices, not ours; a station where TQ's own NPC bid sits above its NPC ask is a fact about
/// the snapshot, and the place to report it is the audit (check 4), not an aborted build.
/// What this must never let through is a crossed pair *we* wrote.
///
/// Per-side deferral widens what that test covers, for free. Once one side of a type at the
/// index station can be ours and the other TQ's, a bid of ours above a TQ ask **at Jita
/// itself** becomes expressible — and it would be the cheapest exploit in the game, needing no
/// haul at all, while sitting in check 4's same-station blind spot. It cannot happen, because
/// [`crate::seedplan::NpcCatalog`] measures TQ's best prices including the index station's, so
/// a surviving bid of ours is at or below every TQ ask there. This query is that argument's
/// receipt.
pub fn verify_seed_tables(
    connection: &Connection,
    planned: &[SeedRow],
    station_id: u64,
    expected: SeedTableCounts,
) -> Result<()> {
    for (table, expected) in [
        ("seed_stock", expected.sell_rows),
        ("seed_buy_orders", expected.buy_rows),
    ] {
        let count: i64 =
            connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
        if count != expected {
            bail!(
                "{table} holds {count} rows but the overlay and the seed plan together account \
                 for {expected}"
            );
        }
        // (station_id, type_id) is the primary key, so a duplicate type on one side cannot
        // survive the insert; it shows up as a short count above. This checks the columns
        // the insert is supposed to keep identical.
        let mismatched: i64 = connection.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE quantity <> initial_quantity"),
            [],
            |row| row.get(0),
        )?;
        if mismatched != 0 {
            bail!("{table} holds {mismatched} rows whose quantity differs from initial_quantity");
        }
        let bad_price: Option<u32> = connection
            .query_row(
                &format!("SELECT type_id FROM {table} WHERE price IS NULL OR price <= 0 LIMIT 1"),
                [],
                |row| row.get(0),
            )
            .ok();
        if let Some(type_id) = bad_price {
            bail!("{table} holds a non-positive price for type {type_id}");
        }
    }

    let total: i64 = connection.query_row(
        "SELECT (SELECT COUNT(*) FROM seed_stock) + (SELECT COUNT(*) FROM seed_buy_orders)",
        [],
        |row| row.get(0),
    )?;
    let expected_total = expected.sell_rows + expected.buy_rows;
    if total != expected_total {
        bail!("seed tables hold {total} rows in total, expected {expected_total}");
    }

    let crossed: Option<(u32, f64, f64)> = connection
        .query_row(
            "SELECT buy.type_id, buy.price, sell.price
               FROM seed_buy_orders AS buy
               JOIN seed_stock AS sell
                 ON sell.station_id = buy.station_id AND sell.type_id = buy.type_id
              WHERE buy.station_id = ?1 AND buy.price > sell.price
              LIMIT 1",
            params![station_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok();
    if let Some((type_id, bid, ask)) = crossed {
        bail!("type {type_id} bids {bid} against its own ask of {ask} at the seed station");
    }

    verify_index_pass_won(connection, planned, station_id)
}

/// **The ordering invariant, proven in SQL.** Every planned *side* must hold exactly the price
/// the index pass computed, at the index station — and every **deferred** side must not.
///
/// The first half catches the one mistake the overlay makes possible: writing the two passes
/// in the wrong order. The overlay carries TQ's own Jita 4-4 prices for a few hundred types
/// the index seed also prices, and both passes write `(60003760, type_id)`. Run the overlay
/// last and it silently replaces our computed 3x-cost price with a TQ snapshot price — no
/// error, no row-count change, just a different economy. So the prices are read back and
/// compared.
///
/// The second half is what `defer_to_npc_catalog = "conflicting-prices"` needs: a side we
/// deferred must not carry our price. The whole point of dropping it was to leave TQ's own
/// row (or no row) there, and a write that leaked through would put back exactly the leg of
/// the haul loop the deferral removed.
fn verify_index_pass_won(
    connection: &Connection,
    planned: &[SeedRow],
    station_id: u64,
) -> Result<()> {
    if planned.is_empty() {
        return Ok(());
    }
    connection.execute_batch(
        "DROP TABLE IF EXISTS temp.seed_plan_check;
         CREATE TEMP TABLE seed_plan_check (
           type_id INTEGER PRIMARY KEY, name TEXT NOT NULL, ask REAL NOT NULL, bid REAL NOT NULL,
           writes_ask INTEGER NOT NULL, writes_bid INTEGER NOT NULL
         );",
    )?;
    {
        let mut insert = connection.prepare(
            "INSERT INTO temp.seed_plan_check
               (type_id, name, ask, bid, writes_ask, writes_bid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for row in planned {
            insert.execute(params![
                row.type_id,
                row.name,
                row.ask,
                row.bid,
                row.writes_ask(),
                row.writes_bid()
            ])?;
        }
    }

    for (table, column, flag, side) in [
        ("seed_stock", "ask", "writes_ask", "ask"),
        ("seed_buy_orders", "bid", "writes_bid", "bid"),
    ] {
        let offender: Option<(u32, String, f64, Option<f64>)> = connection
            .query_row(
                &format!(
                    "SELECT plan.type_id, plan.name, plan.{column}, seed.price
                       FROM temp.seed_plan_check AS plan
                       LEFT JOIN {table} AS seed
                         ON seed.station_id = ?1 AND seed.type_id = plan.type_id
                      WHERE plan.{flag} = 1
                        AND (seed.type_id IS NULL OR seed.price <> plan.{column})
                      LIMIT 1"
                ),
                params![station_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .ok();
        if let Some((type_id, name, planned_price, stored)) = offender {
            connection.execute_batch("DROP TABLE IF EXISTS temp.seed_plan_check;")?;
            match stored {
                None => bail!(
                    "type {type_id} ({name}) has no {side} row at station {station_id}, but the \
                     index pass planned one at {planned_price}."
                ),
                Some(stored) => bail!(
                    "type {type_id} ({name}) holds a {side} of {stored} at station {station_id} \
                     but the index pass computed {planned_price}. The NPC overlay must be \
                     written BEFORE the index pass: both write (station_id, type_id) and the \
                     index pass's INSERT OR REPLACE is what makes it win at this station."
                ),
            }
        }

        // …and the mirror: a deferred side must not be ours.
        let leaked: Option<(u32, String, f64)> = connection
            .query_row(
                &format!(
                    "SELECT plan.type_id, plan.name, seed.price
                       FROM temp.seed_plan_check AS plan
                       JOIN {table} AS seed
                         ON seed.station_id = ?1 AND seed.type_id = plan.type_id
                      WHERE plan.{flag} = 0 AND seed.price = plan.{column}
                      LIMIT 1"
                ),
                params![station_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();
        if let Some((type_id, name, stored)) = leaked {
            connection.execute_batch("DROP TABLE IF EXISTS temp.seed_plan_check;")?;
            bail!(
                "type {type_id} ({name}) holds a {side} of {stored} at station {station_id}, \
                 which is exactly the price the index pass computed — but that side was \
                 deferred to the NPC catalog and must not have been written. Leaving it in \
                 restores the leg of the cross-station haul loop the deferral existed to \
                 remove."
            );
        }
    }

    connection.execute_batch("DROP TABLE IF EXISTS temp.seed_plan_check;")?;
    Ok(())
}

/// Stations where an ask and a bid for one type cross, across the whole database. Non-fatal
/// and reported rather than enforced: after the overlay lands these are TQ's own prices (see
/// [`verify_seed_tables`]), and the audit is where they get judged.
pub fn count_crossed_pairs(connection: &Connection) -> Result<i64> {
    let crossed: i64 = connection.query_row(
        "SELECT COUNT(*)
           FROM seed_buy_orders AS buy
           JOIN seed_stock AS sell
             ON sell.station_id = buy.station_id AND sell.type_id = buy.type_id
          WHERE buy.price > sell.price",
        [],
        |row| row.get(0),
    )?;
    Ok(crossed)
}

// ---------------------------------------------------------------------------- summaries

/// Rebuilds one summary table from the seed tables.
///
/// Ported from v2 (`main.rs:1552-1696`), which spells the identical query out twice — once
/// keyed on `region_id` and once on `solar_system_id`. The only difference between them is
/// that column name, so it is a parameter here. With exactly one seeded station every group
/// has one member on the index-only path; with the NPC overlay imported the aggregate form is
/// what makes a type sold at forty stations summarise to one best ask per region.
pub fn rebuild_summaries(connection: &mut Connection, table: &str, key: &str) -> Result<usize> {
    let updated_at = now_rfc3339();
    let sql = format!(
        "INSERT INTO {table} (
           {key}, type_id, best_ask_price, total_ask_quantity, best_ask_station_id,
           best_bid_price, total_bid_quantity, best_bid_station_id, updated_at
         )
         WITH sell_agg AS (
           SELECT {key}, type_id, MIN(price) AS best_ask_price, SUM(quantity) AS total_ask_quantity
           FROM seed_stock
           WHERE quantity > 0
           GROUP BY {key}, type_id
         ),
         sell_station AS (
           SELECT seed.{key}, seed.type_id, MIN(seed.station_id) AS station_id
           FROM seed_stock AS seed
           JOIN sell_agg
             ON sell_agg.{key} = seed.{key}
            AND sell_agg.type_id = seed.type_id
            AND sell_agg.best_ask_price = seed.price
           WHERE seed.quantity > 0
           GROUP BY seed.{key}, seed.type_id
         ),
         buy_agg AS (
           SELECT {key}, type_id, MAX(price) AS best_bid_price, SUM(quantity) AS total_bid_quantity
           FROM seed_buy_orders
           WHERE quantity > 0
           GROUP BY {key}, type_id
         ),
         buy_station AS (
           SELECT seed.{key}, seed.type_id, MIN(seed.station_id) AS station_id
           FROM seed_buy_orders AS seed
           JOIN buy_agg
             ON buy_agg.{key} = seed.{key}
            AND buy_agg.type_id = seed.type_id
            AND buy_agg.best_bid_price = seed.price
           WHERE seed.quantity > 0
           GROUP BY seed.{key}, seed.type_id
         ),
         keys AS (
           SELECT {key}, type_id FROM sell_agg
           UNION
           SELECT {key}, type_id FROM buy_agg
         )
         SELECT
           keys.{key},
           keys.type_id,
           sell_agg.best_ask_price,
           COALESCE(sell_agg.total_ask_quantity, 0),
           sell_station.station_id,
           buy_agg.best_bid_price,
           COALESCE(buy_agg.total_bid_quantity, 0),
           buy_station.station_id,
           ?1
         FROM keys
         LEFT JOIN sell_agg
           ON sell_agg.{key} = keys.{key} AND sell_agg.type_id = keys.type_id
         LEFT JOIN sell_station
           ON sell_station.{key} = keys.{key} AND sell_station.type_id = keys.type_id
         LEFT JOIN buy_agg
           ON buy_agg.{key} = keys.{key} AND buy_agg.type_id = keys.type_id
         LEFT JOIN buy_station
           ON buy_station.{key} = keys.{key} AND buy_station.type_id = keys.type_id"
    );

    let transaction = connection.transaction()?;
    transaction.execute(&format!("DELETE FROM {table}"), [])?;
    let inserted = transaction.execute(&sql, params![updated_at])?;
    transaction.commit()?;
    Ok(inserted)
}

// ------------------------------------------------------------------- database plumbing

/// v2 `main.rs:1006-1030`, verbatim: bulk-load pragmas, the shared schema, then the runtime
/// indexes dropped so inserts do not maintain them.
pub fn open_build_connection(
    path: &Path,
    cache_size_kib: i32,
    page_size_bytes: u32,
    worker_threads: usize,
) -> Result<Connection> {
    let connection = Connection::open(path).with_context(|| {
        format!(
            "failed to create market seed database at {}",
            path.to_string_lossy()
        )
    })?;
    connection.pragma_update(None, "page_size", page_size_bytes)?;
    connection.pragma_update(None, "journal_mode", "OFF")?;
    connection.pragma_update(None, "synchronous", "OFF")?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    connection.pragma_update(None, "cache_size", -cache_size_kib)?;
    connection.pragma_update(None, "threads", worker_threads.max(1))?;
    connection.pragma_update(None, "foreign_keys", "OFF")?;
    connection.pragma_update(None, "cache_spill", "OFF")?;
    connection.execute_batch(SCHEMA_SQL)?;
    connection.execute_batch(RUNTIME_INDEX_DROP_SQL)?;
    Ok(connection)
}

/// v2 `main.rs:1032-1130`. Clears every table, then writes the four static ones.
///
/// **`market_types.market_group_id` is NOT NULL in the schema but nullable in the SDE**, so
/// v2 writes `market_group_id.unwrap_or(0)` — a type with no market group is stored with
/// group 0 rather than dropped. `group_id`, `category_id` and `portion_size` get the same
/// treatment (0, 0 and 1), and a missing `group_name` becomes `"Unknown"`. `base_price` and
/// `volume` are the two genuinely nullable columns and stay `NULL`. Every published item
/// type is written, not just the seed universe: the daemon resolves names and volumes for
/// types nobody seeds.
pub fn write_static_tables(
    connection: &mut Connection,
    static_data: &StaticData,
) -> Result<StaticCounts> {
    let regions = region_rows(static_data);
    let counts = StaticCounts {
        regions: regions.len(),
        solar_systems: static_data.solar_systems.len(),
        stations: static_data.stations.len(),
        market_types: static_data.item_types.len(),
    };
    let progress = row_progress_bar("Static tables", counts.total() as u64);

    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM manifest", [])?;
    transaction.execute("DELETE FROM price_history", [])?;
    transaction.execute("DELETE FROM region_summaries", [])?;
    transaction.execute("DELETE FROM system_seed_summaries", [])?;
    transaction.execute("DELETE FROM seed_buy_orders", [])?;
    transaction.execute("DELETE FROM market_order_events", [])?;
    transaction.execute("DELETE FROM market_orders", [])?;
    transaction.execute("DELETE FROM seed_stock", [])?;
    transaction.execute("DELETE FROM stations", [])?;
    transaction.execute("DELETE FROM solar_systems", [])?;
    transaction.execute("DELETE FROM regions", [])?;
    transaction.execute("DELETE FROM market_types", [])?;

    {
        let mut statement =
            transaction.prepare("INSERT INTO regions (region_id, region_name) VALUES (?1, ?2)")?;
        for (region_id, region_name) in &regions {
            statement.execute(params![region_id, region_name])?;
            progress.inc(1);
        }
    }

    {
        let mut statement = transaction.prepare(
            "INSERT INTO solar_systems (
               solar_system_id, region_id, constellation_id, solar_system_name, security
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for system in &static_data.solar_systems {
            statement.execute(params![
                system.solar_system_id,
                system.region_id,
                system.constellation_id,
                system.solar_system_name,
                system.security
            ])?;
            progress.inc(1);
        }
    }

    {
        let mut statement = transaction.prepare(
            "INSERT INTO stations (
               station_id, solar_system_id, constellation_id, region_id, station_name, security
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for station in &static_data.stations {
            statement.execute(params![
                station.station_id,
                station.solar_system_id,
                station.constellation_id,
                station.region_id,
                station.station_name,
                station.security
            ])?;
            progress.inc(1);
        }
    }

    {
        let mut statement = transaction.prepare(
            "INSERT INTO market_types (
               type_id, group_id, category_id, market_group_id, name, group_name,
               base_price, volume, portion_size, published
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        for item_type in &static_data.item_types {
            statement.execute(params![
                item_type.type_id,
                item_type.group_id.unwrap_or(0),
                item_type.category_id.unwrap_or(0),
                item_type.market_group_id.unwrap_or(0),
                item_type.name,
                item_type
                    .group_name
                    .clone()
                    .unwrap_or_else(|| "Unknown".to_string()),
                item_type.base_price,
                item_type.volume,
                item_type.portion_size.unwrap_or(1),
                if item_type.published { 1 } else { 0 }
            ])?;
            progress.inc(1);
        }
    }

    transaction.commit()?;
    progress.finish_with_message(format!(
        "Static tables wrote {} rows",
        format_count(counts.total())
    ));
    Ok(counts)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StaticCounts {
    pub regions: usize,
    pub solar_systems: usize,
    pub stations: usize,
    pub market_types: usize,
}

impl StaticCounts {
    pub fn total(self) -> usize {
        self.regions + self.solar_systems + self.stations + self.market_types
    }
}

/// v3's static data carries no region table of its own, so regions are folded out of the
/// station list exactly as v2 does (`main.rs:873-885`).
fn region_rows(static_data: &StaticData) -> Vec<(u32, String)> {
    let mut regions: BTreeMap<u32, String> = BTreeMap::new();
    for station in &static_data.stations {
        regions
            .entry(station.region_id)
            .or_insert_with(|| station.region_name.clone());
    }
    regions.into_iter().collect()
}

/// v2 `main.rs:1744-1749`.
pub(crate) fn build_runtime_indexes(connection: &Connection) -> Result<()> {
    let spinner = spinner("Building runtime indexes");
    connection.execute_batch(RUNTIME_INDEX_SQL)?;
    spinner.finish_with_message("Built runtime indexes");
    Ok(())
}

/// v2 `main.rs:1751-1759`.
pub(crate) fn finalize_database(connection: &Connection) -> Result<()> {
    let spinner = spinner("Finalizing SQLite database");
    connection.pragma_update(None, "analysis_limit", 10_000)?;
    connection.execute_batch("ANALYZE;")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    spinner.finish_with_message("Finalized SQLite database");
    Ok(())
}

/// v2 `main.rs:912-939`: an existing database is never replaced without an explicit,
/// typed-out `OVERWRITE` — or `--yes` for the docker/CI path.
fn confirm_replace_database(database_path: &Path, yes: bool) -> Result<()> {
    if yes {
        println!(
            "{} replacing existing database: {}",
            style("[confirm]").yellow().bold(),
            database_path.to_string_lossy()
        );
        return Ok(());
    }

    println!(
        "{} existing seeded market database detected:",
        style("[confirm]").yellow().bold()
    );
    println!("  {}", database_path.to_string_lossy());
    if let Ok(metadata) = fs::metadata(database_path) {
        println!("  size: {}", format_bytes(metadata.len()));
    }
    println!(
        "  it will be moved aside to {} and deleted once the new build installs cleanly.",
        backup_database_path(database_path).to_string_lossy()
    );
    println!();
    print!("Type OVERWRITE to replace it, or anything else to stop: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim() != "OVERWRITE" {
        bail!("database replacement cancelled");
    }
    Ok(())
}

fn database_family_paths(database_path: &Path) -> Vec<PathBuf> {
    vec![
        database_path.to_path_buf(),
        PathBuf::from(format!("{}-wal", database_path.to_string_lossy())),
        PathBuf::from(format!("{}-shm", database_path.to_string_lossy())),
    ]
}

fn remove_existing_database_files(database_path: &Path) -> Result<()> {
    for path in database_family_paths(database_path) {
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.to_string_lossy()))?;
        }
    }
    Ok(())
}

pub fn staged_database_path(database_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.building", database_path.to_string_lossy()))
}

pub fn backup_database_path(database_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.previous", database_path.to_string_lossy()))
}

/// v2 `main.rs:959-996`. The live database is only ever moved, never truncated: it becomes
/// `.previous`, the staged build is renamed into its place, and only then is the backup
/// deleted. If the install rename fails the backup is put straight back, so a failed build
/// leaves the daemon's database exactly where it was.
pub(crate) fn install_built_database(staging_path: &Path, database_path: &Path) -> Result<()> {
    let backup_path = backup_database_path(database_path);
    remove_existing_database_files(&backup_path)?;

    if database_path.exists() {
        fs::rename(database_path, &backup_path).with_context(|| {
            format!(
                "failed to stage existing database backup {}",
                backup_path.to_string_lossy()
            )
        })?;
    }
    for sidecar in [
        PathBuf::from(format!("{}-wal", database_path.to_string_lossy())),
        PathBuf::from(format!("{}-shm", database_path.to_string_lossy())),
    ] {
        if sidecar.exists() {
            fs::remove_file(&sidecar)
                .with_context(|| format!("failed to remove {}", sidecar.to_string_lossy()))?;
        }
    }

    if let Err(error) = fs::rename(staging_path, database_path) {
        if backup_path.exists() && !database_path.exists() {
            let _ = fs::rename(&backup_path, database_path);
        }
        return Err(error).with_context(|| {
            format!(
                "failed to install built database {}",
                database_path.to_string_lossy()
            )
        });
    }

    remove_existing_database_files(&backup_path)?;
    remove_existing_database_files(staging_path)?;
    Ok(())
}

// ----------------------------------------------------------------------- build manifest

/// Everything the manifest rows need that is not already in the config.
pub struct ManifestFacts<'a> {
    pub config_path: &'a Path,
    pub variant_types_path: &'a Path,
    pub static_data_dir: &'a Path,
    pub static_counts: StaticCounts,
    pub station: &'a SeedStation,
    pub solar_system_name: String,
    pub canonical_stations: &'a [SeedStation],
    pub canonical_solar_system_names: Vec<String>,
    pub plan: &'a SeedPlan,
    /// `(seed_stock, seed_buy_orders)` rows in one canonical book, before five-hub replication.
    pub seed_rows: (usize, usize),
    pub overlay: Option<&'a crate::overlay::OverlayImport>,
    pub crossed_pairs: i64,
    pub snapshot_path: Option<&'a Path>,
    pub region_summary_rows: usize,
    pub system_summary_rows: usize,
    pub price_manifest_path: &'a Path,
    pub price_manifest: &'a PriceManifest,
    /// The synthetic `price_history` this build wrote (plan piece 12).
    pub history: &'a HistoryPlan,
    pub sde_build: Option<u64>,
    pub built_at: &'a str,
}

/// Writes `manifest_json` (the row the daemon parses, `state.rs:1014`) plus flat
/// build-metadata rows. v2's key set is mirrored where it still means something; the
/// v3-specific keys record the price manifest the build consumed and the seed knobs it used,
/// so a database can always be traced back to the manifest that priced it.
pub fn write_manifest(
    connection: &Connection,
    config: &SeederConfig,
    facts: &ManifestFacts<'_>,
) -> Result<usize> {
    let manifest = MarketManifest {
        schema_version: MARKET_SCHEMA_VERSION,
        generated_at: facts.built_at.to_string(),
        static_data_dir: facts.static_data_dir.to_string_lossy().to_string(),
        database_path: config.output.database_path.to_string_lossy().to_string(),
        selection_mode: SELECTION_MODE.to_string(),
        selection_label: format!(
            "Canonical solo-economy market: {} types replicated identically to {} hubs; snapshot data used as reference authority only.",
            facts.plan.seedable_count(),
            facts.canonical_stations.len()
        ),
        selected_solar_system_ids: facts
            .canonical_stations
            .iter()
            .map(|station| station.solar_system_id)
            .collect(),
        selected_solar_system_names: facts.canonical_solar_system_names.clone(),
        region_count: facts.static_counts.regions as u32,
        solar_system_count: facts.static_counts.solar_systems as u32,
        station_count: facts.static_counts.stations as u32,
        market_type_count: facts.static_counts.market_types as u32,
        // One planned canonical book is replicated identically to each approved hub.
        seed_row_count: ((facts.seed_rows.0 + facts.seed_rows.1) * facts.canonical_stations.len())
            as u64,
        default_quantity_per_station_type: u32::try_from(config.canonical_market.quantity)
            .unwrap_or(u32::MAX),
        seed_buy_orders_enabled: true,
        history_days_seeded: facts.history.days,
        seed_markup_percent: if config.policy.is_some() {
            0.0
        } else {
            (config.index_seed.sell_multiplier - 1.0) * 100.0
        },
        station_jitter_percent: 0.0,
        region_jitter_percent: 0.0,
    };

    let mut rows: Vec<(String, String)> = vec![
        (
            MANIFEST_KEY.to_string(),
            serde_json::to_string_pretty(&manifest)?,
        ),
        ("seeder".to_string(), "market-seederv3".to_string()),
        (
            "seeder_version".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        ),
        ("selection_mode".to_string(), SELECTION_MODE.to_string()),
        ("built_at".to_string(), facts.built_at.to_string()),
        (
            "static_data_dir".to_string(),
            facts.static_data_dir.to_string_lossy().to_string(),
        ),
        (
            "static_data_build".to_string(),
            facts
                .sde_build
                .map(|build| build.to_string())
                .unwrap_or_else(|| "null".to_string()),
        ),
        (
            "price_manifest_path".to_string(),
            facts.price_manifest_path.to_string_lossy().to_string(),
        ),
        (
            "price_manifest_sde_build".to_string(),
            facts
                .price_manifest
                .sde_build
                .map(|build| build.to_string())
                .unwrap_or_else(|| "null".to_string()),
        ),
        (
            "price_manifest_cost_model".to_string(),
            facts.price_manifest.cost_model.clone(),
        ),
        (
            "price_manifest_generated_at".to_string(),
            facts.price_manifest.generated_at.clone(),
        ),
        (
            "price_manifest_entries".to_string(),
            facts.price_manifest.prices.len().to_string(),
        ),
        (
            "index_seed_station_id".to_string(),
            facts.station.station_id.to_string(),
        ),
        (
            "index_seed_station_name".to_string(),
            facts.station.station_name.clone(),
        ),
        (
            "index_seed_sell_multiplier".to_string(),
            config.index_seed.sell_multiplier.to_string(),
        ),
        (
            "index_seed_buy_multiplier".to_string(),
            config.index_seed.buy_multiplier.to_string(),
        ),
        (
            "index_seed_price_floor".to_string(),
            config.index_seed.price_floor.to_string(),
        ),
        (
            "index_seed_price_floor_applies_to".to_string(),
            "ask only (a floored bid is a reprocessing money pump)".to_string(),
        ),
        (
            "index_seed_quantity".to_string(),
            config.index_seed.quantity.to_string(),
        ),
        (
            "index_seed_pricing".to_string(),
            config.index_seed.pricing.mode_key().to_string(),
        ),
        (
            "junk_seed_enabled".to_string(),
            config.junk_seed.enabled.to_string(),
        ),
        (
            "junk_seed_categories".to_string(),
            config.junk_seed.categories.join(","),
        ),
        (
            "junk_seed_sell_multiplier".to_string(),
            config.junk_seed.sell_multiplier.to_string(),
        ),
        (
            "junk_seed_buy_multiplier".to_string(),
            config.junk_seed.buy_multiplier.to_string(),
        ),
        (
            "junk_seed_quantity".to_string(),
            config.junk_seed.quantity.to_string(),
        ),
        (
            "seedable_types".to_string(),
            facts.plan.seedable_count().to_string(),
        ),
        (
            "seedable_index_types".to_string(),
            facts.plan.seedable_index_count().to_string(),
        ),
        (
            "seedable_junk_types".to_string(),
            facts.plan.seedable_junk_count().to_string(),
        ),
        (
            "dropped_index_types".to_string(),
            facts.plan.index_drop_count().to_string(),
        ),
        (
            "dropped_junk_types".to_string(),
            facts.plan.junk_drop_count().to_string(),
        ),
        (
            "deferred_to_npc_catalog_types".to_string(),
            facts.plan.deferred_count().to_string(),
        ),
        (
            "deferred_to_npc_catalog_by_bucket".to_string(),
            facts
                .plan
                .deferred_composition()
                .iter()
                .map(|(label, count)| format!("{label} {count}"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            "deferred_to_npc_catalog_by_side".to_string(),
            DeferredSide::ALL
                .into_iter()
                .map(|side| format!("{} {}", side.key(), facts.plan.deferred_side_count(side)))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            "index_seed_defer_to_npc_catalog".to_string(),
            format!(
                "{}: {}",
                config.index_seed.defer_to_npc_catalog.key(),
                facts.plan.deferral.label()
            ),
        ),
        (
            "index_seed_stock_rows".to_string(),
            facts.seed_rows.0.to_string(),
        ),
        (
            "index_seed_buy_order_rows".to_string(),
            facts.seed_rows.1.to_string(),
        ),
        (
            "region_summary_rows".to_string(),
            facts.region_summary_rows.to_string(),
        ),
        (
            "system_summary_rows".to_string(),
            facts.system_summary_rows.to_string(),
        ),
        ("market_orders_rows".to_string(), "0".to_string()),
        (
            "price_history_rows".to_string(),
            facts.history.row_count().to_string(),
        ),
        (
            "price_history_days".to_string(),
            facts.history.days.to_string(),
        ),
        (
            "price_history_types".to_string(),
            facts.history.types.to_string(),
        ),
        (
            "price_history_day_range".to_string(),
            match facts.history.day_range.as_ref() {
                Some((first, last)) => format!("{first} .. {last} (UTC, ends today)"),
                None => "none — no history was written".to_string(),
            },
        ),
        (
            "price_history_types_by_source".to_string(),
            HistorySource::ALL
                .into_iter()
                .map(|source| format!("{} {}", source.key(), facts.history.source_count(source)))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            "price_history_base_price".to_string(),
            format!(
                "best seeded ask anywhere, drifted ±5% deterministically; {} types had no ask \
                 anywhere and were drifted around their best bid. price_history has no station \
                 or region column, so one price per type is the whole series.",
                facts.history.bid_fallbacks
            ),
        ),
        (
            "seed_crossed_pairs".to_string(),
            facts.crossed_pairs.to_string(),
        ),
    ];

    match facts.overlay {
        Some(overlay) => {
            rows.extend([
                (
                    "snapshot_reference".to_string(),
                    format!(
                        "parsed {} source rows for reference authority; zero snapshot rows written",
                        overlay.stats.source_rows
                    ),
                ),
                (
                    "snapshot_reference_order_filter".to_string(),
                    config.import.order_filter.mode_key().to_string(),
                ),
                (
                    "snapshot_reference_duration_threshold_days".to_string(),
                    config.import.npc_order_duration_threshold_days.to_string(),
                ),
                (
                    "snapshot_reference_path".to_string(),
                    facts
                        .snapshot_path
                        .map(|path| path.to_string_lossy().to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                ),
                (
                    "snapshot_reference_window".to_string(),
                    format!(
                        "{} .. {}",
                        overlay
                            .stats
                            .min_http_last_modified
                            .as_deref()
                            .unwrap_or("none"),
                        overlay
                            .stats
                            .max_http_last_modified
                            .as_deref()
                            .unwrap_or("none")
                    ),
                ),
                (
                    "snapshot_reference_source_rows".to_string(),
                    overlay.stats.source_rows.to_string(),
                ),
                (
                    "snapshot_reference_rejected_rows".to_string(),
                    overlay.stats.rejected().to_string(),
                ),
                (
                    "snapshot_reference_jita_types".to_string(),
                    overlay.reference_book.types().len().to_string(),
                ),
                (
                    "snapshot_reference_npc_sell_types".to_string(),
                    overlay.npc_min_sell_cents.len().to_string(),
                ),
            ]);
        }
        None => rows.push(("snapshot_reference".to_string(), "not imported".to_string())),
    }
    if let Some(v2) = &config.policy {
        rows.retain(|(key, _)| {
            !matches!(
                key.as_str(),
                "index_seed_sell_multiplier"
                    | "index_seed_buy_multiplier"
                    | "index_seed_price_floor"
                    | "index_seed_price_floor_applies_to"
                    | "junk_seed_sell_multiplier"
                    | "junk_seed_buy_multiplier"
            )
        });
        rows.push(("item_policy_owner".into(), "v2_json".into()));
        rows.push(("legacy_pricing_profiles_active".into(), "false".into()));
        let mut inputs = BTreeMap::new();
        for (name, path) in [
            ("policy", v2.path.as_path()),
            ("operational_config", facts.config_path),
            ("lp_catalog", v2.lp_offer_catalog_path.as_path()),
            ("price_manifest", facts.price_manifest_path),
            ("sde_raw_types", facts.variant_types_path),
        ] {
            inputs.insert(name.to_string(), input_identity(path)?);
        }
        if let Some(tq_snapshot_path) = &v2.tq_snapshot_path {
            inputs.insert("tq_snapshot".to_string(), input_identity(tq_snapshot_path)?);
        }
        if let Some(snapshot) = facts.snapshot_path {
            inputs.insert("snapshot".to_string(), input_identity(snapshot)?);
        }
        for (name, dir) in [
            ("stations", "stations"),
            ("solar_systems", "solarSystems"),
            ("item_types", "itemTypes"),
            ("industry_blueprints", "industryBlueprints"),
            ("type_dogma", "typeDogma"),
            ("reprocessing", "reprocessingStatic"),
        ] {
            inputs.insert(
                name.to_string(),
                input_identity(&facts.static_data_dir.join(dir).join("data.json"))?,
            );
        }
        if let Ok(executable) = std::env::current_exe() {
            inputs.insert("builder_binary".to_string(), input_identity(&executable)?);
        }
        rows.push((
            "v2_policy_format_version".into(),
            crate::policy::FORMAT_VERSION.to_string(),
        ));
        rows.push(("v2_policy_path".into(), v2.path.display().to_string()));
        rows.push((
            "v2_policy_sha256".into(),
            inputs["policy"]["sha256"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ));
        rows.push((
            "v2_input_provenance_json".into(),
            serde_json::to_string(&inputs)?,
        ));
    }
    rows.sort_by(|left, right| left.0.cmp(&right.0));

    let mut statement =
        connection.prepare("INSERT OR REPLACE INTO manifest (key, value) VALUES (?1, ?2)")?;
    for (key, value) in &rows {
        statement.execute(params![key, value])?;
    }
    Ok(rows.len())
}

fn input_identity(path: &Path) -> Result<serde_json::Value> {
    let bytes =
        fs::read(path).with_context(|| format!("reading provenance input {}", path.display()))?;
    Ok(serde_json::json!({
        "path": path.canonicalize().unwrap_or_else(|_| path.to_path_buf()).display().to_string(),
        "size": bytes.len(),
        "sha256": crate::policy_preview::sha256_hex(&bytes),
    }))
}

// -------------------------------------------------------------------------- the command

/// `build [--yes] [--dry-run] [--skip-overlay]`.
pub fn run_build(config_path: &Path, yes: bool, dry_run: bool, skip_overlay: bool) -> Result<()> {
    let started = Instant::now();
    let config = SeederConfig::load(config_path)?;
    if config
        .policy
        .as_ref()
        .is_some_and(|p| p.preset == crate::config::PolicyPreset::GeneralTq)
    {
        if skip_overlay {
            bail!("GENERAL_TQ does not use --skip-overlay");
        }
        return crate::general_tq::run(config_path, &config, dry_run);
    }
    if skip_overlay {
        bail!(
            "--skip-overlay is not supported by the canonical five-hub market: the snapshot is reference authority for NPC-seeded BPOs and rare Buy-only prices. Snapshot rows are parsed but never written universe-wide."
        );
    }
    let (static_data_dir, static_data_dir_source) =
        resolve_static_data_dir(config.input.static_data_dir.as_deref());
    let database_path = config.output.database_path.clone();

    println!(
        "{} market-seederv3 build{}",
        style("[config]").cyan().bold(),
        if dry_run {
            "  (dry run — nothing is written)"
        } else {
            ""
        }
    );
    crate::print_path("config file", config_path);
    println!(
        "  {:<24}: {} (from {})",
        "static data dir",
        crate::display_path(&static_data_dir),
        static_data_dir_source.label()
    );
    crate::print_path("output database", &database_path);
    crate::print_path("price manifest", &config.price_manifest.path);
    println!(
        "  {:<24}: {} stations, quantity {} per enabled type/side",
        "canonical market",
        config.canonical_market.station_ids.len(),
        format_count(config.canonical_market.quantity)
    );
    if let Some(v2) = &config.policy {
        crate::print_path("item policy JSON", &v2.path);
    } else {
        println!(
            "  {:<24}: {} independent Sell/Buy definitions",
            "pricing profiles",
            config.pricing_profiles.definitions.len()
        );
    }
    println!(
        "  {:<24}: pricing {} (retained manifest/cost input)",
        "legacy cost model",
        config.index_seed.pricing.mode_key()
    );

    println!("  {:<24}: {}", "price history", config.history.label());

    println!(
        "  {:<24}: {}, {} ({}), NPC when duration > {} days; rows are never written",
        "snapshot reference",
        "required",
        config.import.order_filter.mode_key(),
        config.import.order_filter.summary_label(),
        config.import.npc_order_duration_threshold_days
    );

    // ------------------------------------------------------- the snapshot, offline or not
    //
    // The build never resolves the EVE Ref index and never downloads: a build is an offline,
    // reproducible operation (plan §4.3), and a build that reaches the network cannot be run
    // in a sealed environment or replayed against a known snapshot. `snapshot-info
    // --download` is the one command that fetches.
    let snapshot = {
        let cached = crate::snapshot::find_cached_snapshot(&config.source.download_dir)?;
        match cached {
            Some(cached) => {
                println!(
                    "  {:<24}: {} ({})",
                    "snapshot (cached)",
                    crate::display_path(&cached.path),
                    cached.size_label()
                );
                Some(cached)
            }
            None => bail!(
                "no market-order snapshot is cached in {}, so canonical role references cannot \
                 be resolved.\n\n  Run this first:\n      market-seederv3 snapshot-info \
                 --download --reuse-download\n\n  The build itself never downloads: it must \
                 stay offline and reproducible (plan §4.3), so a missing snapshot is a stop, \
                 not a silent fetch. Snapshot orders provide reference prices only and will \
                 not be copied into the output database.",
                crate::display_path(&config.source.download_dir)
            ),
        }
    };
    if !skip_overlay && config.import.order_filter.uses_market_scope() {
        bail!(
            "[import] order_filter = \"{}\" needs a set of in-scope solar systems, and v3 has \
             no [market_scope] config section (v2 built that set from its own station \
             selection pass). Left alone it would match nothing and import an empty overlay, \
             so it is refused instead. Use \"npc_only\" (the v3 default), \"player_only\" or \
             \"all_station\".",
            config.import.order_filter.mode_key()
        );
    }

    println!();
    println!(
        "{} one canonical synthetic book is replicated to exactly five hubs.",
        style("[scope]").yellow().bold()
    );
    println!("  the snapshot supplies references only; no universe-wide NPC rows are written.");
    println!("  no market_orders and no market_order_events rows are written.");
    println!(
        "  price_history is written afterwards, from the finished book: {}",
        config.history.label()
    );

    // v2 asks before anything expensive happens (main.rs:569); a dry run never asks because
    // it never writes.
    if !dry_run && database_path.exists() {
        println!();
        confirm_replace_database(&database_path, yes)?;
    }

    // ------------------------------------------------------------------ resolve the plan
    println!();
    let static_data = StaticData::load(&static_data_dir)?;
    let classification = Classification::compute(&static_data, &config.junk_seed)?;

    // The snapshot is streamed before planning because BPO and rare roles use it as reference
    // authority. Its collapsed station rows remain in memory for compatibility with refresh
    // and audit helpers, but the canonical writer never materializes them.
    let overlay = match &snapshot {
        Some(cached) => {
            let import = crate::overlay::import_overlay(
                &cached.path,
                &static_data,
                config.import.order_filter,
                config.import.npc_order_duration_threshold_days,
                &BTreeSet::new(),
                config.price_manifest.reference_solar_system_id,
                false,
            )?;
            crate::overlay::print_overlay_report(&import, config.import.order_filter);
            Some(import)
        }
        None => None,
    };
    let sde = read_sde_build(&static_data_dir);
    let v2_policy = if let Some(policy_config) = &config.policy {
        let document = PolicyDocument::load(&policy_config.path)?;
        let build = sde
            .build
            .ok_or_else(|| anyhow::anyhow!("V2 policy requires a verified SDE build"))?;
        document.validate_catalog_contract(build)?;
        Some(document)
    } else {
        None
    };
    let v2_resolved = if let Some(document) = &v2_policy {
        Some(resolve_v2_catalog(
            document,
            &config,
            &static_data,
            &classification,
            overlay
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("V2 policy requires NPC reference catalog"))?,
        )?)
    } else {
        None
    };
    let price_manifest = PriceManifest::load(&config.price_manifest.path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no price manifest at {}. The build consumes it read-only and never invents a \
             price, so there is nothing to seed without it. Run `market-seederv3 \
             refresh-prices` first.",
            config.price_manifest.path.to_string_lossy()
        )
    })?;
    if price_manifest.sde_build != sde.build {
        println!(
            "{} price manifest sdeBuild {} != static data build {}. Local prices in the \
             manifest were computed against different static data; re-run refresh-prices.",
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

    if price_manifest.pricing_rules != config.index_seed.pricing_rules() {
        bail!(
            "price manifest pricingRules [{}] != configured [{}]. Market v1 cannot reuse a \
             manifest whose legacy reprocessing floor may have raised the underlying cost and \
             Buy targets. Re-run refresh-prices before build.",
            if price_manifest.pricing_rules.is_empty() {
                "none recorded".to_string()
            } else {
                price_manifest.pricing_rules.join(", ")
            },
            config.index_seed.pricing_rules().join(", ")
        );
    }

    // As in the audit: the build seeds the manifest's prices, so this map only exists to
    // blame the leaves behind a dropped type. No rule overrides.
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
    // The deferral compares our computed prices against TQ's, so the plan needs the
    // multipliers that will write the rows — the same pair `plan_seed_rows` uses below.
    let pricing = SeedPricingProfiles::from_config(&config);
    let sde_build = sde.build.ok_or_else(|| {
        anyhow::anyhow!(
            "generated static data has no verified SDE build; Rare variant-family metadata cannot be used"
        )
    })?;
    let families = VariantFamilies::load_for_generated_data(&static_data_dir, sde_build)?;
    println!(
        "  {:<24}: {} (raw CCP SDE build {})",
        "variant families",
        families.types_path.display(),
        families.build
    );
    let required_rare = if let Some(resolved) = &v2_resolved {
        required_external_market_references_v2(resolved)?
    } else {
        required_external_market_references_from_config(&classification, &static_data, &config)?
    };
    let sibling_fallbacks = strict_sibling_family_fallbacks(
        &required_rare,
        &static_data,
        &families,
        &price_manifest,
        overlay.as_ref(),
        CapturePolicy::from_config(&config),
    );
    let reprocessing = ReprocessingStatic::load(&static_data_dir)?;
    let moon_available = if let Some(resolved) = &v2_resolved {
        resolved.iter().any(|(&type_id, decision)| {
            decision.policy.as_ref().is_some_and(|policy| {
                policy
                    .sell
                    .as_ref()
                    .or(policy.buy.as_ref())
                    .is_some_and(|side| side.source == PolicySource::CoreManifestCost)
            }) && static_data
                .item_type(type_id)
                .and_then(|item| item.group_id)
                .is_some_and(|group| MOON_RESOURCE_GROUP_IDS.contains(&group))
        })
    } else {
        config.market_roles.moon_mining_available
    };
    let ore_normalization = OreFamilyNormalization::derive(
        &static_data,
        &reprocessing,
        &price_manifest,
        CapturePolicy::from_config(&config),
        moon_available,
    );
    let mut partial_plan = if let Some(resolved) = &v2_resolved {
        SeedPlan::resolve_v2_partial(
            &classification,
            &static_data,
            &cost_report,
            &price_manifest,
            overlay.as_ref(),
            &config,
            &sibling_fallbacks,
            resolved,
        )?
    } else {
        SeedPlan::resolve_canonical_partial(
            &classification,
            &static_data,
            &cost_report,
            &price_manifest,
            overlay.as_ref(),
            &config,
            &sibling_fallbacks,
        )?
    };
    ore_normalization.apply_to_plan(&mut partial_plan);
    let industry_semantics = IndustrySemantics::load(&static_data_dir, &static_data)?;
    let v2_partial_rows = if v2_resolved.is_some() {
        Some(plan_v2_canonical_seed_rows(
            &partial_plan,
            &static_data,
            config.canonical_market.quantity,
        )?)
    } else {
        None
    };
    let funded_fallback_report =
        if let (Some(resolved), Some(rows)) = (&v2_resolved, &v2_partial_rows) {
            let eligible = resolved
                .iter()
                .filter_map(|(&type_id, decision)| {
                    let policy = decision.policy.as_ref()?;
                    (policy.profile_id.as_deref() == Some("rare_t2")
                        && policy
                            .buy
                            .as_ref()
                            .is_some_and(|side| side.source == PolicySource::RareReference))
                    .then_some(type_id)
                })
                .collect::<BTreeSet<_>>();
            resolve_rare_t2_fallbacks_v2(
                &partial_plan,
                &static_data,
                &industry_semantics,
                rows,
                &eligible,
            )
        } else {
            resolve_rare_t2_fallbacks(&partial_plan, &static_data, &industry_semantics, &config)
        };
    if v2_policy.is_some() {
        println!(
            "[RareT2 funded evaluation] {} resolved, {} unresolved under V2 source contract",
            funded_fallback_report.fallbacks.len(),
            funded_fallback_report.unfunded.len()
        );
    } else {
        print_rare_t2_funded_resolution(&funded_fallback_report, &config);
    }
    let mut rare_fallbacks = sibling_fallbacks;
    rare_fallbacks.extend(funded_fallback_report.fallbacks);
    let production_intermediate_report = if let (Some(resolved), Some(rows)) =
        (&v2_resolved, &v2_partial_rows)
    {
        let expected = resolved
            .iter()
            .filter_map(|(&type_id, decision)| {
                let policy = decision.policy.as_ref()?;
                (policy
                    .buy
                    .as_ref()
                    .is_some_and(|side| side.source == PolicySource::FundedCost)
                    || policy
                        .sell
                        .as_ref()
                        .is_some_and(|side| side.source == PolicySource::FundedCost))
                .then_some(type_id)
            })
            .collect::<BTreeSet<_>>();
        resolve_production_intermediates_v2(
            &partial_plan,
            &static_data,
            &industry_semantics,
            rows,
            &expected,
        )?
    } else {
        resolve_production_intermediates(&partial_plan, &static_data, &industry_semantics, &config)?
    };
    if v2_policy.is_some() {
        println!(
            "[ProductionIntermediate funded evaluation] {} policy-selected types resolved at guaranteed funded cost",
            production_intermediate_report.prices.len()
        );
    } else {
        print_production_intermediate_resolution(&production_intermediate_report, &config);
    }
    let mut plan = if let Some(resolved) = &v2_resolved {
        SeedPlan::resolve_v2_final(
            &classification,
            &static_data,
            &cost_report,
            &price_manifest,
            overlay.as_ref(),
            &config,
            &rare_fallbacks,
            &production_intermediate_report.prices,
            resolved,
        )?
    } else {
        SeedPlan::resolve_canonical_with_supply_ladder(
            &classification,
            &static_data,
            &cost_report,
            &price_manifest,
            overlay.as_ref(),
            &config,
            &rare_fallbacks,
            &production_intermediate_report.prices,
        )?
    };
    let normalized_ore_types = ore_normalization.apply_to_plan(&mut plan);

    let stations = config
        .canonical_market
        .station_ids
        .iter()
        .map(|station_id| {
            static_data
                .station(*station_id)
                .map(SeedStation::from)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "canonical station_id {} is not in {}",
                        station_id,
                        static_data_dir
                            .join("stations")
                            .join("data.json")
                            .to_string_lossy()
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let station = stations
        .first()
        .expect("configuration validation requires five canonical stations");
    let canonical_solar_system_names = stations
        .iter()
        .map(|station| {
            static_data
                .solar_system(station.solar_system_id)
                .map(|system| system.solar_system_name.clone())
                .unwrap_or_else(|| "<unknown system>".to_string())
        })
        .collect::<Vec<_>>();
    let solar_system_name = static_data
        .solar_system(station.solar_system_id)
        .map(|system| system.solar_system_name.clone())
        .unwrap_or_else(|| "<unknown system>".to_string());

    let mut rows = if v2_resolved.is_some() {
        plan_v2_canonical_seed_rows(&plan, &static_data, config.canonical_market.quantity)?
    } else {
        plan_canonical_seed_rows(
            &plan,
            &static_data,
            &pricing,
            config.canonical_market.quantity,
        )
    };
    print_skillbook_supply_report(&plan, &rows);
    let safety_report = if let Some(resolved) = &v2_resolved {
        let funded = resolved
            .iter()
            .filter_map(|(&type_id, decision)| {
                let buy = decision.policy.as_ref()?.buy.as_ref()?;
                (buy.source == PolicySource::FundedCost
                    && buy.multiplier <= config.market_safety.production_safety_factor)
                    .then_some(type_id)
            })
            .collect::<BTreeSet<_>>();
        apply_market_safety_with_source_contract(
            &mut rows,
            &static_data,
            &reprocessing,
            &industry_semantics,
            &config,
            &funded,
        )?
    } else {
        apply_market_safety(
            &mut rows,
            &static_data,
            &reprocessing,
            &industry_semantics,
            &config,
        )?
    };
    let ore_monotonic_checks = verify_ore_family_monotonicity(&ore_normalization, &rows)?;
    print_rare_fallback_report(&plan, &rows, &config, &funded_fallback_report.unfunded);
    print_market_safety_report(&safety_report);
    print_ore_family_normalization_report(
        &ore_normalization,
        normalized_ore_types,
        ore_monotonic_checks,
    );
    verify_seed_rows(
        &rows,
        plan.seedable_count(),
        config.canonical_market.quantity,
        config.canonical_market.quantity,
    )?;
    if let Some(policy) = &config.policy {
        if policy.allow_intentional_policy_delta {
            anyhow::ensure!(
                policy.parity_baseline_path.is_none(),
                "intentional policy delta cannot also request migration parity baseline"
            );
        } else {
            let baseline = policy.parity_baseline_path.as_deref().ok_or_else(||anyhow::anyhow!("V2 build requires [policy] parity_baseline_path unless allow_intentional_policy_delta=true"))?;
            verify_v2_rows_against_baseline(&rows, baseline)?;
        }
    }
    let (planned_sell, planned_buy) = planned_side_counts(&rows);
    // A summary group may contain more than one canonical hub, so prove its integer headroom.
    let headroom = summary_sum_headroom(stations.len() as u64, config.canonical_market.quantity)?;

    print_plan_report(
        &config,
        &plan,
        &rows,
        &station,
        &solar_system_name,
        headroom,
    );

    // ------------------------------------------------- piece 12: the synthetic price charts
    //
    // The base price is what a player actually pays to buy a type from the seed, so it has to
    // come from the **finished** five-hub book, assembled exactly as `audit` assembles it, so
    // Sell-only BPOs and skillbooks get charts too.
    // Resolved here rather than after the write because a dry run has to be able to report it,
    // and because it reads the plan and touches no price: nothing below this line can move a
    // seed row, a summary or a manifest price. See [`crate::history`].
    let history_book = SeedBook::from_rows_at_stations(&rows, &stations);
    let lp_path = config
        .policy
        .as_ref()
        .map(|v2| &v2.lp_offer_catalog_path)
        .unwrap_or(&config.market_roles.lp_offer_catalog_path);
    let direct_lp_rewards = load_lp_reward_type_ids(lp_path)?;
    check_reward_sell_invariants(&history_book, &static_data, &direct_lp_rewards)?;
    let v2_progression_types = v2_resolved.as_ref().map(|resolved| {
        resolved
            .iter()
            .filter_map(|(&type_id, decision)| {
                decision
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.sell.as_ref())
                    .filter(|side| side.source == PolicySource::NpcMinSell)
                    .map(|_| type_id)
            })
            .collect::<Vec<_>>()
    });
    let progression_groups = if v2_resolved.is_some() {
        &[][..]
    } else {
        config.market_roles.progression_npc_group_ids.as_slice()
    };
    let progression_types = v2_progression_types
        .as_deref()
        .unwrap_or(config.market_roles.progression_npc_type_ids.as_slice());
    check_progression_npc_supply_invariants(
        &history_book,
        &static_data,
        overlay.as_ref(),
        progression_groups,
        progression_types,
    )?;
    let history = crate::history::plan_canonical_history(
        &history_book,
        config.history.effective_days(),
        crate::history::today_utc(),
    );
    print_history_report(&config, &history);

    let static_counts = StaticCounts {
        regions: region_rows(&static_data).len(),
        solar_systems: static_data.solar_systems.len(),
        stations: static_data.stations.len(),
        market_types: static_data.item_types.len(),
    };

    if dry_run {
        if let (Some(v2), Some(resolved)) = (&config.policy, &v2_resolved) {
            if let Some(path) = &v2.plan_preview_path {
                write_v2_dry_plan(path, &config, &plan, &rows, resolved, &safety_report)?;
                println!("  V2 finalized machine preview: {}", path.display());
            }
        }
        println!();
        println!(
            "{} rows that would be written",
            style("[plan]").cyan().bold()
        );
        print_row_table(&[
            ("regions", static_counts.regions),
            ("solar_systems", static_counts.solar_systems),
            ("stations", static_counts.stations),
            ("market_types", static_counts.market_types),
            (
                "seed_stock (5 canonical hubs)",
                planned_sell * stations.len(),
            ),
            (
                "seed_buy_orders (5 canonical hubs)",
                planned_buy * stations.len(),
            ),
            ("market_orders", 0),
            ("market_order_events", 0),
            ("price_history", history.row_count()),
        ]);
        println!(
            "  {:<28}: 0 (snapshot parsed for references only)",
            "seed rows outside hubs"
        );
        println!();
        println!(
            "{} dry run: nothing written. The build would stage {} and install it over {}.",
            style("[write]").cyan().bold(),
            staged_database_path(&database_path).to_string_lossy(),
            crate::display_path(&database_path)
        );
        if database_path.exists() {
            println!(
                "  the existing database would be moved to {} and deleted after a clean install.",
                backup_database_path(&database_path).to_string_lossy()
            );
        }
        return Ok(());
    }

    // ------------------------------------------------------------------------- the build
    println!();
    if let Some(parent) = database_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let staging_path = staged_database_path(&database_path);
    remove_existing_database_files(&staging_path)?;
    println!(
        "{} staging into {}",
        style("[build]").cyan().bold(),
        staging_path.to_string_lossy()
    );

    let mut connection = open_build_connection(
        &staging_path,
        config.build.sqlite_cache_size_kib,
        config.build.sqlite_page_size_bytes,
        config.build.sqlite_worker_threads,
    )?;

    let built_at = now_rfc3339();
    write_static_tables(&mut connection, &static_data)?;

    // One plan, five identical materializations. Snapshot rows never enter the database.
    let expected_counts = SeedTableCounts::canonical(&rows, stations.len());
    let seed_progress = row_progress_bar(
        "Canonical seed rows",
        ((planned_sell + planned_buy) * stations.len()) as u64,
    );
    let (sell_rows, buy_rows) = write_canonical_seed_rows(
        &mut connection,
        &stations,
        &rows,
        &built_at,
        Some(&seed_progress),
    )?;
    seed_progress.finish_with_message(format!(
        "Canonical book wrote {} asks + {} bids across five hubs",
        format_count(sell_rows),
        format_count(buy_rows)
    ));

    // This should be zero: every stored row belongs to the verified canonical book.
    let crossed_pairs = count_crossed_pairs(&connection)?;

    // Summaries come after canonical replication, so best_ask/best_bid see the final book.
    let summary_spinner = spinner("Rebuilding region and system summaries from the seed tables");
    let region_summary_rows = rebuild_summaries(&mut connection, "region_summaries", "region_id")?;
    let system_summary_rows =
        rebuild_summaries(&mut connection, "system_seed_summaries", "solar_system_id")?;
    summary_spinner.finish_with_message(format!(
        "Rebuilt {} region + {} system summary rows",
        format_count(region_summary_rows),
        format_count(system_summary_rows)
    ));

    // ------------------------------------------------------- piece 12: price_history rows
    //
    // Last of the data passes, and deliberately so: it is derived from the finished book and
    // feeds nothing, so it cannot influence a price even by accident. `write_history_rows`
    // re-checks the whole table in SQL inside its own transaction before committing, so a
    // malformed chart is discarded rather than shipped.
    if history.row_count() > 0 {
        let history_progress = row_progress_bar("Price history", history.row_count() as u64);
        let written = crate::history::write_history_rows(
            &mut connection,
            &history.rows,
            history.types,
            history.days,
            Some(&history_progress),
        )?;
        history_progress.finish_with_message(format!(
            "Price history wrote {} rows over {} types x {} days",
            format_count(written),
            format_count(history.types),
            history.days
        ));
        print_history_anchors(&connection, &history)?;
    } else {
        println!(
            "{} price_history left empty: {}. The client's market charts will be blank.",
            style("[history]").yellow().bold(),
            config.history.label()
        );
    }

    let manifest_rows = write_manifest(
        &connection,
        &config,
        &ManifestFacts {
            config_path,
            variant_types_path: &families.types_path,
            static_data_dir: &static_data_dir,
            static_counts,
            station,
            solar_system_name: solar_system_name.clone(),
            canonical_stations: &stations,
            canonical_solar_system_names: canonical_solar_system_names.clone(),
            plan: &plan,
            seed_rows: (planned_sell, planned_buy),
            overlay: overlay.as_ref(),
            crossed_pairs,
            snapshot_path: snapshot.as_ref().map(|cached| cached.path.as_path()),
            region_summary_rows,
            system_summary_rows,
            price_manifest_path: &config.price_manifest.path,
            price_manifest: &price_manifest,
            history: &history,
            sde_build: sde.build,
            built_at: &built_at,
        },
    )?;

    build_runtime_indexes(&connection)?;
    finalize_database(&connection)?;
    drop(connection);

    let backup_path = backup_database_path(&database_path);
    let had_previous = database_path.exists();
    install_built_database(&staging_path, &database_path)?;

    print_build_summary(&BuildSummaryFacts {
        database_path: &database_path,
        backup_path: &backup_path,
        had_previous,
        station,
        solar_system_name: &solar_system_name,
        canonical_station_count: stations.len(),
        plan: &plan,
        rows: &rows,
        static_counts,
        overlay: overlay.as_ref(),
        counts: expected_counts,
        crossed_pairs,
        region_summary_rows,
        system_summary_rows,
        manifest_rows,
        history: &history,
        elapsed: started.elapsed(),
    })?;
    Ok(())
}

// ------------------------------------------------------------------------------ reports

pub fn print_rare_fallback_report(
    plan: &SeedPlan,
    rows: &[SeedRow],
    config: &SeederConfig,
    unfunded: &[crate::funded::UnfundedPath],
) {
    println!();
    println!(
        "{} {} provenance-bearing Rare fallback(s)",
        style("[Rare fallback]").cyan().bold(),
        plan.rare_fallbacks.len()
    );
    for (type_id, provenance) in &plan.rare_fallbacks {
        let Some(row) = rows.iter().find(|row| row.type_id == *type_id) else {
            continue;
        };
        let multiplier = config
            .pricing_profiles
            .get(row.profile.key())
            .map(|profile| profile.buy_multiplier)
            .unwrap_or(0.0);
        match provenance {
            RareFallbackProvenance::SiblingFamily {
                variation_parent_type_id,
                sibling_type_id,
                sibling_name,
                sibling_reference,
                sibling_external_source,
                eligible_referenced_siblings,
            } => println!(
                "  {} {} [role {}, profile {}]: reference_source=sibling-family-fallback; variationParentTypeID {}; sibling {} {}; approved {} reference {:.2}; {} eligible referenced sibling(s), minimum selected; multiplier x{}; resulting Buy {:.2}",
                row.type_id,
                row.name,
                row.role.key(),
                row.profile.key(),
                variation_parent_type_id,
                sibling_type_id,
                sibling_name,
                sibling_external_source.key(),
                sibling_reference,
                eligible_referenced_siblings,
                multiplier,
                row.bid
            ),
            RareFallbackProvenance::FundedCost {
                funded_cost,
                path_summary,
            } => println!(
                "  {} {} [profile {}]: reference_source=funded-cost-fallback; funded cost {:.2}; fallback target x{} = {:.2}; final Buy {:.2}; {}",
                row.type_id,
                row.name,
                row.profile.key(),
                funded_cost,
                multiplier,
                funded_cost * multiplier,
                row.bid,
                path_summary
            ),
        }
    }
    for missing in unfunded {
        println!(
            "  RequiredRareReferenceUnavailable {} {} [RareT2 funded fallback]: {}",
            missing.type_id, missing.name, missing.reason
        );
    }
    for dropped in plan.dropped.iter().filter(|drop| {
        matches!(
            drop.reason,
            DropReason::RequiredRareReferenceUnavailable
                | DropReason::OptionalRareUnreferenced
                | DropReason::OptionalRareBlueprint
        )
    }) {
        println!(
            "  {} {} {} [role {}, profile {}]: no synthetic Sell; no synthetic Buy",
            dropped.reason.key(),
            dropped.type_id,
            dropped.name,
            dropped.role.map(MarketRole::key).unwrap_or("none"),
            dropped.profile.map(PricingProfile::key).unwrap_or("none")
        );
    }
}

pub fn print_skillbook_supply_report(plan: &SeedPlan, rows: &[SeedRow]) {
    let skillbooks = plan
        .seedable
        .values()
        .filter(|entry| entry.role == MarketRole::Skillbook)
        .collect::<Vec<_>>();
    println!();
    println!(
        "[Skillbook supply] {} canonical Sell-only records",
        skillbooks.len()
    );
    for entry in skillbooks {
        let source = match entry.source {
            PriceSource::NpcReference => "npc-reference",
            PriceSource::CcpEsiAverage | PriceSource::CcpSnapshotJitaSplit => {
                "external-exact-reference"
            }
            PriceSource::BasePrice => "static-base-price",
            PriceSource::SkillbookFixedFallback => "skillbook-fixed-fallback",
            _ => entry.source.key(),
        };
        let row = rows
            .iter()
            .find(|row| row.type_id == entry.type_id)
            .expect("every seedable Skillbook has a planned row");
        println!(
            "  {} {}: source={}; final Sell {:.2}; synthetic Buy=false",
            entry.type_id, row.name, source, row.ask
        );
    }
}

fn print_plan_report(
    config: &SeederConfig,
    plan: &SeedPlan,
    rows: &[SeedRow],
    station: &SeedStation,
    solar_system_name: &str,
    headroom: i64,
) {
    println!();
    println!(
        "{} first hub {} at {} ({}, {}), system {} / constellation {} / region {}",
        style("[canonical market]").cyan().bold(),
        station.station_name,
        station.station_id,
        station.region_name,
        solar_system_name,
        station.solar_system_id,
        station.constellation_id,
        station.region_id
    );

    let (planned_sell, planned_buy) = planned_side_counts(rows);
    println!();
    println!(
        "{} {} seedable types -> {} seed rows ({} asks + {} bids)",
        style("[seed plan]").cyan().bold(),
        format_count(plan.seedable_count()),
        format_count(planned_sell + planned_buy),
        format_count(planned_sell),
        format_count(planned_buy)
    );
    println!(
        "  {:<30}: {}",
        "roles",
        plan.seedable_role_composition()
            .iter()
            .map(|(label, count)| format!("{label} {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  {:<30}: {}",
        "pricing profiles",
        plan.seedable_profile_composition()
            .iter()
            .map(|(label, count)| format!("{label} {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  {:<30}: {} unpriced Core types (budget {}), {} uncaptured Core junk, {} unpriced required roles",
        "dropped, never guessed at",
        plan.index_drop_count(),
        config.index_seed.max_unpriced_drops,
        plan.junk_drop_count(),
        plan.unpriced_role_drop_count()
    );
    for dropped in plan.dropped_with(DropReason::UnpricedIndexType) {
        println!(
            "    {:>7}  {:<46} bucket {:<4} blocked by {:?}",
            dropped.type_id,
            dropped.name,
            dropped.bucket_label(),
            dropped.blocking_leaves
        );
    }
    print_deferral_report(plan);
    println!(
        "  {:<30}: {} per row; SUM(quantity) per (region, type) fits i64 with {}x headroom",
        "quantity",
        format_count(config.canonical_market.quantity),
        format_count(headroom)
    );

    println!();
    println!(
        "{} independent Sell/Buy targets selected by each row's pricing profile",
        style("[pricing]").cyan().bold()
    );
    let profile_pricing = SeedPricingProfiles::from_config(config);
    for (profile, count) in plan.seedable_profile_composition() {
        let configured = config
            .pricing_profiles
            .get(profile)
            .expect("validated canonical profile");
        println!(
            "  {:<30}: {} types, Sell x{} floor {}, Buy x{}",
            profile,
            format_count(count),
            configured.sell_multiplier,
            configured.sell_floor,
            configured.buy_multiplier
        );
    }
    let floored = rows
        .iter()
        .filter(|row| {
            row.writes_ask()
                && profile_pricing
                    .for_profile(row.profile)
                    .ask_is_floored(row.cost)
        })
        .count();
    println!(
        "  {:<30}: {} of {} asks sit at the floor; 0 bids do, by design",
        "floor bites",
        format_count(floored),
        format_count(rows.len())
    );
    print_price_extremes(rows);
    print_anchors(rows);
}

/// Canonical production has no NPC overlay or NPC side deferral. Snapshot orders are read as
/// reference authority only; this diagnostic makes that boundary explicit.
pub fn print_deferral_report(plan: &SeedPlan) {
    println!(
        "  {:<30}: reference authority only; zero snapshot rows and zero NPC side deferrals",
        "snapshot"
    );
    if !plan.deferrals.is_empty() {
        println!(
            "  {:<30}: unexpected legacy deferral state: {} entries",
            "validation warning",
            format_count(plan.deferrals.len())
        );
    }
}

/// Cheapest / median / dearest, over the rows that actually carry that side. A deferred ask is
/// not a 0.00 ask and must not drag the minimum down: it is simply not a row.
fn print_price_extremes(rows: &[SeedRow]) {
    for (side, price, writes) in [
        (
            "ask",
            (|row: &SeedRow| row.ask) as fn(&SeedRow) -> f64,
            SeedRow::writes_ask as fn(&SeedRow) -> bool,
        ),
        ("bid", |row: &SeedRow| row.bid, SeedRow::writes_bid),
    ] {
        let mut sorted = rows.iter().filter(|row| writes(row)).collect::<Vec<_>>();
        if sorted.is_empty() {
            println!("  {side:<6} {:<7}: nothing to price", "");
            continue;
        }
        sorted.sort_by(|left, right| {
            price(left)
                .total_cmp(&price(right))
                .then(left.type_id.cmp(&right.type_id))
        });
        for (label, row) in [
            ("min", sorted[0]),
            ("median", sorted[sorted.len() / 2]),
            ("max", sorted[sorted.len() - 1]),
        ] {
            println!(
                "  {:<6} {:<7}: {:>24} ISK  {} ({}, bucket {})",
                side,
                label,
                crate::isk(price(row)),
                row.name,
                row.type_id,
                row.bucket
            );
        }
    }
}

fn print_anchors(rows: &[SeedRow]) {
    let index = rows
        .iter()
        .map(|row| (row.type_id, row))
        .collect::<BTreeMap<_, _>>();
    for type_id in [ANCHOR_VENTURE, ANCHOR_TRITANIUM] {
        match index.get(&type_id) {
            Some(row) => println!(
                "  {:<14}: {} ({}) role {} profile {} reference {} -> ask {} / bid {}",
                "sample",
                row.name,
                type_id,
                row.role.key(),
                row.profile.key(),
                crate::isk(row.cost),
                if row.writes_ask() {
                    crate::isk(row.ask)
                } else {
                    "deferred".to_string()
                },
                if row.writes_bid() {
                    crate::isk(row.bid)
                } else {
                    "deferred".to_string()
                }
            ),
            None => println!(
                "  {:<14}: type {} is not in the canonical plan",
                "sample", type_id
            ),
        }
    }
}

/// Plan piece 12, as a report a human can judge: how many rows, over how many types, drawn
/// around which price, and how the coverage splits across the three seeding mechanisms.
fn print_history_report(config: &SeederConfig, history: &HistoryPlan) {
    println!();
    println!(
        "{} {}",
        style("[price history]").cyan().bold(),
        config.history.label()
    );
    if history.row_count() == 0 {
        println!(
            "  {:<30}: nothing is written, so every market chart in the client stays empty.",
            "effect"
        );
        return;
    }
    let (first, last) = history
        .day_range
        .clone()
        .unwrap_or_else(|| ("?".to_string(), "?".to_string()));
    println!(
        "  {:<30}: {} rows = {} types x {} days, {} .. {} UTC (ends today)",
        "coverage",
        format_count(history.row_count()),
        format_count(history.types),
        history.days,
        first,
        last
    );
    println!(
        "  {:<30}: {}",
        "types by seeding mechanism",
        HistorySource::ALL
            .into_iter()
            .map(|source| format!(
                "{} {}",
                format_count(history.source_count(source)),
                source.key()
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "  {:<30}: the best seeded ask ANYWHERE — what a player actually pays — drifted ±5% \
         deterministically on (type_id, day)",
        "base price"
    );
    println!(
        "  {:<30}: {} types have no ask anywhere and are charted around their best bid instead",
        "  bid fallback",
        format_count(history.bid_fallbacks)
    );
    println!(
        "  {:<30}: price_history is keyed (type_id, day) with NO station or region column, so \
         one price per type is the whole series and there is nothing to reconcile per station",
        "why one price per type"
    );
    println!(
        "  {:<30}: {} days >= 2, so splitHistoryRows gives the client a non-empty old half \
         ({} rows) and new half (1 row)",
        "client split",
        history.days,
        history.days.saturating_sub(1)
    );
}

/// Reads three anchors' newest history rows straight back out of the staged database, using
/// the daemon's own query shape (`externalservices/market-server/src/state.rs:2119-2149`).
///
/// A plan that looks right and a table the daemon can actually serve are two different claims,
/// and this is the one that matters — the piece exists because the charts were empty. The
/// third anchor is a blueprint on purpose: blueprints are an excluded category for the index
/// and junk buckets, so a BPO with a chart proves the coverage reaches the NPC overlay.
fn print_history_anchors(connection: &Connection, history: &HistoryPlan) -> Result<()> {
    if history.row_count() == 0 {
        return Ok(());
    }
    let mut anchors: Vec<(u32, &str)> = vec![
        (ANCHOR_VENTURE, "index anchor"),
        (ANCHOR_TRITANIUM, "index anchor (floored)"),
    ];
    if let Some(blueprint) = history.blueprint_anchor() {
        anchors.push((blueprint.type_id, "overlay anchor (T1 BPO)"));
    }

    println!(
        "  {} the newest 3 rows of each anchor, read back through the daemon's own query:",
        style("spot-check:").bold()
    );
    for (type_id, label) in anchors {
        match history.base(type_id) {
            Some(base) => println!(
                "    {} {} ({}) — base {} ISK from its {}, {} source",
                label,
                base.name,
                type_id,
                crate::isk(base.base),
                base.basis.key(),
                base.source.key()
            ),
            None => {
                println!("    {label} type {type_id} has no history (nothing seeds it)");
                continue;
            }
        }
        for row in crate::history::recent_rows(connection, type_id, 3)? {
            println!(
                "      {}  low {:>18}  avg {:>18}  high {:>18}  vol {:>7}  orders {:>3}",
                row.day,
                crate::isk(row.low_price),
                crate::isk(row.avg_price),
                crate::isk(row.high_price),
                format_count(row.volume),
                row.order_count
            );
        }
    }
    Ok(())
}

fn print_row_table(rows: &[(&str, usize)]) {
    for (table, count) in rows {
        println!("  {:<24}: {:>9}", table, format_count(*count));
    }
}

/// Everything the closing report prints, so the two seed passes can each be reported
/// separately without a fourteen-argument function.
struct BuildSummaryFacts<'a> {
    database_path: &'a Path,
    backup_path: &'a Path,
    had_previous: bool,
    station: &'a SeedStation,
    solar_system_name: &'a str,
    canonical_station_count: usize,
    plan: &'a SeedPlan,
    rows: &'a [SeedRow],
    static_counts: StaticCounts,
    overlay: Option<&'a crate::overlay::OverlayImport>,
    counts: SeedTableCounts,
    crossed_pairs: i64,
    region_summary_rows: usize,
    system_summary_rows: usize,
    manifest_rows: usize,
    history: &'a HistoryPlan,
    elapsed: StdDuration,
}

fn print_build_summary(facts: &BuildSummaryFacts<'_>) -> Result<()> {
    println!();
    println!(
        "{}",
        style(match facts.history.row_count() > 0 {
            true => "Canonical five-hub seed + synthetic price history written.",
            false => {
                "Canonical five-hub seed written. price_history is EMPTY — charts will be blank."
            }
        })
        .green()
        .bold()
    );
    println!(
        "{} {}",
        style("Database:").bold(),
        facts.database_path.to_string_lossy()
    );
    println!(
        "{} {}",
        style("Size:").bold(),
        format_bytes(fs::metadata(facts.database_path)?.len())
    );
    println!(
        "{} {} identical hubs; first is {} ({}, {}), system {}",
        style("Canonical market:").bold(),
        facts.canonical_station_count,
        facts.station.station_name,
        facts.station.station_id,
        facts.station.region_name,
        facts.solar_system_name
    );
    println!(
        "{} {}",
        style("Previous database:").bold(),
        if facts.had_previous {
            format!(
                "backed up to {} during the install, then deleted after it succeeded",
                facts.backup_path.to_string_lossy()
            )
        } else {
            "none — this build created the file".to_string()
        }
    );
    println!();
    print_row_table(&[
        ("regions", facts.static_counts.regions),
        ("solar_systems", facts.static_counts.solar_systems),
        ("stations", facts.static_counts.stations),
        ("market_types", facts.static_counts.market_types),
        ("seed_stock", facts.counts.sell_rows as usize),
        ("seed_buy_orders", facts.counts.buy_rows as usize),
        ("region_summaries", facts.region_summary_rows),
        ("system_seed_summaries", facts.system_summary_rows),
        ("price_history", facts.history.row_count()),
        ("manifest", facts.manifest_rows),
        ("market_orders", 0),
        ("market_order_events", 0),
    ]);

    // The two passes, counted apart: a single "seed rows" total hides which half moved.
    let (planned_sell, planned_buy) = planned_side_counts(facts.rows);
    println!();
    println!(
        "{} {} asks + {} bids per hub at {} identical hubs ({} types)",
        style("Canonical book:").bold(),
        format_count(planned_sell),
        format_count(planned_buy),
        facts.canonical_station_count,
        format_count(facts.rows.len())
    );
    println!("  this exact book is replicated to all five approved hubs");
    match facts.overlay {
        Some(overlay) => {
            println!(
                "{} parsed {} source rows for prices only; zero snapshot rows written",
                style("Snapshot reference:").bold(),
                format_count(overlay.stats.source_rows)
            );
        }
        None => println!("{} not imported", style("Snapshot reference:").bold()),
    }
    if facts.crossed_pairs > 0 {
        println!(
            "{} {} (station, type) pairs hold a bid above their own ask; the canonical writer \
             should reject these before commit.",
            style("Crossed pairs:").yellow().bold(),
            format_count(facts.crossed_pairs)
        );
    }

    println!();
    println!(
        "{} {} seedable ({} index, {} junk), {} not seeded ({} unpriced index, {} uncaptured \
         junk, {} deferred to the NPC catalog on both sides)",
        style("Plan:").bold(),
        format_count(facts.plan.seedable_count()),
        format_count(facts.plan.seedable_index_count()),
        format_count(facts.plan.seedable_junk_count()),
        format_count(facts.plan.dropped.len()),
        facts.plan.index_drop_count(),
        facts.plan.junk_drop_count(),
        format_count(facts.plan.deferred_count())
    );
    if facts.plan.deferral_count() > 0 {
        println!(
            "{} {} — a deferred side is a price of ours that would have crossed TQ's, so TQ \
             keeps that side at TQ's own stations and we stand down on it at {}.",
            style("Deferrals:").bold(),
            DeferredSide::ALL
                .into_iter()
                .map(|side| format!(
                    "{} {}",
                    format_count(facts.plan.deferred_side_count(side)),
                    side.key()
                ))
                .collect::<Vec<_>>()
                .join(", "),
            facts.station.station_name
        );
    }
    println!(
        "{} {}",
        style("Price history:").bold(),
        if facts.history.row_count() == 0 {
            "none written — every market chart in the client will be empty".to_string()
        } else {
            let (first, last) = facts
                .history
                .day_range
                .clone()
                .unwrap_or_else(|| ("?".to_string(), "?".to_string()));
            format!(
                "{} rows = {} types x {} days, {first} .. {last} UTC ({}). Base price is the \
                 best seeded ask anywhere ({} types fell back to their bid); price_history has \
                 no station dimension, so one series per type is all there is.",
                format_count(facts.history.row_count()),
                format_count(facts.history.types),
                facts.history.days,
                HistorySource::ALL
                    .into_iter()
                    .map(|source| format!(
                        "{} {}",
                        format_count(facts.history.source_count(source)),
                        source.key()
                    ))
                    .collect::<Vec<_>>()
                    .join(", "),
                format_count(facts.history.bid_fallbacks)
            )
        }
    );
    println!(
        "{} {:.1}s",
        style("Elapsed:").bold(),
        facts.elapsed.as_secs_f64()
    );
    Ok(())
}

// ------------------------------------------------------------------------------ helpers

/// v2 `main.rs:1941-1952`.
fn row_progress_bar(message: &str, total: u64) -> ProgressBar {
    let bar = ProgressBar::new(total.max(1));
    bar.set_style(
        ProgressStyle::with_template(
            "  [{bar:36.cyan/blue}] {percent:>3}% | {msg} | {pos}/{len} rows | eta {eta_precise}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> "),
    );
    bar.set_message(message.to_string());
    bar
}

fn spinner(message: &str) -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(StdDuration::from_millis(120));
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    spinner.set_message(message.to_string());
    spinner
}

fn format_bytes(value: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let value_f = value as f64;
    if value_f >= GIB {
        format!("{:.2} GiB", value_f / GIB)
    } else if value_f >= MIB {
        format!("{:.2} MiB", value_f / MIB)
    } else if value_f >= KIB {
        format!("{:.2} KiB", value_f / KIB)
    } else {
        format!("{value} B")
    }
}

fn format_count<T>(value: T) -> String
where
    T: ToString,
{
    let digits = value.to_string();
    let mut parts = Vec::new();
    let mut end = digits.len();
    while end > 3 {
        parts.push(digits[end - 3..end].to_string());
        end -= 3;
    }
    parts.push(digits[..end].to_string());
    parts.reverse();
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUANTITY: i64 = 2_147_483_647;

    fn pricing() -> SeedPricing {
        SeedPricing {
            sell_multiplier: 3.0,
            buy_multiplier: 1.0,
            price_floor: 100.0,
        }
    }

    fn row(type_id: u32, name: &str, cost: f64, pricing: SeedPricing) -> SeedRow {
        SeedRow {
            type_id,
            name: name.to_string(),
            bucket: "B",
            junk: false,
            role: MarketRole::Core,
            profile: PricingProfile::CoreGeneral,
            cost,
            ask: pricing.ask(cost),
            bid: pricing.bid(cost),
            sides: SeedSides::BOTH,
            quantity: QUANTITY,
        }
    }

    fn station() -> SeedStation {
        test_station()
    }

    fn canonical_stations() -> Vec<SeedStation> {
        crate::config::CANONICAL_HUB_STATION_IDS
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
            .collect()
    }

    fn memory_db() -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory sqlite opens");
        connection
            .execute_batch(SCHEMA_SQL)
            .expect("schema applies");
        connection
    }

    /// The whole reason `price_floor` is ask-only.
    ///
    /// Tritanium costs 2 ISK. Floor the bid as the plan text originally said and the seed
    /// pays 100 ISK for it — 50x cost. Buying a Venture at its 403,200 ask, reprocessing it
    /// and selling the minerals into floored bids returns 1,417,500 ISK: a 3.52x money pump
    /// that runs forever at int32-max quantities. The bid must track cost exactly.
    #[test]
    fn the_floor_lifts_the_ask_only_so_tritanium_cannot_pump_isk_through_reprocessing() {
        let pricing = pricing();

        // 2 ISK Tritanium: the ask is lifted to the floor, the bid is NOT.
        assert_eq!(
            pricing.ask(2.0),
            100.0,
            "a 2 ISK cost asks at the 100 ISK floor"
        );
        assert_eq!(
            pricing.bid(2.0),
            2.0,
            "…and bids at cost, never at the floor"
        );
        assert!(pricing.ask_is_floored(2.0));

        // The Venture anchor, well above the floor: both sides are pure multiples of cost.
        assert_eq!(pricing.ask(134_400.0), 403_200.0);
        assert_eq!(pricing.bid(134_400.0), 134_400.0);
        assert!(!pricing.ask_is_floored(134_400.0));

        // The pump itself, in numbers. A Venture reprocesses into (roughly) these minerals;
        // what matters is that the bid side pays cost, not the floor.
        let venture_ask = pricing.ask(134_400.0);
        let mineral_costs = [2.0_f64, 4.0, 20.0, 80.0];
        let quantities = [10_000.0_f64, 2_500.0, 500.0, 100.0];
        let floored_bid_payout: f64 = quantities
            .iter()
            .map(|quantity| quantity * 100.0_f64.max(0.0))
            .sum();
        let honest_bid_payout: f64 = mineral_costs
            .iter()
            .zip(quantities)
            .map(|(cost, quantity)| pricing.bid(*cost) * quantity)
            .sum();
        assert!(
            floored_bid_payout > venture_ask,
            "a floored bid would pay {floored_bid_payout} for a ship bought at {venture_ask}"
        );
        assert!(
            honest_bid_payout < venture_ask,
            "the cost-tracking bid pays {honest_bid_payout}, below the {venture_ask} ask, so \
             the loop is lossy"
        );

        // Sub-cent costs still never produce a free item or a zero bid.
        assert_eq!(pricing.bid(0.001), MIN_SEED_PRICE);
        let floorless = SeedPricing {
            price_floor: 0.0,
            ..pricing
        };
        assert_eq!(floorless.ask(0.001), MIN_SEED_PRICE);
        assert_eq!(floorless.bid(0.001), MIN_SEED_PRICE);
    }

    #[test]
    fn canonical_generic_profiles_use_only_the_technical_minimum_sell() {
        let config = SeederConfig::from_toml_str("").expect("default profiles validate");
        let profiles = SeedPricingProfiles::from_config(&config);
        let minerals = profiles.for_profile(PricingProfile::Minerals);
        let raw_ore = profiles.for_profile(PricingProfile::RawOre);

        assert_eq!(
            minerals.ask(3.78),
            3.97,
            "cheap minerals are not lifted to 100 ISK"
        );
        assert_eq!(
            raw_ore.ask(9.60),
            10.08,
            "cheap raw ore is not lifted to 100 ISK"
        );
        assert_eq!(minerals.ask(0.001), MIN_SEED_PRICE);
        assert_eq!(raw_ore.ask(-1.0), MIN_SEED_PRICE);
    }

    #[test]
    fn bpo_and_reaction_formula_profiles_are_separate_sell_only_prices() {
        let config = SeederConfig::from_toml_str("").expect("default profiles validate");
        let profiles = SeedPricingProfiles::from_config(&config);
        let bpo = profiles.for_profile(PricingProfile::Bpo);
        let reaction_formula = profiles.for_profile(PricingProfile::ReactionFormula);
        let finished_item = profiles.for_profile(PricingProfile::T1ModuleAmmoRig);

        assert_eq!(bpo.ask(5_000.0), 1_000.0, "the BPO floor remains");
        assert_eq!(
            bpo.ask(2_000_000.0),
            40_000.0,
            "ordinary reusable BPOs use 0.02 of their proven reference"
        );
        assert_eq!(
            reaction_formula.ask(2_000_000.0),
            2_000.0,
            "Reaction Formulas retain the accepted 0.001 multiplier"
        );
        assert_eq!(
            finished_item.ask(2_000_000.0),
            2_240_000.0,
            "finished-item pricing is independent of BPO acquisition pricing"
        );
        assert_eq!(
            MarketRole::Bpo.sides(),
            crate::classify::MarketSidePolicy::SELL_ONLY
        );

        let row = SeedRow {
            type_id: 1_000,
            name: "Test Blueprint".to_string(),
            bucket: "-",
            junk: false,
            role: MarketRole::Bpo,
            profile: PricingProfile::Bpo,
            cost: 2_000_000.0,
            ask: bpo.ask(2_000_000.0),
            bid: bpo.bid(2_000_000.0),
            sides: SeedSides::SELL_ONLY,
            quantity: QUANTITY,
        };
        verify_seed_rows(&[row], 1, QUANTITY, QUANTITY)
            .expect("disabled BPO Buy target cannot create a crossed-pair failure");
    }

    #[test]
    fn the_bid_never_exceeds_the_ask_and_a_bad_multiplier_pair_is_caught_by_name() {
        let good = vec![
            row(34, "Tritanium", 2.0, pricing()),
            row(32_880, "Venture", 134_400.0, pricing()),
        ];
        verify_seed_rows(&good, good.len(), QUANTITY, QUANTITY).expect("3x/1x is arbitrage-safe");

        // buy_multiplier above sell_multiplier: the bid crosses its own ask.
        let inverted = SeedPricing {
            sell_multiplier: 1.0,
            buy_multiplier: 3.0,
            price_floor: 100.0,
        };
        let bad = vec![row(32_880, "Venture", 134_400.0, inverted)];
        let error = verify_seed_rows(&bad, bad.len(), QUANTITY, QUANTITY)
            .expect_err("a bid above the ask must fail the build");
        let message = format!("{error:#}");
        assert!(message.contains("32880"), "{message}");
        assert!(message.contains("Venture"), "{message}");
        assert!(message.contains("index_seed"), "{message}");

        // The floor cannot create a crossed pair either: it lifts the ask, never the bid.
        let cheap = row(34, "Tritanium", 2.0, inverted);
        assert!(
            cheap.bid <= cheap.ask,
            "bid {} ask {}",
            cheap.bid,
            cheap.ask
        );
    }

    #[test]
    fn tq_crossing_mode_retains_structural_checks_and_legacy_contract() {
        let mut quote = row(34, "Test", 100., pricing());
        quote.bid = 120.;
        quote.ask = 80.;
        assert!(verify_seed_rows(&[quote.clone()], 1, QUANTITY, QUANTITY).is_err());
        assert!(verify_general_tq_seed_rows(&[quote.clone()], 1, QUANTITY, QUANTITY).is_ok());
        assert!(
            verify_general_tq_seed_rows(&[quote.clone(), quote.clone()], 2, QUANTITY, QUANTITY)
                .is_err()
        );
        quote.bid = 0.;
        assert!(verify_general_tq_seed_rows(&[quote.clone()], 1, QUANTITY, QUANTITY).is_err());
        quote.bid = f64::NAN;
        assert!(verify_general_tq_seed_rows(&[quote], 1, QUANTITY, QUANTITY).is_err());
    }

    #[test]
    fn a_duplicated_type_is_caught_before_it_can_silently_shorten_the_row_count() {
        let mut competing_rule = row(34, "Tritanium", 2.0, pricing());
        competing_rule.role = MarketRole::Rare;
        competing_rule.profile = PricingProfile::RareT2;
        competing_rule.sides = SeedSides::BUY_ONLY;
        let duplicated = vec![row(34, "Tritanium", 2.0, pricing()), competing_rule];
        // Two rows for two "seedable" types looks self-consistent by count alone; the
        // duplicate check is what catches it, and the seed tables would have written one row.
        let error = verify_seed_rows(&duplicated, 2, QUANTITY, QUANTITY)
            .expect_err("a duplicate type must fail the build");
        let message = format!("{error:#}");
        assert!(message.contains("34"), "{message}");
        assert!(message.contains("twice"), "{message}");
        assert!(message.contains("role/profile"), "{message}");

        // And the plain count mismatch is caught too, naming both numbers.
        let short = vec![row(34, "Tritanium", 2.0, pricing())];
        let error = verify_seed_rows(&short, 2, QUANTITY, QUANTITY)
            .expect_err("one row for two seedable types must fail");
        let message = format!("{error:#}");
        assert!(message.contains('2') && message.contains('1'), "{message}");

        // A row planned at the wrong quantity is caught by the same pass.
        let mut wrong = vec![row(34, "Tritanium", 2.0, pricing())];
        wrong[0].quantity = 1;
        let error = verify_seed_rows(&wrong, 1, QUANTITY, QUANTITY)
            .expect_err("a mis-quantified row must fail");
        assert!(format!("{error:#}").contains("index_seed"));
    }

    #[test]
    fn every_written_row_carries_the_configured_quantity_in_both_quantity_columns() {
        let mut connection = memory_db();
        let station = station();
        let rows = vec![
            row(34, "Tritanium", 2.0, pricing()),
            row(32_880, "Venture", 134_400.0, pricing()),
        ];
        let (sells, buys) = write_seed_rows(
            &mut connection,
            &station,
            &rows,
            "2026-08-10T00:00:00Z",
            SeedTableCounts::index_only(&rows),
            None,
        )
        .expect("the seed rows write");
        assert_eq!((sells, buys), (2, 2));

        for table in ["seed_stock", "seed_buy_orders"] {
            let mismatched: i64 = connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE quantity <> initial_quantity"),
                    [],
                    |row| row.get(0),
                )
                .expect("query runs");
            assert_eq!(
                mismatched, 0,
                "{table} must write quantity into both columns"
            );

            let off_target: i64 = connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE quantity <> ?1"),
                    params![QUANTITY],
                    |row| row.get(0),
                )
                .expect("query runs");
            assert_eq!(off_target, 0, "{table} must write the configured quantity");

            let versions: i64 = connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE price_version <> 1"),
                    [],
                    |row| row.get(0),
                )
                .expect("query runs");
            assert_eq!(versions, 0, "{table} must write price_version 1");
        }

        // The station geometry is denormalised onto every row.
        let (system, constellation, region): (u32, u32, u32) = connection
            .query_row(
                "SELECT solar_system_id, constellation_id, region_id FROM seed_stock WHERE type_id = 34",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query runs");
        assert_eq!(
            (system, constellation, region),
            (
                station.solar_system_id,
                station.constellation_id,
                station.region_id
            )
        );

        // Tritanium: floored ask, honest bid, all the way through to the stored row.
        let (ask, bid): (f64, f64) = connection
            .query_row(
                "SELECT (SELECT price FROM seed_stock WHERE type_id = 34),
                        (SELECT price FROM seed_buy_orders WHERE type_id = 34)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query runs");
        assert_eq!((ask, bid), (100.0, 2.0));
    }

    #[test]
    fn canonical_writer_replicates_identical_rows_to_exactly_five_hubs() {
        let mut connection = memory_db();
        let stations = canonical_stations();
        let mut sell_only = row(4_000, "Test Skillbook", 1_000.0, pricing());
        sell_only.role = MarketRole::Skillbook;
        sell_only.profile = PricingProfile::Skillbook;
        sell_only.sides = SeedSides::SELL_ONLY;
        let rows = vec![row(34, "Tritanium", 2.0, pricing()), sell_only];

        let written = write_canonical_seed_rows(
            &mut connection,
            &stations,
            &rows,
            "2026-08-22T00:00:00Z",
            None,
        )
        .expect("canonical rows write and verify");
        assert_eq!(written, (10, 5));

        let stored_station_ids = connection
            .prepare("SELECT DISTINCT station_id FROM seed_stock ORDER BY station_id")
            .expect("query prepares")
            .query_map([], |row| row.get::<_, u64>(0))
            .expect("query runs")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows decode");
        let mut expected_station_ids = crate::config::CANONICAL_HUB_STATION_IDS.to_vec();
        expected_station_ids.sort_unstable();
        assert_eq!(stored_station_ids, expected_station_ids);

        let distinct_books: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM (
                   SELECT type_id, price, quantity, initial_quantity
                     FROM seed_stock
                    GROUP BY type_id, price, quantity, initial_quantity
                 )",
                [],
                |row| row.get(0),
            )
            .expect("query runs");
        assert_eq!(distinct_books, 2, "all hubs share the same two Sell rows");
    }

    #[test]
    fn a_crossed_pair_aborts_the_write_transaction_instead_of_committing_it() {
        let mut connection = memory_db();
        let inverted = SeedPricing {
            sell_multiplier: 1.0,
            buy_multiplier: 3.0,
            price_floor: 0.0,
        };
        let rows = vec![row(32_880, "Venture", 134_400.0, inverted)];
        let error = write_seed_rows(
            &mut connection,
            &station(),
            &rows,
            "2026-08-10T00:00:00Z",
            SeedTableCounts::index_only(&rows),
            None,
        )
        .expect_err("the SQL invariant must reject a crossed pair");
        assert!(format!("{error:#}").contains("32880"), "{error:#}");

        // The transaction rolled back on drop, so nothing was left behind.
        let remaining: i64 = connection
            .query_row("SELECT COUNT(*) FROM seed_stock", [], |row| row.get(0))
            .expect("query runs");
        assert_eq!(remaining, 0);
    }

    #[test]
    fn summaries_over_one_seeded_station_take_that_station_s_own_price_and_quantity() {
        let mut connection = memory_db();
        let station = station();
        let rows = vec![
            row(34, "Tritanium", 2.0, pricing()),
            row(32_880, "Venture", 134_400.0, pricing()),
        ];
        write_seed_rows(
            &mut connection,
            &station,
            &rows,
            "2026-08-10T00:00:00Z",
            SeedTableCounts::index_only(&rows),
            None,
        )
        .expect("the seed rows write");

        let region_rows = rebuild_summaries(&mut connection, "region_summaries", "region_id")
            .expect("region summaries rebuild");
        let system_rows =
            rebuild_summaries(&mut connection, "system_seed_summaries", "solar_system_id")
                .expect("system summaries rebuild");
        assert_eq!(region_rows, rows.len());
        assert_eq!(system_rows, rows.len());

        for (table, key, key_value) in [
            (
                "region_summaries",
                "region_id",
                u64::from(station.region_id),
            ),
            (
                "system_seed_summaries",
                "solar_system_id",
                u64::from(station.solar_system_id),
            ),
        ] {
            let (
                summary_key,
                best_ask,
                total_ask,
                ask_station,
                best_bid,
                total_bid,
                bid_station,
            ): (u64, f64, i64, u64, f64, i64, u64) = connection
                .query_row(
                    &format!(
                        "SELECT {key}, best_ask_price, total_ask_quantity, best_ask_station_id,
                                best_bid_price, total_bid_quantity, best_bid_station_id
                           FROM {table} WHERE type_id = 32880"
                    ),
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )
                .expect("query runs");
            assert_eq!(summary_key, key_value, "{table}");
            assert_eq!(best_ask, 403_200.0, "{table} best ask is our own ask");
            assert_eq!(best_bid, 134_400.0, "{table} best bid is our own bid");
            assert_eq!(
                total_ask, QUANTITY,
                "{table} total is the one row's quantity"
            );
            assert_eq!(total_bid, QUANTITY, "{table}");
            assert_eq!(ask_station, station.station_id, "{table}");
            assert_eq!(bid_station, station.station_id, "{table}");
        }

        // A rebuild is idempotent: it deletes before it inserts.
        let again = rebuild_summaries(&mut connection, "region_summaries", "region_id")
            .expect("region summaries rebuild again");
        assert_eq!(again, rows.len());
        let total: i64 = connection
            .query_row("SELECT COUNT(*) FROM region_summaries", [], |row| {
                row.get(0)
            })
            .expect("query runs");
        assert_eq!(total, rows.len() as i64);
    }

    #[test]
    fn the_summary_quantity_sum_cannot_overflow_i64_at_the_configured_quantity() {
        // One seeded station today; the headroom says how many it would take to matter.
        let headroom = summary_sum_headroom(1, QUANTITY).expect("one station cannot overflow");
        assert_eq!(headroom, i64::MAX / QUANTITY);
        assert!(headroom > 4_000_000_000, "headroom is {headroom} stations");

        // Every station in New Eden (~5,200) at the int32 ceiling is still nowhere near it.
        summary_sum_headroom(5_200, QUANTITY).expect("the whole universe still fits");

        // And the guard does bite when asked something absurd.
        assert!(summary_sum_headroom(u64::MAX, QUANTITY).is_err());
    }

    /// One overlay book, written the way a snapshot import would produce it.
    fn overlay(rows: &[(u64, u32, bool, f64, u64)]) -> crate::overlay::OverlayImport {
        let mut import = crate::overlay::OverlayImport::default();
        for (station_id, type_id, is_buy, price, quantity) in rows {
            let liquidity = crate::overlay::SeedLiquidity {
                station_id: *station_id,
                solar_system_id: 30_000_142,
                constellation_id: 20_000_020,
                region_id: 10_000_002,
                type_id: *type_id,
                price_cents: (*price * 100.0).round() as i64,
                quantity: *quantity,
            };
            let book = if *is_buy {
                &mut import.buy
            } else {
                &mut import.sell
            };
            book.insert((*station_id, *type_id), liquidity);
        }
        import
    }

    const RENS: u64 = 60_004_588;
    /// Expert Systems: Standard Cruiser — a type only TQ's NPCs seed, so nothing in the index
    /// plan touches it.
    const OVERLAY_ONLY: u32 = 3_380;

    /// The ordering rule the whole two-pass build hangs on.
    ///
    /// TQ sells the Venture at Jita 4-4 too. The overlay writes first, the index pass writes
    /// second with `INSERT OR REPLACE`, and the outcome must be: our computed price at Jita
    /// 4-4, TQ's price everywhere else, and every overlay-only type left alone.
    #[test]
    fn the_index_pass_wins_at_its_own_station_and_the_overlay_survives_everywhere_else() {
        let mut connection = memory_db();
        let station = station();

        // TQ's book: a Venture ask + bid at Jita 4-4 AND at Rens, plus a skillbook at Rens.
        let import = overlay(&[
            (station.station_id, 32_880, false, 500_000.0, 12),
            (station.station_id, 32_880, true, 480_000.0, 40),
            (RENS, 32_880, false, 600_000.0, 3),
            (RENS, 32_880, true, 450_000.0, 7),
            (RENS, OVERLAY_ONLY, false, 20_000.0, 55),
        ]);
        let (overlay_sells, overlay_buys) = crate::overlay::write_overlay_rows(
            &mut connection,
            &import,
            "2026-08-10T00:00:00Z",
            None,
        )
        .expect("the overlay writes");
        assert_eq!((overlay_sells, overlay_buys), (3, 2));

        // Then our own pass, over the top.
        let rows = vec![
            row(34, "Tritanium", 2.0, pricing()),
            row(32_880, "Venture", 252_906.5, pricing()),
        ];
        let (replaced_sell, replaced_buy) =
            overlay_rows_replaced_at(&import, station.station_id, &rows);
        assert_eq!(
            (replaced_sell, replaced_buy),
            (1, 1),
            "exactly the Venture's two Jita rows are overwritten, not the Rens ones"
        );
        let expected = SeedTableCounts::after_overlay(
            &rows,
            import.sell_rows(),
            import.buy_rows(),
            replaced_sell,
            replaced_buy,
        );
        assert_eq!(
            expected,
            SeedTableCounts {
                sell_rows: 4,
                buy_rows: 3
            }
        );

        write_seed_rows(
            &mut connection,
            &station,
            &rows,
            "2026-08-10T00:00:00Z",
            expected,
            None,
        )
        .expect("the index pass writes over the overlay");

        let price = |table: &str, station_id: u64, type_id: u32| -> Option<f64> {
            connection
                .query_row(
                    &format!("SELECT price FROM {table} WHERE station_id = ?1 AND type_id = ?2"),
                    params![station_id, type_id],
                    |row| row.get(0),
                )
                .ok()
        };

        // Jita 4-4: ours, not TQ's.
        assert_eq!(
            price("seed_stock", station.station_id, 32_880),
            Some(pricing().ask(252_906.5)),
            "the index ask must survive the overlay's 500,000 TQ price"
        );
        assert_eq!(
            price("seed_buy_orders", station.station_id, 32_880),
            Some(pricing().bid(252_906.5))
        );
        // Rens: TQ's, untouched.
        assert_eq!(price("seed_stock", RENS, 32_880), Some(600_000.0));
        assert_eq!(price("seed_buy_orders", RENS, 32_880), Some(450_000.0));
        assert_eq!(price("seed_stock", RENS, OVERLAY_ONLY), Some(20_000.0));
        // And the type only we seed exists only where we put it.
        assert_eq!(price("seed_stock", station.station_id, 34), Some(100.0));
        assert_eq!(price("seed_stock", RENS, 34), None);

        // Overlay rows keep the snapshot's volume; index rows keep the configured quantity.
        let overlay_quantity: i64 = connection
            .query_row(
                "SELECT quantity FROM seed_stock WHERE station_id = ?1 AND type_id = ?2",
                params![RENS, 32_880u32],
                |row| row.get(0),
            )
            .expect("query runs");
        assert_eq!(overlay_quantity, 3);
        let index_quantity: i64 = connection
            .query_row(
                "SELECT quantity FROM seed_stock WHERE station_id = ?1 AND type_id = ?2",
                params![station.station_id, 32_880u32],
                |row| row.get(0),
            )
            .expect("query runs");
        assert_eq!(index_quantity, QUANTITY);
    }

    /// The other order, caught rather than shipped.
    #[test]
    fn an_overlay_row_left_standing_at_the_index_station_fails_the_build_by_name() {
        let mut connection = memory_db();
        let station = station();
        let import = overlay(&[
            (station.station_id, 32_880, false, 500_000.0, 12),
            (station.station_id, 32_880, true, 480_000.0, 40),
        ]);
        crate::overlay::write_overlay_rows(&mut connection, &import, "2026-08-10T00:00:00Z", None)
            .expect("the overlay writes");

        // Exactly what running the passes the wrong way round leaves behind: TQ's price
        // sitting where ours should be. Nothing about the row count gives it away.
        let rows = vec![row(32_880, "Venture", 252_906.5, pricing())];
        let error = verify_index_pass_won(&connection, &rows, station.station_id)
            .expect_err("a TQ price at the index station must fail the build");
        let message = format!("{error:#}");
        assert!(message.contains("32880"), "{message}");
        assert!(message.contains("500000"), "{message}");
        assert!(message.contains("BEFORE"), "{message}");

        // A missing row is caught too, with its own message.
        let missing = vec![row(34, "Tritanium", 2.0, pricing())];
        let error = verify_index_pass_won(&connection, &missing, station.station_id)
            .expect_err("a planned type with no row must fail");
        assert!(format!("{error:#}").contains("no ask row"), "{error:#}");
    }

    /// TQ's own crossed pairs are reported, not enforced — the build only owns its own rows.
    #[test]
    fn a_crossed_overlay_pair_is_counted_and_left_alone_while_ours_still_aborts() {
        let mut connection = memory_db();
        let station = station();
        // Rens bids 90 for a type it sells at 50: TQ's problem, imported as-is.
        let import = overlay(&[
            (RENS, OVERLAY_ONLY, false, 50.0, 4),
            (RENS, OVERLAY_ONLY, true, 90.0, 4),
        ]);
        crate::overlay::write_overlay_rows(&mut connection, &import, "2026-08-10T00:00:00Z", None)
            .expect("the overlay writes");

        let rows = vec![row(34, "Tritanium", 2.0, pricing())];
        write_seed_rows(
            &mut connection,
            &station,
            &rows,
            "2026-08-10T00:00:00Z",
            SeedTableCounts::after_overlay(&rows, 1, 1, 0, 0),
            None,
        )
        .expect("a crossed pair at another station must not fail our build");
        assert_eq!(
            count_crossed_pairs(&connection).expect("the count runs"),
            1,
            "it is reported instead, and the audit is what judges it"
        );
    }

    /// **Per-side deferral, end to end at the write.** A type whose ask was deferred writes
    /// only its bid, and TQ's own ask at the index station is left exactly where the overlay
    /// put it — which is the point of deferring, and the thing the row-count arithmetic has to
    /// agree with.
    #[test]
    fn a_deferred_ask_writes_only_the_bid_and_leaves_tqs_row_standing_at_the_index_station() {
        let mut connection = memory_db();
        let station = station();
        // TQ holds the Venture on both sides at Jita 4-4 and sells a skillbook at Rens.
        let import = overlay(&[
            (station.station_id, 32_880, false, 500_000.0, 12),
            (station.station_id, 32_880, true, 480_000.0, 40),
            (RENS, OVERLAY_ONLY, false, 20_000.0, 55),
        ]);
        crate::overlay::write_overlay_rows(&mut connection, &import, "2026-08-10T00:00:00Z", None)
            .expect("the overlay writes");

        // Our Venture ask crossed TQ's 480,000 buyback, so the plan kept only the bid.
        let mut venture = row(32_880, "Venture", 252_906.5, pricing());
        venture.sides = SeedSides {
            ask: false,
            bid: true,
        };
        let rows = vec![row(34, "Tritanium", 2.0, pricing()), venture];
        assert_eq!(planned_side_counts(&rows), (1, 2));

        let (replaced_sell, replaced_buy) =
            overlay_rows_replaced_at(&import, station.station_id, &rows);
        assert_eq!(
            (replaced_sell, replaced_buy),
            (0, 1),
            "only the bid is ours now, so only TQ's Jita bid is replaced"
        );
        let expected = SeedTableCounts::after_overlay(
            &rows,
            import.sell_rows(),
            import.buy_rows(),
            replaced_sell,
            replaced_buy,
        );
        // 2 overlay asks + 1 planned ask - 0 replaced; 1 overlay bid + 2 planned bids - 1.
        assert_eq!(
            expected,
            SeedTableCounts {
                sell_rows: 3,
                buy_rows: 2
            }
        );

        let (sell_rows, buy_rows) = write_seed_rows(
            &mut connection,
            &station,
            &rows,
            "2026-08-10T00:00:00Z",
            expected,
            None,
        )
        .expect("a one-sided row set writes and verifies");
        assert_eq!((sell_rows, buy_rows), (1, 2));

        let price = |table: &str, station_id: u64, type_id: u32| -> Option<f64> {
            connection
                .query_row(
                    &format!("SELECT price FROM {table} WHERE station_id = ?1 AND type_id = ?2"),
                    params![station_id, type_id],
                    |row| row.get(0),
                )
                .ok()
        };
        assert_eq!(
            price("seed_stock", station.station_id, 32_880),
            Some(500_000.0),
            "TQ still sells the Venture at Jita: our ask was deferred, not overwritten by it"
        );
        assert_eq!(
            price("seed_buy_orders", station.station_id, 32_880),
            Some(pricing().bid(252_906.5)),
            "…and our bid still won the side we did write"
        );
        assert_eq!(price("seed_stock", station.station_id, 34), Some(100.0));

        // And a leaked deferred side is caught: put our computed ask back where the plan said
        // it must not be, and the same verification that passed above now fails by name.
        connection
            .execute(
                "UPDATE seed_stock SET price = ?1 WHERE station_id = ?2 AND type_id = ?3",
                params![pricing().ask(252_906.5), station.station_id, 32_880u32],
            )
            .expect("the update runs");
        let error = verify_index_pass_won(&connection, &rows, station.station_id)
            .expect_err("a deferred side that carries our price must fail the build");
        let message = format!("{error:#}");
        assert!(message.contains("32880"), "{message}");
        assert!(message.contains("deferred to the NPC catalog"), "{message}");
    }

    /// A planned row that writes nothing is a plan bug, not a no-op: the type should have been
    /// dropped instead.
    #[test]
    fn a_planned_row_with_neither_side_is_rejected_before_anything_is_written() {
        let mut nothing = row(34, "Tritanium", 2.0, pricing());
        nothing.sides = SeedSides {
            ask: false,
            bid: false,
        };
        let rows = vec![nothing];
        let error = verify_seed_rows(&rows, rows.len(), QUANTITY, QUANTITY)
            .expect_err("a row that writes neither side must be refused");
        let message = format!("{error:#}");
        assert!(message.contains("34"), "{message}");
        assert!(message.contains("neither an ask nor a bid"), "{message}");
    }

    #[test]
    fn plan_rows_price_index_and_junk_buckets_from_their_own_config_sections() {
        // Bucket D carries its own multipliers and quantity; the ask floor is shared.
        let index = pricing();
        let junk = SeedPricing {
            sell_multiplier: 5.0,
            buy_multiplier: 0.5,
            price_floor: 100.0,
        };
        assert_eq!(index.ask(1_000.0), 3_000.0);
        assert_eq!(junk.ask(1_000.0), 5_000.0);
        assert_eq!(junk.bid(1_000.0), 500.0);
        assert!(junk.bid(1_000.0) <= junk.ask(1_000.0));
    }
}
