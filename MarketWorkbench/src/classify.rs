//! Seed-universe classifier — `doc/JITA_INDEX_SEED_PLAN.md` §2.
//!
//! Splits the published/market-grouped type universe into the buckets the later passes
//! price and write:
//!
//! | bucket | contents                                   | priced by                    |
//! |--------|--------------------------------------------|------------------------------|
//! | **A**  | manufacturable T1 (incl. reaction outputs) | bottom-up blueprint cost     |
//! | **B**  | ores / minerals / materials                | cost if craftable, else leaf |
//! | **D**  | market-priced junk candidates              | captured CCP price only      |
//!
//! Two data traps this module exists to avoid:
//!
//! 1. **The reaction index hole.** All 120 reaction blueprints carry a top-level
//!    `productTypeID` of 0, so `industryBlueprints`' precomputed
//!    `blueprintTypeIDsByProductTypeID` misses every reaction output. The producer index
//!    here is therefore built by scanning `activities.manufacturing.products[]` *and*
//!    `activities.reaction.products[]`; the precomputed index is never read.
//! 2. **The faction leak.** 714 types with an SDE blueprint and Tech Level 1 are
//!    faction/storyline/officer/deadspace gear (navy hulls, CONCORD capital modules, AT
//!    prizes). Only dogma attribute 1692 (metaGroupID) separates them from real T1, and
//!    only after the category and lineage rules have already run — see [`BucketAFunnel`].
//!
//! A and B deliberately overlap (~183 types: fuel blocks, composites, intermediates are
//! both manufacturable and raw materials). Membership is therefore a flag set
//! ([`BucketFlags`]), not an enum, and the seeded set is the *union* A ∪ B ∪ D so an
//! overlapping type is counted exactly once.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::JunkSeedConfig;
use crate::staticdata::{ItemTypeRecord, MaterialEntry, StaticData};

pub const BLUEPRINT_CATEGORY_ID: u32 = 9;
pub const SKILL_CATEGORY_ID: u32 = 16;
pub const SHIP_CATEGORY_ID: u32 = 6;
pub const MODULE_CATEGORY_ID: u32 = 7;
pub const CHARGE_CATEGORY_ID: u32 = 8;
pub const COMMODITY_CATEGORY_ID: u32 = 17;
pub const COMMAND_CENTER_GROUP_ID: u32 = 1027;
pub const MINERAL_GROUP_ID: u32 = 18;
pub const NATURAL_SALVAGE_GROUP_ID: u32 = 754;
pub const APPAREL_CATEGORY_ID: u32 = 30;
pub const SKIN_CATEGORY_ID: u32 = 91;

/// Authoritative legacy Group B/C discovery set from
/// `_market-v1-progression-npc/t2-intermediate-sell-gaps.csv`. The enclosing SDE groups
/// contain 43 additional marketable types, so a broad group predicate would violate the
/// reviewed 114-type boundary.
pub const PRODUCTION_INTERMEDIATE_TYPE_IDS: &[u32] = &[
    11_475, 11_476, 11_478, 11_481, 11_482, 11_483, 11_484, 11_486, 11_530, 11_531, 11_532, 11_533,
    11_534, 11_535, 11_536, 11_537, 11_538, 11_539, 11_540, 11_541, 11_542, 11_543, 11_544, 11_545,
    11_547, 11_548, 11_549, 11_550, 11_551, 11_552, 11_553, 11_554, 11_555, 11_556, 11_557, 11_558,
    11_688, 11_689, 11_690, 11_691, 11_692, 11_693, 11_694, 11_695, 21_009, 21_011, 21_013, 21_017,
    21_019, 21_021, 21_023, 21_025, 21_027, 21_029, 21_035, 21_037, 21_039, 21_041, 24_558, 24_560,
    29_039, 29_041, 29_043, 29_045, 29_049, 29_051, 29_053, 29_055, 29_059, 29_061, 29_065, 29_067,
    29_069, 29_071, 29_073, 29_075, 29_079, 29_081, 29_085, 29_089, 29_091, 29_093, 29_095, 29_097,
    29_101, 29_103, 29_107, 29_109, 57_470, 57_471, 57_472, 57_473, 57_474, 57_475, 57_476, 57_477,
    57_478, 57_479, 57_480, 57_481, 57_482, 57_483, 57_484, 57_485, 57_486, 57_487, 57_488, 81_063,
    81_064, 81_065, 81_066, 81_067, 81_068, 81_069,
];

/// Exact manufacturable Structure Component extension. This is deliberately separate from
/// the legacy T2-oriented discovery set: SDE group 536 also contains 49_720, whose funded
/// manufacturing route is incomplete because 33_577 has no acquisition path.
pub const STRUCTURE_COMPONENT_PRODUCTION_INTERMEDIATE_TYPE_IDS: &[u32] = &[
    21_947, 21_949, 21_951, 21_953, 21_955, 21_957, 21_959, 21_961, 21_963, 21_965, 21_967, 21_969,
    36_956, 36_957, 36_958,
];

pub fn is_production_intermediate_type(type_id: u32) -> bool {
    PRODUCTION_INTERMEDIATE_TYPE_IDS.contains(&type_id)
        || STRUCTURE_COMPONENT_PRODUCTION_INTERMEDIATE_TYPE_IDS.contains(&type_id)
}

pub const MOON_RESOURCE_GROUP_IDS: &[u32] = &[427, 1_884, 1_920, 1_921, 1_922, 1_923];
/// Authoritative SDE categories for genuine PI outputs: extracted planetary resources and
/// processed planetary commodities. Equipment, Command Centers, deployables and NPC trade
/// goods live in other categories and therefore cannot enter this set by adjacency.
pub const PLANETARY_INDUSTRY_CATEGORY_IDS: &[u32] = &[42, 43];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RareLiquidityRequirement {
    RequiredGameplay,
    OptionalCosmetic,
    OptionalBlueprint,
}

impl RareLiquidityRequirement {
    pub fn key(self) -> &'static str {
        match self {
            Self::RequiredGameplay => "required-gameplay-rare",
            Self::OptionalCosmetic => "optional-cosmetic",
            Self::OptionalBlueprint => "optional-blueprint-bpc",
        }
    }
}

/// Market v1 requires liquidity for gameplay Rare final items, not every marketable LP
/// reward. Authoritative categories decide cosmetics and blueprints; names never participate.
pub fn rare_liquidity_requirement(
    item: &crate::staticdata::ItemTypeRecord,
) -> RareLiquidityRequirement {
    match item.category_id {
        Some(BLUEPRINT_CATEGORY_ID) => RareLiquidityRequirement::OptionalBlueprint,
        Some(APPAREL_CATEGORY_ID | SKIN_CATEGORY_ID) => RareLiquidityRequirement::OptionalCosmetic,
        _ => RareLiquidityRequirement::RequiredGameplay,
    }
}

/// One exclusive owner for a seeded type. `Bpo` covers reusable recipe access (ordinary
/// manufacturing BPOs and published Reaction Formulas). The side policy is part of the role,
/// not a later overlay collision decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MarketRole {
    Core,
    Bpo,
    Bridge,
    ProgressionNpc,
    ProductionIntermediate,
    Rare,
    Skillbook,
    CommandCenter,
}

impl MarketRole {
    pub fn key(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Bpo => "bpo",
            Self::Bridge => "bridge",
            Self::ProgressionNpc => "progression-npc",
            Self::ProductionIntermediate => "production-intermediate",
            Self::Rare => "rare",
            Self::Skillbook => "skillbook",
            Self::CommandCenter => "command-center",
        }
    }

    pub fn sides(self) -> MarketSidePolicy {
        match self {
            Self::Core => MarketSidePolicy::BOTH,
            Self::Rare => MarketSidePolicy::BUY_ONLY,
            Self::Bpo
            | Self::Bridge
            | Self::ProgressionNpc
            | Self::Skillbook
            | Self::CommandCenter => MarketSidePolicy::SELL_ONLY,
            Self::ProductionIntermediate => MarketSidePolicy::BOTH,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketSidePolicy {
    pub sell: bool,
    pub buy: bool,
}

impl MarketSidePolicy {
    pub const BOTH: Self = Self {
        sell: true,
        buy: true,
    };
    pub const SELL_ONLY: Self = Self {
        sell: true,
        buy: false,
    };
    pub const BUY_ONLY: Self = Self {
        sell: false,
        buy: true,
    };
}

/// Independent price target selected after role assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PricingProfile {
    CoreGeneral,
    CompressedOre,
    RawOre,
    Minerals,
    GasIceSalvage,
    T1ModuleAmmoRig,
    Destroyer,
    Cruiser,
    Battlecruiser,
    Battleship,
    CapitalStructureComponent,
    OrcaFreighterStructure,
    Bpo,
    ReactionFormula,
    Bridge,
    PlanetaryIndustry,
    ProgressionNpc,
    ProductionIntermediate,
    RareT2,
    RareFactionLp,
    RareDeadspaceDed,
    RareOfficer,
    Skillbook,
    CommandCenter,
}

impl PricingProfile {
    pub fn key(self) -> &'static str {
        match self {
            Self::CoreGeneral => "core_general",
            Self::CompressedOre => "compressed_ore",
            Self::RawOre => "raw_ore",
            Self::Minerals => "minerals",
            Self::GasIceSalvage => "gas_ice_salvage",
            Self::T1ModuleAmmoRig => "t1_module_ammo_rig",
            Self::Destroyer => "destroyer",
            Self::Cruiser => "cruiser",
            Self::Battlecruiser => "battlecruiser",
            Self::Battleship => "battleship",
            Self::CapitalStructureComponent => "capital_structure_component",
            Self::OrcaFreighterStructure => "orca_freighter_structure",
            Self::Bpo => "bpo",
            Self::ReactionFormula => "reaction_formula",
            Self::Bridge => "bridge",
            Self::PlanetaryIndustry => "planetary_industry",
            Self::ProgressionNpc => "progression_npc",
            Self::ProductionIntermediate => "production_intermediate",
            Self::RareT2 => "rare_t2",
            Self::RareFactionLp => "rare_faction_lp",
            Self::RareDeadspaceDed => "rare_deadspace_ded",
            Self::RareOfficer => "rare_officer",
            Self::Skillbook => "skillbook",
            Self::CommandCenter => "command_center",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketAssignment {
    pub role: MarketRole,
    pub profile: PricingProfile,
    pub sides: MarketSidePolicy,
}

impl MarketAssignment {
    fn new(role: MarketRole, profile: PricingProfile) -> Self {
        Self {
            role,
            profile,
            sides: if profile == PricingProfile::PlanetaryIndustry {
                MarketSidePolicy::BOTH
            } else {
                role.sides()
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RoleMatch {
    rule: &'static str,
    assignment: MarketAssignment,
}

#[derive(Debug, Deserialize)]
struct LpOfferCatalogFile {
    #[serde(rename = "offersByCorpID", default)]
    offers_by_corp_id: BTreeMap<String, Vec<LpOfferRecord>>,
}

#[derive(Debug, Deserialize)]
struct LpOfferRecord {
    #[serde(alias = "typeID")]
    type_id: u32,
}

/// Reads only the committed LP offer outputs. The LP implementation and offer file remain
/// untouched; this set is used solely to prevent those rewards receiving synthetic Sells.
pub fn load_lp_reward_type_ids(path: &Path) -> Result<BTreeSet<u32>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read LP offer catalog {}", path.to_string_lossy()))?;
    let catalog = serde_json::from_str::<LpOfferCatalogFile>(&raw).with_context(|| {
        format!(
            "failed to parse LP offer catalog {}",
            path.to_string_lossy()
        )
    })?;
    Ok(catalog
        .offers_by_corp_id
        .into_values()
        .flatten()
        .map(|offer| offer.type_id)
        .filter(|type_id| *type_id > 0)
        .collect())
}

/// Expands the authoritative LP catalog only across its direct manufacturing-blueprint
/// rewards. A published definition is not enough by itself: the blueprint typeID must be an
/// immediate LP offer, and only marketable finished products inherit the LP identity.
pub fn expand_lp_manufacturing_products(
    data: &StaticData,
    direct_rewards: &BTreeSet<u32>,
) -> BTreeSet<u32> {
    let mut expanded = direct_rewards.clone();
    for blueprint in &data.blueprints {
        if !blueprint.published || !direct_rewards.contains(&blueprint.blueprint_type_id) {
            continue;
        }
        let Some(manufacturing) = blueprint.activities.manufacturing.as_ref() else {
            continue;
        };
        expanded.extend(manufacturing.products.iter().filter_map(|product| {
            data.item_type(product.type_id)
                .is_some_and(ItemTypeRecord::is_marketable)
                .then_some(product.type_id)
        }));
    }
    expanded
}

pub fn load_expanded_lp_reward_type_ids(path: &Path, data: &StaticData) -> Result<BTreeSet<u32>> {
    Ok(expand_lp_manufacturing_products(
        data,
        &load_lp_reward_type_ids(path)?,
    ))
}

/// Categories that are never index- or junk-seeded (plan §2.1, §2.5). Blueprints are
/// excluded here because approved reusable recipes have their own Sell-only canonical role,
/// while invention outputs and other blueprints are never seeded at all.
pub const EXCLUDED_CATEGORY_IDS: &[(u32, &str)] = &[
    (5, "PLEX / services / injectors"),
    (9, "Blueprints"),
    (16, "Skills"),
    (30, "Apparel"),
    (63, "Special Edition Commodities"),
    (91, "SKINs (permanent / 30-day)"),
    (2100, "Expert Systems"),
    (2118, "Sequence Binders (SKIN design)"),
];

/// Bucket B categories (plan §2.2). Deliberately **not** filtered by tech level or
/// metaGroup: these are raw materials, and the T1/faction vocabulary does not apply to
/// ore, ice, moon goo, PI, or reagents.
pub const MATERIAL_CATEGORY_IDS: &[(u32, &str)] = &[
    (4, "Material"),
    (25, "Asteroid"),
    (42, "Planetary Resources"),
    (43, "Planetary Commodities"),
    (2143, "Colony Reagents"),
];

/// Category 2 (Celestial) is mostly scenery, so only the two harvestable gas groups join
/// bucket B rather than the whole category.
pub const GAS_CATEGORY_ID: u32 = 2;
/// Group names inside [`GAS_CATEGORY_ID`] that are bucket B materials.
pub const GAS_GROUP_NAMES: &[&str] = &["Harvestable Cloud", "Compressed Gas"];

/// `[junk_seed] categories` vocabulary → category ID (plan §2.4, decision record #3).
/// This is the whole legal vocabulary: a name outside this table is a hard config error,
/// never a silently ignored line.
pub const JUNK_CATEGORY_TABLE: &[(&str, u32, &str)] = &[
    ("modules", 7, "Module"),
    ("commodities", 17, "Commodity"),
    ("charges", 8, "Charge"),
    ("drones", 18, "Drone"),
    // Category 35 holds 44 types, but only the 8 Generic Decryptors (34201-34208) carry a
    // market group and so reach the universe at all. The other 36 — the racial
    // Esoteric/Cryptic/Occult/Incognito decryptors and the four Subsystems Data Interfaces —
    // are published with `marketGroupID: null` and never enter the seed. No category-35 type
    // has a blueprint or a reprocessing row, so this adds bucket D members and nothing else.
    ("decryptors", 35, "Decryptor"),
];

/// A resolved `[junk_seed] categories` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JunkCategory {
    /// The config spelling, e.g. `"modules"`.
    pub config_name: &'static str,
    pub category_id: u32,
    /// The SDE category name, for reporting.
    pub category_name: &'static str,
}

/// Maps configured junk-category names onto category IDs, rejecting anything outside
/// [`JUNK_CATEGORY_TABLE`]. Repeated names collapse to one entry (order preserved) so a
/// duplicated line cannot double-count bucket D.
pub fn resolve_junk_categories(names: &[String]) -> Result<Vec<JunkCategory>> {
    let mut resolved: Vec<JunkCategory> = Vec::with_capacity(names.len());
    for name in names {
        let key = name.trim().to_ascii_lowercase();
        let Some(&(config_name, category_id, category_name)) = JUNK_CATEGORY_TABLE
            .iter()
            .find(|(candidate, _, _)| *candidate == key)
        else {
            let legal = JUNK_CATEGORY_TABLE
                .iter()
                .map(|(candidate, id, _)| format!("{candidate} ({id})"))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "[junk_seed] categories contains unknown category \"{name}\". Legal values: {legal}"
            );
        };
        if resolved
            .iter()
            .any(|entry| entry.category_id == category_id)
        {
            continue;
        }
        resolved.push(JunkCategory {
            config_name,
            category_id,
            category_name,
        });
    }
    Ok(resolved)
}

/// Which industry activity yields a product. Only these two make things; invention makes
/// *blueprints*, which is what [`Classification::invention_lineage`] tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerActivity {
    Manufacturing,
    Reaction,
}

impl ProducerActivity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Manufacturing => "Manufacturing",
            Self::Reaction => "Reaction",
        }
    }
}

