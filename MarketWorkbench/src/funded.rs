//! Market v1 funded-production and refining safety.
//!
//! Profile prices remain the desired market. This module only lowers synthetic Buys when a
//! fully guaranteed manufacturing path is cheaper, lowers ore Buys beneath realistic refine
//! value, and finally raises Sells (never Buys or cost bases) above perfect-yield reprocessing.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::build::{MIN_SEED_PRICE, SeedPricingProfiles, SeedRow, plan_canonical_seed_rows};
use crate::classify::{
    MarketRole, PRODUCTION_INTERMEDIATE_TYPE_IDS, PricingProfile, ProducerActivity,
    STRUCTURE_COMPONENT_PRODUCTION_INTERMEDIATE_TYPE_IDS,
};
use crate::config::SeederConfig;
use crate::manifest::PriceSource;
use crate::seedplan::{
    DropReason, ProductionIntermediatePriceBook, RareFallbackAnchor, RareFallbackBook,
    RareFallbackProvenance, SeedPlan,
};
use crate::staticdata::{BlueprintDefinition, MaterialEntry, ReprocessingStatic, StaticData};

const MAX_MANUFACTURING_RUNS: i64 = 1_000_000;
const MAX_INVENTION_SKILL_LEVEL: f64 = 5.0;
const INVENTION_SKILL_PROBABILITY: f64 = 1.0 / 30.0;
const INVENTION_SKILL_PROBABILITY_LOWER: f64 = 1.0 / 40.0;
const INVENTION_BASE_ME: i64 = 2;
const DECRYPTOR_GROUP_ID: u32 = 1304;
const ATTRIBUTE_TECH_LEVEL: u32 = 422;
const ATTRIBUTE_META_GROUP_ID: u32 = 1692;
const ATTRIBUTE_NULLSEC_MODIFIER: u32 = 2357;
const ATTRIBUTE_ENGINEERING_HULL_MATERIAL: u32 = 2600;
const ATTRIBUTE_INVENTION_PROBABILITY: u32 = 1112;
const ATTRIBUTE_INVENTION_ME: u32 = 1113;
const ATTRIBUTE_INVENTION_RUNS: u32 = 1124;
const LOWER_INVENTION_SKILLS: [u32; 7] = [23087, 21790, 23121, 21791, 3408, 52308, 55025];
const SECURITY_SCALED_RIG_ATTRIBUTES: [u32; 6] = [2593, 2594, 2595, 2653, 2713, 2714];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundedActivity {
    Manufacturing,
    Reaction,
    InventionManufacturing,
}

