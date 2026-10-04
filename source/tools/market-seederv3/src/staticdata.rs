//! Loads the generated Public EveJS static data tables the v3 seeder classifies against.
//!
//! Inputs live at `<static_data_dir>/<table>/data.json` and are produced by
//! `tools\DatabaseCreator\CreateDatabase.bat`. v3 reads five tables:
//!
//! | table                | used for                                              |
//! |----------------------|-------------------------------------------------------|
//! | `stations`           | seed station resolution, NPC overlay placement         |
//! | `solarSystems`       | region/system summaries                                |
//! | `itemTypes`          | the published + market-group universe, basePrice leaves|
//! | `industryBlueprints` | manufacturable / reaction / invention lineage (§2.1)   |
//! | `typeDogma`          | attributes 422 (Tech Level) and 1692 (metaGroupID)     |
//! | `reprocessingStatic` | authoritative raw → compressed type relationships      |
//!
//! `typeDogma` is 23 MB and `industryBlueprints` 21 MB, so both are projected down while
//! parsing: only `blueprintDefinitions` is materialised from the blueprint file, and only
//! the two attributes the classifier needs survive from the dogma file.
//!
//! The general loader projects only `compressedTypeBySourceTypeID` out of
//! `reprocessingStatic`; full refine yields remain on-demand through [`ReprocessingStatic::load`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::config::StaticDataDirSource;

/// Tech Level dogma attribute. Absent means Tech 1 (plan §2.1 rule 3).
pub const ATTRIBUTE_TECH_LEVEL: u32 = 422;
/// metaGroupID dogma attribute. Present and >= 3 means faction/storyline/officer/deadspace.
pub const ATTRIBUTE_META_GROUP_ID: u32 = 1692;

/// Authoritative CCP variant-family metadata from the raw JSONL SDE matching the generated
/// tables. No numeric price field is parsed or retained.
#[derive(Debug, Clone)]
pub struct VariantFamilies {
    pub build: u64,
    pub types_path: PathBuf,
    parents: HashMap<u32, u32>,
}

#[derive(Debug, Deserialize)]
struct RawSdeHeader {
    #[serde(rename = "buildNumber")]
    build_number: u64,
}

#[derive(Debug, Deserialize)]
struct RawVariantType {
    #[serde(rename = "_key")]
    type_id: u32,
    #[serde(rename = "variationParentTypeID")]
    variation_parent_type_id: Option<u32>,
}

impl VariantFamilies {
    /// Loads only `typeID -> variationParentTypeID` after proving the raw and generated SDE
    /// build numbers are identical. The path follows DatabaseCreator's existing `_local`
    /// layout and fails closed if that correspondence cannot be demonstrated.
    pub fn load_for_generated_data(static_data_dir: &Path, expected_build: u64) -> Result<Self> {
        let local_dir = static_data_dir
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot locate the raw SDE beside generated static data {}",
                    static_data_dir.display()
                )
            })?;
        let raw_dir = local_dir
            .join("sde")
            .join(format!("eve-online-static-data-{expected_build}-jsonl"));
        let header_path = raw_dir.join("_sde.jsonl");
        let header_line =
            BufReader::new(fs::File::open(&header_path).with_context(|| {
                format!("failed to open raw SDE header {}", header_path.display())
            })?)
            .lines()
            .next()
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("raw SDE header {} is empty", header_path.display()))?;
        let header: RawSdeHeader = serde_json::from_str(&header_line)
            .with_context(|| format!("failed to parse raw SDE header {}", header_path.display()))?;
        if header.build_number != expected_build {
            bail!(
                "raw SDE build {} at {} does not match generated static-data build {}; variant-family metadata cannot be used",
                header.build_number,
                header_path.display(),
                expected_build
            );
        }

        let types_path = raw_dir.join("types.jsonl");
        let reader =
            BufReader::new(fs::File::open(&types_path).with_context(|| {
                format!("failed to open raw SDE types {}", types_path.display())
            })?);
        let mut parents = HashMap::new();
        for (index, line) in reader.lines().enumerate() {
            let line = line.with_context(|| {
                format!("failed reading {} line {}", types_path.display(), index + 1)
            })?;
            let row: RawVariantType = serde_json::from_str(&line).with_context(|| {
                format!("failed parsing {} line {}", types_path.display(), index + 1)
            })?;
            if let Some(parent) = row.variation_parent_type_id.filter(|parent| *parent != 0) {
                parents.insert(row.type_id, parent);
            }
        }
        Ok(Self {
            build: expected_build,
            types_path,
            parents,
        })
    }

    pub fn parent(&self, type_id: u32) -> Option<u32> {
        self.parents.get(&type_id).copied()
    }

    #[cfg(test)]
    pub fn fixture(parents: &[(u32, u32)]) -> Self {
        Self {
            build: 1,
            types_path: PathBuf::from("fixture-types.jsonl"),
            parents: parents.iter().copied().collect(),
        }
    }
}