/// How one product type is made. `per_run` is the *product* quantity, which the cost map
/// must divide by — 368 blueprints yield more than one unit per run (plan §3.1).
#[derive(Debug, Clone)]
pub struct Producer {
    pub blueprint_type_id: u32,
    /// The blueprint's own `published` flag. Unpublished formulas exist in the data and
    /// lose the tie-break to a published rival regardless of file order — see
    /// [`ProducerCollision`].
    pub blueprint_published: bool,
    pub activity: ProducerActivity,
    pub per_run: i64,
    pub materials: Vec<MaterialEntry>,
}

/// A second blueprint claiming an already-claimed product type. A published blueprint
/// beats an unpublished one; between two blueprints with the same `published` flag the
/// first in `blueprintDefinitions` order wins, which keeps the index deterministic. The
/// loser is recorded with enough detail for the report to show what the tie-break chose.
///
/// This is not cosmetic. Tungsten Carbide (16672) is claimed by an unpublished "Test
/// Reaction Blueprint" (45732, 20 per run) *before* the real formula (46207, 10,000 per
/// run), so plain first-wins would divide EIV by 20 instead of 10,000 and price the
/// material ~500x too high. The published rule is what keeps 46207. The report still
/// flags any collision whose per-run output or published flag disagrees.
#[derive(Debug, Clone, Copy)]
pub struct ProducerCollision {
    pub product_type_id: u32,
    pub kept_blueprint_type_id: u32,
    pub kept_activity: ProducerActivity,
    pub kept_per_run: i64,
    pub kept_published: bool,
    pub dropped_blueprint_type_id: u32,
    pub dropped_activity: ProducerActivity,
    pub dropped_per_run: i64,
    pub dropped_published: bool,
}

impl ProducerCollision {
    /// True when the losing blueprint would have priced the product differently — the
    /// only collisions the cost map has to care about.
    pub fn is_material_disagreement(&self) -> bool {
        self.kept_per_run != self.dropped_per_run || self.kept_published != self.dropped_published
    }
}

/// Everything a collision report needs from one side of a tie-break. Copied out of the
/// index so the winner can be written back without holding a borrow on it.
#[derive(Debug, Clone, Copy)]
struct ProducerSummary {
    blueprint_type_id: u32,
    activity: ProducerActivity,
    per_run: i64,
    published: bool,
}

impl Producer {
    fn summary(&self) -> ProducerSummary {
        ProducerSummary {
            blueprint_type_id: self.blueprint_type_id,
            activity: self.activity,
            per_run: self.per_run,
            published: self.blueprint_published,
        }
    }
}

/// product typeID → how it is made, built by scanning activity products (never the
/// precomputed `blueprintTypeIDsByProductTypeID` index — see the module docs).
#[derive(Debug, Default)]
pub struct ProducerIndex {
    by_product: HashMap<u32, Producer>,
    collisions: Vec<ProducerCollision>,
}

impl ProducerIndex {
    pub fn build(data: &StaticData) -> Self {
        let mut index = Self::default();
        for blueprint in &data.blueprints {
            let activities = [
                (
                    ProducerActivity::Manufacturing,
                    blueprint.activities.manufacturing.as_ref(),
                ),
                (
                    ProducerActivity::Reaction,
                    blueprint.activities.reaction.as_ref(),
                ),
            ];
            for (activity, definition) in activities {
                let Some(definition) = definition else {
                    continue;
                };
                for product in &definition.products {
                    let candidate = Producer {
                        blueprint_type_id: blueprint.blueprint_type_id,
                        blueprint_published: blueprint.published,
                        activity,
                        per_run: product.quantity,
                        materials: definition.materials.clone(),
                    };
                    let Some(incumbent) = index
                        .by_product
                        .get(&product.type_id)
                        .map(Producer::summary)
                    else {
                        index.by_product.insert(product.type_id, candidate);
                        continue;
                    };

                    // Tie-break, in order:
                    //   1. published beats unpublished (Tungsten Carbide 16672 — see
                    //      `ProducerCollision`);
                    //   2. otherwise the incumbent stays, so equal-published rivals resolve
                    //      by `blueprintDefinitions` order and the index is deterministic.
                    let candidate_wins = candidate.blueprint_published && !incumbent.published;
                    let (kept, dropped) = if candidate_wins {
                        (candidate.summary(), incumbent)
                    } else {
                        (incumbent, candidate.summary())
                    };
                    index.collisions.push(ProducerCollision {
                        product_type_id: product.type_id,
                        kept_blueprint_type_id: kept.blueprint_type_id,
                        kept_activity: kept.activity,
                        kept_per_run: kept.per_run,
                        kept_published: kept.published,
                        dropped_blueprint_type_id: dropped.blueprint_type_id,
                        dropped_activity: dropped.activity,
                        dropped_per_run: dropped.per_run,
                        dropped_published: dropped.published,
                    });
                    if candidate_wins {
                        index.by_product.insert(product.type_id, candidate);
                    }
                }
            }
        }
        index
    }

    pub fn get(&self, product_type_id: u32) -> Option<&Producer> {
        self.by_product.get(&product_type_id)
    }

    pub fn len(&self) -> usize {
        self.by_product.len()
    }

    pub fn collisions(&self) -> &[ProducerCollision] {
        &self.collisions
    }
}

/// Every blueprint typeID that some `activities.invention.products[]` yields. Everything
/// those blueprints build is T2/T3 by construction, and the blueprints themselves are
/// never seeded (plan §2.1 rule 2). Expected size at SDE 3396210: 1,209.
fn build_invention_lineage(data: &StaticData) -> HashSet<u32> {
    let mut lineage = HashSet::new();
    for blueprint in &data.blueprints {
        let Some(invention) = blueprint.activities.invention.as_ref() else {
            continue;
        };
        for product in &invention.products {
            lineage.insert(product.type_id);
        }
    }
    lineage
}

/// Bucket A candidate survival after each rule, applied **in sequence**.
///
/// The order matters for reading the numbers: the raw universe holds ~2,683 types with
/// metaGroup ≥ 3, but most are SKINs and apparel that the category rule already removed.
/// Applied where it actually sits — after category, lineage and tech level — the
/// metaGroup rule removes the ~714 faction/storyline items the plan calls load-bearing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketAFunnel {
    /// Published types carrying a market group.
    pub universe: usize,
    /// …of which some blueprint manufactures or reacts them.
    pub manufacturable: usize,
    /// …minus the excluded categories (SKINs, apparel, skills, blueprints, …).
    pub after_category_exclusions: usize,
    /// …minus products whose blueprint is an invention product (T2/T3 lineage).
    pub after_invention_lineage: usize,
    /// …minus dogma Tech Level ≥ 2.
    pub after_tech_level: usize,
    /// …minus dogma metaGroupID ≥ 3. Equals `|A|`.
    pub after_meta_group: usize,
    /// Universe types with metaGroup ≥ 3 before any other rule, for contrast only.
    pub universe_meta_group_three_plus: usize,
}

impl BucketAFunnel {
    pub fn removed_as_not_manufacturable(&self) -> usize {
        self.universe - self.manufacturable
    }
    pub fn removed_by_category_exclusions(&self) -> usize {
        self.manufacturable - self.after_category_exclusions
    }
    pub fn removed_by_invention_lineage(&self) -> usize {
        self.after_category_exclusions - self.after_invention_lineage
    }
    pub fn removed_by_tech_level(&self) -> usize {
        self.after_invention_lineage - self.after_tech_level
    }
    pub fn removed_by_meta_group(&self) -> usize {
        self.after_tech_level - self.after_meta_group
    }
}

/// Bucket membership. A type may be in A and B at once (manufacturable materials), which
/// is why this is a flag set: it takes A's computed cost for pricing while still being
/// counted once in the seeded union.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketFlags {
    /// Bucket A — manufacturable T1.
    pub manufacturable_t1: bool,
    /// Bucket B — ore / mineral / material.
    pub material: bool,
    /// Bucket D — junk candidate, seeded only if a captured price exists (plan §2.4).
    pub junk_candidate: bool,
}

impl BucketFlags {
    /// D is mutually exclusive with A and B by construction, so any true flag means the
    /// type is part of the seeded set (subject to `[junk_seed] enabled` for D).
    pub fn is_classified(self) -> bool {
        self.manufacturable_t1 || self.material || self.junk_candidate
    }

    /// Short label for reports: `A`, `B`, `A+B`, `D`, or `-`.
    pub fn label(self) -> &'static str {
        match (self.manufacturable_t1, self.material, self.junk_candidate) {
            (true, true, _) => "A+B",
            (true, false, _) => "A",
            (false, true, _) => "B",
            (false, false, true) => "D",
            _ => "-",
        }
    }
}