impl FundedActivity {
    pub fn key(self) -> &'static str {
        match self {
            Self::Manufacturing => "manufacturing",
            Self::Reaction => "reaction",
            Self::InventionManufacturing => "invention+manufacturing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundedChoiceKind {
    Buy,
    Build,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FundedChoice {
    pub type_id: u32,
    pub kind: FundedChoiceKind,
    pub unit_cost: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FundedPath {
    pub unit_cost: f64,
    pub activity: FundedActivity,
    pub blueprint_type_id: u32,
    pub choices: Vec<FundedChoice>,
    dependencies: BTreeSet<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnfundedPath {
    pub type_id: u32,
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
struct ProductionRecipe {
    blueprint_type_id: u32,
    activity: ProducerActivity,
    product_type_id: u32,
    product_per_run: i64,
    materials: Vec<MaterialEntry>,
}

#[derive(Debug, Clone)]
struct InventionRecipe {
    source_blueprint_type_id: u32,
    output_blueprint_type_id: u32,
    output_runs: i64,
    probability: f64,
    materials: Vec<MaterialEntry>,
    skills: Vec<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct ProductionCatalog {
    recipes: BTreeMap<u32, Vec<ProductionRecipe>>,
    inventions: BTreeMap<u32, Vec<InventionRecipe>>,
}

impl ProductionCatalog {
    pub fn from_static(data: &StaticData) -> Self {
        let mut catalog = Self::default();
        for blueprint in &data.blueprints {
            catalog.absorb_blueprint(blueprint);
        }
        catalog
    }

    fn absorb_blueprint(&mut self, blueprint: &BlueprintDefinition) {
        if !blueprint.published {
            return;
        }
        for (activity, definition) in [
            (
                ProducerActivity::Manufacturing,
                blueprint.activities.manufacturing.as_ref(),
            ),
            (
                ProducerActivity::Reaction,
                blueprint.activities.reaction.as_ref(),
            ),
        ] {
            let Some(definition) = definition else {
                continue;
            };
            for product in &definition.products {
                if product.type_id == 0 || product.quantity <= 0 || definition.materials.is_empty()
                {
                    continue;
                }
                self.recipes
                    .entry(product.type_id)
                    .or_default()
                    .push(ProductionRecipe {
                        blueprint_type_id: blueprint.blueprint_type_id,
                        activity,
                        product_type_id: product.type_id,
                        product_per_run: product.quantity,
                        materials: definition.materials.clone(),
                    });
            }
        }
        let Some(invention) = blueprint.activities.invention.as_ref() else {
            return;
        };
        if invention.products.is_empty() {
            return;
        }
        // EveJS applies one activity-level base chance: the average probability of every
        // output in the invention activity. The selected output still retains its own BPC
        // type and run count.
        let activity_probability = invention
            .products
            .iter()
            .map(|product| product.probability.unwrap_or(1.0).clamp(0.0, 1.0))
            .sum::<f64>()
            / invention.products.len() as f64;
        for product in &invention.products {
            if product.type_id == 0 || product.quantity <= 0 || invention.materials.is_empty() {
                continue;
            }
            self.inventions
                .entry(product.type_id)
                .or_default()
                .push(InventionRecipe {
                    source_blueprint_type_id: blueprint.blueprint_type_id,
                    output_blueprint_type_id: product.type_id,
                    output_runs: product.quantity,
                    probability: activity_probability,
                    materials: invention.materials.clone(),
                    skills: invention.skills.iter().map(|skill| skill.type_id).collect(),
                });
        }
    }

    pub fn has_recipe(&self, type_id: u32) -> bool {
        self.recipes
            .get(&type_id)
            .is_some_and(|recipes| !recipes.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecryptorSemantics {
    pub probability_multiplier: f64,
    pub material_efficiency: i64,
    pub max_runs: i64,
}

#[derive(Debug, Clone, Default)]
struct RigSemantics {
    modifiers: Vec<(u32, f64)>,
}

#[derive(Debug, Clone)]
pub struct IndustrySemantics {
    manufacturing_hull_modifier: f64,
    rigs: Vec<RigSemantics>,
    decryptors: BTreeMap<u32, DecryptorSemantics>,
}

impl Default for IndustrySemantics {
    fn default() -> Self {
        Self {
            manufacturing_hull_modifier: 1.0,
            rigs: Vec::new(),
            decryptors: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct IndustryDogmaFile {
    #[serde(rename = "typesByTypeID", default)]
    types: HashMap<u32, IndustryDogmaType>,
    #[serde(rename = "effectTypesByID", default)]
    effects: HashMap<u32, IndustryDogmaEffect>,
}

#[derive(Debug, Deserialize)]
struct IndustryDogmaType {
    #[serde(default)]
    attributes: HashMap<u32, f64>,
    #[serde(default)]
    effects: Vec<u32>,
}

#[derive(Debug, Deserialize)]
struct IndustryDogmaEffect {
    #[serde(rename = "modifierInfo", default)]
    modifiers: Vec<IndustryModifier>,
}

#[derive(Debug, Deserialize)]
struct IndustryModifier {
    #[serde(default)]
    domain: String,
    #[serde(default)]
    func: String,
    #[serde(default)]
    operation: i64,
    #[serde(rename = "modifiedAttributeID", default)]
    target: u32,
    #[serde(rename = "modifyingAttributeID", default)]
    source: u32,
}

impl IndustrySemantics {
    /// Derives the exact material factors exposed by EveJS: ME is applied separately, the
    /// best engineering-complex hull modifier applies to manufacturing, and the strongest
    /// single applicable nullsec rig is selected using the runtime's target map.
    pub fn load(dir: &Path, data: &StaticData) -> Result<Self> {
        let path = dir.join("typeDogma").join("data.json");
        let file = File::open(&path)
            .with_context(|| format!("failed to open industry dogma {}", path.display()))?;
        let dogma: IndustryDogmaFile = serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("failed to parse industry dogma {}", path.display()))?;
        let semantics = Self::from_dogma(&dogma, data);
        semantics.validate()?;
        Ok(semantics)
    }

    fn from_dogma(dogma: &IndustryDogmaFile, data: &StaticData) -> Self {
        let manufacturing_hull_modifier = dogma
            .types
            .iter()
            .filter(|(type_id, _)| {
                data.item_type(**type_id)
                    .is_some_and(|item| item.published && item.category_id == Some(65))
            })
            .filter_map(|(_, record)| {
                record
                    .attributes
                    .get(&ATTRIBUTE_ENGINEERING_HULL_MATERIAL)
                    .copied()
            })
            .filter(|value| value.is_finite() && *value > 0.0)
            .fold(1.0_f64, f64::min);

        let mut rigs = Vec::new();
        let mut decryptors = BTreeMap::new();
        for (type_id, record) in &dogma.types {
            let published = data.item_type(*type_id).is_some_and(|item| item.published);
            if published
                && data
                    .item_type(*type_id)
                    .is_some_and(|item| item.group_id == Some(DECRYPTOR_GROUP_ID))
            {
                decryptors.insert(
                    *type_id,
                    DecryptorSemantics {
                        probability_multiplier: record
                            .attributes
                            .get(&ATTRIBUTE_INVENTION_PROBABILITY)
                            .copied()
                            .unwrap_or(1.0),
                        material_efficiency: record
                            .attributes
                            .get(&ATTRIBUTE_INVENTION_ME)
                            .copied()
                            .unwrap_or(0.0)
                            .trunc() as i64,
                        max_runs: record
                            .attributes
                            .get(&ATTRIBUTE_INVENTION_RUNS)
                            .copied()
                            .unwrap_or(0.0)
                            .trunc() as i64,
                    },
                );
            }
            if !published {
                continue;
            }
            let mut rig = RigSemantics::default();
            for effect_id in &record.effects {
                let Some(effect) = dogma.effects.get(effect_id) else {
                    continue;
                };
                for modifier in &effect.modifiers {
                    if modifier.domain != "structureID"
                        || modifier.func != "ItemModifier"
                        || modifier.operation != 6
                        || !is_material_target(modifier.target)
                    {
                        continue;
                    }
                    let mut percent = record
                        .attributes
                        .get(&modifier.source)
                        .copied()
                        .unwrap_or(0.0);
                    if SECURITY_SCALED_RIG_ATTRIBUTES.contains(&modifier.source) {
                        percent *= record
                            .attributes
                            .get(&ATTRIBUTE_NULLSEC_MODIFIER)
                            .copied()
                            .unwrap_or(1.0);
                    }
                    let amount = (1.0 + percent / 100.0).max(0.0);
                    if amount > 0.0 && (amount - 1.0).abs() > 1e-9 {
                        rig.modifiers.push((modifier.target, amount));
                    }
                }
            }
            if !rig.modifiers.is_empty() {
                rigs.push(rig);
            }
        }
        Self {
            manufacturing_hull_modifier,
            rigs,
            decryptors,
        }
    }

    fn validate(&self) -> Result<()> {
        if !(self.manufacturing_hull_modifier > 0.0 && self.manufacturing_hull_modifier < 1.0) {
            bail!(
                "industry dogma exposes no supported engineering-complex material modifier; refusing to approximate funded manufacturing"
            );
        }
        let targets = self
            .rigs
            .iter()
            .flat_map(|rig| rig.modifiers.iter().map(|(target, _)| *target))
            .collect::<BTreeSet<_>>();
        let missing = [
            2538, 2540, 2542, 2544, 2546, 2548, 2550, 2552, 2555, 2557, 2559, 2561, 2575, 2592,
            2658, 2716, 2718, 2720,
        ]
        .into_iter()
        .filter(|target| !targets.contains(target))
        .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!(
                "industry dogma is missing supported nullsec material-rig target attributes {:?}; refusing to use a blanket material factor",
                missing
            );
        }
        if self.decryptors.is_empty() {
            bail!(
                "industry dogma exposes no published decryptor modifiers; funded invention cannot be evaluated safely"
            );
        }
        Ok(())
    }

    pub fn fixture(manufacturing_hull_modifier: f64) -> Self {
        Self {
            manufacturing_hull_modifier,
            ..Self::default()
        }
    }

    pub fn add_fixture_rig(&mut self, target: u32, amount: f64) {
        self.rigs.push(RigSemantics {
            modifiers: vec![(target, amount)],
        });
    }

    pub fn add_fixture_decryptor(&mut self, type_id: u32, semantics: DecryptorSemantics) {
        self.decryptors.insert(type_id, semantics);
    }

    pub fn material_modifier(
        &self,
        activity: ProducerActivity,
        item: &crate::staticdata::ItemTypeRecord,
    ) -> f64 {
        let hull = if activity == ProducerActivity::Manufacturing {
            self.manufacturing_hull_modifier
        } else {
            1.0
        };
        let rig = self
            .rigs
            .iter()
            .map(|rig| {
                rig.modifiers
                    .iter()
                    .filter(|(target, _)| target_matches(*target, activity, item))
                    .map(|(_, amount)| *amount)
                    .product::<f64>()
            })
            .filter(|amount| *amount > 0.0 && *amount < 1.0)
            .fold(1.0_f64, f64::min);
        hull * rig
    }
}

fn is_material_target(target: u32) -> bool {
    matches!(
        target,
        2538 | 2540
            | 2542
            | 2544
            | 2546
            | 2548
            | 2550
            | 2552
            | 2555
            | 2557
            | 2559
            | 2561
            | 2575
            | 2592
            | 2658
            | 2716
            | 2718
            | 2720
    )
}

fn target_matches(
    target: u32,
    activity: ProducerActivity,
    item: &crate::staticdata::ItemTypeRecord,
) -> bool {
    let category = item.category_id;
    let group = item.group_id;
    match (activity, target) {
        (ProducerActivity::Manufacturing, 2538) => category == Some(7),
        (ProducerActivity::Manufacturing, 2540) => category == Some(8),
        (ProducerActivity::Manufacturing, 2542) => matches!(category, Some(18 | 87)),
        (ProducerActivity::Manufacturing, 2544) => matches!(group, Some(25 | 420 | 31)),
        (ProducerActivity::Manufacturing, 2546) => {
            matches!(group, Some(26 | 419 | 1201 | 4902 | 28 | 463))
        }
        (ProducerActivity::Manufacturing, 2548) => matches!(group, Some(27 | 513 | 941)),
        (ProducerActivity::Manufacturing, 2550) => matches!(
            group,
            Some(324 | 830 | 831 | 834 | 893 | 1527 | 1283 | 541 | 1534 | 1305)
        ),
        (ProducerActivity::Manufacturing, 2552) => matches!(
            group,
            Some(
                358 | 832 | 833 | 906 | 894 | 540 | 380 | 1202 | 543 | 963 | 954 | 956 | 957 | 958
            )
        ),
        (ProducerActivity::Manufacturing, 2555) => matches!(group, Some(898 | 900 | 902)),
        (ProducerActivity::Manufacturing, 2557) => matches!(group, Some(334 | 332 | 716 | 964)),
        (ProducerActivity::Manufacturing, 2559) => group == Some(873),
        (ProducerActivity::Manufacturing, 2561) => {
            matches!(category, Some(23 | 65 | 66)) || matches!(group, Some(536 | 1136))
        }
        (ProducerActivity::Manufacturing, 2575) => {
            matches!(group, Some(30 | 485 | 547 | 659 | 883 | 1538 | 4594 | 5120))
        }
        (ProducerActivity::Manufacturing, 2592) => category == Some(6),
        (ProducerActivity::Manufacturing, 2658) => group == Some(913),
        (ProducerActivity::Reaction, 2716) => group == Some(974),
        (ProducerActivity::Reaction, 2718) => matches!(group, Some(428 | 429 | 4932)),
        (ProducerActivity::Reaction, 2720) => matches!(group, Some(712 | 4096)),
        _ => false,
    }
}

#[derive(Debug)]
pub struct FundedResolver<'a> {
    data: &'a StaticData,
    catalog: &'a ProductionCatalog,
    semantics: &'a IndustrySemantics,
    guaranteed_sells: &'a BTreeMap<u32, f64>,
    decryptor_sells: BTreeMap<u32, f64>,
    memo: HashMap<u32, FundedPath>,
    active: BTreeSet<u32>,
    blockers: BTreeSet<u32>,
    saw_cycle: bool,
}

impl<'a> FundedResolver<'a> {
    pub fn new(
        data: &'a StaticData,
        catalog: &'a ProductionCatalog,
        semantics: &'a IndustrySemantics,
        guaranteed_sells: &'a BTreeMap<u32, f64>,
    ) -> Self {
        let decryptor_sells = semantics
            .decryptors
            .keys()
            .filter_map(|type_id| {
                guaranteed_sells
                    .get(type_id)
                    .copied()
                    .map(|price| (*type_id, price))
            })
            .collect();
        Self {
            data,
            catalog,
            semantics,
            guaranteed_sells,
            decryptor_sells,
            memo: HashMap::new(),
            active: BTreeSet::new(),
            blockers: BTreeSet::new(),
            saw_cycle: false,
        }
    }

    pub fn production_cost(
        &mut self,
        type_id: u32,
    ) -> std::result::Result<FundedPath, UnfundedPath> {
        self.memo.clear();
        self.active.clear();
        self.blockers.clear();
        self.saw_cycle = false;
        self.best_production(type_id).ok_or_else(|| UnfundedPath {
            type_id,
            name: self
                .data
                .item_type(type_id)
                .map(|item| item.name.clone())
                .unwrap_or_else(|| "<unknown type>".to_string()),
            reason: if self.catalog.has_recipe(type_id) {
                format!(
                    "all supported recipes are unfunded{}; blocking typeIDs [{}]",
                    if self.saw_cycle { " or cyclic" } else { "" },
                    self.blockers
                        .iter()
                        .take(8)
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                "no supported manufacturing or reaction recipe".to_string()
            },
        })
    }

    fn best_input(
        &mut self,
        type_id: u32,
    ) -> Option<(f64, FundedChoiceKind, BTreeSet<u32>, Vec<FundedChoice>)> {
        if self.active.contains(&type_id) {
            self.saw_cycle = true;
            self.blockers.insert(type_id);
            return None;
        }
        let buy = self
            .guaranteed_sells
            .get(&type_id)
            .copied()
            .filter(|price| *price > 0.0);
        let built = if let Some(cached) = self.memo.get(&type_id) {
            if cached.dependencies.is_disjoint(&self.active) {
                Some(cached.clone())
            } else {
                None
            }
        } else {
            let path = self.best_production(type_id);
            if let Some(path) = &path {
                self.memo.insert(type_id, path.clone());
            }
            path
        };
        match (buy, built) {
            (Some(price), Some(path)) if path.unit_cost < price => Some((
                path.unit_cost,
                FundedChoiceKind::Build,
                path.dependencies,
                path.choices,
            )),
            (Some(price), _) => Some((
                price,
                FundedChoiceKind::Buy,
                BTreeSet::from([type_id]),
                Vec::new(),
            )),
            (None, Some(path)) => Some((
                path.unit_cost,
                FundedChoiceKind::Build,
                path.dependencies,
                path.choices,
            )),
            (None, None) => {
                self.blockers.insert(type_id);
                None
            }
        }
    }

    fn best_production(&mut self, type_id: u32) -> Option<FundedPath> {
        if !self.active.insert(type_id) {
            return None;
        }
        let recipes = self
            .catalog
            .recipes
            .get(&type_id)
            .cloned()
            .unwrap_or_default();
        let mut best: Option<FundedPath> = None;
        for recipe in recipes {
            let candidate = if recipe.activity == ProducerActivity::Manufacturing
                && self
                    .catalog
                    .inventions
                    .contains_key(&recipe.blueprint_type_id)
            {
                self.invented_recipe_cost(&recipe)
            } else {
                self.ordinary_recipe_cost(&recipe, 10, MAX_MANUFACTURING_RUNS, None, true)
            };
            if candidate.as_ref().is_some_and(|path| {
                best.as_ref()
                    .is_none_or(|current| path.unit_cost < current.unit_cost)
            }) {
                best = candidate;
            }
        }
        self.active.remove(&type_id);
        best
    }

    fn ordinary_recipe_cost(
        &mut self,
        recipe: &ProductionRecipe,
        material_efficiency: i64,
        runs: i64,
        invention_cost: Option<f64>,
        require_reusable_recipe_access: bool,
    ) -> Option<FundedPath> {
        // A reusable blueprint/formula must be purchasable from guaranteed canonical supply.
        // Its one-time price proves access but is deliberately not amortized into unit cost.
        // Invention-produced BPCs pass `false`: their access is proven by the invention path.
        if require_reusable_recipe_access
            && !self
                .guaranteed_sells
                .get(&recipe.blueprint_type_id)
                .is_some_and(|price| *price > 0.0)
        {
            self.blockers.insert(recipe.blueprint_type_id);
            return None;
        }
        let item = self.data.item_type(recipe.product_type_id)?;
        let modifier = self.semantics.material_modifier(recipe.activity, item);
        let blueprint_modifier = if recipe.activity == ProducerActivity::Manufacturing {
            (1.0 - material_efficiency.max(0) as f64 / 100.0).max(0.0)
        } else {
            1.0
        };
        let mut total = invention_cost.unwrap_or(0.0);
        let mut choices = Vec::new();
        let mut dependencies = BTreeSet::from([recipe.product_type_id]);
        for material in &recipe.materials {
            if material.type_id == 0 || material.quantity <= 0 {
                return None;
            }
            let (unit_cost, kind, child_dependencies, child_choices) =
                self.best_input(material.type_id)?;
            let quantity =
                material_quantity(material.quantity, runs, blueprint_modifier * modifier);
            total += quantity as f64 * unit_cost;
            dependencies.extend(child_dependencies);
            choices.push(FundedChoice {
                type_id: material.type_id,
                kind,
                unit_cost,
            });
            choices.extend(child_choices);
        }
        let output = recipe.product_per_run.checked_mul(runs)?;
        if output <= 0 || !total.is_finite() {
            return None;
        }
        Some(FundedPath {
            unit_cost: total / output as f64,
            activity: if invention_cost.is_some() {
                FundedActivity::InventionManufacturing
            } else if recipe.activity == ProducerActivity::Reaction {
                FundedActivity::Reaction
            } else {
                FundedActivity::Manufacturing
            },
            blueprint_type_id: recipe.blueprint_type_id,
            choices,
            dependencies,
        })
    }

    fn invented_recipe_cost(&mut self, recipe: &ProductionRecipe) -> Option<FundedPath> {
        let inventions = self
            .catalog
            .inventions
            .get(&recipe.blueprint_type_id)?
            .clone();
        let mut options = vec![(
            None,
            DecryptorSemantics {
                probability_multiplier: 1.0,
                material_efficiency: 0,
                max_runs: 0,
            },
            0.0,
        )];
        options.extend(self.decryptor_sells.iter().filter_map(|(type_id, price)| {
            self.semantics
                .decryptors
                .get(type_id)
                .copied()
                .map(|semantics| (Some(*type_id), semantics, *price))
        }));
        let mut best: Option<FundedPath> = None;
        for invention in inventions {
            // Copying consumes no materials in EveJS. The original is therefore a required
            // guaranteed tool/lineage proof, but its one-time purchase price is not charged
            // to every invented unit in the long-run safety bound.
            let Some(source_bpo_price) = self
                .guaranteed_sells
                .get(&invention.source_blueprint_type_id)
                .copied()
            else {
                continue;
            };
            for (decryptor_type_id, decryptor, decryptor_price) in &options {
                let probability =
                    invention_probability(&invention, decryptor.probability_multiplier);
                if !(probability > 0.0) {
                    continue;
                }
                let mut attempt = *decryptor_price;
                let mut invention_choices = vec![FundedChoice {
                    type_id: invention.source_blueprint_type_id,
                    kind: FundedChoiceKind::Buy,
                    unit_cost: source_bpo_price,
                }];
                let mut invention_dependencies = BTreeSet::new();
                let mut valid = true;
                for material in &invention.materials {
                    let Some((unit_cost, kind, child_dependencies, child_choices)) =
                        self.best_input(material.type_id)
                    else {
                        valid = false;
                        break;
                    };
                    attempt += material.quantity.max(0) as f64 * unit_cost;
                    invention_dependencies.extend(child_dependencies);
                    invention_choices.push(FundedChoice {
                        type_id: material.type_id,
                        kind,
                        unit_cost,
                    });
                    invention_choices.extend(child_choices);
                }
                if !valid {
                    continue;
                }
                if let Some(type_id) = decryptor_type_id {
                    invention_choices.push(FundedChoice {
                        type_id: *type_id,
                        kind: FundedChoiceKind::Buy,
                        unit_cost: *decryptor_price,
                    });
                }
                let bpc_runs = (invention.output_runs + decryptor.max_runs).max(1);
                let invention_cost = attempt / probability;
                let mut candidate = self.ordinary_recipe_cost(
                    recipe,
                    INVENTION_BASE_ME + decryptor.material_efficiency,
                    bpc_runs,
                    Some(invention_cost),
                    false,
                );
                if let Some(path) = &mut candidate {
                    path.choices.extend(invention_choices);
                    path.dependencies.extend(invention_dependencies);
                    path.dependencies.insert(invention.source_blueprint_type_id);
                    path.dependencies.insert(invention.output_blueprint_type_id);
                }
                if candidate.as_ref().is_some_and(|path| {
                    best.as_ref()
                        .is_none_or(|current| path.unit_cost < current.unit_cost)
                }) {
                    best = candidate;
                }
            }
        }
        best
    }
}

fn material_quantity(base: i64, runs: i64, modifier: f64) -> i64 {
    let exact = base.max(0) as f64 * runs.max(0) as f64 * modifier;
    let rounded_hundredths = (exact * 100.0).round() / 100.0;
    (rounded_hundredths.ceil() as i64).max(runs)
}

fn invention_probability(invention: &InventionRecipe, decryptor_multiplier: f64) -> f64 {
    let skill_bonus = invention
        .skills
        .iter()
        .map(|type_id| {
            if LOWER_INVENTION_SKILLS.contains(type_id) {
                INVENTION_SKILL_PROBABILITY_LOWER
            } else {
                INVENTION_SKILL_PROBABILITY
            }
        })
        .sum::<f64>()
        * MAX_INVENTION_SKILL_LEVEL;
    (invention.probability * (1.0 + skill_bonus) * decryptor_multiplier).clamp(0.0, 1.0)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProductionDecision {
    pub type_id: u32,
    pub name: String,
    pub target_buy: f64,
    pub funded_cost: f64,
    pub safety_ceiling: f64,
    pub final_buy: f64,
    pub capped: bool,
    pub path: FundedPath,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OreDecision {
    pub type_id: u32,
    pub reference_target: f64,
    pub max_refine_value: f64,
    pub refine_ceiling: f64,
    pub final_buy: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefiningBuyBlocker {
    pub type_id: u32,
    pub name: String,
    pub missing_output_type_ids: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReprocessingFloorDecision {
    pub type_id: u32,
    pub original_sell: f64,
    pub perfect_reprocess_value: f64,
    pub final_sell: f64,
}

#[derive(Debug, Clone, Default)]
pub struct MarketSafetyReport {
    pub production: Vec<ProductionDecision>,
    pub unfunded: Vec<UnfundedPath>,
    pub ore: Vec<OreDecision>,
    pub reprocessing_floors: Vec<ReprocessingFloorDecision>,
}

fn refining_buy_blockers(
    rows: &[SeedRow],
    reprocessing: &ReprocessingStatic,
    ore_buy_kinds: &BTreeMap<u32, OreRefineKind>,
) -> Vec<RefiningBuyBlocker> {
    let buy_types = rows
        .iter()
        .filter(|row| row.writes_bid())
        .map(|row| row.type_id)
        .collect::<BTreeSet<_>>();
    rows.iter()
        .filter(|row| row.writes_bid() && ore_buy_kinds.contains_key(&row.type_id))
        .filter_map(|row| {
            let entry = reprocessing.get(row.type_id)?;
            if entry.portion_size <= 0 {
                return None;
            }
            let missing_output_type_ids = entry
                .materials
                .iter()
                .filter(|material| !buy_types.contains(&material.type_id))
                .map(|material| material.type_id)
                .collect::<Vec<_>>();
            (!missing_output_type_ids.is_empty()).then(|| RefiningBuyBlocker {
                type_id: row.type_id,
                name: row.name.clone(),
                missing_output_type_ids,
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OreRefineKind {
    Raw,
    Compressed,
}

// The legacy Core profile classifier uses category 25, excludes ice groups, and
// distinguishes compressed ore through the authoritative SDE pairing. Keep the
// same factual definition here so a policy profile rename cannot remove a Buy
// from the refine ceiling.
fn ore_refine_kind(
    type_id: u32,
    data: &StaticData,
    compressed_type_ids: &BTreeSet<u32>,
) -> Option<OreRefineKind> {
    let item = data.item_type(type_id)?;
    if item.category_id != Some(25)
        || item
            .group_name
            .as_deref()
            .is_some_and(|name| name.to_ascii_lowercase().contains("ice"))
    {
        return None;
    }
    Some(
        if data.is_authoritative_compressed_type(type_id) || compressed_type_ids.contains(&type_id)
        {
            OreRefineKind::Compressed
        } else {
            OreRefineKind::Raw
        },
    )
}

#[derive(Debug, Clone, Default)]
pub struct RareT2FallbackReport {
    pub fallbacks: RareFallbackBook,
    pub unfunded: Vec<UnfundedPath>,
}

/// Resolves only missing canonical RareT2 rows from the exact same fully funded invention
/// and production engine used by the Buy safety ceiling. The returned anchor is unit funded
/// cost; the existing RareT2 profile applies x0.70 when the final row is planned.
pub fn resolve_rare_t2_fallbacks(
    partial_plan: &SeedPlan,
    data: &StaticData,
    semantics: &IndustrySemantics,
    config: &SeederConfig,
) -> RareT2FallbackReport {
    let pricing = SeedPricingProfiles::from_config(config);
    let rows = plan_canonical_seed_rows(
        partial_plan,
        data,
        &pricing,
        config.canonical_market.quantity,
    );
    let eligible_type_ids = partial_plan
        .dropped_with(DropReason::RequiredRareReferenceUnavailable)
        .filter(|drop| {
            drop.role == Some(MarketRole::Rare) && drop.profile == Some(PricingProfile::RareT2)
        })
        .map(|drop| drop.type_id)
        .collect::<BTreeSet<_>>();
    resolve_rare_t2_fallbacks_v2(partial_plan, data, semantics, &rows, &eligible_type_ids)
}

/// V2 uses its already priced partial Sell book as the sole guaranteed supply.
/// The caller provides the eligible set from factual finished-T2 membership
/// and the final `rare_reference` Buy source, independent of profile names.
pub fn resolve_rare_t2_fallbacks_v2(
    partial_plan: &SeedPlan,
    data: &StaticData,
    semantics: &IndustrySemantics,
    rows: &[SeedRow],
    eligible_type_ids: &BTreeSet<u32>,
) -> RareT2FallbackReport {
    let guaranteed_sells = rows
        .iter()
        .filter(|row| row.writes_ask())
        .map(|row| (row.type_id, row.ask))
        .collect::<BTreeMap<_, _>>();
    let catalog = ProductionCatalog::from_static(data);
    let mut resolver = FundedResolver::new(data, &catalog, semantics, &guaranteed_sells);
    let mut report = RareT2FallbackReport::default();
    for dropped in partial_plan
        .dropped_with(DropReason::RequiredRareReferenceUnavailable)
        .filter(|drop| eligible_type_ids.contains(&drop.type_id))
    {
        if data.tech_level(dropped.type_id) != 2 {
            report.unfunded.push(UnfundedPath {
                type_id: dropped.type_id,
                name: dropped.name.clone(),
                reason: "RareT2 funded fallback requires Tech Level exactly 2".to_string(),
            });
            continue;
        }
        match resolver.production_cost(dropped.type_id) {
            Ok(path) => {
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
                let path_summary = format!(
                    "{} blueprint {}; {} direct/recursive Buy choices, {} Build choices",
                    path.activity.key(),
                    path.blueprint_type_id,
                    buys,
                    builds
                );
                report.fallbacks.insert(
                    dropped.type_id,
                    RareFallbackAnchor {
                        price: path.unit_cost,
                        source: PriceSource::FundedCostFallback,
                        provenance: RareFallbackProvenance::FundedCost {
                            funded_cost: path.unit_cost,
                            path_summary,
                        },
                    },
                );
            }
            Err(unfunded) => report.unfunded.push(unfunded),
        }
    }
    report
}

pub fn print_rare_t2_funded_resolution(report: &RareT2FallbackReport, config: &SeederConfig) {
    let multiplier = SeedPricingProfiles::from_config(config)
        .for_profile(PricingProfile::RareT2)
        .buy_multiplier;
    println!();
    println!(
        "[RareT2 funded evaluation] {} resolved, {} unresolved",
        report.fallbacks.len(),
        report.unfunded.len()
    );
    for (type_id, fallback) in &report.fallbacks {
        if let RareFallbackProvenance::FundedCost {
            funded_cost,
            path_summary,
        } = &fallback.provenance
        {
            println!(
                "  {}: funded cost {:.2}; target Buy x{} = {:.2}; {}",
                type_id,
                funded_cost,
                multiplier,
                funded_cost * multiplier,
                path_summary
            );
        }
    }
    for missing in &report.unfunded {
        println!(
            "  RequiredRareReferenceUnavailable {} {}: {}",
            missing.type_id, missing.name, missing.reason
        );
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProductionIntermediateReport {
    pub prices: ProductionIntermediatePriceBook,
    pub paths: BTreeMap<u32, FundedPath>,
}

/// Prices the reviewed 129-type convenience layer from recurring funded cost. The first
/// resolver intentionally cannot buy these intermediates because they are absent from the
/// partial plan; it must prove a complete build path. Once the resulting asks are in the
/// final rows, the ordinary safety resolver can choose `min(ask, build)` for later layers.
pub fn resolve_production_intermediates(
    partial_plan: &SeedPlan,
    data: &StaticData,
    semantics: &IndustrySemantics,
    config: &SeederConfig,
) -> Result<ProductionIntermediateReport> {
    let candidates = partial_plan
        .dropped_with(DropReason::UnpricedRoleReference)
        .filter(|drop| drop.role == Some(MarketRole::ProductionIntermediate))
        .map(|drop| drop.type_id)
        .collect::<BTreeSet<_>>();
    let expected = PRODUCTION_INTERMEDIATE_TYPE_IDS
        .iter()
        .chain(STRUCTURE_COMPONENT_PRODUCTION_INTERMEDIATE_TYPE_IDS)
        .copied()
        .collect::<BTreeSet<_>>();
    if candidates != expected {
        let missing = expected
            .difference(&candidates)
            .copied()
            .collect::<Vec<_>>();
        let extra = candidates
            .difference(&expected)
            .copied()
            .collect::<Vec<_>>();
        bail!(
            "ProductionIntermediate coverage must be exactly {} types (got {}); missing {:?}, extra {:?}",
            expected.len(),
            candidates.len(),
            missing,
            extra
        );
    }

    resolve_production_intermediate_candidates(
        partial_plan,
        data,
        semantics,
        config,
        expected.into_iter(),
    )
}

fn resolve_production_intermediate_candidates(
    partial_plan: &SeedPlan,
    data: &StaticData,
    semantics: &IndustrySemantics,
    config: &SeederConfig,
    candidate_type_ids: impl IntoIterator<Item = u32>,
) -> Result<ProductionIntermediateReport> {
    let pricing = SeedPricingProfiles::from_config(config);
    let rows = plan_canonical_seed_rows(
        partial_plan,
        data,
        &pricing,
        config.canonical_market.quantity,
    );
    resolve_production_intermediate_candidates_from_rows(data, semantics, &rows, candidate_type_ids)
}

/// The policy-selected funded candidates must be unresolved in the partial
/// plan and absent from its guaranteed Sell book. This prevents a recursive
/// intermediate from financing its own source price.
pub fn resolve_production_intermediates_v2(
    partial_plan: &SeedPlan,
    data: &StaticData,
    semantics: &IndustrySemantics,
    rows: &[SeedRow],
    expected: &BTreeSet<u32>,
) -> Result<ProductionIntermediateReport> {
    let candidates = partial_plan
        .dropped_with(DropReason::UnpricedRoleReference)
        .map(|drop| drop.type_id)
        .filter(|type_id| expected.contains(type_id))
        .collect::<BTreeSet<_>>();
    if &candidates != expected {
        let missing = expected
            .difference(&candidates)
            .copied()
            .collect::<Vec<_>>();
        bail!(
            "policy funded_cost intermediate coverage must be exactly {} unresolved partial types; missing {:?}",
            expected.len(),
            missing,
        );
    }
    if let Some(type_id) = rows
        .iter()
        .filter(|row| row.writes_ask())
        .map(|row| row.type_id)
        .find(|type_id| expected.contains(type_id))
    {
        bail!(
            "policy funded_cost intermediate type {type_id} already has a partial guaranteed Sell"
        );
    }
    resolve_production_intermediate_candidates_from_rows(
        data,
        semantics,
        rows,
        expected.iter().copied(),
    )
}

fn resolve_production_intermediate_candidates_from_rows(
    data: &StaticData,
    semantics: &IndustrySemantics,
    rows: &[SeedRow],
    candidate_type_ids: impl IntoIterator<Item = u32>,
) -> Result<ProductionIntermediateReport> {
    let guaranteed_sells = rows
        .iter()
        .filter(|row| row.writes_ask())
        .map(|row| (row.type_id, row.ask))
        .collect::<BTreeMap<_, _>>();
    let catalog = ProductionCatalog::from_static(data);
    let mut resolver = FundedResolver::new(data, &catalog, semantics, &guaranteed_sells);
    let mut report = ProductionIntermediateReport::default();
    let mut unfunded = Vec::new();
    for type_id in candidate_type_ids {
        match resolver.production_cost(type_id) {
            Ok(path) => {
                report.prices.insert(type_id, path.unit_cost);
                report.paths.insert(type_id, path);
            }
            Err(anomaly) => unfunded.push(anomaly),
        }
    }
    if !unfunded.is_empty() {
        bail!(
            "{} required ProductionIntermediate type(s) have no fully funded recurring production path; no fallback price was invented:\n  {}",
            unfunded.len(),
            unfunded
                .iter()
                .map(|entry| format!("{} {}: {}", entry.type_id, entry.name, entry.reason))
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
    Ok(report)
}

pub fn print_production_intermediate_resolution(
    report: &ProductionIntermediateReport,
    config: &SeederConfig,
) {
    let profile = SeedPricingProfiles::from_config(config)
        .for_profile(PricingProfile::ProductionIntermediate);
    println!();
    println!(
        "[ProductionIntermediate funded evaluation] {} resolved at funded cost: Sell x{}, Buy x{}",
        report.prices.len(),
        profile.sell_multiplier,
        profile.buy_multiplier
    );
    for (type_id, path) in report.paths.iter().take(20) {
        println!(
            "  {}: funded cost {:.2}; target Sell {:.2}; target Buy {:.2}; {} blueprint {}",
            type_id,
            path.unit_cost,
            path.unit_cost * profile.sell_multiplier,
            path.unit_cost * profile.buy_multiplier,
            path.activity.key(),
            path.blueprint_type_id
        );
    }
}

pub fn apply_market_safety(
    rows: &mut [SeedRow],
    data: &StaticData,
    reprocessing: &ReprocessingStatic,
    semantics: &IndustrySemantics,
    config: &SeederConfig,
) -> Result<MarketSafetyReport> {
    apply_market_safety_with_source_contract(
        rows,
        data,
        reprocessing,
        semantics,
        config,
        &BTreeSet::new(),
    )
}

/// A Buy whose declared source is an already validated recurring funded cost
/// still needs a funded path, but it must not be capped a second time against
/// a cost recomputed from the final Sell book. The V1 intermediate source is
/// resolved before those final Sells exist, so the second cap can move cents.
pub fn apply_market_safety_with_source_contract(
    rows: &mut [SeedRow],
    data: &StaticData,
    reprocessing: &ReprocessingStatic,
    semantics: &IndustrySemantics,
    config: &SeederConfig,
    funded_source_type_ids: &BTreeSet<u32>,
) -> Result<MarketSafetyReport> {
    let enabled_buy_types = rows
        .iter()
        .filter(|row| row.writes_bid())
        .map(|row| row.type_id)
        .collect::<BTreeSet<_>>();
    if let Some(type_id) = funded_source_type_ids.difference(&enabled_buy_types).next() {
        bail!("funded_cost source contract names type {type_id} without a final Buy");
    }
    let catalog = ProductionCatalog::from_static(data);
    let profile_buy_targets = rows
        .iter()
        .map(|row| (row.type_id, row.bid))
        .collect::<BTreeMap<_, _>>();
    let guaranteed_sells = rows
        .iter()
        .filter(|row| row.writes_ask())
        .map(|row| (row.type_id, row.ask))
        .collect::<BTreeMap<_, _>>();
    let mut resolver = FundedResolver::new(data, &catalog, semantics, &guaranteed_sells);
    let mut report = MarketSafetyReport::default();
    let compressed_type_ids = reprocessing
        .compressed_by_source
        .values()
        .copied()
        .collect::<BTreeSet<_>>();
    let ore_buy_kinds = rows
        .iter()
        .filter(|row| row.writes_bid())
        .filter_map(|row| {
            ore_refine_kind(row.type_id, data, &compressed_type_ids).map(|kind| (row.type_id, kind))
        })
        .collect::<BTreeMap<_, _>>();

    for row in rows.iter_mut().filter(|row| row.writes_bid()) {
        if funded_source_type_ids.contains(&row.type_id) {
            resolver.production_cost(row.type_id).map_err(|unfunded| {
                anyhow::anyhow!(
                    "funded_cost source contract for type {} ({}) has no guaranteed funded path: {}",
                    row.type_id,
                    row.name,
                    unfunded.reason,
                )
            })?;
            continue;
        }
        if ore_buy_kinds.contains_key(&row.type_id) || !catalog.has_recipe(row.type_id) {
            continue;
        }
        match resolver.production_cost(row.type_id) {
            Ok(path) => {
                let target_buy = row.bid;
                let safety_ceiling =
                    floor_isk(path.unit_cost * config.market_safety.production_safety_factor);
                if safety_ceiling < MIN_SEED_PRICE {
                    bail!(
                        "funded production ceiling for type {} ({}) is {:.4}, below the minimum market price; refusing to create an unsafe Buy",
                        row.type_id,
                        row.name,
                        path.unit_cost * config.market_safety.production_safety_factor,
                    );
                }
                row.bid = row.bid.min(safety_ceiling);
                report.production.push(ProductionDecision {
                    type_id: row.type_id,
                    name: row.name.clone(),
                    target_buy,
                    funded_cost: path.unit_cost,
                    safety_ceiling,
                    final_buy: row.bid,
                    capped: row.bid < target_buy,
                    path,
                });
            }
            Err(unfunded) => report.unfunded.push(unfunded),
        }
    }

    let mut buy_prices = rows
        .iter()
        .filter(|row| row.writes_bid())
        .map(|row| (row.type_id, row.bid))
        .collect::<BTreeMap<_, _>>();
    let refining_blockers = refining_buy_blockers(rows, reprocessing, &ore_buy_kinds);
    if !refining_blockers.is_empty() {
        bail!(
            "{} ore refining Buy ceiling blocker(s) found in the complete sweep; no price was invented:\n  {}",
            refining_blockers.len(),
            refining_blockers
                .iter()
                .map(|entry| format!(
                    "{} {} [missing output Buy(s): {:?}]",
                    entry.type_id, entry.name, entry.missing_output_type_ids
                ))
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
    for _ in 0..64 {
        let mut changed = false;
        for row in rows
            .iter_mut()
            .filter(|row| row.writes_bid() && ore_buy_kinds.contains_key(&row.type_id))
        {
            let Some(entry) = reprocessing.get(row.type_id) else {
                continue;
            };
            if entry.portion_size <= 0 {
                continue;
            }
            let output_buy = entry
                .materials
                .iter()
                .filter_map(|material| {
                    buy_prices
                        .get(&material.type_id)
                        .map(|price| material.quantity.max(0) as f64 * price)
                })
                .sum::<f64>()
                / entry.portion_size as f64;
            let max_refine_value = output_buy * config.market_safety.max_refine_yield;
            let share = if ore_buy_kinds.get(&row.type_id) == Some(&OreRefineKind::Raw) {
                config.market_safety.raw_ore_refine_share
            } else {
                config.market_safety.compressed_ore_refine_share
            };
            let ceiling = floor_isk(max_refine_value * share);
            if ceiling < MIN_SEED_PRICE {
                bail!(
                    "refining ceiling for ore type {} ({}) is {:.4}, below the minimum market price; refusing to create an unsafe Buy",
                    row.type_id,
                    row.name,
                    max_refine_value * share,
                );
            }
            if ceiling < row.bid {
                row.bid = ceiling;
                buy_prices.insert(row.type_id, ceiling);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for row in rows
        .iter()
        .filter(|row| row.writes_bid() && ore_buy_kinds.contains_key(&row.type_id))
    {
        let Some(entry) = reprocessing.get(row.type_id) else {
            continue;
        };
        if entry.portion_size <= 0 {
            continue;
        }
        let output_buy = entry
            .materials
            .iter()
            .filter_map(|material| {
                buy_prices
                    .get(&material.type_id)
                    .map(|price| material.quantity.max(0) as f64 * price)
            })
            .sum::<f64>()
            / entry.portion_size as f64;
        let max_refine_value = output_buy * config.market_safety.max_refine_yield;
        let share = if ore_buy_kinds.get(&row.type_id) == Some(&OreRefineKind::Raw) {
            config.market_safety.raw_ore_refine_share
        } else {
            config.market_safety.compressed_ore_refine_share
        };
        report.ore.push(OreDecision {
            type_id: row.type_id,
            reference_target: profile_buy_targets
                .get(&row.type_id)
                .copied()
                .unwrap_or(row.bid),
            max_refine_value,
            refine_ceiling: floor_isk(max_refine_value * share),
            final_buy: row.bid,
        });
    }

    for row in rows.iter_mut().filter(|row| row.writes_ask()) {
        let Some(entry) = reprocessing.get(row.type_id) else {
            continue;
        };
        if entry.portion_size <= 0 {
            continue;
        }
        let perfect = entry
            .materials
            .iter()
            .filter_map(|material| {
                buy_prices
                    .get(&material.type_id)
                    .map(|price| material.quantity.max(0) as f64 * price)
            })
            .sum::<f64>()
            / entry.portion_size as f64;
        let required = ceil_isk(perfect * (1.0 + config.index_seed.reprocessing_floor_margin));
        if required > row.ask {
            let original_sell = row.ask;
            row.ask = required;
            report.reprocessing_floors.push(ReprocessingFloorDecision {
                type_id: row.type_id,
                original_sell,
                perfect_reprocess_value: perfect,
                final_sell: row.ask,
            });
        }
    }

    for row in rows.iter() {
        if row.writes_ask() && row.writes_bid() && row.bid > row.ask {
            bail!(
                "Phase 2 safety crossed type {} ({}): Buy {} > Sell {}",
                row.type_id,
                row.name,
                row.bid,
                row.ask
            );
        }
    }
    verify_market_safety(rows, reprocessing, config, &report)?;
    Ok(report)
}

fn verify_market_safety(
    rows: &[SeedRow],
    reprocessing: &ReprocessingStatic,
    config: &SeederConfig,
    report: &MarketSafetyReport,
) -> Result<()> {
    let by_type = rows
        .iter()
        .map(|row| (row.type_id, row))
        .collect::<BTreeMap<_, _>>();
    let final_buys = rows
        .iter()
        .filter(|row| row.writes_bid())
        .map(|row| (row.type_id, row.bid))
        .collect::<BTreeMap<_, _>>();

    for decision in &report.production {
        let row = by_type.get(&decision.type_id).copied().with_context(|| {
            format!(
                "funded-production report references missing type {}",
                decision.type_id
            )
        })?;
        if row.bid > decision.target_buy || row.bid > decision.safety_ceiling {
            bail!(
                "funded-production verification failed for type {}: final Buy {:.2}, target {:.2}, ceiling {:.2}",
                row.type_id,
                row.bid,
                decision.target_buy,
                decision.safety_ceiling,
            );
        }
    }
    for decision in &report.ore {
        let row = by_type.get(&decision.type_id).copied().with_context(|| {
            format!(
                "ore safety report references missing type {}",
                decision.type_id
            )
        })?;
        if row.bid > decision.reference_target || row.bid > decision.refine_ceiling {
            bail!(
                "ore safety verification failed for type {}: final Buy {:.2}, target {:.2}, ceiling {:.2}",
                row.type_id,
                row.bid,
                decision.reference_target,
                decision.refine_ceiling,
            );
        }
    }
    for row in rows.iter().filter(|row| row.writes_ask()) {
        let Some(entry) = reprocessing.get(row.type_id) else {
            continue;
        };
        if entry.portion_size <= 0 {
            continue;
        }
        let perfect = entry
            .materials
            .iter()
            .filter_map(|material| {
                final_buys
                    .get(&material.type_id)
                    .map(|price| material.quantity.max(0) as f64 * price)
            })
            .sum::<f64>()
            / entry.portion_size as f64;
        let required = ceil_isk(perfect * (1.0 + config.index_seed.reprocessing_floor_margin));
        if row.ask < required {
            bail!(
                "profile-aware reprocessing verification failed for type {}: Sell {:.2} < required {:.2}",
                row.type_id,
                row.ask,
                required,
            );
        }
    }
    Ok(())
}

fn floor_isk(value: f64) -> f64 {
    (value.max(0.0) * 100.0).floor() / 100.0
}

fn ceil_isk(value: f64) -> f64 {
    (value.max(0.0) * 100.0).ceil() / 100.0
}

pub fn print_market_safety_report(report: &MarketSafetyReport) {
    const SAMPLE: usize = 8;
    let capped = report
        .production
        .iter()
        .filter(|entry| entry.capped)
        .count();
    println!();
    println!(
        "[funded safety] {} funded production paths; {} Buy targets capped",
        report.production.len(),
        capped
    );
    for entry in report
        .production
        .iter()
        .filter(|entry| entry.capped)
        .take(SAMPLE)
    {
        let choices = entry
            .path
            .choices
            .iter()
            .take(5)
            .map(|choice| {
                format!(
                    "{}:{}",
                    choice.type_id,
                    match choice.kind {
                        FundedChoiceKind::Buy => "buy",
                        FundedChoiceKind::Build => "build",
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  {} {}: target {:.2}, funded {:.2}, x0.95 ceiling {:.2}, final {:.2}; {} blueprint {}; {}",
            entry.type_id,
            entry.name,
            entry.target_buy,
            entry.funded_cost,
            entry.safety_ceiling,
            entry.final_buy,
            entry.path.activity.key(),
            entry.path.blueprint_type_id,
            choices
        );
    }
    if let Some(entry) = report.production.iter().find(|entry| !entry.capped) {
        println!(
            "  uncapped sample {} {}: target {:.2} <= funded ceiling {:.2}; final {:.2}",
            entry.type_id, entry.name, entry.target_buy, entry.safety_ceiling, entry.final_buy
        );
    }
    println!("  unfunded supported paths: {}", report.unfunded.len());
    for entry in report.unfunded.iter().take(SAMPLE) {
        println!("    {} {}: {}", entry.type_id, entry.name, entry.reason);
    }
    println!("  ore refine ceilings evaluated: {}", report.ore.len());
    for entry in report.ore.iter().take(SAMPLE) {
        println!(
            "    {}: target {:.2}, max refine {:.2}, ceiling {:.2}, final {:.2}",
            entry.type_id,
            entry.reference_target,
            entry.max_refine_value,
            entry.refine_ceiling,
            entry.final_buy
        );
    }
    println!(
        "  perfect-yield Sell floors raised: {}",
        report.reprocessing_floors.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{BucketFlags, MarketSidePolicy};
    use crate::seedplan::{DropReason, DroppedType, SeedSides, SeedableType};
    use crate::staticdata::{
        BlueprintActivities, BlueprintActivity, DogmaProjection, InventionActivity,
        InventionProductEntry, ItemTypeRecord, ProductEntry, ReprocessingMaterial,
        ReprocessingType, SkillEntry,
    };

    fn item(type_id: u32, category_id: u32, group_id: u32, name: &str) -> ItemTypeRecord {
        ItemTypeRecord {
            type_id,
            group_id: Some(group_id),
            category_id: Some(category_id),
            group_name: Some("Fixture Group".to_string()),
            name: name.to_string(),
            mass: None,
            volume: None,
            capacity: None,
            portion_size: Some(1),
            race_id: None,
            base_price: Some(100.0),
            market_group_id: Some(1),
            icon_id: None,
            sound_id: None,
            graphic_id: None,
            radius: None,
            published: true,
        }
    }

    fn manufacturing(
        blueprint_type_id: u32,
        product_type_id: u32,
        output: i64,
        materials: &[(u32, i64)],
    ) -> BlueprintDefinition {
        BlueprintDefinition {
            blueprint_type_id,
            blueprint_name: format!("Blueprint {blueprint_type_id}"),
            product_type_id,
            product_name: format!("Product {product_type_id}"),
            max_production_limit: Some(1_000),
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
                        quantity: output,
                    }],
                    time: Some(1),
                }),
                ..BlueprintActivities::default()
            },
        }
    }

    fn reaction(
        blueprint_type_id: u32,
        product_type_id: u32,
        output: i64,
        materials: &[(u32, i64)],
    ) -> BlueprintDefinition {
        let mut blueprint = manufacturing(blueprint_type_id, product_type_id, output, materials);
        blueprint.product_type_id = 0;
        blueprint.activities.reaction = blueprint.activities.manufacturing.take();
        blueprint
    }

    fn invention(
        source_blueprint: u32,
        output_blueprint: u32,
        probability: f64,
        output_runs: i64,
        materials: &[(u32, i64)],
    ) -> BlueprintDefinition {
        BlueprintDefinition {
            blueprint_type_id: source_blueprint,
            blueprint_name: format!("Source Blueprint {source_blueprint}"),
            product_type_id: 0,
            product_name: String::new(),
            max_production_limit: Some(300),
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
                        type_id: output_blueprint,
                        quantity: output_runs,
                        probability: Some(probability),
                    }],
                    skills: vec![SkillEntry {
                        type_id: 111,
                        level: 1,
                    }],
                    time: Some(1),
                }),
                ..BlueprintActivities::default()
            },
        }
    }

    fn data(items: Vec<ItemTypeRecord>, blueprints: Vec<BlueprintDefinition>) -> StaticData {
        let mut items = items;
        items.sort_by_key(|entry| entry.type_id);
        let item_type_index = items
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.type_id, index))
            .collect();
        StaticData {
            dir: Path::new("fixture").to_path_buf(),
            stations: Vec::new(),
            station_index: HashMap::new(),
            solar_systems: Vec::new(),
            solar_system_index: HashMap::new(),
            item_types: items,
            item_type_index,
            blueprints,
            dogma: HashMap::<u32, DogmaProjection>::new(),
            compressed_type_ids: BTreeSet::new(),
        }
    }

    fn resolver<'a>(
        data: &'a StaticData,
        catalog: &'a ProductionCatalog,
        semantics: &'a IndustrySemantics,
        sells: &'a BTreeMap<u32, f64>,
    ) -> FundedResolver<'a> {
        FundedResolver::new(data, catalog, semantics, sells)
    }

    fn row(
        type_id: u32,
        name: &str,
        profile: PricingProfile,
        role: MarketRole,
        ask: f64,
        bid: f64,
        sides: SeedSides,
    ) -> SeedRow {
        SeedRow {
            type_id,
            name: name.to_string(),
            bucket: "B",
            junk: false,
            role,
            profile,
            cost: 100.0,
            ask,
            bid,
            sides,
            quantity: 1_000,
        }
    }

    #[test]
    fn simple_t1_buy_is_capped_at_ninety_five_percent_of_funded_cost() {
        let data = data(
            vec![
                item(1, 4, 1, "Input"),
                item(2, 7, 1, "Capped Output"),
                item(3, 7, 1, "Uncapped Output"),
            ],
            vec![
                manufacturing(100, 2, 1, &[(1, 10)]),
                manufacturing(101, 3, 1, &[(1, 10)]),
            ],
        );
        let catalog = ProductionCatalog::from_static(&data);
        let sells = BTreeMap::from([(1, 10.0), (100, 1_000.0), (101, 1_000.0)]);
        let semantics = IndustrySemantics::default();
        let mut resolver = resolver(&data, &catalog, &semantics, &sells);
        let path = resolver.production_cost(2).expect("fully funded");
        assert_eq!(path.unit_cost, 90.0);
        assert_eq!(floor_isk(path.unit_cost * 0.95), 85.5);

        let mut rows = vec![
            row(
                1,
                "Input",
                PricingProfile::Minerals,
                MarketRole::Core,
                10.0,
                6.2,
                SeedSides::BOTH,
            ),
            row(
                2,
                "Capped Output",
                PricingProfile::CoreGeneral,
                MarketRole::Core,
                120.0,
                100.0,
                SeedSides::BOTH,
            ),
            row(
                3,
                "Uncapped Output",
                PricingProfile::CoreGeneral,
                MarketRole::Core,
                120.0,
                50.0,
                SeedSides::BOTH,
            ),
            row(
                100,
                "Capped Output Blueprint",
                PricingProfile::Bpo,
                MarketRole::Bpo,
                1_000.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
            row(
                101,
                "Uncapped Output Blueprint",
                PricingProfile::Bpo,
                MarketRole::Bpo,
                1_000.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
        ];
        let report = apply_market_safety(
            &mut rows,
            &data,
            &ReprocessingStatic::default(),
            &semantics,
            &SeederConfig::default(),
        )
        .expect("safety applies");
        assert_eq!(rows[1].bid, 85.5);
        assert_eq!(rows[2].bid, 50.0);
        assert!(
            report
                .production
                .iter()
                .any(|entry| entry.type_id == 2 && entry.capped)
        );
        assert!(
            report
                .production
                .iter()
                .any(|entry| entry.type_id == 3 && !entry.capped)
        );
    }

    #[test]
    fn producible_buy_is_capped_independent_of_legacy_role_and_profile() {
        let data = data(
            vec![item(1, 4, 1, "Input"), item(2, 7, 1, "Output")],
            vec![manufacturing(100, 2, 1, &[(1, 10)])],
        );
        let mut rows = vec![
            row(
                1,
                "Input",
                PricingProfile::Minerals,
                MarketRole::Core,
                10.0,
                6.0,
                SeedSides::SELL_ONLY,
            ),
            row(
                2,
                "Output",
                PricingProfile::Bridge,
                MarketRole::Bridge,
                120.0,
                100.0,
                SeedSides::BOTH,
            ),
            row(
                100,
                "BPO",
                PricingProfile::Bpo,
                MarketRole::Bpo,
                1_000.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
        ];
        let report = apply_market_safety(
            &mut rows,
            &data,
            &ReprocessingStatic::default(),
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .unwrap();
        assert_eq!(rows[1].bid, 85.5);
        assert!(
            report
                .production
                .iter()
                .any(|decision| decision.type_id == 2)
        );
    }

    #[test]
    fn funded_source_contract_keeps_validated_target_without_second_cent_cap() {
        let data = data(
            vec![item(1, 4, 1, "Input"), item(2, 7, 1, "Intermediate")],
            vec![manufacturing(100, 2, 1, &[(1, 10)])],
        );
        let make_rows = || {
            vec![
                row(
                    1,
                    "Input",
                    PricingProfile::Minerals,
                    MarketRole::Core,
                    10.0,
                    6.0,
                    SeedSides::SELL_ONLY,
                ),
                row(
                    2,
                    "Intermediate",
                    PricingProfile::ProductionIntermediate,
                    MarketRole::ProductionIntermediate,
                    120.0,
                    85.51,
                    SeedSides::BOTH,
                ),
                row(
                    100,
                    "BPO",
                    PricingProfile::Bpo,
                    MarketRole::Bpo,
                    1_000.0,
                    0.01,
                    SeedSides::SELL_ONLY,
                ),
            ]
        };
        let mut unmarked = make_rows();
        apply_market_safety(
            &mut unmarked,
            &data,
            &ReprocessingStatic::default(),
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .unwrap();
        assert_eq!(unmarked[1].bid, 85.50);

        let mut marked = make_rows();
        let report = apply_market_safety_with_source_contract(
            &mut marked,
            &data,
            &ReprocessingStatic::default(),
            &IndustrySemantics::default(),
            &SeederConfig::default(),
            &BTreeSet::from([2]),
        )
        .unwrap();
        assert_eq!(marked[1].bid, 85.51);
        assert!(report.production.is_empty());
    }

    #[test]
    fn funded_source_contract_rejects_missing_guaranteed_acquisition() {
        let data = data(
            vec![item(1, 4, 1, "Input"), item(2, 7, 1, "Intermediate")],
            vec![manufacturing(100, 2, 1, &[(1, 10)])],
        );
        let mut rows = vec![
            row(
                1,
                "Input",
                PricingProfile::Minerals,
                MarketRole::Core,
                10.0,
                6.0,
                SeedSides::BUY_ONLY,
            ),
            row(
                2,
                "Intermediate",
                PricingProfile::ProductionIntermediate,
                MarketRole::ProductionIntermediate,
                120.0,
                85.51,
                SeedSides::BOTH,
            ),
            row(
                100,
                "BPO",
                PricingProfile::Bpo,
                MarketRole::Bpo,
                1_000.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
        ];
        let error = apply_market_safety_with_source_contract(
            &mut rows,
            &data,
            &ReprocessingStatic::default(),
            &IndustrySemantics::default(),
            &SeederConfig::default(),
            &BTreeSet::from([2]),
        )
        .expect_err("a synthetic Buy cannot prove funded_cost acquisition");
        assert!(format!("{error:#}").contains("no guaranteed funded path"));
    }

    #[test]
    fn synthetic_buy_does_not_fund_a_production_input() {
        let data = data(
            vec![item(1, 4, 1, "Unavailable Input"), item(2, 7, 1, "Output")],
            vec![manufacturing(100, 2, 1, &[(1, 10)])],
        );
        let mut rows = vec![
            row(
                1,
                "Unavailable Input",
                PricingProfile::CoreGeneral,
                MarketRole::Core,
                10.0,
                6.0,
                SeedSides::BUY_ONLY,
            ),
            row(
                2,
                "Output",
                PricingProfile::Bridge,
                MarketRole::Bridge,
                120.0,
                100.0,
                SeedSides::BOTH,
            ),
            row(
                100,
                "BPO",
                PricingProfile::Bpo,
                MarketRole::Bpo,
                1_000.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
        ];
        let report = apply_market_safety(
            &mut rows,
            &data,
            &ReprocessingStatic::default(),
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .unwrap();
        assert_eq!(rows[1].bid, 100.0);
        assert!(report.production.is_empty());
        assert!(report.unfunded.iter().any(|path| path.type_id == 2));
    }

    #[test]
    fn ore_buy_ceiling_uses_catalog_facts_after_profile_rename() {
        let data = data(
            vec![item(1, 4, 18, "Mineral"), item(2, 25, 1, "Ore")],
            Vec::new(),
        );
        let reprocessing = ReprocessingStatic::new(
            vec![ReprocessingType {
                type_id: 2,
                name: "Ore".into(),
                portion_size: 100,
                is_refinable: true,
                is_recyclable: true,
                family: "ore".into(),
                materials: vec![ReprocessingMaterial {
                    type_id: 1,
                    quantity: 400,
                }],
            }],
            BTreeMap::new(),
        );
        let mut rows = vec![
            row(
                1,
                "Mineral",
                PricingProfile::Minerals,
                MarketRole::Core,
                3.0,
                2.0,
                SeedSides::BOTH,
            ),
            row(
                2,
                "Ore",
                PricingProfile::Bridge,
                MarketRole::Bridge,
                105.0,
                58.0,
                SeedSides::BOTH,
            ),
        ];
        let report = apply_market_safety(
            &mut rows,
            &data,
            &reprocessing,
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .unwrap();
        assert_eq!(rows[1].bid, floor_isk(0.906 * 8.0 * 0.75));
        assert_eq!(report.ore.len(), 1);
    }

    #[test]
    fn progression_npc_sell_is_a_guaranteed_funded_input() {
        let data = data(
            vec![
                item(56_201, 4, 4_086, "Astrahus Upwell Quantum Core"),
                item(90_010, 7, 1, "Progression Consumer"),
                item(90_011, 9, 1, "Progression Consumer Blueprint"),
            ],
            vec![manufacturing(90_011, 90_010, 1, &[(56_201, 1)])],
        );
        let mut rows = vec![
            row(
                56_201,
                "Astrahus Upwell Quantum Core",
                PricingProfile::ProgressionNpc,
                MarketRole::ProgressionNpc,
                600_000_000.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
            row(
                90_010,
                "Progression Consumer",
                PricingProfile::CoreGeneral,
                MarketRole::Core,
                900_000_000.0,
                800_000_000.0,
                SeedSides::BOTH,
            ),
            row(
                90_011,
                "Progression Consumer Blueprint",
                PricingProfile::Bpo,
                MarketRole::Bpo,
                1_000_000.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
        ];
        let report = apply_market_safety(
            &mut rows,
            &data,
            &ReprocessingStatic::default(),
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .expect("progression NPC Sell funds the manufacturing path");
        let decision = report
            .production
            .iter()
            .find(|entry| entry.type_id == 90_010)
            .expect("consumer has a funded production decision");
        assert_eq!(decision.funded_cost, 600_000_000.0);
        assert_eq!(rows[0].sides, SeedSides::SELL_ONLY);
        assert!(!rows[0].writes_bid());
    }

    #[test]
    fn intermediate_chooses_buy_or_build_whichever_is_cheaper() {
        for (intermediate_ask, leaf_ask, expected_kind, expected_cost) in [
            (5.0, 10.0, FundedChoiceKind::Buy, 5.0),
            (20.0, 1.0, FundedChoiceKind::Build, 1.0),
        ] {
            let data = data(
                vec![
                    item(1, 4, 1, "Leaf"),
                    item(2, 4, 1, "Intermediate"),
                    item(3, 7, 1, "Final"),
                ],
                vec![
                    manufacturing(100, 2, 1, &[(1, 1)]),
                    manufacturing(101, 3, 1, &[(2, 1)]),
                ],
            );
            let catalog = ProductionCatalog::from_static(&data);
            let sells = BTreeMap::from([
                (1, leaf_ask),
                (2, intermediate_ask),
                (100, 1_000.0),
                (101, 1_000.0),
            ]);
            let semantics = IndustrySemantics::default();
            let mut resolver = resolver(&data, &catalog, &semantics, &sells);
            let path = resolver.production_cost(3).expect("funded");
            assert_eq!(path.choices[0].kind, expected_kind);
            assert_eq!(path.unit_cost, expected_cost);
        }
    }

    #[test]
    fn multiple_producers_choose_the_cheapest_valid_recipe() {
        let data = data(
            vec![
                item(1, 4, 1, "Expensive"),
                item(2, 4, 1, "Cheap"),
                item(3, 7, 1, "Final"),
            ],
            vec![
                manufacturing(100, 3, 1, &[(1, 1)]),
                manufacturing(101, 3, 1, &[(2, 1)]),
            ],
        );
        let catalog = ProductionCatalog::from_static(&data);
        assert_eq!(catalog.recipes.get(&3).map(Vec::len), Some(2));
        let sells = BTreeMap::from([(1, 10.0), (2, 2.0), (100, 1_000.0), (101, 1_000.0)]);
        let semantics = IndustrySemantics::default();
        let mut resolver = resolver(&data, &catalog, &semantics, &sells);
        assert_eq!(
            resolver
                .production_cost(3)
                .expect("funded")
                .blueprint_type_id,
            101
        );
    }

    #[test]
    fn missing_terminal_and_unfunded_cycle_never_fabricate_a_cost() {
        let data = data(
            vec![
                item(1, 4, 1, "A"),
                item(2, 4, 1, "B"),
                item(3, 7, 1, "Missing Leaf Product"),
            ],
            vec![
                manufacturing(100, 1, 1, &[(2, 1)]),
                manufacturing(101, 2, 1, &[(1, 1)]),
                manufacturing(102, 3, 1, &[(99, 1)]),
            ],
        );
        let catalog = ProductionCatalog::from_static(&data);
        let sells = BTreeMap::from([(100, 1_000.0), (101, 1_000.0), (102, 1_000.0)]);
        let semantics = IndustrySemantics::default();
        let mut resolver = resolver(&data, &catalog, &semantics, &sells);
        assert!(resolver.production_cost(1).is_err());
        assert!(resolver.production_cost(3).is_err());
    }

    #[test]
    fn ordinary_manufacturing_requires_reusable_blueprint_access_without_amortizing_it() {
        let data = data(
            vec![item(1, 4, 1, "Material"), item(2, 7, 1, "Product")],
            vec![manufacturing(100, 2, 1, &[(1, 10)])],
        );
        let catalog = ProductionCatalog::from_static(&data);
        let semantics = IndustrySemantics::default();
        let materials_only = BTreeMap::from([(1, 10.0)]);
        let mut missing_access = resolver(&data, &catalog, &semantics, &materials_only);
        let error = missing_access
            .production_cost(2)
            .expect_err("materials alone do not prove blueprint access");
        assert!(error.reason.contains("100"), "{error:?}");

        let fully_funded = BTreeMap::from([(1, 10.0), (100, 9_999_999.0)]);
        let mut with_access = resolver(&data, &catalog, &semantics, &fully_funded);
        let path = with_access
            .production_cost(2)
            .expect("blueprint is available");
        assert_eq!(
            path.unit_cost, 90.0,
            "one-time blueprint price is access proof, not recurring unit cost"
        );
    }

    #[test]
    fn reaction_requires_formula_access_without_amortizing_it() {
        let data = data(
            vec![
                item(1, 4, 1, "Reactant"),
                item(2, 4, 974, "Reaction Product"),
            ],
            vec![reaction(100, 2, 1, &[(1, 10)])],
        );
        let catalog = ProductionCatalog::from_static(&data);
        let semantics = IndustrySemantics::default();
        let materials_only = BTreeMap::from([(1, 2.0)]);
        let mut missing_access = resolver(&data, &catalog, &semantics, &materials_only);
        let error = missing_access
            .production_cost(2)
            .expect_err("reactants alone do not prove formula access");
        assert!(error.reason.contains("100"), "{error:?}");

        let fully_funded = BTreeMap::from([(1, 2.0), (100, 5_000_000.0)]);
        let mut with_access = resolver(&data, &catalog, &semantics, &fully_funded);
        let path = with_access
            .production_cost(2)
            .expect("formula is available");
        assert_eq!(path.activity, FundedActivity::Reaction);
        assert_eq!(path.unit_cost, 20.0);
    }

    #[test]
    fn invention_uses_activity_average_probability_for_every_output() {
        let invention_blueprint = BlueprintDefinition {
            blueprint_type_id: 100,
            blueprint_name: "T1 Blueprint".to_string(),
            product_type_id: 0,
            product_name: String::new(),
            max_production_limit: Some(300),
            published: true,
            activities: BlueprintActivities {
                invention: Some(InventionActivity {
                    materials: vec![MaterialEntry {
                        type_id: 10,
                        quantity: 2,
                    }],
                    products: vec![
                        InventionProductEntry {
                            type_id: 200,
                            quantity: 1,
                            probability: Some(0.2),
                        },
                        InventionProductEntry {
                            type_id: 201,
                            quantity: 1,
                            probability: Some(0.6),
                        },
                    ],
                    skills: Vec::new(),
                    time: Some(1),
                }),
                ..BlueprintActivities::default()
            },
        };
        let data = data(
            vec![
                item(10, 4, 333, "Datacore"),
                item(11, 4, 1, "Advanced Component"),
                item(100, 9, 1, "T1 Blueprint"),
                item(30, 7, 1, "T2 Output A"),
                item(31, 7, 1, "T2 Output B"),
            ],
            vec![
                invention_blueprint,
                manufacturing(200, 30, 1, &[(11, 10)]),
                manufacturing(201, 31, 1, &[(11, 10)]),
            ],
        );
        let catalog = ProductionCatalog::from_static(&data);
        assert_eq!(catalog.inventions[&200][0].probability, 0.4);
        assert_eq!(catalog.inventions[&201][0].probability, 0.4);
        let sells = BTreeMap::from([(10, 5.0), (11, 2.0), (100, 1_000.0)]);
        let semantics = IndustrySemantics::default();
        let mut resolver = resolver(&data, &catalog, &semantics, &sells);
        let first = resolver.production_cost(30).expect("first output funded");
        let second = resolver.production_cost(31).expect("second output funded");
        assert_eq!(first.unit_cost, 45.0);
        assert_eq!(second.unit_cost, 45.0);
    }

    #[test]
    fn tech_two_path_includes_invention_and_bridge_inputs_without_changing_bridge_sides() {
        let data = data(
            vec![
                item(10, 4, 333, "Datacore"),
                item(11, 4, 1, "Advanced Component"),
                item(100, 9, 1, "T1 Blueprint"),
                item(30, 7, 1, "Tech II Final"),
            ],
            vec![
                invention(100, 200, 0.5, 1, &[(10, 2)]),
                manufacturing(200, 30, 1, &[(11, 10)]),
            ],
        );
        let mut rows = vec![
            row(
                10,
                "Datacore",
                PricingProfile::Bridge,
                MarketRole::Bridge,
                5.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
            row(
                100,
                "T1 Blueprint",
                PricingProfile::Bpo,
                MarketRole::Bpo,
                1_000.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
            row(
                11,
                "Advanced Component",
                PricingProfile::CoreGeneral,
                MarketRole::Core,
                2.0,
                1.0,
                SeedSides::BOTH,
            ),
            row(
                30,
                "Tech II Final",
                PricingProfile::RareT2,
                MarketRole::Rare,
                100.0,
                70.0,
                SeedSides::BUY_ONLY,
            ),
        ];
        let report = apply_market_safety(
            &mut rows,
            &data,
            &ReprocessingStatic::default(),
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .expect("safety resolves");
        let t2 = rows.iter().find(|row| row.type_id == 30).unwrap();
        let decision = report
            .production
            .iter()
            .find(|entry| entry.type_id == 30)
            .unwrap();
        assert_eq!(
            decision.path.activity,
            FundedActivity::InventionManufacturing
        );
        assert!((decision.funded_cost - 37.142_857_142_857_14).abs() < 1e-9);
        assert_eq!(decision.safety_ceiling, 35.28);
        assert!(t2.bid <= floor_isk(decision.funded_cost * 0.95));
        assert_eq!(t2.sides, SeedSides::BUY_ONLY);
        assert!(
            rows.iter().all(|row| row.type_id != 200),
            "invented output BPC is not required as a market Sell"
        );
        assert_eq!(rows[0].sides, SeedSides::SELL_ONLY);
        assert!(!rows[0].writes_bid());
    }

    fn rare_t2_partial_plan(with_recipe: bool, tech_level: f64) -> (StaticData, SeedPlan) {
        let mut data = data(
            vec![
                item(10, 4, 333, "Datacore"),
                item(11, 4, 1, "Advanced Component"),
                item(100, 9, 1, "T1 Blueprint"),
                item(30, 7, 1, "Unreferenced Tech II Final"),
            ],
            if with_recipe {
                vec![
                    invention(100, 200, 0.5, 1, &[(10, 2)]),
                    manufacturing(200, 30, 1, &[(11, 10)]),
                ]
            } else {
                Vec::new()
            },
        );
        data.dogma.insert(
            30,
            DogmaProjection {
                tech_level: Some(tech_level),
                meta_group_id: Some(2),
            },
        );
        let mut plan = SeedPlan::default();
        for (type_id, price, role, profile) in [
            (10, 5.0, MarketRole::Bridge, PricingProfile::Bridge),
            (11, 2.0, MarketRole::Core, PricingProfile::CoreGeneral),
            (100, 1_000_000.0, MarketRole::Bpo, PricingProfile::Bpo),
        ] {
            plan.seedable.insert(
                type_id,
                SeedableType {
                    type_id,
                    price,
                    source: PriceSource::CcpEsiAverage,
                    bucket: BucketFlags::default(),
                    role,
                    profile,
                    sides: SeedSides::SELL_ONLY,
                },
            );
        }
        plan.dropped.push(DroppedType {
            type_id: 30,
            name: "Unreferenced Tech II Final".to_string(),
            bucket: BucketFlags::default(),
            reason: DropReason::RequiredRareReferenceUnavailable,
            role: Some(MarketRole::Rare),
            profile: Some(PricingProfile::RareT2),
            blocking_leaves: Vec::new(),
        });
        (data, plan)
    }

    #[test]
    fn missing_rare_t2_uses_funded_cost_times_existing_point_seven_profile() {
        let (data, plan) = rare_t2_partial_plan(true, 2.0);
        let config = SeederConfig::default();
        let report =
            resolve_rare_t2_fallbacks(&plan, &data, &IndustrySemantics::default(), &config);
        assert!(report.unfunded.is_empty());
        let anchor = &report.fallbacks[&30];
        assert_eq!(anchor.source, PriceSource::FundedCostFallback);
        let multiplier = SeedPricingProfiles::from_config(&config)
            .for_profile(PricingProfile::RareT2)
            .buy_multiplier;
        assert_eq!(multiplier, 0.70);
        assert!((anchor.price * multiplier - anchor.price * 0.70).abs() < 1e-9);
    }

    #[test]
    fn missing_rare_t2_without_a_funded_path_is_reported_without_a_fabricated_price() {
        let (data, plan) = rare_t2_partial_plan(false, 2.0);
        let report = resolve_rare_t2_fallbacks(
            &plan,
            &data,
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        );
        assert!(!report.fallbacks.contains_key(&30));
        assert_eq!(report.unfunded.len(), 1);
        assert!(report.unfunded[0].reason.contains("no supported"));
    }

    #[test]
    fn v2_rare_fallback_uses_policy_eligibility_and_v2_sell_rows() {
        let (data, mut plan) = rare_t2_partial_plan(true, 2.0);
        plan.dropped[0].role = Some(MarketRole::Core);
        plan.dropped[0].profile = Some(PricingProfile::CoreGeneral);
        let rows = vec![
            row(
                10,
                "Datacore",
                PricingProfile::Bridge,
                MarketRole::Bridge,
                5.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
            row(
                11,
                "Advanced Component",
                PricingProfile::CoreGeneral,
                MarketRole::Core,
                3.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
            row(
                100,
                "T1 Blueprint",
                PricingProfile::Bpo,
                MarketRole::Bpo,
                1_000_000.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
        ];
        let report = resolve_rare_t2_fallbacks_v2(
            &plan,
            &data,
            &IndustrySemantics::default(),
            &rows,
            &BTreeSet::from([30]),
        );
        assert!(report.unfunded.is_empty());
        assert!(report.fallbacks[&30].price > 0.0);
        let ineligible = resolve_rare_t2_fallbacks_v2(
            &plan,
            &data,
            &IndustrySemantics::default(),
            &rows,
            &BTreeSet::new(),
        );
        assert!(ineligible.fallbacks.is_empty());
    }

    fn production_intermediate_partial_plan(with_recipe: bool) -> (StaticData, SeedPlan) {
        const INTERMEDIATE: u32 = 11_539;
        let data = data(
            vec![
                item(10, 4, 1, "Funded Raw Input"),
                item(100, 9, 1, "Reusable Blueprint"),
                item(INTERMEDIATE, 17, 334, "Nanoelectrical Microprocessor"),
            ],
            if with_recipe {
                vec![manufacturing(100, INTERMEDIATE, 1, &[(10, 10)])]
            } else {
                Vec::new()
            },
        );
        let mut plan = SeedPlan::default();
        for (type_id, price, role, profile) in [
            (10, 10.0, MarketRole::Core, PricingProfile::CoreGeneral),
            (100, 10_000.0, MarketRole::Bpo, PricingProfile::Bpo),
        ] {
            plan.seedable.insert(
                type_id,
                SeedableType {
                    type_id,
                    price,
                    source: PriceSource::CcpEsiAverage,
                    bucket: BucketFlags::default(),
                    role,
                    profile,
                    sides: SeedSides::SELL_ONLY,
                },
            );
        }
        plan.dropped.push(DroppedType {
            type_id: INTERMEDIATE,
            name: "Nanoelectrical Microprocessor".to_string(),
            bucket: BucketFlags::default(),
            reason: DropReason::UnpricedRoleReference,
            role: Some(MarketRole::ProductionIntermediate),
            profile: Some(PricingProfile::ProductionIntermediate),
            blocking_leaves: Vec::new(),
        });
        (data, plan)
    }

    #[test]
    fn production_intermediate_uses_one_funded_cost_for_safe_buy_and_sell() {
        const INTERMEDIATE: u32 = 11_539;
        let (data, partial) = production_intermediate_partial_plan(true);
        let config = SeederConfig::default();
        let report = resolve_production_intermediate_candidates(
            &partial,
            &data,
            &IndustrySemantics::default(),
            &config,
            [INTERMEDIATE],
        )
        .expect("the recurring production path is fully funded");
        let funded_cost = report.prices[&INTERMEDIATE];
        assert!(funded_cost > 0.0);
        let profile = SeedPricingProfiles::from_config(&config)
            .for_profile(PricingProfile::ProductionIntermediate);
        assert_eq!(profile.sell_multiplier, 1.25);
        assert_eq!(profile.buy_multiplier, 0.95);
        let row = row(
            INTERMEDIATE,
            "Nanoelectrical Microprocessor",
            PricingProfile::ProductionIntermediate,
            MarketRole::ProductionIntermediate,
            profile.ask(funded_cost),
            profile.bid(funded_cost),
            SeedSides::BOTH,
        );
        assert!(row.writes_ask());
        assert!(row.writes_bid());
        assert_eq!(row.role.sides(), MarketSidePolicy::BOTH);
        assert_eq!(row.ask, profile.ask(funded_cost));
        assert_eq!(row.bid, profile.bid(funded_cost));
        assert!(row.bid < funded_cost);
        assert!(row.bid < row.ask);
    }

    #[test]
    fn v2_intermediate_funding_uses_policy_membership_and_partial_sells() {
        const INTERMEDIATE: u32 = 11_539;
        let (data, partial) = production_intermediate_partial_plan(true);
        let rows = vec![
            row(
                10,
                "Funded Raw Input",
                PricingProfile::CoreGeneral,
                MarketRole::Core,
                12.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
            row(
                100,
                "Reusable Blueprint",
                PricingProfile::Bpo,
                MarketRole::Bpo,
                1_000.0,
                0.01,
                SeedSides::SELL_ONLY,
            ),
        ];
        let report = resolve_production_intermediates_v2(
            &partial,
            &data,
            &IndustrySemantics::default(),
            &rows,
            &BTreeSet::from([INTERMEDIATE]),
        )
        .expect("policy-selected intermediate has a funded V2 path");
        assert_eq!(report.prices[&INTERMEDIATE], 108.0);

        let mut self_funded = rows;
        self_funded.push(row(
            INTERMEDIATE,
            "Intermediate",
            PricingProfile::ProductionIntermediate,
            MarketRole::ProductionIntermediate,
            1.0,
            0.01,
            SeedSides::SELL_ONLY,
        ));
        assert!(
            resolve_production_intermediates_v2(
                &partial,
                &data,
                &IndustrySemantics::default(),
                &self_funded,
                &BTreeSet::from([INTERMEDIATE]),
            )
            .is_err()
        );
    }

    #[test]
    fn production_intermediate_missing_funded_path_fails_closed() {
        const INTERMEDIATE: u32 = 11_539;
        let (data, partial) = production_intermediate_partial_plan(false);
        let error = resolve_production_intermediate_candidates(
            &partial,
            &data,
            &IndustrySemantics::default(),
            &SeederConfig::default(),
            [INTERMEDIATE],
        )
        .expect_err("no external/static fallback is permitted");
        assert!(format!("{error:#}").contains("no fully funded recurring production path"));
    }

    #[test]
    fn bridge_signaller_funds_neurolink_protection_cell_without_a_buy_side() {
        const SIGNALLER: u32 = 57_450;
        const RESERVOIR: u32 = 57_483;
        const FILTER: u32 = 57_482;
        const ENHANCER: u32 = 57_458;
        const ELECTRONICS: u32 = 9_842;
        const SYNAPSES: u32 = 2_346;
        const BLUEPRINT: u32 = 57_525;
        const CELL: u32 = 57_488;
        let data = data(
            vec![
                item(SIGNALLER, 17, 526, "Electro-Neural Signaller"),
                item(RESERVOIR, 17, 1, "Neurolink Enhancer Reservoir"),
                item(FILTER, 17, 1, "Genetic Safeguard Filter"),
                item(ENHANCER, 17, 1, "Meta-Operant Neurolink Enhancer"),
                item(ELECTRONICS, 17, 1, "Miniature Electronics"),
                item(SYNAPSES, 17, 1, "Synthetic Synapses"),
                item(BLUEPRINT, 9, 1, "Neurolink Protection Cell Blueprint"),
                item(CELL, 17, 873, "Neurolink Protection Cell"),
            ],
            vec![manufacturing(
                BLUEPRINT,
                CELL,
                1,
                &[
                    (SIGNALLER, 1),
                    (RESERVOIR, 1),
                    (FILTER, 5),
                    (ENHANCER, 10),
                    (ELECTRONICS, 75),
                    (SYNAPSES, 300),
                ],
            )],
        );
        let mut plan = SeedPlan::default();
        for (type_id, role, profile) in [
            (SIGNALLER, MarketRole::Bridge, PricingProfile::Bridge),
            (RESERVOIR, MarketRole::Core, PricingProfile::CoreGeneral),
            (FILTER, MarketRole::Core, PricingProfile::CoreGeneral),
            (ENHANCER, MarketRole::Core, PricingProfile::CoreGeneral),
            (ELECTRONICS, MarketRole::Core, PricingProfile::CoreGeneral),
            (SYNAPSES, MarketRole::Core, PricingProfile::CoreGeneral),
            (BLUEPRINT, MarketRole::Bpo, PricingProfile::Bpo),
        ] {
            plan.seedable.insert(
                type_id,
                SeedableType {
                    type_id,
                    price: 100.0,
                    source: PriceSource::CcpEsiAverage,
                    bucket: BucketFlags::default(),
                    role,
                    profile,
                    sides: SeedSides::SELL_ONLY,
                },
            );
        }
        plan.dropped.push(DroppedType {
            type_id: CELL,
            name: "Neurolink Protection Cell".to_string(),
            bucket: BucketFlags::default(),
            reason: DropReason::UnpricedRoleReference,
            role: Some(MarketRole::ProductionIntermediate),
            profile: Some(PricingProfile::ProductionIntermediate),
            blocking_leaves: Vec::new(),
        });

        assert_eq!(plan.seedable[&SIGNALLER].sides, SeedSides::SELL_ONLY);
        let report = resolve_production_intermediate_candidates(
            &plan,
            &data,
            &IndustrySemantics::default(),
            &SeederConfig::default(),
            [CELL],
        )
        .expect("Bridge-supplied Electro-Neural Signaller completes the funded route");
        assert!(report.prices[&CELL] > 0.0);
    }

    #[test]
    fn later_funded_layers_choose_cheapest_build_or_intermediate_ask() {
        const INTERMEDIATE: u32 = 11_539;
        const OUTPUT: u32 = 30;
        let data = data(
            vec![
                item(10, 4, 1, "Raw Input"),
                item(100, 9, 1, "Intermediate Blueprint"),
                item(101, 9, 1, "Output Blueprint"),
                item(INTERMEDIATE, 17, 334, "Nanoelectrical Microprocessor"),
                item(OUTPUT, 7, 1, "Finished Consumer"),
            ],
            vec![
                manufacturing(100, INTERMEDIATE, 1, &[(10, 1)]),
                manufacturing(101, OUTPUT, 1, &[(INTERMEDIATE, 1)]),
            ],
        );
        let catalog = ProductionCatalog::from_static(&data);
        let semantics = IndustrySemantics::default();

        let expensive_ask =
            BTreeMap::from([(10, 10.0), (100, 1.0), (101, 1.0), (INTERMEDIATE, 50.0)]);
        let mut expensive_resolver = resolver(&data, &catalog, &semantics, &expensive_ask);
        let built = expensive_resolver
            .production_cost(OUTPUT)
            .expect("output funded by building input");
        assert!(built.choices.iter().any(|choice| {
            choice.type_id == INTERMEDIATE && choice.kind == FundedChoiceKind::Build
        }));

        let cheap_ask = BTreeMap::from([(10, 10.0), (100, 1.0), (101, 1.0), (INTERMEDIATE, 1.0)]);
        let mut cheap_resolver = resolver(&data, &catalog, &semantics, &cheap_ask);
        let bought = cheap_resolver
            .production_cost(OUTPUT)
            .expect("output funded by buying input");
        assert!(bought.choices.iter().any(|choice| {
            choice.type_id == INTERMEDIATE && choice.kind == FundedChoiceKind::Buy
        }));
    }

    #[test]
    fn tech_three_never_uses_the_rare_t2_funded_fallback() {
        let (data, plan) = rare_t2_partial_plan(true, 3.0);
        let report = resolve_rare_t2_fallbacks(
            &plan,
            &data,
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        );
        assert!(report.fallbacks.is_empty());
        assert!(report.unfunded[0].reason.contains("exactly 2"));
    }

    #[test]
    fn raw_and_compressed_ore_use_distinct_refine_progression_ceilings() {
        let data = data(
            vec![
                item(1, 4, 18, "Mineral"),
                item(2, 25, 1, "Raw Ore"),
                item(3, 25, 1, "Compressed Ore"),
            ],
            Vec::new(),
        );
        let reprocessing = ReprocessingStatic::new(
            vec![
                ReprocessingType {
                    type_id: 2,
                    name: "Raw Ore".into(),
                    portion_size: 100,
                    is_refinable: true,
                    is_recyclable: true,
                    family: "ore".into(),
                    materials: vec![ReprocessingMaterial {
                        type_id: 1,
                        quantity: 400,
                    }],
                },
                ReprocessingType {
                    type_id: 3,
                    name: "Compressed Ore".into(),
                    portion_size: 100,
                    is_refinable: true,
                    is_recyclable: true,
                    family: "ore".into(),
                    materials: vec![ReprocessingMaterial {
                        type_id: 1,
                        quantity: 400,
                    }],
                },
            ],
            BTreeMap::from([(2, 3)]),
        );
        let mut rows = vec![
            row(
                1,
                "Mineral",
                PricingProfile::Minerals,
                MarketRole::Core,
                3.0,
                2.0,
                SeedSides::BOTH,
            ),
            row(
                2,
                "Raw Ore",
                PricingProfile::RawOre,
                MarketRole::Core,
                105.0,
                58.0,
                SeedSides::BOTH,
            ),
            row(
                3,
                "Compressed Ore",
                PricingProfile::CompressedOre,
                MarketRole::Core,
                105.0,
                52.0,
                SeedSides::BOTH,
            ),
        ];
        apply_market_safety(
            &mut rows,
            &data,
            &reprocessing,
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .unwrap();
        assert_eq!(rows[1].bid, floor_isk(0.906 * 8.0 * 0.75));
        assert_eq!(rows[2].bid, floor_isk(0.906 * 8.0 * 0.68));
        assert_ne!(rows[1].bid, rows[2].bid);
        let config = SeederConfig::default();
        assert_eq!(
            config
                .pricing_profiles
                .get("raw_ore")
                .unwrap()
                .buy_multiplier,
            0.58
        );
        assert_eq!(
            config
                .pricing_profiles
                .get("compressed_ore")
                .unwrap()
                .buy_multiplier,
            0.52
        );
        assert_eq!(config.market_safety.raw_ore_refine_share, 0.75);
        assert_eq!(config.market_safety.compressed_ore_refine_share, 0.68);
    }

    #[test]
    fn moon_bridge_resource_is_sell_only_and_skips_the_ore_buy_ceiling() {
        let data = data(
            vec![
                item(1, 4, 427, "Moon Material"),
                item(2, 25, 1920, "Moon Resource"),
            ],
            Vec::new(),
        );
        let reprocessing = ReprocessingStatic::new(
            vec![ReprocessingType {
                type_id: 2,
                name: "Moon Resource".into(),
                portion_size: 100,
                is_refinable: true,
                is_recyclable: true,
                family: "moon_ore".into(),
                materials: vec![ReprocessingMaterial {
                    type_id: 1,
                    quantity: 40,
                }],
            }],
            BTreeMap::new(),
        );
        let mut rows = vec![row(
            2,
            "Moon Resource",
            PricingProfile::Bridge,
            MarketRole::Bridge,
            1_020.0,
            1_000.0,
            SeedSides::SELL_ONLY,
        )];
        let report = apply_market_safety(
            &mut rows,
            &data,
            &reprocessing,
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .expect("Sell-only Bridge resources do not have a Buy ceiling");
        assert!(rows[0].writes_ask());
        assert!(!rows[0].writes_bid());
        assert_eq!(rows[0].ask, 1_020.0);
        assert!(report.ore.is_empty());
    }

    #[test]
    fn mineable_moon_ore_ceiling_uses_refined_moon_material_buy() {
        let data = data(
            vec![
                item(1, 17, 427, "Moon Material"),
                item(2, 25, 1920, "Moon Resource"),
            ],
            Vec::new(),
        );
        let reprocessing = ReprocessingStatic::new(
            vec![ReprocessingType {
                type_id: 2,
                name: "Moon Resource".into(),
                portion_size: 100,
                is_refinable: true,
                is_recyclable: true,
                family: "moon_ore".into(),
                materials: vec![ReprocessingMaterial {
                    type_id: 1,
                    quantity: 40,
                }],
            }],
            BTreeMap::new(),
        );
        let mut rows = vec![
            row(
                1,
                "Moon Material",
                PricingProfile::CoreGeneral,
                MarketRole::Core,
                120.0,
                100.0,
                SeedSides::BOTH,
            ),
            row(
                2,
                "Moon Resource",
                PricingProfile::RawOre,
                MarketRole::Core,
                100.0,
                80.0,
                SeedSides::BOTH,
            ),
        ];
        let report = apply_market_safety(
            &mut rows,
            &data,
            &reprocessing,
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .expect("refined moon material Buy closes the moon-ore ceiling");
        assert_eq!(rows[1].bid, floor_isk(0.4 * 100.0 * 0.906 * 0.75));
        assert_eq!(report.ore.len(), 1);
    }

    #[test]
    fn complete_refining_blocker_sweep_reports_every_core_input_and_missing_output() {
        let data = data(
            vec![
                item(1, 4, 18, "Priced Mineral"),
                item(9, 4, 427, "Unbuyable Output"),
                item(2, 25, 1, "Natural Ore A"),
                item(3, 25, 1, "Natural Ore B"),
            ],
            Vec::new(),
        );
        let reprocessing = ReprocessingStatic::new(
            vec![
                ReprocessingType {
                    type_id: 2,
                    name: "Natural Ore A".into(),
                    portion_size: 100,
                    is_refinable: true,
                    is_recyclable: true,
                    family: "ore".into(),
                    materials: vec![ReprocessingMaterial {
                        type_id: 9,
                        quantity: 40,
                    }],
                },
                ReprocessingType {
                    type_id: 3,
                    name: "Natural Ore B".into(),
                    portion_size: 100,
                    is_refinable: true,
                    is_recyclable: true,
                    family: "ore".into(),
                    materials: vec![
                        ReprocessingMaterial {
                            type_id: 1,
                            quantity: 20,
                        },
                        ReprocessingMaterial {
                            type_id: 9,
                            quantity: 20,
                        },
                    ],
                },
            ],
            BTreeMap::new(),
        );
        let mut rows = vec![
            row(
                1,
                "Priced Mineral",
                PricingProfile::Minerals,
                MarketRole::Core,
                3.0,
                2.0,
                SeedSides::BOTH,
            ),
            row(
                2,
                "Natural Ore A",
                PricingProfile::RawOre,
                MarketRole::Core,
                100.0,
                58.0,
                SeedSides::BOTH,
            ),
            row(
                3,
                "Natural Ore B",
                PricingProfile::CompressedOre,
                MarketRole::Core,
                100.0,
                52.0,
                SeedSides::BOTH,
            ),
        ];
        let error = apply_market_safety(
            &mut rows,
            &data,
            &reprocessing,
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .expect_err("every missing output Buy must fail in one aggregate sweep");
        let message = format!("{error:#}");
        assert!(message.contains("2 ore refining Buy"), "{message}");
        assert!(message.contains("2 Natural Ore A"), "{message}");
        assert!(message.contains("3 Natural Ore B"), "{message}");
        assert!(message.contains("[9]"), "{message}");
    }

    #[test]
    fn perfect_yield_floor_uses_final_output_buy_and_changes_sell_only() {
        let data = data(
            vec![item(1, 4, 18, "Mineral"), item(2, 7, 1, "Hull")],
            Vec::new(),
        );
        let reprocessing = ReprocessingStatic::new(
            vec![ReprocessingType {
                type_id: 2,
                name: "Hull".into(),
                portion_size: 1,
                is_refinable: false,
                is_recyclable: true,
                family: "recycling".into(),
                materials: vec![ReprocessingMaterial {
                    type_id: 1,
                    quantity: 100,
                }],
            }],
            BTreeMap::new(),
        );
        let mut rows = vec![
            row(
                1,
                "Mineral",
                PricingProfile::Minerals,
                MarketRole::Core,
                3.0,
                2.0,
                SeedSides::BOTH,
            ),
            row(
                2,
                "Hull",
                PricingProfile::CoreGeneral,
                MarketRole::Core,
                100.0,
                50.0,
                SeedSides::BOTH,
            ),
        ];
        apply_market_safety(
            &mut rows,
            &data,
            &reprocessing,
            &IndustrySemantics::default(),
            &SeederConfig::default(),
        )
        .unwrap();
        assert_eq!(rows[1].ask, 200.20);
        assert_eq!(rows[1].bid, 50.0);
    }

    #[test]
    fn facility_and_rig_modifiers_are_product_specific_not_one_blanket_factor() {
        let module = item(1, 7, 1, "Module");
        let ship = item(2, 6, 25, "Frigate");
        let mut semantics = IndustrySemantics::fixture(0.99);
        semantics.add_fixture_rig(2538, 0.95);
        assert_eq!(
            semantics.material_modifier(ProducerActivity::Manufacturing, &module),
            0.99 * 0.95
        );
        assert_eq!(
            semantics.material_modifier(ProducerActivity::Manufacturing, &ship),
            0.99
        );
        assert_eq!(
            semantics.material_modifier(ProducerActivity::Reaction, &module),
            1.0
        );
    }

    #[test]
    fn representative_deep_structure_chain_resolves_recursively() {
        let data = data(
            vec![
                item(1, 4, 1, "Leaf"),
                item(2, 4, 334, "Component"),
                item(3, 4, 873, "Capital Component"),
                item(4, 6, 941, "Orca"),
            ],
            vec![
                reaction(100, 2, 1, &[(1, 2)]),
                manufacturing(101, 3, 1, &[(2, 3)]),
                manufacturing(102, 4, 1, &[(3, 4)]),
            ],
        );
        let catalog = ProductionCatalog::from_static(&data);
        let sells = BTreeMap::from([(1, 1.0), (100, 1_000.0), (101, 1_000.0), (102, 1_000.0)]);
        let semantics = IndustrySemantics::default();
        let mut resolver = resolver(&data, &catalog, &semantics, &sells);
        let path = resolver
            .production_cost(4)
            .expect("deep chain is fully funded");
        assert!(path.unit_cost > 0.0);
        assert_eq!(path.choices[0].kind, FundedChoiceKind::Build);
    }

    #[test]
    fn role_side_contracts_used_by_safety_remain_pinned() {
        assert_eq!(MarketRole::Core.sides(), MarketSidePolicy::BOTH);
        assert_eq!(MarketRole::Bridge.sides(), MarketSidePolicy::SELL_ONLY);
        assert_eq!(MarketRole::Rare.sides(), MarketSidePolicy::BUY_ONLY);
        assert_eq!(BucketFlags::default().is_classified(), false);
    }

    #[test]
    fn configured_real_static_data_matches_the_runtime_material_semantics() {
        let Some(dir) = std::env::var_os("EVEJS_PHASE2_STATIC_DATA").map(std::path::PathBuf::from)
        else {
            return;
        };
        let data = StaticData::load(&dir).expect("runtime static data loads read-only");
        let semantics =
            IndustrySemantics::load(&dir, &data).expect("industry dogma seam validates");
        assert!((semantics.manufacturing_hull_modifier - 0.99).abs() < 1e-9);
        let module = data
            .item_types
            .iter()
            .find(|item| item.published && item.category_id == Some(7))
            .expect("published module");
        assert!(semantics.material_modifier(ProducerActivity::Manufacturing, module) < 0.99);
        let reaction_product = data
            .item_types
            .iter()
            .find(|item| item.published && item.group_id == Some(974))
            .expect("reaction product");
        assert!(semantics.material_modifier(ProducerActivity::Reaction, reaction_product) < 1.0);
        assert!(!semantics.decryptors.is_empty());
    }
}