/// Resolution order, identical to market-seederv2: configured value, else
/// `EVEJS_GAMESTORE_DATA_DIR`, else the in-repo `_local` default.
pub fn resolve_static_data_dir(configured: Option<&Path>) -> (PathBuf, StaticDataDirSource) {
    if let Some(dir) = configured {
        return (dir.to_path_buf(), StaticDataDirSource::Config);
    }
    if let Some(dir) = env::var_os("EVEJS_GAMESTORE_DATA_DIR") {
        return (PathBuf::from(dir), StaticDataDirSource::Environment);
    }
    (
        PathBuf::from("../../_local/gameStore/data"),
        StaticDataDirSource::Fallback,
    )
}

#[derive(Debug, Deserialize)]
struct StationsFile {
    stations: Vec<StationRecord>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StationRecord {
    #[serde(rename = "stationID")]
    pub station_id: u64,
    #[serde(rename = "solarSystemID")]
    pub solar_system_id: u32,
    #[serde(rename = "constellationID")]
    pub constellation_id: u32,
    #[serde(rename = "regionID")]
    pub region_id: u32,
    #[serde(rename = "regionName")]
    pub region_name: String,
    #[serde(rename = "stationName")]
    pub station_name: String,
    pub security: f64,
}

#[derive(Debug, Deserialize)]
struct SolarSystemsFile {
    #[serde(rename = "solarSystems")]
    solar_systems: Vec<SolarSystemRecord>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SolarSystemRecord {
    #[serde(rename = "solarSystemID")]
    pub solar_system_id: u32,
    #[serde(rename = "regionID")]
    pub region_id: u32,
    #[serde(rename = "constellationID")]
    pub constellation_id: u32,
    #[serde(rename = "solarSystemName")]
    pub solar_system_name: String,
    pub security: f64,
}

#[derive(Debug, Deserialize)]
struct ItemTypesFile {
    #[serde(rename = "types")]
    item_types: Vec<ItemTypeRecord>,
}

/// One `itemTypes/data.json` row. `basePrice` and `marketGroupID` are nullable in the
/// real data and load-bearing for the cost model, so both stay `Option`.
#[derive(Debug, Clone, Deserialize)]
pub struct ItemTypeRecord {
    #[serde(rename = "typeID")]
    pub type_id: u32,
    #[serde(rename = "groupID")]
    pub group_id: Option<u32>,
    #[serde(rename = "categoryID")]
    pub category_id: Option<u32>,
    #[serde(rename = "groupName")]
    pub group_name: Option<String>,
    pub name: String,
    #[serde(default)]
    pub mass: Option<f64>,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default)]
    pub capacity: Option<f64>,
    #[serde(rename = "portionSize", default)]
    pub portion_size: Option<u32>,
    #[serde(rename = "raceID", default)]
    pub race_id: Option<i64>,
    #[serde(rename = "basePrice", default)]
    pub base_price: Option<f64>,
    #[serde(rename = "marketGroupID", default)]
    pub market_group_id: Option<u32>,
    #[serde(rename = "iconID", default)]
    pub icon_id: Option<i64>,
    #[serde(rename = "soundID", default)]
    pub sound_id: Option<i64>,
    #[serde(rename = "graphicID", default)]
    pub graphic_id: Option<i64>,
    #[serde(default)]
    pub radius: Option<f64>,
    #[serde(default)]
    pub published: bool,
}