/// Which bucket-D candidates the price manifest lets through (plan §2.4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JunkSeedability {
    /// Candidates holding a captured real-market price: these get seed rows.
    pub seedable: BTreeSet<u32>,
    /// Candidates with no capture. Unseeded **by design** — never an error, and never a
    /// `basePrice` fallback.
    pub dropped: BTreeSet<u32>,
    /// `[junk_seed] enabled = false`: the bucket is off entirely, so nothing was dropped
    /// for want of a price.
    pub junk_disabled: bool,
}

/// The classified seed universe.
#[derive(Debug)]
pub struct Classification {
    pub universe_size: usize,
    pub producers: ProducerIndex,
    pub invention_lineage: HashSet<u32>,
    pub bucket_a: BTreeSet<u32>,
    pub bucket_b: BTreeSet<u32>,
    /// Junk *candidates*. They only become seed rows once a captured price exists.
    pub bucket_d: BTreeSet<u32>,
    pub junk_categories: Vec<JunkCategory>,
    pub junk_counts_by_category: BTreeMap<u32, usize>,
    /// Mirrors `[junk_seed] enabled`; when false, D contributes nothing to the seeded set.
    pub junk_enabled: bool,
    pub funnel: BucketAFunnel,
}

impl Classification {
    pub fn compute(data: &StaticData, junk: &JunkSeedConfig) -> Result<Self> {
        let junk_categories = resolve_junk_categories(&junk.categories)?;
        let producers = ProducerIndex::build(data);
        let invention_lineage = build_invention_lineage(data);

        let excluded_categories = EXCLUDED_CATEGORY_IDS
            .iter()
            .map(|(id, _)| *id)
            .collect::<HashSet<_>>();
        let material_categories = MATERIAL_CATEGORY_IDS
            .iter()
            .map(|(id, _)| *id)
            .collect::<HashSet<_>>();
        let junk_category_ids = junk_categories
            .iter()
            .map(|category| category.category_id)
            .collect::<HashSet<_>>();

        let mut funnel = BucketAFunnel::default();
        let mut bucket_a = BTreeSet::new();
        let mut bucket_b = BTreeSet::new();
        let mut bucket_d = BTreeSet::new();
        let mut junk_counts_by_category = BTreeMap::new();

        let universe = data.marketable_item_types().collect::<Vec<_>>();
        funnel.universe = universe.len();
        funnel.universe_meta_group_three_plus = universe
            .iter()
            .filter(|item| {
                data.meta_group_id(item.type_id)
                    .is_some_and(|value| value >= 3)
            })
            .count();

        for item in &universe {
            let type_id = item.type_id;
            let category_id = item.category_id;
            let excluded = category_id.is_some_and(|id| excluded_categories.contains(&id));

            // --- Bucket A, one rule at a time so the funnel can report where each bites.
            if let Some(producer) = producers.get(type_id) {
                funnel.manufacturable += 1;
                let mut survives = true;
                if excluded {
                    survives = false;
                }
                if survives {
                    funnel.after_category_exclusions += 1;
                    if invention_lineage.contains(&producer.blueprint_type_id) {
                        survives = false;
                    }
                }
                if survives {
                    funnel.after_invention_lineage += 1;
                    if data.tech_level(type_id) >= 2 {
                        survives = false;
                    }
                }
                if survives {
                    funnel.after_tech_level += 1;
                    if data
                        .meta_group_id(type_id)
                        .is_some_and(|meta_group| meta_group >= 3)
                    {
                        survives = false;
                    }
                }
                if survives {
                    funnel.after_meta_group += 1;
                    bucket_a.insert(type_id);
                }
            }

            // --- Bucket B: raw materials. No tech-level or metaGroup filter by design;
            // the overlap with A (~183 manufacturable materials) is expected and fine.
            if !excluded {
                let is_material = category_id.is_some_and(|id| material_categories.contains(&id))
                    || (category_id == Some(GAS_CATEGORY_ID)
                        && item
                            .group_name
                            .as_deref()
                            .is_some_and(|group| GAS_GROUP_NAMES.contains(&group)));
                if is_material {
                    bucket_b.insert(type_id);
                }
            }

            // --- Bucket D: the T1, metaGroup <= 1 leftovers in the enabled categories.
            if !excluded
                && !bucket_a.contains(&type_id)
                && !bucket_b.contains(&type_id)
                && category_id.is_some_and(|id| junk_category_ids.contains(&id))
                && !is_invention_lineage(&producers, &invention_lineage, type_id)
                && data.tech_level(type_id) < 2
                && data
                    .meta_group_id(type_id)
                    .is_none_or(|meta_group| meta_group <= 1)
            {
                bucket_d.insert(type_id);
                if let Some(id) = category_id {
                    *junk_counts_by_category.entry(id).or_insert(0) += 1;
                }
            }
        }

        Ok(Self {
            universe_size: universe.len(),
            producers,
            invention_lineage,
            bucket_a,
            bucket_b,
            bucket_d,
            junk_categories,
            junk_counts_by_category,
            junk_enabled: junk.enabled,
            funnel,
        })
    }

    pub fn flags(&self, type_id: u32) -> BucketFlags {
        BucketFlags {
            manufacturable_t1: self.bucket_a.contains(&type_id),
            material: self.bucket_b.contains(&type_id),
            junk_candidate: self.bucket_d.contains(&type_id),
        }
    }

    /// Resolves one canonical-market owner while validating every matching predicate. The
    /// order documents intentional precedence; overlaps outside [`approved_role_overlap`]
    /// are hard failures rather than silent first-match wins.
    pub fn market_assignment_checked(
        &self,
        data: &StaticData,
        type_id: u32,
        bpo_npc_eligible: bool,
        lp_reward: bool,
        bridge_group_ids: &BTreeSet<u32>,
        bridge_type_ids: &BTreeSet<u32>,
    ) -> Result<Option<MarketAssignment>> {
        self.market_assignment_checked_with_progression(
            data,
            type_id,
            bpo_npc_eligible,
            lp_reward,
            bridge_group_ids,
            bridge_type_ids,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
    }

    pub fn market_assignment_checked_with_progression(
        &self,
        data: &StaticData,
        type_id: u32,
        bpo_npc_eligible: bool,
        lp_reward: bool,
        bridge_group_ids: &BTreeSet<u32>,
        bridge_type_ids: &BTreeSet<u32>,
        progression_npc_group_ids: &BTreeSet<u32>,
        progression_npc_type_ids: &BTreeSet<u32>,
    ) -> Result<Option<MarketAssignment>> {
        self.market_assignment_checked_with_supply_ladder(
            data,
            type_id,
            bpo_npc_eligible,
            lp_reward,
            bridge_group_ids,
            bridge_type_ids,
            progression_npc_group_ids,
            progression_npc_type_ids,
            false,
        )
    }

    pub fn market_assignment_checked_with_supply_ladder(
        &self,
        data: &StaticData,
        type_id: u32,
        bpo_npc_eligible: bool,
        lp_reward: bool,
        bridge_group_ids: &BTreeSet<u32>,
        bridge_type_ids: &BTreeSet<u32>,
        progression_npc_group_ids: &BTreeSet<u32>,
        progression_npc_type_ids: &BTreeSet<u32>,
        moon_mining_available: bool,
    ) -> Result<Option<MarketAssignment>> {
        let Some(item) = data.item_type(type_id) else {
            return Ok(None);
        };
        if !item.is_marketable() {
            return Ok(None);
        }

        let mut matches = Vec::<RoleMatch>::new();
        let mut add = |rule, role, profile| {
            matches.push(RoleMatch {
                rule,
                assignment: MarketAssignment::new(role, profile),
            })
        };

        let reaction_formula = self.is_published_reaction_formula(data, type_id);
        if item.category_id == Some(BLUEPRINT_CATEGORY_ID)
            && ((bpo_npc_eligible && self.is_t1_bpo(data, type_id)) || reaction_formula)
        {
            add(
                if reaction_formula {
                    "published-reaction-formula"
                } else {
                    "npc-seeded-t1-bpo"
                },
                MarketRole::Bpo,
                if reaction_formula {
                    PricingProfile::ReactionFormula
                } else {
                    PricingProfile::Bpo
                },
            );
        }
        if item.category_id == Some(SKILL_CATEGORY_ID) {
            add(
                "skill-category",
                MarketRole::Skillbook,
                PricingProfile::Skillbook,
            );
        }
        if item.group_id == Some(COMMAND_CENTER_GROUP_ID) {
            add(
                "command-center-group",
                MarketRole::CommandCenter,
                PricingProfile::CommandCenter,
            );
        }
        let progression_npc_match = progression_npc_type_ids.contains(&type_id)
            || item
                .group_id
                .is_some_and(|group_id| progression_npc_group_ids.contains(&group_id));
        if progression_npc_match {
            add(
                "progression-npc-allowlist",
                MarketRole::ProgressionNpc,
                PricingProfile::ProgressionNpc,
            );
        }
        let production_intermediate_match = is_production_intermediate_type(type_id);
        if production_intermediate_match {
            add(
                "production-intermediate-allowlist",
                MarketRole::ProductionIntermediate,
                PricingProfile::ProductionIntermediate,
            );
        }
        let planetary_industry_match =
            PLANETARY_INDUSTRY_CATEGORY_IDS.contains(&item.category_id.unwrap_or_default());
        if planetary_industry_match {
            add(
                "planetary-industry-commodity",
                MarketRole::Bridge,
                PricingProfile::PlanetaryIndustry,
            );
        }
        let moon_resource = item
            .group_id
            .is_some_and(|group_id| MOON_RESOURCE_GROUP_IDS.contains(&group_id));
        let bridge_match = (bridge_type_ids.contains(&type_id)
            || item
                .group_id
                .is_some_and(|group_id| bridge_group_ids.contains(&group_id)))
            && !(moon_mining_available && moon_resource);
        if bridge_match {
            add(
                "bridge-allowlist",
                MarketRole::Bridge,
                PricingProfile::Bridge,
            );
        }

        let tech_level = data.tech_level(type_id);
        let finished_tech_three = is_finished_good_category(item.category_id) && tech_level >= 3;
        if finished_tech_three
            && !bridge_match
            && !progression_npc_match
            && !production_intermediate_match
        {
            return Ok(None);
        }

        let meta_group = data.meta_group_id(type_id);
        if self.is_special_reward_placeholder_hull(data, type_id) {
            add(
                "special-reward-placeholder-hull",
                MarketRole::Rare,
                PricingProfile::RareFactionLp,
            );
        }
        if meta_group == Some(5) {
            add(
                "meta-group-officer",
                MarketRole::Rare,
                PricingProfile::RareOfficer,
            );
        }
        if meta_group == Some(6) {
            add(
                "meta-group-deadspace",
                MarketRole::Rare,
                PricingProfile::RareDeadspaceDed,
            );
        }
        if lp_reward {
            add("lp-reward", MarketRole::Rare, PricingProfile::RareFactionLp);
        }
        if matches!(meta_group, Some(3) | Some(4)) {
            add(
                "meta-group-faction",
                MarketRole::Rare,
                PricingProfile::RareFactionLp,
            );
        }
        // Category 17 is the legacy authenticity/trade-good bucket. Bridge inputs and LP
        // rewards were handled above; the generic remainder does not justify synthetic stock
        // or a buyback in the canonical solo market.
        if is_finished_good_category(item.category_id)
            && tech_level < 3
            && (tech_level == 2
                || meta_group == Some(2)
                || is_invention_lineage(&self.producers, &self.invention_lineage, type_id))
        {
            add("finished-t2", MarketRole::Rare, PricingProfile::RareT2);
        }

        if moon_mining_available && moon_resource {
            add(
                "mineable-moon-resource",
                MarketRole::Core,
                core_profile(data, item),
            );
        } else if item.category_id != Some(COMMODITY_CATEGORY_ID)
            && self.flags(type_id).is_classified()
        {
            add("core-bucket", MarketRole::Core, core_profile(data, item));
        }

        let Some(selected) = matches.first().copied() else {
            return Ok(None);
        };
        let unexpected = matches
            .iter()
            .skip(1)
            .copied()
            .filter(|candidate| !approved_role_overlap(selected, *candidate))
            .collect::<Vec<_>>();
        if !unexpected.is_empty() {
            let all = matches
                .iter()
                .map(|candidate| {
                    format!(
                        "{}={}/{}",
                        candidate.rule,
                        candidate.assignment.role.key(),
                        candidate.assignment.profile.key()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "type {type_id} ({}) matches unexpected multiple canonical role predicates: {all}. Add an explicit precedence rule or correct the role configuration before SQLite writing",
                item.name
            );
        }
        Ok(Some(selected.assignment))
    }

    /// The published SDE contains a narrow reward-hull placeholder shape: a nonmarketable,
    /// non-invented blueprint manufactures one marketable ship from exactly one Tritanium.
    /// Such a definition proves neither reusable recipe access nor an economic production
    /// cost, so it must never grant ordinary Core Sell supply.
    pub fn is_special_reward_placeholder_hull(&self, data: &StaticData, type_id: u32) -> bool {
        let Some(item) = data.item_type(type_id) else {
            return false;
        };
        if !item.is_marketable() || item.category_id != Some(SHIP_CATEGORY_ID) {
            return false;
        }
        let Some(producer) = self.producers.get(type_id) else {
            return false;
        };
        if producer.activity != ProducerActivity::Manufacturing
            || !producer.blueprint_published
            || producer.per_run != 1
            || producer.materials.len() != 1
            || producer.materials[0].type_id != 34
            || producer.materials[0].quantity != 1
            || self.invention_lineage.contains(&producer.blueprint_type_id)
        {
            return false;
        }
        data.item_type(producer.blueprint_type_id)
            .is_some_and(|blueprint| !blueprint.is_marketable())
    }

    fn is_t1_bpo(&self, data: &StaticData, type_id: u32) -> bool {
        let Some(blueprint) = data
            .blueprints
            .iter()
            .find(|blueprint| blueprint.blueprint_type_id == type_id)
        else {
            return false;
        };
        if !blueprint.published
            || blueprint.activities.manufacturing.is_none()
            || self.invention_lineage.contains(&type_id)
        {
            return false;
        }
        let Some(activity) = blueprint.activities.manufacturing.as_ref() else {
            return false;
        };
        !activity.products.is_empty()
            && activity.products.iter().all(|product| {
                data.tech_level(product.type_id) < 2
                    && data
                        .meta_group_id(product.type_id)
                        .is_none_or(|meta_group| meta_group <= 1)
            })
    }

    /// Reusable SDE blueprint evidence; NPC catalog eligibility is checked separately.
    pub fn is_approved_reusable_bpo(&self, data: &StaticData, type_id: u32) -> bool {
        self.is_t1_bpo(data, type_id)
    }

    /// Static reaction activity is authoritative proof that this marketable category-9 item
    /// is a published reusable Reaction Formula. This deliberately does not admit arbitrary
    /// manufacturing blueprints or invention-produced BPCs.
    pub fn is_published_reaction_formula(&self, data: &StaticData, type_id: u32) -> bool {
        let Some(item) = data.item_type(type_id) else {
            return false;
        };
        item.is_marketable()
            && item.category_id == Some(BLUEPRINT_CATEGORY_ID)
            && data.blueprints.iter().any(|blueprint| {
                blueprint.blueprint_type_id == type_id
                    && blueprint.published
                    && blueprint.activities.reaction.is_some()
            })
    }

    /// Types in both A and B — manufacturable materials such as fuel blocks.
    pub fn overlap_ab(&self) -> BTreeSet<u32> {
        self.bucket_a
            .intersection(&self.bucket_b)
            .copied()
            .collect()
    }

    /// The index seed set.
    pub fn union_ab(&self) -> BTreeSet<u32> {
        self.bucket_a.union(&self.bucket_b).copied().collect()
    }

    /// Everything that *may* get a seed row: A ∪ B ∪ D, each type counted once. D drops out
    /// entirely when junk seeding is disabled.
    ///
    /// D members here are **candidates**: whether each one is actually seeded depends on the
    /// price manifest holding a capture for it — see [`Classification::junk_seedability`].
    pub fn seeded_types(&self) -> BTreeSet<u32> {
        let mut seeded = self.union_ab();
        if self.junk_enabled {
            seeded.extend(self.bucket_d.iter().copied());
        }
        seeded
    }

    /// The seeded set broken down by membership label (`A`, `A+B`, `B`, `D`). The parts
    /// sum to `seeded_types().len()`, which is the point: an A∩B type appears once, under
    /// `A+B`, never twice.
    pub fn seeded_composition(&self) -> Vec<(&'static str, usize)> {
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        for type_id in self.seeded_types() {
            let flags = self.flags(type_id);
            debug_assert!(flags.is_classified(), "seeded type {type_id} has no bucket");
            *counts.entry(flags.label()).or_insert(0) += 1;
        }
        ["A", "A+B", "B", "D"]
            .into_iter()
            .map(|label| (label, counts.get(label).copied().unwrap_or(0)))
            .collect()
    }

    /// Splits bucket D by whether the price manifest holds a **captured** price for each
    /// candidate (plan §2.4, §3.3).
    ///
    /// This is the whole bucket-D pricing rule, in one place:
    /// `cost(type) := manifest.fetchedPrice(type)` and nothing else. A D type is seeded
    /// **iff** such a capture exists. There is deliberately no ladder here — no computed
    /// cost, no `basePrice` fallback — because a D price that did not come from the real
    /// market is an invented price, and §2.4 exists to avoid inventing prices for loot.
    /// A candidate without a capture is dropped, which is the designed outcome and not an
    /// error condition.
    ///
    /// `has_capture` is a predicate rather than the manifest itself so this rule stays
    /// testable without one; production callers pass `|id| manifest.has_capture(id)`.
    pub fn junk_seedability(&self, has_capture: impl Fn(u32) -> bool) -> JunkSeedability {
        let mut seedability = JunkSeedability::default();
        if !self.junk_enabled {
            // The whole bucket is off; nothing is seedable and nothing was "dropped" for
            // want of a price.
            seedability.junk_disabled = true;
            return seedability;
        }
        for type_id in &self.bucket_d {
            if has_capture(*type_id) {
                seedability.seedable.insert(*type_id);
            } else {
                seedability.dropped.insert(*type_id);
            }
        }
        seedability
    }

    /// Bucket A types whose producer is a reaction — proves the reaction scan works.
    pub fn reaction_produced_in_a(&self) -> usize {
        self.bucket_a
            .iter()
            .filter(|type_id| {
                self.producers
                    .get(**type_id)
                    .is_some_and(|producer| producer.activity == ProducerActivity::Reaction)
            })
            .count()
    }

    /// Bucket A types whose blueprint yields more than one unit per run. The cost map
    /// must divide by `per_run` for these (plan §3.1).
    pub fn multi_output_in_a(&self) -> usize {
        self.bucket_a
            .iter()
            .filter(|type_id| {
                self.producers
                    .get(**type_id)
                    .is_some_and(|producer| producer.per_run > 1)
            })
            .count()
    }

    /// Distinct input material types across bucket A's blueprints, and how many of them
    /// sit outside A ∪ B — a preview of the cost map's leaf surface (plan §3.2/§3.3).
    pub fn bucket_a_material_surface(&self) -> (usize, usize) {
        let union = self.union_ab();
        let mut materials = HashSet::new();
        for type_id in &self.bucket_a {
            let Some(producer) = self.producers.get(*type_id) else {
                continue;
            };
            for material in &producer.materials {
                materials.insert(material.type_id);
            }
        }
        let outside = materials
            .iter()
            .filter(|type_id| !union.contains(type_id))
            .count();
        (materials.len(), outside)
    }

    /// Bucket A grouped by category, most populous first, with a couple of sample names.
    pub fn bucket_a_by_category<'a>(
        &self,
        data: &'a StaticData,
    ) -> Vec<(Option<u32>, usize, Vec<&'a str>)> {
        let mut by_category: BTreeMap<Option<u32>, (usize, Vec<&str>)> = BTreeMap::new();
        for type_id in &self.bucket_a {
            let Some(item) = data.item_type(*type_id) else {
                continue;
            };
            let entry = by_category
                .entry(item.category_id)
                .or_insert((0, Vec::new()));
            entry.0 += 1;
            if entry.1.len() < 2 {
                entry.1.push(item.name.as_str());
            }
        }
        let mut rows = by_category
            .into_iter()
            .map(|(category_id, (count, samples))| (category_id, count, samples))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        rows
    }
}

fn is_finished_good_category(category_id: Option<u32>) -> bool {
    matches!(
        category_id,
        Some(6 | 7 | 8 | 18 | 20 | 22 | 23 | 32 | 65 | 66 | 87)
    )
}

fn approved_role_overlap(selected: RoleMatch, candidate: RoleMatch) -> bool {
    if selected.assignment == candidate.assignment {
        return true;
    }
    if candidate.assignment.role == MarketRole::Core {
        return true;
    }
    if matches!(
        selected.assignment.role,
        MarketRole::Bpo
            | MarketRole::Bridge
            | MarketRole::ProgressionNpc
            | MarketRole::ProductionIntermediate
            | MarketRole::Skillbook
            | MarketRole::CommandCenter
    ) && candidate.rule == "lp-reward"
    {
        return true;
    }
    selected.assignment.role == MarketRole::Rare && candidate.assignment.role == MarketRole::Rare
}

fn core_profile(data: &StaticData, item: &crate::staticdata::ItemTypeRecord) -> PricingProfile {
    let group = item.group_name.as_deref().unwrap_or_default();
    let group_lower = group.to_ascii_lowercase();
    if item.category_id == Some(25) {
        if group_lower.contains("ice") {
            return PricingProfile::GasIceSalvage;
        }
        if data.is_authoritative_compressed_type(item.type_id) {
            return PricingProfile::CompressedOre;
        }
        return PricingProfile::RawOre;
    }
    if item.group_id == Some(MINERAL_GROUP_ID) {
        return PricingProfile::Minerals;
    }
    if item.group_id == Some(NATURAL_SALVAGE_GROUP_ID)
        || matches!(group, "Harvestable Cloud" | "Compressed Gas")
        || group_lower.contains("ice product")
    {
        return PricingProfile::GasIceSalvage;
    }
    if item.category_id == Some(SHIP_CATEGORY_ID) {
        if group_lower.contains("battlecruiser") {
            return PricingProfile::Battlecruiser;
        }
        if group_lower.contains("battleship") {
            return PricingProfile::Battleship;
        }
        if group_lower.contains("destroyer") {
            return PricingProfile::Destroyer;
        }
        if group_lower.contains("cruiser") {
            return PricingProfile::Cruiser;
        }
        if matches!(item.group_id, Some(513 | 902 | 941)) || item.name == "Orca" {
            return PricingProfile::OrcaFreighterStructure;
        }
    }
    if item.category_id == Some(65) {
        return PricingProfile::OrcaFreighterStructure;
    }
    if matches!(item.group_id, Some(334 | 536 | 873 | 913)) {
        return PricingProfile::CapitalStructureComponent;
    }
    if matches!(
        item.category_id,
        Some(MODULE_CATEGORY_ID) | Some(CHARGE_CATEGORY_ID) | Some(66)
    ) || group_lower.contains("rig")
    {
        return PricingProfile::T1ModuleAmmoRig;
    }
    PricingProfile::CoreGeneral
}

/// True when a type's T2/T3 lineage is provable: either the blueprint that makes it is an
/// invention product, or the type *is* such a blueprint. The second arm is belt and
/// braces — category 9 is excluded before this runs — but it keeps the rule honest if the
/// category list ever changes.
fn is_invention_lineage(
    producers: &ProducerIndex,
    invention_lineage: &HashSet<u32>,
    type_id: u32,
) -> bool {
    if invention_lineage.contains(&type_id) {
        return true;
    }
    producers
        .get(type_id)
        .is_some_and(|producer| invention_lineage.contains(&producer.blueprint_type_id))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{JunkSeedConfig, SeederConfig};
    use crate::staticdata::{
        BlueprintActivities, BlueprintActivity, BlueprintDefinition, DogmaProjection,
        InventionActivity, InventionProductEntry, ItemTypeRecord, MaterialEntry, ProductEntry,
    };

    const CATEGORY_MODULE: u32 = 7;
    const CATEGORY_MATERIAL: u32 = 4;
    const CATEGORY_ASTEROID: u32 = 25;
    const CATEGORY_SKILL: u32 = 16;
    const CATEGORY_BLUEPRINT: u32 = 9;
    const CATEGORY_DECRYPTOR: u32 = 35;
    /// The category the four unpublished "decryptor"-named test rows live in. Not in any
    /// table here, and deliberately so.
    const CATEGORY_SALVAGE_DECRYPTOR: u32 = 350_001;

    fn item(type_id: u32, category_id: u32, name: &str) -> ItemTypeRecord {
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
            base_price: Some(1.0),
            market_group_id: Some(1),
            icon_id: None,
            sound_id: None,
            graphic_id: None,
            radius: None,
            published: true,
        }
    }

    fn blueprint(blueprint_type_id: u32, activities: BlueprintActivities) -> BlueprintDefinition {
        BlueprintDefinition {
            blueprint_type_id,
            blueprint_name: format!("Blueprint {blueprint_type_id}"),
            // Deliberately 0 for every fixture blueprint: the real reaction rows carry 0
            // here, and the classifier must never depend on this field.
            product_type_id: 0,
            product_name: String::new(),
            max_production_limit: None,
            published: true,
            activities,
        }
    }

    fn activity(product_type_id: u32, per_run: i64, materials: &[(u32, i64)]) -> BlueprintActivity {
        BlueprintActivity {
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
        }
    }

    fn manufacturing(
        blueprint_type_id: u32,
        product_type_id: u32,
        per_run: i64,
    ) -> BlueprintDefinition {
        blueprint(
            blueprint_type_id,
            BlueprintActivities {
                manufacturing: Some(activity(product_type_id, per_run, &[(34, 100)])),
                ..BlueprintActivities::default()
            },
        )
    }

    fn reaction(blueprint_type_id: u32, product_type_id: u32, per_run: i64) -> BlueprintDefinition {
        blueprint(
            blueprint_type_id,
            BlueprintActivities {
                reaction: Some(activity(product_type_id, per_run, &[(16634, 100)])),
                ..BlueprintActivities::default()
            },
        )
    }

    /// A T1 blueprint whose invention activity yields `invented_blueprint_type_id` — the
    /// shape that puts a blueprint into the invention-lineage set.
    fn inventor(blueprint_type_id: u32, invented_blueprint_type_id: u32) -> BlueprintDefinition {
        blueprint(
            blueprint_type_id,
            BlueprintActivities {
                invention: Some(InventionActivity {
                    materials: Vec::new(),
                    products: vec![InventionProductEntry {
                        type_id: invented_blueprint_type_id,
                        quantity: 1,
                        probability: Some(0.3),
                    }],
                    skills: Vec::new(),
                    time: Some(100),
                }),
                ..BlueprintActivities::default()
            },
        )
    }

    fn dogma(tech_level: Option<f64>, meta_group_id: Option<i64>) -> DogmaProjection {
        DogmaProjection {
            tech_level,
            meta_group_id,
        }
    }

    fn fixture(
        item_types: Vec<ItemTypeRecord>,
        blueprints: Vec<BlueprintDefinition>,
        dogma: Vec<(u32, DogmaProjection)>,
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
            dogma: dogma.into_iter().collect::<HashMap<_, _>>(),
            compressed_type_ids: BTreeSet::new(),
        }
    }