impl ItemTypeRecord {
    /// The v1/v2 seed universe filter: published types that carry a market group.
    pub fn is_marketable(&self) -> bool {
        self.published && self.market_group_id.is_some()
    }
}

/// Only `blueprintDefinitions` is parsed; the three precomputed indexes in the file are
/// ignored on purpose (`blueprintTypeIDsByProductTypeID` misses all 120 reaction
/// blueprints, whose top-level `productTypeID` is 0 — plan §2.1 rule 1).
#[derive(Debug, Deserialize)]
struct IndustryBlueprintsFile {
    #[serde(rename = "blueprintDefinitions")]
    blueprint_definitions: Vec<BlueprintDefinition>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BlueprintDefinition {
    #[serde(rename = "blueprintTypeID")]
    pub blueprint_type_id: u32,
    #[serde(rename = "blueprintName", default)]
    pub blueprint_name: String,
    /// 0 for reaction formulas; their real output lives in `activities.reaction.products`.
    #[serde(rename = "productTypeID", default)]
    pub product_type_id: u32,
    #[serde(rename = "productName", default)]
    pub product_name: String,
    #[serde(rename = "maxProductionLimit", default)]
    pub max_production_limit: Option<i64>,
    #[serde(default)]
    pub published: bool,
    #[serde(default)]
    pub activities: BlueprintActivities,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BlueprintActivities {
    #[serde(default)]
    pub manufacturing: Option<BlueprintActivity>,
    #[serde(default)]
    pub reaction: Option<BlueprintActivity>,
    #[serde(default)]
    pub invention: Option<InventionActivity>,
    #[serde(default)]
    pub copying: Option<BlueprintActivity>,
    #[serde(rename = "research_material", default)]
    pub research_material: Option<BlueprintActivity>,
    #[serde(rename = "research_time", default)]
    pub research_time: Option<BlueprintActivity>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BlueprintActivity {
    #[serde(default)]
    pub materials: Vec<MaterialEntry>,
    #[serde(default)]
    pub products: Vec<ProductEntry>,
    #[serde(default)]
    pub time: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct InventionActivity {
    #[serde(default)]
    pub materials: Vec<MaterialEntry>,
    #[serde(default)]
    pub products: Vec<InventionProductEntry>,
    #[serde(default)]
    pub skills: Vec<SkillEntry>,
    #[serde(default)]
    pub time: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct SkillEntry {
    #[serde(rename = "typeID")]
    pub type_id: u32,
    #[serde(default)]
    pub level: i64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct MaterialEntry {
    #[serde(rename = "typeID")]
    pub type_id: u32,
    pub quantity: i64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ProductEntry {
    #[serde(rename = "typeID")]
    pub type_id: u32,
    pub quantity: i64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct InventionProductEntry {
    #[serde(rename = "typeID")]
    pub type_id: u32,
    pub quantity: i64,
    #[serde(default)]
    pub probability: Option<f64>,
}

/// Only `typesByTypeID` is parsed out of `typeDogma/data.json`; the attribute and effect
/// dictionaries are skipped.
#[derive(Debug, Deserialize)]
struct TypeDogmaFile {
    #[serde(rename = "typesByTypeID")]
    types_by_type_id: HashMap<u32, TypeDogmaRecord>,
}

#[derive(Debug, Deserialize)]
struct TypeDogmaRecord {
    #[serde(default)]
    attributes: DogmaProjection,
}

/// The two dogma attributes the classifier needs, projected out of the ~643,000-entry
/// attribute soup while parsing so nothing else is ever allocated.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DogmaProjection {
    /// Attribute 422. `None` means the attribute is absent, i.e. Tech 1.
    pub tech_level: Option<f64>,
    /// Attribute 1692. `None` means the type has no authored meta group.
    pub meta_group_id: Option<i64>,
}

impl<'de> Deserialize<'de> for DogmaProjection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ProjectionVisitor;

        impl<'de> Visitor<'de> for ProjectionVisitor {
            type Value = DogmaProjection;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map of attributeID strings to numeric values")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut projection = DogmaProjection::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.parse::<u32>() {
                        Ok(ATTRIBUTE_TECH_LEVEL) => {
                            projection.tech_level = Some(map.next_value::<f64>()?);
                        }
                        Ok(ATTRIBUTE_META_GROUP_ID) => {
                            projection.meta_group_id =
                                Some(map.next_value::<f64>()?.round() as i64);
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(projection)
            }
        }

        deserializer.deserialize_map(ProjectionVisitor)
    }
}

/// Every static table v3 reads, plus the by-ID indexes the later passes need.
#[derive(Debug)]
pub struct StaticData {
    pub dir: PathBuf,
    pub stations: Vec<StationRecord>,
    pub station_index: HashMap<u64, usize>,
    pub solar_systems: Vec<SolarSystemRecord>,
    pub solar_system_index: HashMap<u32, usize>,
    pub item_types: Vec<ItemTypeRecord>,
    pub item_type_index: HashMap<u32, usize>,
    pub blueprints: Vec<BlueprintDefinition>,
    pub dogma: HashMap<u32, DogmaProjection>,
    /// Authoritative compressed typeIDs, projected from raw→compressed static pairings.
    pub compressed_type_ids: BTreeSet<u32>,
}

#[derive(Debug, Deserialize)]
struct CompressedPairingFile {
    #[serde(rename = "compressedTypeBySourceTypeID", default)]
    compressed_by_source: BTreeMap<u32, u32>,
}

impl StaticData {
    pub fn load(dir: &Path) -> Result<Self> {
        let mut stations = read_table::<StationsFile>(dir, "stations")?.stations;
        let mut solar_systems = read_table::<SolarSystemsFile>(dir, "solarSystems")?.solar_systems;
        let mut item_types = read_table::<ItemTypesFile>(dir, "itemTypes")?.item_types;
        let blueprints =
            read_table::<IndustryBlueprintsFile>(dir, "industryBlueprints")?.blueprint_definitions;
        let dogma_file = read_table::<TypeDogmaFile>(dir, "typeDogma")?;
        let compressed_type_ids = read_table::<CompressedPairingFile>(dir, "reprocessingStatic")?
            .compressed_by_source
            .into_values()
            .collect::<BTreeSet<_>>();

        stations.sort_by_key(|station| station.station_id);
        solar_systems.sort_by_key(|system| system.solar_system_id);
        item_types.sort_by_key(|item| item.type_id);

        let station_index = stations
            .iter()
            .enumerate()
            .map(|(index, station)| (station.station_id, index))
            .collect::<HashMap<_, _>>();
        let solar_system_index = solar_systems
            .iter()
            .enumerate()
            .map(|(index, system)| (system.solar_system_id, index))
            .collect::<HashMap<_, _>>();
        let item_type_index = item_types
            .iter()
            .enumerate()
            .map(|(index, item)| (item.type_id, index))
            .collect::<HashMap<_, _>>();

        let dogma = dogma_file
            .types_by_type_id
            .into_iter()
            .map(|(type_id, record)| (type_id, record.attributes))
            .collect::<HashMap<_, _>>();

        Ok(Self {
            dir: dir.to_path_buf(),
            stations,
            station_index,
            solar_systems,
            solar_system_index,
            item_types,
            item_type_index,
            blueprints,
            dogma,
            compressed_type_ids,
        })
    }

    pub fn station(&self, station_id: u64) -> Option<&StationRecord> {
        self.station_index
            .get(&station_id)
            .and_then(|index| self.stations.get(*index))
    }

    pub fn solar_system(&self, solar_system_id: u32) -> Option<&SolarSystemRecord> {
        self.solar_system_index
            .get(&solar_system_id)
            .and_then(|index| self.solar_systems.get(*index))
    }

    pub fn item_type(&self, type_id: u32) -> Option<&ItemTypeRecord> {
        self.item_type_index
            .get(&type_id)
            .and_then(|index| self.item_types.get(*index))
    }

    /// Tech Level for a type. **An absent attribute 422 means Tech 1** — the entire T1
    /// universe relies on this default (plan §2.1 rule 3).
    pub fn tech_level(&self, type_id: u32) -> i64 {
        self.dogma
            .get(&type_id)
            .and_then(|projection| projection.tech_level)
            .map(|value| value.round() as i64)
            .unwrap_or(1)
    }

    /// metaGroupID for a type, or `None` when the attribute is not authored. Sparse by
    /// design: only where it exists can the faction/officer/deadspace filter bite.
    pub fn meta_group_id(&self, type_id: u32) -> Option<i64> {
        self.dogma
            .get(&type_id)
            .and_then(|projection| projection.meta_group_id)
    }

    pub fn is_authoritative_compressed_type(&self, type_id: u32) -> bool {
        self.compressed_type_ids.contains(&type_id)
    }

    /// Published types carrying a market group — the base seed universe (19,352 at SDE
    /// build 3396210).
    pub fn marketable_item_types(&self) -> impl Iterator<Item = &ItemTypeRecord> {
        self.item_types.iter().filter(|item| item.is_marketable())
    }
}

// ------------------------------------------------------------------- reprocessing static

/// One row of `reprocessingStatic/data.json`: what one **`portion_size` batch** of a type
/// refines into.
///
/// The per-batch shape is the whole trap in this file. Veldspar has `portion_size` 100 and
/// materials `400 x Tritanium`, which is 4 Tritanium per Veldspar, not 400. Every consumer
/// must divide by [`ReprocessingType::portion_size`] before comparing against a per-unit
/// price.
#[derive(Debug, Clone, Deserialize)]
pub struct ReprocessingType {
    #[serde(rename = "typeID")]
    pub type_id: u32,
    #[serde(default)]
    pub name: String,
    /// Units consumed by one refine batch. `>= 1` in the real table; guarded anyway,
    /// because a zero here would divide a revenue by nothing.
    #[serde(rename = "portionSize", default)]
    pub portion_size: i64,
    /// Ore/ice/gas refining. False for the general recycling path.
    #[serde(rename = "isRefinable", default)]
    pub is_refinable: bool,
    /// Scrapmetal reprocessing of a built item. True for every row in the real table.
    #[serde(rename = "isRecyclable", default)]
    pub is_recyclable: bool,
    #[serde(rename = "reprocessingFamily", default)]
    pub family: String,
    /// Output of one batch, before any yield multiplier.
    #[serde(default)]
    pub materials: Vec<ReprocessingMaterial>,
}

impl ReprocessingType {
    /// Whether either reprocessing path is open for this type.
    pub fn is_reprocessable(&self) -> bool {
        self.is_refinable || self.is_recyclable
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ReprocessingMaterial {
    #[serde(rename = "materialTypeID")]
    pub type_id: u32,
    pub quantity: i64,
}

#[derive(Debug, Deserialize)]
struct ReprocessingStaticFile {
    #[serde(rename = "reprocessingTypes")]
    reprocessing_types: Vec<ReprocessingType>,
    /// Raw ore/ice/gas typeID → the compressed type it packs into. 212 pairs at SDE
    /// 3396210. String keys in the JSON; serde reads them straight into `u32`.
    #[serde(rename = "compressedTypeBySourceTypeID", default)]
    compressed_by_source: BTreeMap<u32, u32>,
}

/// Refine yields plus the compressed/uncompressed pairing, loaded on demand by the audit.
#[derive(Debug, Default)]
pub struct ReprocessingStatic {
    pub types: Vec<ReprocessingType>,
    index: HashMap<u32, usize>,
    pub compressed_by_source: BTreeMap<u32, u32>,
}

impl ReprocessingStatic {
    pub fn load(dir: &Path) -> Result<Self> {
        let file = read_table::<ReprocessingStaticFile>(dir, "reprocessingStatic")?;
        Ok(Self::new(
            file.reprocessing_types,
            file.compressed_by_source,
        ))
    }

    /// Builds the by-typeID index. Public so tests can assemble a miniature table without a
    /// data directory.
    pub fn new(types: Vec<ReprocessingType>, compressed_by_source: BTreeMap<u32, u32>) -> Self {
        let index = types
            .iter()
            .enumerate()
            .map(|(position, entry)| (entry.type_id, position))
            .collect::<HashMap<_, _>>();
        Self {
            types,
            index,
            compressed_by_source,
        }
    }

    pub fn get(&self, type_id: u32) -> Option<&ReprocessingType> {
        self.index
            .get(&type_id)
            .and_then(|position| self.types.get(*position))
    }

    pub fn len(&self) -> usize {
        self.types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}

fn read_table<T>(dir: &Path, table: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let path = dir.join(table).join("data.json");
    let spinner = spinner(&format!("Loading static data table {table}"));
    let value = read_json_file::<T>(&path);
    match &value {
        Ok(_) => spinner.finish_with_message(format!("Loaded static data table {table}")),
        Err(_) => spinner.finish_and_clear(),
    }
    value
}

fn read_json_file<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let raw = fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read {}. Run tools\\DatabaseCreator\\CreateDatabase.bat if local static data has not been generated.",
            path.to_string_lossy()
        )
    })?;
    serde_json::from_str::<T>(&raw)
        .with_context(|| format!("failed to parse {}", path.to_string_lossy()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dogma_projection_keeps_only_tech_level_and_meta_group() {
        let raw = r#"{"182":3386,"422":2,"633":5,"1692":4,"2711":18}"#;
        let projection = serde_json::from_str::<DogmaProjection>(raw).expect("projection parses");
        assert_eq!(projection.tech_level, Some(2.0));
        assert_eq!(projection.meta_group_id, Some(4));
    }

    #[test]
    fn dogma_projection_tolerates_an_empty_attribute_map() {
        let projection = serde_json::from_str::<DogmaProjection>("{}").expect("projection parses");
        assert_eq!(projection, DogmaProjection::default());
    }

    #[test]
    fn absent_tech_level_defaults_to_one() {
        let mut data = empty_static_data();
        data.dogma.insert(
            34,
            DogmaProjection {
                tech_level: None,
                meta_group_id: None,
            },
        );
        data.dogma.insert(
            2488,
            DogmaProjection {
                tech_level: Some(2.0),
                meta_group_id: Some(2),
            },
        );

        // Present but empty projection.
        assert_eq!(data.tech_level(34), 1);
        // Entirely absent from typeDogma.
        assert_eq!(data.tech_level(9_999_999), 1);
        assert_eq!(data.tech_level(2488), 2);
        assert_eq!(data.meta_group_id(34), None);
        assert_eq!(data.meta_group_id(2488), Some(2));
    }

    #[test]
    fn raw_variant_metadata_requires_and_records_exact_generated_sde_build() {
        let root = std::env::temp_dir().join(format!(
            "market-seederv3-variant-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let static_dir = root.join("gameStore").join("data");
        let raw_dir = root
            .join("sde")
            .join("eve-online-static-data-3396210-jsonl");
        fs::create_dir_all(&static_dir).expect("static fixture directory");
        fs::create_dir_all(&raw_dir).expect("raw fixture directory");
        fs::write(
            raw_dir.join("_sde.jsonl"),
            "{\"_key\":\"sde\",\"buildNumber\":3396210}\n",
        )
        .expect("header");
        fs::write(
            raw_dir.join("types.jsonl"),
            "{\"_key\":14443,\"variationParentTypeID\":462,\"basePrice\":999999999}\n",
        )
        .expect("types");

        let families = VariantFamilies::load_for_generated_data(&static_dir, 3_396_210)
            .expect("matching raw build is accepted");
        assert_eq!(families.build, 3_396_210);
        assert_eq!(families.parent(14_443), Some(462));
        assert!(families.types_path.ends_with("types.jsonl"));
        let mismatch = VariantFamilies::load_for_generated_data(&static_dir, 3_396_211)
            .expect_err("a different generated build cannot reuse this metadata");
        assert!(format!("{mismatch:#}").contains("3396211"));

        fs::remove_dir_all(root).expect("fixture cleanup");
    }

    fn empty_static_data() -> StaticData {
        StaticData {
            dir: PathBuf::from("."),
            stations: Vec::new(),
            station_index: HashMap::new(),
            solar_systems: Vec::new(),
            solar_system_index: HashMap::new(),
            item_types: Vec::new(),
            item_type_index: HashMap::new(),
            blueprints: Vec::new(),
            dogma: HashMap::new(),
            compressed_type_ids: BTreeSet::new(),
        }
    }
}