    fn classify(data: &StaticData) -> Classification {
        Classification::compute(data, &JunkSeedConfig::default()).expect("fixture classifies")
    }

    #[test]
    fn reaction_only_blueprint_products_are_found_despite_top_level_product_type_id_zero() {
        // Regression for the trap that motivates the whole producer index: reaction
        // blueprints carry productTypeID 0, so the precomputed
        // blueprintTypeIDsByProductTypeID index misses all 120 of them.
        let data = fixture(
            vec![item(16670, CATEGORY_MATERIAL, "Crystalline Carbonide")],
            vec![reaction(46168, 16670, 10_000)],
            Vec::new(),
        );

        let result = classify(&data);
        let producer = result
            .producers
            .get(16670)
            .expect("the reaction product must be indexed");
        assert_eq!(producer.blueprint_type_id, 46168);
        assert_eq!(producer.activity, ProducerActivity::Reaction);
        assert_eq!(producer.per_run, 10_000);
        assert_eq!(producer.materials.len(), 1);
        assert!(result.bucket_a.contains(&16670));
        assert_eq!(result.reaction_produced_in_a(), 1);
        assert_eq!(result.multi_output_in_a(), 1);
    }

    #[test]
    fn invention_lineage_product_is_excluded_from_bucket_a() {
        // 2488 is built by blueprint 2489, which invention yields → the product is T2.
        let data = fixture(
            vec![
                item(2488, CATEGORY_MODULE, "Warp Disruptor II"),
                item(2487, CATEGORY_MODULE, "Warp Disruptor I"),
            ],
            vec![
                manufacturing(2489, 2488, 1),
                manufacturing(2486, 2487, 1),
                inventor(2486, 2489),
            ],
            Vec::new(),
        );

        let result = classify(&data);
        assert!(result.invention_lineage.contains(&2489));
        assert!(!result.bucket_a.contains(&2488));
        assert!(result.bucket_a.contains(&2487));
        assert_eq!(result.funnel.removed_by_invention_lineage(), 1);
        // Nor may it fall through into the junk bucket.
        assert!(!result.bucket_d.contains(&2488));
    }

    #[test]
    fn tech_level_two_type_is_excluded_from_bucket_a() {
        // Dogma-only T2 signal: no invention lineage recorded, attribute 422 = 2.
        let data = fixture(
            vec![item(3001, CATEGORY_MODULE, "Something II")],
            vec![manufacturing(3002, 3001, 1)],
            vec![(3001, dogma(Some(2.0), Some(2)))],
        );

        let result = classify(&data);
        assert!(!result.bucket_a.contains(&3001));
        assert_eq!(result.funnel.removed_by_tech_level(), 1);
        assert!(!result.bucket_d.contains(&3001));
    }

    #[test]
    fn meta_group_four_faction_type_with_a_t1_blueprint_is_excluded_from_bucket_a_714_item_leak() {
        // The 714-item leak class from plan §2.1 rule 4: an SDE blueprint, Tech Level 1
        // (attribute 422 absent), and only metaGroupID 4 marks it as faction gear.
        let data = fixture(
            vec![
                item(4005, CATEGORY_MODULE, "Domination Warp Disruptor"),
                item(4006, CATEGORY_MODULE, "Warp Disruptor I"),
            ],
            vec![manufacturing(4105, 4005, 1), manufacturing(4106, 4006, 1)],
            vec![(4005, dogma(None, Some(4))), (4006, dogma(None, Some(1)))],
        );

        let result = classify(&data);
        assert_eq!(data.tech_level(4005), 1, "the leak class is Tech Level 1");
        assert!(!result.bucket_a.contains(&4005));
        assert!(result.bucket_a.contains(&4006));
        assert_eq!(result.funnel.removed_by_meta_group(), 1);
        // metaGroup 4 also fails bucket D's `<= 1` rule, so it is seeded nowhere.
        assert!(!result.bucket_d.contains(&4005));
        assert!(!result.flags(4005).is_classified());
    }

    #[test]
    fn skills_and_blueprints_never_land_in_any_bucket() {
        let data = fixture(
            vec![
                item(3300, CATEGORY_SKILL, "Gunnery"),
                item(1234, CATEGORY_BLUEPRINT, "Rifter Blueprint"),
                item(587, CATEGORY_MODULE, "Rifter"),
            ],
            // The blueprint type is itself manufacturable (BPO copies aside, the fixture
            // just needs it to reach the funnel) and produces the ship.
            vec![manufacturing(1234, 587, 1), manufacturing(9999, 1234, 1)],
            Vec::new(),
        );

        let result = classify(&data);
        for type_id in [3300_u32, 1234] {
            assert!(
                !result.flags(type_id).is_classified(),
                "type {type_id} must not be classified"
            );
        }
        assert!(result.bucket_a.contains(&587));
        assert_eq!(result.funnel.removed_by_category_exclusions(), 1);
    }

    #[test]
    fn ore_lands_in_b_and_a_fuel_block_lands_in_both_a_and_b_counted_once() {
        let data = fixture(
            vec![
                item(1230, CATEGORY_ASTEROID, "Veldspar"),
                item(4051, CATEGORY_MATERIAL, "Nitrogen Fuel Block"),
                item(587, CATEGORY_MODULE, "Rifter"),
            ],
            vec![manufacturing(4312, 4051, 40), manufacturing(688, 587, 1)],
            Vec::new(),
        );

        let result = classify(&data);
        assert_eq!(result.bucket_b, BTreeSet::from([1230, 4051]));
        assert_eq!(result.bucket_a, BTreeSet::from([4051, 587]));
        assert_eq!(result.overlap_ab(), BTreeSet::from([4051]));
        assert_eq!(result.flags(4051).label(), "A+B");
        assert_eq!(result.flags(1230).label(), "B");

        // |A| + |B| = 4, but the union — the number of seeded types — is 3.
        assert_eq!(result.bucket_a.len() + result.bucket_b.len(), 4);
        assert_eq!(result.union_ab().len(), 3);
        assert_eq!(result.seeded_types().len(), 3);
        // Ore carries no blueprint and is never filtered on tech level or metaGroup.
        assert!(result.producers.get(1230).is_none());
    }

    #[test]
    fn junk_candidates_exclude_a_and_b_members_and_respect_the_enabled_flag() {
        let data = fixture(
            vec![
                item(1230, CATEGORY_ASTEROID, "Veldspar"),
                item(587, CATEGORY_MODULE, "Rifter"),
                item(5001, CATEGORY_MODULE, "Some Named Module"),
            ],
            vec![manufacturing(688, 587, 1)],
            Vec::new(),
        );

        let result = classify(&data);
        assert_eq!(result.bucket_d, BTreeSet::from([5001]));
        assert_eq!(
            result.junk_counts_by_category.get(&CATEGORY_MODULE),
            Some(&1)
        );
        assert_eq!(result.seeded_types().len(), 3);

        let disabled = JunkSeedConfig {
            enabled: false,
            ..JunkSeedConfig::default()
        };
        let result = Classification::compute(&data, &disabled).expect("fixture classifies");
        assert_eq!(result.bucket_d.len(), 1, "candidates are still reported");
        assert_eq!(result.seeded_types().len(), 2, "but none are seeded");
    }

    #[test]
    fn decryptors_resolve_to_category_thirty_five() {
        let resolved = resolve_junk_categories(&["decryptors".to_string()])
            .expect("decryptors is a legal category name");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].config_name, "decryptors");
        assert_eq!(resolved[0].category_id, CATEGORY_DECRYPTOR);
        assert_eq!(resolved[0].category_name, "Decryptor");

        // The default config enables it, so a stock build seeds the bucket.
        assert!(
            JunkSeedConfig::default()
                .categories
                .iter()
                .any(|name| name == "decryptors"),
            "the shipped default must enable the category the table now knows"
        );
    }

    #[test]
    fn published_decryptors_land_in_bucket_d_and_the_unpublished_test_rows_do_not() {
        // 34201 is the real shape: published, marketGroupID 1873, no dogma at all (so Tech 1
        // and no metaGroup), no blueprint. 367777 is one of the four category-350001 test
        // rows: unpublished, no market group, and outside every table here.
        let mut salvage_junk = item(367_777, CATEGORY_SALVAGE_DECRYPTOR, "Basic Decryptor2");
        salvage_junk.published = false;
        salvage_junk.market_group_id = None;

        let data = fixture(
            vec![
                item(34_201, CATEGORY_DECRYPTOR, "Accelerant Decryptor"),
                item(34_207, CATEGORY_DECRYPTOR, "Optimized Attainment Decryptor"),
                salvage_junk,
            ],
            Vec::new(),
            Vec::new(),
        );

        let result = classify(&data);
        assert_eq!(
            result.universe_size, 2,
            "only the two published, market-grouped decryptors"
        );
        assert_eq!(result.bucket_d, BTreeSet::from([34_201, 34_207]));
        assert_eq!(
            result.junk_counts_by_category.get(&CATEGORY_DECRYPTOR),
            Some(&2)
        );
        assert_eq!(result.flags(34_201).label(), "D");
        // Bucket D is capture-only, and every real decryptor has basePrice 0, so nothing here
        // can be seeded off static data alone.
        assert!(result.bucket_a.is_empty());
        assert!(result.bucket_b.is_empty());

        // The test junk is in neither the universe nor any bucket, whatever its name says.
        assert!(!result.flags(367_777).is_classified());
        assert!(!result.bucket_d.contains(&367_777));

        // And category 350001 is not a legal config word, so it cannot be opted into either.
        resolve_junk_categories(&["salvage decryptors".to_string()])
            .expect_err("only the table's own names resolve");
    }

    #[test]
    fn junk_seedability_needs_a_capture_and_never_falls_back_to_base_price() {
        // Every fixture item carries basePrice 1.0, so if bucket D had any fallback at all
        // this would seed both candidates. §2.4 says only the captured one is seeded.
        let data = fixture(
            vec![
                item(5001, CATEGORY_MODULE, "Captured Named Module"),
                item(5002, CATEGORY_MODULE, "Uncaptured Named Module"),
            ],
            Vec::new(),
            Vec::new(),
        );
        let result = classify(&data);
        assert_eq!(result.bucket_d, BTreeSet::from([5001, 5002]));
        assert!(
            data.item_type(5002)
                .and_then(|item| item.base_price)
                .unwrap()
                > 0.0,
            "the uncaptured candidate does have a basePrice — that is the trap"
        );

        let seedability = result.junk_seedability(|type_id| type_id == 5001);
        assert_eq!(seedability.seedable, BTreeSet::from([5001]));
        assert_eq!(seedability.dropped, BTreeSet::from([5002]));
        assert!(!seedability.junk_disabled);

        // With no captures at all, the whole bucket is dropped — the designed outcome, not
        // an error.
        let none = result.junk_seedability(|_| false);
        assert!(none.seedable.is_empty());
        assert_eq!(none.dropped.len(), 2);

        let disabled = Classification::compute(
            &data,
            &JunkSeedConfig {
                enabled: false,
                ..JunkSeedConfig::default()
            },
        )
        .expect("fixture classifies");
        let off = disabled.junk_seedability(|_| true);
        assert!(off.junk_disabled);
        assert!(off.seedable.is_empty());
        assert!(off.dropped.is_empty());
    }

    #[test]
    fn unknown_junk_category_name_is_rejected() {
        let error = resolve_junk_categories(&["modules".to_string(), "wormholes".to_string()])
            .expect_err("an unknown category name must be a hard error");
        let message = format!("{error:#}");
        assert!(message.contains("wormholes"), "message: {message}");
        assert!(message.contains("modules (7)"), "message: {message}");
        assert!(message.contains("drones (18)"), "message: {message}");
        assert!(message.contains("decryptors (35)"), "message: {message}");

        // …and the same rejection happens at config-parse time, not at classify time.
        let error = SeederConfig::from_toml_str("[junk_seed]\ncategories = [\"wormholes\"]\n")
            .expect_err("config must reject the unknown category");
        assert!(format!("{error:#}").contains("wormholes"));
    }

    #[test]
    fn every_documented_junk_category_name_resolves() {
        let names = JUNK_CATEGORY_TABLE
            .iter()
            .map(|(name, _, _)| (*name).to_string())
            .collect::<Vec<_>>();
        let resolved = resolve_junk_categories(&names).expect("the whole table is legal");
        assert_eq!(resolved.len(), JUNK_CATEGORY_TABLE.len());
        assert_eq!(
            resolved.iter().map(|c| c.category_id).collect::<Vec<_>>(),
            vec![7, 17, 8, 18, 35]
        );

        // Duplicates collapse rather than double-counting bucket D.
        let deduped = resolve_junk_categories(&["modules".to_string(), "modules".to_string()])
            .expect("duplicates are legal");
        assert_eq!(deduped.len(), 1);
    }

    #[test]
    fn unpublished_and_market_groupless_types_are_outside_the_universe() {
        let mut unpublished = item(6001, CATEGORY_MODULE, "Unpublished Module");
        unpublished.published = false;
        let mut no_market_group = item(6002, CATEGORY_MODULE, "Non-market Module");
        no_market_group.market_group_id = None;

        let data = fixture(
            vec![
                unpublished,
                no_market_group,
                item(587, CATEGORY_MODULE, "Rifter"),
            ],
            vec![
                manufacturing(6101, 6001, 1),
                manufacturing(6102, 6002, 1),
                manufacturing(688, 587, 1),
            ],
            Vec::new(),
        );

        let result = classify(&data);
        assert_eq!(result.universe_size, 1);
        assert_eq!(result.bucket_a, BTreeSet::from([587]));
        assert!(!result.flags(6001).is_classified());
        assert!(!result.flags(6002).is_classified());
    }

    #[test]
    fn two_published_blueprints_for_one_product_keep_the_first_and_are_counted() {
        // Both sides published, so rule 1 cannot separate them and file order decides.
        // This is the determinism guarantee: the published tie-break must not reorder
        // collisions that it has no opinion about.
        let data = fixture(
            vec![item(7001, CATEGORY_MATERIAL, "Contested Product")],
            vec![manufacturing(7100, 7001, 1), reaction(7200, 7001, 5)],
            Vec::new(),
        );

        let result = classify(&data);
        let producer = result.producers.get(7001).expect("indexed");
        assert!(producer.blueprint_published);
        assert_eq!(producer.blueprint_type_id, 7100);
        assert_eq!(producer.per_run, 1);
        assert_eq!(producer.activity, ProducerActivity::Manufacturing);
        assert_eq!(result.producers.collisions().len(), 1);
        let collision = result.producers.collisions()[0];
        assert_eq!(collision.product_type_id, 7001);
        assert_eq!(collision.kept_blueprint_type_id, 7100);
        assert_eq!(collision.dropped_blueprint_type_id, 7200);
        assert_eq!(collision.dropped_activity, ProducerActivity::Reaction);
        assert_eq!(collision.kept_per_run, 1);
        assert_eq!(collision.dropped_per_run, 5);
        assert!(collision.kept_published);
        assert!(collision.dropped_published);
    }

    #[test]
    fn a_published_formula_registered_after_an_unpublished_one_wins_tungsten_carbide_16672() {
        // The real shape of Tungsten Carbide (product 16672) at SDE 3396210:
        //   bp 45732 "Test Reaction Blueprint"        published false,     20 / run
        //   bp 46207 "Tungsten Carbide Reaction Fmla" published true,  10,000 / run
        // 45732 comes first in blueprintDefinitions order, so plain first-wins would keep
        // it and the cost map's EIV / per-run divide would price the material ~500x high.
        // Rule 1 (published beats unpublished) has to override file order here.
        let mut test_formula = reaction(45732, 16672, 20);
        test_formula.published = false;
        test_formula.activities.reaction = Some(activity(16672, 20, &[(16657, 100), (16661, 100)]));
        let real_formula = blueprint(
            46207,
            BlueprintActivities {
                reaction: Some(activity(
                    16672,
                    10_000,
                    &[(4051, 5), (16657, 100), (16661, 100)],
                )),
                ..BlueprintActivities::default()
            },
        );
        let data = fixture(
            vec![item(16672, CATEGORY_MATERIAL, "Tungsten Carbide")],
            vec![test_formula, real_formula],
            Vec::new(),
        );

        let result = classify(&data);
        let producer = result.producers.get(16672).expect("indexed");
        assert_eq!(producer.blueprint_type_id, 46207);
        assert!(producer.blueprint_published);
        assert_eq!(producer.per_run, 10_000);
        // The materials the cost map will read must come from the winner, not the loser.
        assert_eq!(
            producer
                .materials
                .iter()
                .map(|entry| (entry.type_id, entry.quantity))
                .collect::<Vec<_>>(),
            vec![(4051, 5), (16657, 100), (16661, 100)]
        );

        // …and the losing side is still reported, oriented the way the rule chose.
        assert_eq!(result.producers.collisions().len(), 1);
        let collision = result.producers.collisions()[0];
        assert!(collision.is_material_disagreement());
        assert_eq!(collision.kept_blueprint_type_id, 46207);
        assert_eq!(collision.kept_per_run, 10_000);
        assert!(collision.kept_published);
        assert_eq!(collision.dropped_blueprint_type_id, 45732);
        assert_eq!(collision.dropped_per_run, 20);
        assert!(!collision.dropped_published);
    }

    #[test]
    fn a_resolved_tie_break_still_reports_the_collision_in_either_file_order() {
        // Same pair as above with the published blueprint first, where rule 1 and file
        // order agree. The kept side must be identical, and the collision must still be
        // recorded and still flagged `!` — resolving a tie-break never silences it.
        let mut test_formula = reaction(45732, 16672, 20);
        test_formula.published = false;
        let data = fixture(
            vec![item(16672, CATEGORY_MATERIAL, "Tungsten Carbide")],
            vec![reaction(46207, 16672, 10_000), test_formula],
            Vec::new(),
        );

        let result = classify(&data);
        let producer = result.producers.get(16672).expect("indexed");
        assert_eq!(producer.blueprint_type_id, 46207);
        assert_eq!(producer.per_run, 10_000);

        assert_eq!(result.producers.collisions().len(), 1);
        let collision = result.producers.collisions()[0];
        assert!(collision.is_material_disagreement());
        assert_eq!(collision.product_type_id, 16672);
        assert_eq!(collision.kept_blueprint_type_id, 46207);
        assert_eq!(collision.dropped_blueprint_type_id, 45732);
    }

    #[test]
    fn canonical_role_precedence_and_side_policies_are_deterministic() {
        let bpo = item(1_000, CATEGORY_BLUEPRINT, "Test Module Blueprint");
        let product = item(1_001, CATEGORY_MODULE, "Test Module I");

        let mut bridge = item(2_000, CATEGORY_MATERIAL, "Test Datacore");
        bridge.group_id = Some(333);

        let t2 = item(3_000, CATEGORY_MODULE, "Test Module II");
        let faction = item(3_001, CATEGORY_MODULE, "Navy Test Module");
        let deadspace = item(3_002, CATEGORY_MODULE, "Deadspace Test Module");
        let officer = item(3_003, CATEGORY_MODULE, "Officer Test Module");
        let skillbook = item(4_000, CATEGORY_SKILL, "Test Skill");
        let trade_good = item(4_500, COMMODITY_CATEGORY_ID, "Test NPC Trade Good");

        let mut command_center = item(5_000, CATEGORY_MODULE, "Test Command Center");
        command_center.group_id = Some(COMMAND_CENTER_GROUP_ID);

        let data = fixture(
            vec![
                bpo,
                product,
                bridge,
                t2,
                faction,
                deadspace,
                officer,
                skillbook,
                trade_good,
                command_center,
            ],
            vec![manufacturing(1_000, 1_001, 1)],
            vec![
                (3_000, dogma(Some(2.0), Some(2))),
                (3_001, dogma(Some(1.0), Some(4))),
                (3_002, dogma(Some(1.0), Some(6))),
                (3_003, dogma(Some(1.0), Some(5))),
            ],
        );
        let result = classify(&data);
        let bridge_groups = BTreeSet::from([333]);
        let bridge_types = BTreeSet::new();
        let assignment = |type_id, lp_reward| {
            result
                .market_assignment_checked(
                    &data,
                    type_id,
                    true,
                    lp_reward,
                    &bridge_groups,
                    &bridge_types,
                )
                .expect("role overlap validates")
                .expect("type receives one canonical role")
        };

        let bpo = assignment(1_000, true);
        assert_eq!(
            (bpo.role, bpo.profile),
            (MarketRole::Bpo, PricingProfile::Bpo)
        );
        assert_eq!(bpo.sides, MarketSidePolicy::SELL_ONLY);

        let ordinary_product = assignment(1_001, false);
        assert_eq!(ordinary_product.role, MarketRole::Core);
        assert_eq!(ordinary_product.sides, MarketSidePolicy::BOTH);
        let lp_product = assignment(1_001, true);
        assert_eq!(lp_product.role, MarketRole::Rare);
        assert_eq!(lp_product.profile, PricingProfile::RareFactionLp);
        assert_eq!(lp_product.sides, MarketSidePolicy::BUY_ONLY);

        let bridge = assignment(2_000, true);
        assert_eq!(
            (bridge.role, bridge.profile, bridge.sides),
            (
                MarketRole::Bridge,
                PricingProfile::Bridge,
                MarketSidePolicy::SELL_ONLY
            )
        );

        for (type_id, profile) in [
            (3_000, PricingProfile::RareT2),
            (3_001, PricingProfile::RareFactionLp),
            (3_002, PricingProfile::RareDeadspaceDed),
            (3_003, PricingProfile::RareOfficer),
        ] {
            let rare = assignment(type_id, false);
            assert_eq!(rare.role, MarketRole::Rare);
            assert_eq!(rare.profile, profile);
            assert_eq!(rare.sides, MarketSidePolicy::BUY_ONLY);
        }

        let skillbook = assignment(4_000, true);
        assert_eq!(skillbook.role, MarketRole::Skillbook);
        assert_eq!(skillbook.sides, MarketSidePolicy::SELL_ONLY);

        let command_center = assignment(5_000, true);
        assert_eq!(command_center.role, MarketRole::CommandCenter);
        assert_eq!(command_center.sides, MarketSidePolicy::SELL_ONLY);

        assert!(
            result
                .market_assignment_checked(
                    &data,
                    4_500,
                    false,
                    false,
                    &bridge_groups,
                    &bridge_types,
                )
                .expect("role overlap validates")
                .is_none(),
            "authenticity-only NPC trade goods are not replicated into canonical hubs"
        );

        assert!(
            result
                .market_assignment_checked(
                    &data,
                    1_000,
                    false,
                    false,
                    &bridge_groups,
                    &bridge_types,
                )
                .expect("role overlap validates")
                .is_none(),
            "a heuristic T1 blueprint is not supplied without NPC-catalog eligibility"
        );
    }

    #[test]
    fn progression_npc_allowlists_are_sell_only_and_do_not_admit_other_trade_goods() {
        const QUANTUM_CORES: [u32; 9] = [
            56_201, 56_202, 56_203, 56_204, 56_205, 56_206, 56_207, 56_208, 81_920,
        ];
        const ACTIVITY_INPUTS: [u32; 7] = [41, 3_685, 3_687, 3_812, 3_814, 9_850, 3_773];
        let mut items = QUANTUM_CORES
            .into_iter()
            .map(|type_id| {
                let mut item = item(type_id, 66, &format!("Quantum-group type {type_id}"));
                item.group_id = Some(4_086);
                item
            })
            .collect::<Vec<_>>();
        items.extend(
            ACTIVITY_INPUTS
                .into_iter()
                .map(|type_id| item(type_id, COMMODITY_CATEGORY_ID, &format!("Input {type_id}"))),
        );
        items.push(item(42, COMMODITY_CATEGORY_ID, "Spiced Wine"));
        items.push(item(43, COMMODITY_CATEGORY_ID, "Antibiotics"));
        let data = fixture(items, Vec::new(), Vec::new());
        let classification = classify(&data);
        let none = BTreeSet::new();
        let progression_groups = BTreeSet::from([4_086]);
        let progression_types = BTreeSet::from(ACTIVITY_INPUTS);

        for type_id in QUANTUM_CORES.into_iter().chain(ACTIVITY_INPUTS) {
            let assignment = classification
                .market_assignment_checked_with_progression(
                    &data,
                    type_id,
                    false,
                    false,
                    &none,
                    &none,
                    &progression_groups,
                    &progression_types,
                )
                .expect("progression role does not collide")
                .expect("confirmed progression type receives a role");
            assert_eq!(assignment.role, MarketRole::ProgressionNpc);
            assert_eq!(assignment.profile, PricingProfile::ProgressionNpc);
            assert_eq!(assignment.sides, MarketSidePolicy::SELL_ONLY);
        }

        for type_id in [42, 43] {
            assert!(
                classification
                    .market_assignment_checked_with_progression(
                        &data,
                        type_id,
                        false,
                        false,
                        &none,
                        &none,
                        &progression_groups,
                        &progression_types,
                    )
                    .expect("unrelated trade good classification is deterministic")
                    .is_none()
            );
        }
    }

    #[test]
    fn lp_offered_blueprint_products_are_buy_only_before_core_assignment() {
        let cases = [
            (72_811, 72_923, "Cyclone Fleet Issue"),
            (72_812, 72_926, "Ferox Navy Issue"),
            (72_869, 72_925, "Myrmidon Navy Issue"),
            (72_872, 72_924, "Prophecy Navy Issue"),
            (78_333, 78_360, "Mekubal"),
        ];
        let ordinary_product = 600;
        let ordinary_blueprint = 601;
        let established_faction_cases = [
            (17_841, "Federation Navy Comet"),
            (33_151, "Brutix Navy Issue"),
            (33_153, "Drake Navy Issue"),
        ];
        let mut items = vec![item(ordinary_product, SHIP_CATEGORY_ID, "Ordinary T1 Hull")];
        items.extend(
            established_faction_cases
                .into_iter()
                .map(|(type_id, name)| item(type_id, SHIP_CATEGORY_ID, name)),
        );
        let mut blueprints = vec![manufacturing(ordinary_blueprint, ordinary_product, 1)];
        let mut direct_lp = BTreeSet::new();
        for (product, blueprint_id, name) in cases {
            items.push(item(product, SHIP_CATEGORY_ID, name));
            let mut blueprint_item = item(blueprint_id, BLUEPRINT_CATEGORY_ID, "LP BPC");
            blueprint_item.market_group_id = None;
            items.push(blueprint_item);
            blueprints.push(manufacturing(blueprint_id, product, 1));
            direct_lp.insert(blueprint_id);
        }
        let data = fixture(
            items,
            blueprints,
            established_faction_cases
                .into_iter()
                .map(|(type_id, _)| (type_id, dogma(Some(1.0), Some(4))))
                .collect(),
        );
        let classification = classify(&data);
        let expanded = expand_lp_manufacturing_products(&data, &direct_lp);
        let none = BTreeSet::new();

        for (product, blueprint_id, _) in cases {
            assert!(
                expanded.contains(&blueprint_id),
                "direct LP reward is preserved"
            );
            assert!(
                expanded.contains(&product),
                "LP BPC product inherits LP identity"
            );
            let assignment = classification
                .market_assignment_checked(
                    &data,
                    product,
                    false,
                    expanded.contains(&product),
                    &none,
                    &none,
                )
                .expect("role overlap validates")
                .expect("LP BPC product receives liquidity");
            assert_eq!(assignment.role, MarketRole::Rare);
            assert_eq!(assignment.profile, PricingProfile::RareFactionLp);
            assert_eq!(assignment.sides, MarketSidePolicy::BUY_ONLY);
        }

        for (type_id, _) in established_faction_cases {
            let assignment = classification
                .market_assignment_checked(&data, type_id, false, false, &none, &none)
                .expect("role overlap validates")
                .expect("existing faction hull retains liquidity");
            assert_eq!(assignment.role, MarketRole::Rare);
            assert_eq!(assignment.profile, PricingProfile::RareFactionLp);
            assert_eq!(assignment.sides, MarketSidePolicy::BUY_ONLY);
        }

        let ordinary = classification
            .market_assignment_checked(
                &data,
                ordinary_product,
                false,
                expanded.contains(&ordinary_product),
                &none,
                &none,
            )
            .expect("role overlap validates")
            .expect("ordinary T1 hull remains seeded");
        assert_eq!(ordinary.role, MarketRole::Core);
        assert_eq!(ordinary.sides, MarketSidePolicy::BOTH);
    }

    #[test]
    fn one_tritanium_reward_hulls_are_buy_only_but_normal_bpc_recipes_remain_core() {
        let reward_cases = [
            (3_756, 33_327, "Gnosis"),
            (42_685, 44_106, "Sunesis"),
            (47_466, 47_716, "Praxis"),
            (77_114, 79_214, "Metamorphosis"),
        ];
        let normal_product = 900;
        let normal_blueprint = 901;
        let mut items = vec![item(
            normal_product,
            SHIP_CATEGORY_ID,
            "Accessible Rare BPC Hull",
        )];
        let mut normal_blueprint_item = item(normal_blueprint, BLUEPRINT_CATEGORY_ID, "Normal BPC");
        normal_blueprint_item.market_group_id = None;
        items.push(normal_blueprint_item);
        let mut blueprints = vec![blueprint(
            normal_blueprint,
            BlueprintActivities {
                manufacturing: Some(activity(normal_product, 1, &[(34, 100)])),
                ..BlueprintActivities::default()
            },
        )];
        for (product, blueprint_id, name) in reward_cases {
            items.push(item(product, SHIP_CATEGORY_ID, name));
            let mut blueprint_item = item(blueprint_id, BLUEPRINT_CATEGORY_ID, "Reward BPC");
            blueprint_item.market_group_id = None;
            items.push(blueprint_item);
            blueprints.push(blueprint(
                blueprint_id,
                BlueprintActivities {
                    manufacturing: Some(activity(product, 1, &[(34, 1)])),
                    ..BlueprintActivities::default()
                },
            ));
        }
        let data = fixture(items, blueprints, Vec::new());
        let classification = classify(&data);
        let none = BTreeSet::new();

        for (product, _, _) in reward_cases {
            assert!(classification.is_special_reward_placeholder_hull(&data, product));
            let assignment = classification
                .market_assignment_checked(&data, product, false, false, &none, &none)
                .expect("role overlap validates")
                .expect("reward hull receives Buy liquidity");
            assert_eq!(assignment.role, MarketRole::Rare);
            assert_eq!(assignment.profile, PricingProfile::RareFactionLp);
            assert_eq!(assignment.sides, MarketSidePolicy::BUY_ONLY);
        }

        assert!(!classification.is_special_reward_placeholder_hull(&data, normal_product));
        let normal = classification
            .market_assignment_checked(&data, normal_product, false, false, &none, &none)
            .expect("role overlap validates")
            .expect("normal non-placeholder recipe remains eligible");
        assert_eq!(normal.role, MarketRole::Core);
        assert_eq!(normal.sides, MarketSidePolicy::BOTH);
    }

    #[test]
    fn reusable_recipe_supply_includes_published_formulas_but_not_arbitrary_blueprints() {
        let formula = item(1_000, CATEGORY_BLUEPRINT, "Test Reaction Formula");
        let manufacturing_bpo = item(2_000, CATEGORY_BLUEPRINT, "Test Module Blueprint");
        let arbitrary_blueprint = item(3_000, CATEGORY_BLUEPRINT, "Arbitrary Blueprint");
        let reaction_product = item(1_001, CATEGORY_MATERIAL, "Reaction Product");
        let manufactured_product = item(2_001, CATEGORY_MODULE, "Test Module I");
        let tech_three_final = item(4_000, CATEGORY_MODULE, "Test Module III");
        let data = fixture(
            vec![
                formula,
                manufacturing_bpo,
                arbitrary_blueprint,
                reaction_product,
                manufactured_product,
                tech_three_final,
            ],
            vec![
                reaction(1_000, 1_001, 10),
                manufacturing(2_000, 2_001, 1),
                blueprint(3_000, BlueprintActivities::default()),
            ],
            vec![(4_000, dogma(Some(3.0), None))],
        );
        let classification = classify(&data);
        let no_bridges = BTreeSet::new();
        let assignment = |type_id, npc_catalog| {
            classification
                .market_assignment_checked(
                    &data,
                    type_id,
                    npc_catalog,
                    false,
                    &no_bridges,
                    &no_bridges,
                )
                .expect("role predicates validate")
        };

        let bpo = assignment(2_000, true).expect("NPC T1 BPO remains supplied");
        assert_eq!(
            (bpo.role, bpo.profile),
            (MarketRole::Bpo, PricingProfile::Bpo)
        );
        assert_eq!(bpo.sides, MarketSidePolicy::SELL_ONLY);

        let formula = assignment(1_000, false).expect("published formula is supplied");
        assert_eq!(
            (formula.role, formula.profile, formula.sides),
            (
                MarketRole::Bpo,
                PricingProfile::ReactionFormula,
                MarketSidePolicy::SELL_ONLY,
            )
        );
        assert!(
            !formula.sides.buy,
            "Reaction Formulas never receive synthetic Buy"
        );
        assert_eq!(assignment(3_000, false), None);
        assert_eq!(
            assignment(4_000, false),
            None,
            "T3 final goods remain unseeded"
        );
    }

    #[test]
    fn finished_tech_two_is_buy_only_but_tech_three_never_reaches_rare_t2_or_core() {
        const T2_MODULE: u32 = 8_000;
        const T3_MODULE: u32 = 8_001;
        const T3_SUBSYSTEM: u32 = 8_002;
        const CATEGORY_SUBSYSTEM: u32 = 32;

        let data = fixture(
            vec![
                item(T2_MODULE, CATEGORY_MODULE, "Test Module II"),
                item(T3_MODULE, CATEGORY_MODULE, "Test Module III"),
                item(T3_SUBSYSTEM, CATEGORY_SUBSYSTEM, "Test Defensive Subsystem"),
            ],
            Vec::new(),
            vec![
                (T2_MODULE, dogma(Some(2.0), Some(2))),
                (T3_MODULE, dogma(Some(3.0), Some(2))),
                (T3_SUBSYSTEM, dogma(Some(3.0), Some(2))),
            ],
        );
        let mut result = classify(&data);
        // Exercise the canonical guard directly: even if an upstream bucket starts admitting
        // these types, Tech III must not fall through into generic Core supply.
        result.bucket_a.insert(T3_MODULE);
        result.bucket_a.insert(T3_SUBSYSTEM);
        let none = BTreeSet::new();

        let t2 = result
            .market_assignment_checked(&data, T2_MODULE, false, false, &none, &none)
            .expect("role overlap validates")
            .expect("Tech II receives canonical buy liquidity");
        assert_eq!(t2.role, MarketRole::Rare);
        assert_eq!(t2.profile, PricingProfile::RareT2);
        assert_eq!(t2.sides, MarketSidePolicy::BUY_ONLY);

        for type_id in [T3_MODULE, T3_SUBSYSTEM] {
            assert!(
                result
                    .market_assignment_checked(&data, type_id, false, false, &none, &none)
                    .expect("role overlap validates")
                    .is_none(),
                "Tech III type {type_id} must remain outside the Market v1 canonical book"
            );
        }
    }

    #[test]
    fn missing_normal_salvage_allowlist_is_bridge_but_other_group_754_salvage_stays_core() {
        const MISSING: [u32; 9] = [
            25_591, 25_592, 25_594, 25_610, 25_618, 25_619, 25_620, 25_621, 25_623,
        ];
        let mut items = MISSING
            .into_iter()
            .map(|type_id| {
                let mut salvage = item(type_id, CATEGORY_MATERIAL, &format!("Missing {type_id}"));
                salvage.group_id = Some(NATURAL_SALVAGE_GROUP_ID);
                salvage
            })
            .collect::<Vec<_>>();
        let mut ordinary = item(25_600, CATEGORY_MATERIAL, "Obtainable Salvage");
        ordinary.group_id = Some(NATURAL_SALVAGE_GROUP_ID);
        items.push(ordinary);
        let data = fixture(items, Vec::new(), Vec::new());
        let result = classify(&data);
        let bridge_types = MISSING.into_iter().collect::<BTreeSet<_>>();
        let bridge_groups = BTreeSet::new();

        for type_id in MISSING {
            let assignment = result
                .market_assignment_checked(
                    &data,
                    type_id,
                    false,
                    false,
                    &bridge_groups,
                    &bridge_types,
                )
                .expect("overlap validates")
                .expect("allowlisted salvage is assigned");
            assert_eq!(assignment.role, MarketRole::Bridge);
            assert_eq!(assignment.profile, PricingProfile::Bridge);
            assert_eq!(assignment.sides, MarketSidePolicy::SELL_ONLY);
        }

        let control = result
            .market_assignment_checked(&data, 25_600, false, false, &bridge_groups, &bridge_types)
            .expect("overlap validates")
            .expect("ordinary salvage remains seeded");
        assert_eq!(control.role, MarketRole::Core);
        assert_eq!(control.profile, PricingProfile::GasIceSalvage);
        assert_eq!(control.sides, MarketSidePolicy::BOTH);
    }

    #[test]
    fn authoritative_pairing_classifies_compressed_ore_without_name_heuristics_and_preserves_bridge()
     {
        const RAW: u32 = 8_100;
        const COMPRESSED: u32 = 8_101;
        const MOON_RAW: u32 = 8_102;
        const MOON_COMPRESSED: u32 = 8_103;
        const MOON_GROUP: u32 = 1_920;

        let mut raw = item(RAW, CATEGORY_ASTEROID, "Opaque Ore Source");
        raw.group_name = Some("Opaque Ore Group".to_string());
        let mut compressed = item(COMPRESSED, CATEGORY_ASTEROID, "Packed Variant");
        compressed.group_name = Some("Opaque Ore Group".to_string());
        let mut moon_raw = item(MOON_RAW, CATEGORY_ASTEROID, "Moon Source");
        moon_raw.group_id = Some(MOON_GROUP);
        moon_raw.group_name = Some("Moon Resource".to_string());
        let mut moon_compressed = item(MOON_COMPRESSED, CATEGORY_ASTEROID, "Packed Moon Variant");
        moon_compressed.group_id = Some(MOON_GROUP);
        moon_compressed.group_name = Some("Moon Resource".to_string());

        let mut data = fixture(
            vec![raw, compressed, moon_raw, moon_compressed],
            Vec::new(),
            Vec::new(),
        );
        data.compressed_type_ids = [COMPRESSED, MOON_COMPRESSED].into_iter().collect();
        let result = classify(&data);
        let no_types = BTreeSet::new();
        let bridge_groups = [MOON_GROUP].into_iter().collect::<BTreeSet<_>>();

        let assignment = |type_id| {
            result
                .market_assignment_checked(&data, type_id, false, false, &bridge_groups, &no_types)
                .expect("role overlap validates")
                .expect("fixture type is assigned")
        };
        assert_eq!(assignment(RAW).profile, PricingProfile::RawOre);
        assert_eq!(
            assignment(COMPRESSED).profile,
            PricingProfile::CompressedOre
        );
        for type_id in [MOON_RAW, MOON_COMPRESSED] {
            let moon = assignment(type_id);
            assert_eq!(moon.role, MarketRole::Bridge);
            assert_eq!(moon.profile, PricingProfile::Bridge);
            assert_eq!(moon.sides, MarketSidePolicy::SELL_ONLY);
        }
    }

    #[test]
    fn moon_mining_toggle_changes_only_the_proven_moon_resource_family() {
        const RAW_MOON: u32 = 8_200;
        const COMPRESSED_MOON: u32 = 8_201;
        const REFINED_MOON: u32 = 8_202;
        const ORDINARY_ORE: u32 = 8_203;
        const DATACORE: u32 = 8_204;
        let mut raw = item(RAW_MOON, CATEGORY_ASTEROID, "Raw Moon Ore");
        raw.group_id = Some(1_920);
        let mut compressed = item(COMPRESSED_MOON, CATEGORY_ASTEROID, "Compressed Moon Ore");
        compressed.group_id = Some(1_920);
        let mut refined = item(REFINED_MOON, COMMODITY_CATEGORY_ID, "Refined Moon Material");
        refined.group_id = Some(427);
        let ordinary = item(ORDINARY_ORE, CATEGORY_ASTEROID, "Ordinary Ore");
        let mut datacore = item(DATACORE, CATEGORY_MATERIAL, "Datacore");
        datacore.group_id = Some(333);
        let mut data = fixture(
            vec![raw, compressed, refined, ordinary, datacore],
            Vec::new(),
            Vec::new(),
        );
        data.compressed_type_ids.insert(COMPRESSED_MOON);
        let result = classify(&data);
        let bridge_groups = [333, 427, 1_920].into_iter().collect::<BTreeSet<_>>();
        let none = BTreeSet::new();
        let assignment = |type_id, moon_mining_available| {
            result
                .market_assignment_checked_with_supply_ladder(
                    &data,
                    type_id,
                    false,
                    false,
                    &bridge_groups,
                    &none,
                    &none,
                    &none,
                    moon_mining_available,
                )
                .expect("role predicates validate")
                .expect("fixture type is assigned")
        };

        for type_id in [RAW_MOON, COMPRESSED_MOON, REFINED_MOON] {
            let disabled = assignment(type_id, false);
            assert_eq!(disabled.role, MarketRole::Bridge);
            assert_eq!(disabled.sides, MarketSidePolicy::SELL_ONLY);
        }
        let raw = assignment(RAW_MOON, true);
        assert_eq!(
            (raw.role, raw.profile, raw.sides),
            (
                MarketRole::Core,
                PricingProfile::RawOre,
                MarketSidePolicy::BOTH,
            )
        );
        let compressed = assignment(COMPRESSED_MOON, true);
        assert_eq!(
            (compressed.role, compressed.profile, compressed.sides),
            (
                MarketRole::Core,
                PricingProfile::CompressedOre,
                MarketSidePolicy::BOTH,
            )
        );
        let refined = assignment(REFINED_MOON, true);
        assert_eq!(
            (refined.role, refined.profile, refined.sides),
            (
                MarketRole::Core,
                PricingProfile::CoreGeneral,
                MarketSidePolicy::BOTH,
            )
        );
        assert_eq!(
            assignment(ORDINARY_ORE, true),
            assignment(ORDINARY_ORE, false)
        );
        assert_eq!(assignment(DATACORE, true).role, MarketRole::Bridge);
        assert_eq!(
            assignment(DATACORE, true).sides,
            MarketSidePolicy::SELL_ONLY
        );
    }

    #[test]
    fn production_intermediate_allowlists_are_exact_buy_sell_and_exclude_unfunded_group_536() {
        assert_eq!(PRODUCTION_INTERMEDIATE_TYPE_IDS.len(), 114);
        assert_eq!(
            PRODUCTION_INTERMEDIATE_TYPE_IDS
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len(),
            114,
            "the authoritative allowlist must not contain duplicates"
        );
        assert_eq!(
            STRUCTURE_COMPONENT_PRODUCTION_INTERMEDIATE_TYPE_IDS.len(),
            15
        );
        assert_eq!(
            STRUCTURE_COMPONENT_PRODUCTION_INTERMEDIATE_TYPE_IDS
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len(),
            15,
            "the Structure Component allowlist must not contain duplicates"
        );
        let approved = PRODUCTION_INTERMEDIATE_TYPE_IDS
            .iter()
            .chain(STRUCTURE_COMPONENT_PRODUCTION_INTERMEDIATE_TYPE_IDS)
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            approved.len(),
            129,
            "legacy and Structure sets must stay disjoint"
        );
        let group_f = [34_575, 57_443, 57_452, 57_445, 57_446, 57_447, 57_448];
        let mut items = approved
            .iter()
            .map(|type_id| {
                item(
                    *type_id,
                    COMMODITY_CATEGORY_ID,
                    &format!("Intermediate {type_id}"),
                )
            })
            .collect::<Vec<_>>();
        items.extend(group_f.iter().map(|type_id| {
            item(
                *type_id,
                COMMODITY_CATEGORY_ID,
                &format!("Group F {type_id}"),
            )
        }));
        items.push(item(
            49_720,
            COMMODITY_CATEGORY_ID,
            "Structure EXERT Conduit Coupler",
        ));
        let data = fixture(items, Vec::new(), Vec::new());
        let result = classify(&data);
        let none = BTreeSet::new();
        for type_id in approved {
            let assignment = result
                .market_assignment_checked_with_supply_ladder(
                    &data, type_id, false, false, &none, &none, &none, &none, true,
                )
                .expect("role predicates validate")
                .expect("approved intermediate is assigned");
            assert_eq!(assignment.role, MarketRole::ProductionIntermediate);
            assert_eq!(assignment.profile, PricingProfile::ProductionIntermediate);
            assert_eq!(assignment.sides, MarketSidePolicy::BOTH);
        }
        assert!(!is_production_intermediate_type(49_720));
        assert!(
            result
                .market_assignment_checked_with_supply_ladder(
                    &data, 49_720, false, false, &none, &none, &none, &none, true,
                )
                .expect("role predicates validate")
                .is_none(),
            "49_720 must remain unseeded until its funded route is complete"
        );
        for type_id in group_f {
            assert!(
                result
                    .market_assignment_checked_with_supply_ladder(
                        &data, type_id, false, false, &none, &none, &none, &none, true,
                    )
                    .expect("role predicates validate")
                    .is_none(),
                "Group F type {type_id} must remain unchanged"
            );
        }
        for representative in [11_539, 11_543, 29_103, 11_475] {
            assert!(PRODUCTION_INTERMEDIATE_TYPE_IDS.contains(&representative));
        }
        assert!(!PRODUCTION_INTERMEDIATE_TYPE_IDS.contains(&33_195));
        assert_eq!(
            PRODUCTION_INTERMEDIATE_TYPE_IDS
                .iter()
                .filter(|type_id| (11_475..=11_486).contains(*type_id))
                .count(),
            8,
            "exactly the eight approved R.A.M. tools are present"
        );
    }

    #[test]
    fn planetary_industry_boundary_is_exact_categories_and_does_not_broaden_bridge() {
        let mut resource = item(1, 42, "Aqueous Liquids");
        resource.group_id = Some(1_032);
        let mut commodity = item(2, 43, "Water");
        commodity.group_id = Some(1_034);
        let mut command_center = item(3, 6, "Command Center");
        command_center.group_id = Some(COMMAND_CENTER_GROUP_ID);
        let mut unrelated_bridge = item(33_195, COMMODITY_CATEGORY_ID, "Spatial Attunement Unit");
        unrelated_bridge.group_id = Some(526);
        let data = fixture(
            vec![resource, commodity, command_center, unrelated_bridge],
            Vec::new(),
            Vec::new(),
        );
        let result = classify(&data);
        let none = BTreeSet::new();
        let bridge_types = BTreeSet::from([33_195]);

        for type_id in [1, 2] {
            let assignment = result
                .market_assignment_checked_with_supply_ladder(
                    &data,
                    type_id,
                    false,
                    false,
                    &none,
                    &bridge_types,
                    &none,
                    &none,
                    true,
                )
                .expect("PI role predicates validate")
                .expect("PI output is assigned");
            assert_eq!(assignment.role, MarketRole::Bridge);
            assert_eq!(assignment.profile, PricingProfile::PlanetaryIndustry);
            assert_eq!(assignment.sides, MarketSidePolicy::BOTH);
        }

        let command = result
            .market_assignment_checked_with_supply_ladder(
                &data,
                3,
                false,
                false,
                &none,
                &bridge_types,
                &none,
                &none,
                true,
            )
            .expect("Command Center classification validates")
            .expect("Command Center remains seeded");
        assert_eq!(command.role, MarketRole::CommandCenter);
        assert_eq!(command.sides, MarketSidePolicy::SELL_ONLY);

        let unrelated = result
            .market_assignment_checked_with_supply_ladder(
                &data,
                33_195,
                false,
                false,
                &none,
                &bridge_types,
                &none,
                &none,
                true,
            )
            .expect("unrelated Bridge classification validates")
            .expect("explicit Bridge input remains seeded");
        assert_eq!(unrelated.profile, PricingProfile::Bridge);
        assert_eq!(unrelated.sides, MarketSidePolicy::SELL_ONLY);
    }

    #[test]
    fn intentional_lp_precedence_is_allowed_but_unexpected_supply_overlap_fails() {
        let skill = item(7_000, CATEGORY_SKILL, "LP Skillbook");
        let data = fixture(vec![skill], Vec::new(), Vec::new());
        let result = classify(&data);
        let none = BTreeSet::new();
        let assignment = result
            .market_assignment_checked(&data, 7_000, false, true, &none, &none)
            .expect("Skillbook over LP reward is documented precedence")
            .expect("skillbook is assigned");
        assert_eq!(assignment.role, MarketRole::Skillbook);

        let error = result
            .market_assignment_checked(&data, 7_000, false, false, &BTreeSet::from([1]), &none)
            .expect_err("Skillbook plus Bridge is an unexpected supply overlap");
        let message = format!("{error:#}");
        assert!(message.contains("7000"), "{message}");
        assert!(message.contains("skill-category"), "{message}");
        assert!(message.contains("bridge-allowlist"), "{message}");
    }
}
