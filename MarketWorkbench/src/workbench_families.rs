//! Additional Quick Setup discovery facets. No pricing or broad membership ownership.
use crate::policy_preview::sha256_hex;
use crate::staticdata::{ItemTypeRecord, StaticData};
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

pub const WARNING: &str = "Explicit published opt-in only. Mission, expired, license or instance objects may not follow ordinary TQ market mechanics. Missing price evidence remains unresolved; configuring a market does not implement those mechanics.";
pub struct Definition {
    pub id: &'static str,
    pub parent: &'static str,
    pub label: &'static str,
    pub section: &'static str,
    pub inherits: Option<&'static str>,
    pub rank: i32,
    pub opt_in: bool,
}
macro_rules! d {
    ($id:literal,$parent:literal,$label:literal,$section:literal,$inherit:expr,$rank:literal,$opt:literal) => {
        Definition {
            id: $id,
            parent: $parent,
            label: $label,
            section: $section,
            inherits: $inherit,
            rank: $rank,
            opt_in: $opt,
        }
    };
}
pub const DEFINITIONS: &[Definition] = &[
    d!(
        "industry_reactions_all",
        "industry",
        "Reactions (all materials)",
        "Aggregate controls",
        None,
        11,
        false
    ),
    d!(
        "resources_asteroid_ore",
        "resources",
        "Asteroid Ore",
        "Ore",
        Some("resources_ore"),
        20,
        false
    ),
    d!(
        "resources_moon_ore",
        "resources",
        "Moon Ore",
        "Ore",
        Some("resources_ore"),
        20,
        false
    ),
    d!(
        "resources_ice_ore",
        "resources",
        "Ice",
        "Ore",
        Some("resources_ore"),
        20,
        false
    ),
    d!(
        "resources_pi_p0",
        "resources",
        "P0 — Extracted Resources",
        "PI",
        Some("resources_pi"),
        20,
        false
    ),
    d!(
        "resources_pi_p1",
        "resources",
        "P1 — Basic Commodities",
        "PI",
        Some("resources_pi"),
        20,
        false
    ),
    d!(
        "resources_pi_p2",
        "resources",
        "P2 — Refined Commodities",
        "PI",
        Some("resources_pi"),
        20,
        false
    ),
    d!(
        "resources_pi_p3",
        "resources",
        "P3 — Specialized Commodities",
        "PI",
        Some("resources_pi"),
        20,
        false
    ),
    d!(
        "resources_pi_p4",
        "resources",
        "P4 — Advanced Commodities",
        "PI",
        Some("resources_pi"),
        20,
        false
    ),
    d!(
        "resources_gas_myko",
        "resources",
        "Mykoserocin",
        "Gas",
        Some("resources_gas"),
        20,
        false
    ),
    d!(
        "resources_gas_cyto",
        "resources",
        "Cytoserocin",
        "Gas",
        Some("resources_gas"),
        20,
        false
    ),
    d!(
        "resources_gas_fullerites",
        "resources",
        "Fullerites",
        "Gas",
        Some("resources_gas"),
        20,
        false
    ),
    d!(
        "resources_gas_special",
        "resources",
        "Special Gas",
        "Gas",
        Some("resources_gas"),
        20,
        false
    ),
    d!(
        "resources_salvage_normal",
        "resources",
        "Normal Salvage",
        "Salvage",
        Some("resources_salvage"),
        20,
        false
    ),
    d!(
        "resources_salvage_ancient",
        "resources",
        "Ancient Salvage",
        "Salvage",
        Some("resources_salvage"),
        20,
        false
    ),
    d!(
        "resources_raw",
        "resources",
        "Raw (Ore / Gas / Prismaticite)",
        "Overlapping filters",
        None,
        30,
        false
    ),
    d!(
        "resources_compressed",
        "resources",
        "Compressed (Ore / Gas / Prismaticite)",
        "Overlapping filters",
        None,
        30,
        false
    ),
    d!(
        "resources_batch_legacy",
        "resources",
        "Legacy — Batch Compressed Ore",
        "Overlapping filters",
        None,
        40,
        false
    ),
    d!(
        "resources_compressed_gas",
        "resources",
        "Compressed Gas",
        "Functional cross filters",
        None,
        20,
        false
    ),
    d!(
        "resources_prismaticite",
        "resources",
        "Prismaticite",
        "Functional cross filters",
        None,
        20,
        false
    ),
    d!(
        "resources_refinables",
        "resources",
        "Refinables",
        "Functional cross filters",
        None,
        20,
        false
    ),
    d!(
        "resources_unrefined",
        "resources",
        "Unrefined Minerals",
        "Functional cross filters",
        None,
        30,
        false
    ),
    d!(
        "resources_mission_ore",
        "resources",
        "Mission Ore (published opt-in)",
        "Published special opt-in",
        None,
        50,
        true
    ),
    d!(
        "resources_mission_ice",
        "resources",
        "Mission Ice (published opt-in)",
        "Published special opt-in",
        None,
        50,
        true
    ),
    d!(
        "resources_special_gas_optin",
        "resources",
        "Special Gas & Compressed Gas (published opt-in)",
        "Published special opt-in",
        None,
        50,
        true
    ),
    d!(
        "resources_rare_optin",
        "resources",
        "Temporal / Special Asteroids (published opt-in)",
        "Published special opt-in",
        None,
        50,
        true
    ),
    d!(
        "industry_standard_components",
        "industry",
        "Standard Construction Components",
        "Construction Components",
        Some("industry_components"),
        20,
        false
    ),
    d!(
        "industry_advanced_components",
        "industry",
        "Advanced Construction Components",
        "Construction Components",
        Some("industry_components"),
        20,
        false
    ),
    d!(
        "industry_capital_components",
        "industry",
        "Capital Components",
        "Construction Components",
        Some("industry_components"),
        20,
        false
    ),
    d!(
        "industry_advanced_capital",
        "industry",
        "Advanced Capital Components",
        "Construction Components",
        Some("industry_components"),
        20,
        false
    ),
    d!(
        "industry_hybrid_components",
        "industry",
        "T3 / Hybrid Components",
        "Construction Components",
        Some("industry_components"),
        20,
        false
    ),
    d!(
        "industry_unknown_components",
        "industry",
        "Unknown Components — Special",
        "Construction Components",
        Some("industry_components"),
        20,
        false
    ),
    d!(
        "industry_reaction_intermediate",
        "industry",
        "Intermediate Materials",
        "Reactions",
        Some("industry_reactions_all"),
        20,
        false
    ),
    d!(
        "industry_reaction_composite",
        "industry",
        "Composite Materials",
        "Reactions",
        Some("industry_reactions_all"),
        20,
        false
    ),
    d!(
        "industry_reaction_biochemical",
        "industry",
        "Biochemical Materials",
        "Reactions",
        Some("industry_reactions_all"),
        20,
        false
    ),
    d!(
        "industry_reaction_hybrid",
        "industry",
        "Hybrid Polymers",
        "Reactions",
        Some("industry_reactions_all"),
        20,
        false
    ),
    d!(
        "industry_reaction_molecular",
        "industry",
        "Molecular-Forged Materials",
        "Reactions",
        Some("industry_reactions_all"),
        20,
        false
    ),
    d!(
        "industry_abyssal",
        "industry",
        "Abyssal Materials",
        "Other materials",
        None,
        20,
        false
    ),
    d!(
        "industry_peculiar",
        "industry",
        "Peculiar Materials",
        "Other materials",
        None,
        20,
        false
    ),
    d!(
        "industry_unrefined",
        "industry",
        "Unrefined Minerals",
        "Other materials",
        None,
        20,
        false
    ),
    d!(
        "industry_general",
        "industry",
        "General",
        "Other materials",
        None,
        20,
        false
    ),
    d!(
        "industry_research_tools",
        "industry",
        "Research Tools (R.A.M. / R.Db)",
        "Functional cross filters",
        None,
        30,
        false
    ),
    d!(
        "industry_cosmos",
        "industry",
        "COSMOS Materials",
        "Functional cross filters",
        None,
        30,
        false
    ),
    d!(
        "industry_named_optin",
        "industry",
        "Named Components (published review opt-in)",
        "Published special opt-in",
        None,
        40,
        true
    ),
    d!(
        "industry_sleeper_relics",
        "industry",
        "Sleeper Relics (published review opt-in)",
        "Published special opt-in",
        None,
        40,
        true
    ),
    d!(
        "industry_decryptors_amarr",
        "industry",
        "Amarr Decryptors (published opt-in)",
        "Published Decryptor families",
        None,
        40,
        true
    ),
    d!(
        "industry_decryptors_minmatar",
        "industry",
        "Minmatar Decryptors (published opt-in)",
        "Published Decryptor families",
        None,
        40,
        true
    ),
    d!(
        "industry_decryptors_gallente",
        "industry",
        "Gallente Decryptors (published opt-in)",
        "Published Decryptor families",
        None,
        40,
        true
    ),
    d!(
        "industry_decryptors_caldari",
        "industry",
        "Caldari Decryptors (published opt-in)",
        "Published Decryptor families",
        None,
        40,
        true
    ),
    d!(
        "industry_decryptors_hybrid",
        "industry",
        "Hybrid Decryptors (published opt-in)",
        "Published Decryptor families",
        None,
        40,
        true
    ),
    d!(
        "industry_general_optin",
        "industry",
        "General — Quafe Cargo (published opt-in)",
        "Published special opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_apparel",
        "other",
        "Apparel",
        "Main families",
        None,
        10,
        false
    ),
    d!(
        "other_mutaplasmids",
        "other",
        "Mutaplasmids",
        "Main families",
        None,
        10,
        false
    ),
    d!(
        "other_tags",
        "other",
        "Tags / Insignias / Security Tags",
        "Main families",
        None,
        10,
        false
    ),
    d!(
        "other_filaments",
        "other",
        "Filaments",
        "Main families",
        None,
        10,
        false
    ),
    d!(
        "other_containers",
        "other",
        "Containers / Strong Boxes",
        "Main families",
        None,
        10,
        false
    ),
    d!(
        "other_trade_goods",
        "other",
        "Trade Goods",
        "Main families",
        None,
        10,
        false
    ),
    d!(
        "other_exploration",
        "other",
        "Exploration / COSMOS Materials",
        "Main families",
        None,
        10,
        false
    ),
    d!(
        "other_collectibles",
        "other",
        "Collectibles / Event Items",
        "Main families",
        None,
        10,
        false
    ),
    d!(
        "other_pilot_services",
        "other",
        "Pilot Services",
        "Main families",
        None,
        10,
        false
    ),
    d!(
        "other_miscellaneous",
        "other",
        "Miscellaneous Market Items",
        "Main families",
        None,
        10,
        false
    ),
    d!(
        "other_research_tools",
        "other",
        "Research Tools",
        "Functional filters",
        None,
        20,
        false
    ),
    d!(
        "other_resource_neighbors",
        "other",
        "Compressed Gas / Prismaticite / Refinables",
        "Functional filters",
        None,
        20,
        false
    ),
    d!(
        "other_legacy",
        "other",
        "Legacy / Expired",
        "Overlapping filters",
        None,
        30,
        false
    ),
    d!(
        "other_mission_keys",
        "other",
        "Mission Keys (published opt-in)",
        "Narrow published opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_mission_cargo",
        "other",
        "Mission Cargo / Crates (published opt-in)",
        "Narrow published opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_expired_filaments",
        "other",
        "Expired Filaments (published opt-in)",
        "Narrow published opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_special_packages",
        "other",
        "Special Packages (published opt-in)",
        "Narrow published opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_apparel_optin",
        "other",
        "Apparel (published opt-in)",
        "Narrow published opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_tags_optin",
        "other",
        "Tags / Insignias (published opt-in)",
        "Narrow published opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_services_optin",
        "other",
        "Pilot Service Certificates / Injectors (published opt-in)",
        "Narrow published opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_skin_permanent",
        "other",
        "Permanent SKIN Licenses (published opt-in)",
        "SKIN / License opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_skin_volatile",
        "other",
        "Volatile SKIN Licenses (published opt-in)",
        "SKIN / License opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_skin_7d",
        "other",
        "7-Day SKIN Licenses (published opt-in)",
        "SKIN / License opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_skin_30d",
        "other",
        "30-Day SKIN Licenses (published opt-in)",
        "SKIN / License opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_skin_90d",
        "other",
        "90-Day SKIN Licenses (published opt-in)",
        "SKIN / License opt-in",
        None,
        40,
        true
    ),
    d!(
        "other_skin_1y",
        "other",
        "1-Year SKIN Licenses (published opt-in)",
        "SKIN / License opt-in",
        None,
        40,
        true
    ),
];
pub fn definition(id: &str) -> Option<&'static Definition> {
    DEFINITIONS.iter().find(|d| d.id == id)
}
#[derive(Debug, Serialize)]
pub struct FamilyItem {
    pub type_id: u32,
    pub scopes: Vec<String>,
    pub opt_in: bool,
    pub warnings: Vec<String>,
}
pub struct Catalog {
    pub items: BTreeMap<u32, FamilyItem>,
    pub scopes: BTreeMap<String, BTreeSet<u32>>,
    pub provenance: Value,
}
fn ore_family(i: &ItemTypeRecord) -> &'static str {
    match i.group_id.unwrap_or(0) {
        465 => "resources_ice_ore",
        1884 | 1920 | 1921 | 1922 | 1923 => "resources_moon_ore",
        _ => "resources_asteroid_ore",
    }
}
fn component_family(i: &ItemTypeRecord, ancestors: &BTreeSet<u32>) -> &'static str {
    match i.group_id.unwrap_or(0) {
        873 if !ancestors.contains(&2693) => "industry_capital_components",
        873 | 913 => "industry_advanced_capital",
        964 => "industry_hybrid_components",
        1314 => "industry_unknown_components",
        334 if ancestors.contains(&1147) => "industry_hybrid_components",
        334 if ancestors
            .iter()
            .any(|g| [802, 803, 1888, 1889, 2658, 3594].contains(g)) =>
        {
            "industry_advanced_components"
        }
        _ => "industry_standard_components",
    }
}
fn other_family(i: &ItemTypeRecord) -> &'static str {
    if i.category_id == Some(30) {
        return "other_apparel";
    }
    match i.group_id.unwrap_or(0) {
        1964 => "other_mutaplasmids",
        370 | 409 | 1206 => "other_tags",
        1979 | 4087 | 4041 | 4145 | 4072 | 4776 => "other_filaments",
        12 | 340 | 448 | 649 | 1818 => "other_containers",
        1739 | 1301 | 1875 => "other_pilot_services",
        1194 | 1195 | 1225 | 976 | 4843 | 943 => "other_collectibles",
        528 | 530 | 732 | 733 | 734 | 735 | 886 | 880 | 2002 | 1995 | 5067 | 5068 | 4142 | 4827
        | 4900 | 4716 | 1886 => "other_exploration",
        280 | 313 | 281 | 282 | 283 | 284 | 526 | 652 | 1248 | 493 | 879 | 4972 | 4575 | 4821
        | 4824 | 1136 => "other_trade_goods",
        _ => "other_miscellaneous",
    }
}
fn legacy(i: &ItemTypeRecord) -> bool {
    [943, 4072, 4776, 976].contains(&i.group_id.unwrap_or(0))
        || ["Deprecated ", "Defunct ", "Obsolete "]
            .iter()
            .any(|p| i.name.starts_with(p))
}
fn special_scope(i: &ItemTypeRecord) -> Option<&'static str> {
    match (i.category_id, i.group_id.unwrap_or(0)) {
        (Some(25), 465) => Some("resources_mission_ice"),
        (Some(25), 2022 | 2024 | 1911) => Some("resources_rare_optin"),
        (Some(25), _) => Some("resources_mission_ore"),
        (_, 711 | 4168) => Some("resources_special_gas_optin"),
        (Some(34), _) => Some("industry_sleeper_relics"),
        (_, 1676) => Some("industry_named_optin"),
        (_, 728) => Some("industry_decryptors_amarr"),
        (_, 729) => Some("industry_decryptors_minmatar"),
        (_, 730) => Some("industry_decryptors_gallente"),
        (_, 731) => Some("industry_decryptors_caldari"),
        (_, 979) => Some("industry_decryptors_hybrid"),
        (_, 280) => Some("industry_general_optin"),
        (_, 474) => Some("other_mission_keys"),
        (_, 4072 | 4776) => Some("other_expired_filaments"),
        (Some(30), _) => Some("other_apparel_optin"),
        (_, 370 | 409 | 1206) => Some("other_tags_optin"),
        (_, 1739 | 1301) => Some("other_services_optin"),
        (_, 1950) => Some("other_skin_permanent"),
        (_, 1951) => Some("other_skin_volatile"),
        (_, 1952) => Some("other_skin_7d"),
        (_, 1953) => Some("other_skin_30d"),
        (_, 1954) => Some("other_skin_90d"),
        (_, 1955) => Some("other_skin_1y"),
        (_, 1194)
            if ["package", "crate", "container"]
                .iter()
                .any(|p| i.name.to_ascii_lowercase().contains(p)) =>
        {
            Some("other_special_packages")
        }
        (_, 314 | 526)
            if i.market_group_id.is_none()
                && (i.name.to_ascii_lowercase().contains("mission")
                    || i.name.contains("Crate")
                    || i.name.contains("Crates")) =>
        {
            Some("other_mission_cargo")
        }
        _ => None,
    }
}
impl Catalog {
    pub fn load(
        data: &StaticData,
        ordinary: &BTreeSet<u32>,
        broad: &[crate::workbench_quick::Scope],
        raw_groups: &Path,
    ) -> Result<Self> {
        let bytes = fs::read(raw_groups)?;
        let mut parents = BTreeMap::new();
        for line in std::str::from_utf8(&bytes)?
            .lines()
            .filter(|s| !s.trim().is_empty())
        {
            let r: Value = serde_json::from_str(line)?;
            let id = r["_key"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("invalid market group ID"))?
                as u32;
            ensure!(
                parents
                    .insert(id, r["parentGroupID"].as_u64().map(|n| n as u32))
                    .is_none(),
                "duplicate market group"
            );
        }
        let mut scopes: BTreeMap<String, BTreeSet<u32>> = DEFINITIONS
            .iter()
            .map(|d| (d.id.to_owned(), BTreeSet::new()))
            .collect();
        let mut items = BTreeMap::new();
        let members = |parent: &str, id: u32| {
            broad
                .iter()
                .any(|s| s.id == parent && s.parent.is_none() && s.members.contains(&id))
        };
        for i in data.item_types.iter().filter(|i| i.published) {
            let id = i.type_id;
            let mut found = BTreeSet::new();
            let mut ancestors = BTreeSet::new();
            let mut next = i.market_group_id;
            while let Some(g) = next {
                ensure!(ancestors.insert(g), "market group cycle at {g}");
                next = *parents
                    .get(&g)
                    .ok_or_else(|| anyhow::anyhow!("unknown market group {g}"))?;
            }
            if ordinary.contains(&id) {
                if members("resources", id) {
                    if i.category_id == Some(25) {
                        found.insert(ore_family(i));
                    }
                    if [Some(42), Some(43)].contains(&i.category_id) {
                        found.insert(match i.group_id.unwrap_or(0) {
                            1032 | 1033 | 1035 => "resources_pi_p0",
                            1042 => "resources_pi_p1",
                            1034 => "resources_pi_p2",
                            1040 => "resources_pi_p3",
                            1041 => "resources_pi_p4",
                            _ => anyhow::bail!("unclassified PI group {}", i.group_id.unwrap_or(0)),
                        });
                    }
                    if i.group_id.unwrap_or(0) == 711 {
                        found.insert(if i.name.contains("Mykoserocin") {
                            "resources_gas_myko"
                        } else if i.name.contains("Cytoserocin") {
                            "resources_gas_cyto"
                        } else if i.name.starts_with("Fullerite-") {
                            "resources_gas_fullerites"
                        } else {
                            "resources_gas_special"
                        });
                    }
                    if i.group_id.unwrap_or(0) == 754 {
                        found.insert("resources_salvage_normal");
                    }
                    if i.group_id.unwrap_or(0) == 966 {
                        found.insert("resources_salvage_ancient");
                    }
                }
                if i.category_id == Some(25) || [711, 4168, 4915].contains(&i.group_id.unwrap_or(0))
                {
                    found.insert(
                        if i.name.starts_with("Compressed ")
                            || i.name.starts_with("Batch Compressed ")
                        {
                            "resources_compressed"
                        } else {
                            "resources_raw"
                        },
                    );
                    if i.name.starts_with("Batch Compressed ") {
                        found.insert("resources_batch_legacy");
                    }
                }
                match i.group_id.unwrap_or(0) {
                    4168 => {
                        found.insert("resources_compressed_gas");
                    }
                    4915 => {
                        found.insert("resources_prismaticite");
                    }
                    355 => {
                        found.insert("resources_refinables");
                    }
                    4932 => {
                        found.insert("resources_unrefined");
                    }
                    _ => {}
                }
                if members("industry", id) {
                    match i.group_id.unwrap_or(0) {
                        334 | 873 | 913 | 964 | 1314 => {
                            found.insert(component_family(i, &ancestors));
                        }
                        428 => {
                            found.insert("industry_reactions_all");
                            found.insert("industry_reaction_intermediate");
                        }
                        429 => {
                            found.insert("industry_reactions_all");
                            found.insert("industry_reaction_composite");
                        }
                        712 => {
                            found.insert("industry_reactions_all");
                            found.insert("industry_reaction_biochemical");
                        }
                        974 => {
                            found.insert("industry_reactions_all");
                            found.insert("industry_reaction_hybrid");
                        }
                        4096 => {
                            found.insert("industry_reactions_all");
                            found.insert("industry_reaction_molecular");
                        }
                        1996 => {
                            found.insert("industry_abyssal");
                        }
                        4165 => {
                            found.insert("industry_peculiar");
                        }
                        4932 => {
                            found.insert("industry_unrefined");
                        }
                        280 => {
                            found.insert("industry_general");
                        }
                        _ => {}
                    }
                }
                if i.group_id.unwrap_or(0) == 332 {
                    found.insert("industry_research_tools");
                }
                if [732, 733, 734, 735].contains(&i.group_id.unwrap_or(0))
                    || ([528, 530].contains(&i.group_id.unwrap_or(0))
                        && ancestors
                            .iter()
                            .any(|g| [1902, 1903, 1904, 1905].contains(g)))
                {
                    found.insert("industry_cosmos");
                }
                if members("other", id) {
                    found.insert(other_family(i));
                    if i.group_id.unwrap_or(0) == 332 {
                        found.insert("other_research_tools");
                    }
                    if [4168, 4915, 355].contains(&i.group_id.unwrap_or(0)) {
                        found.insert("other_resource_neighbors");
                    }
                    if legacy(i) {
                        found.insert("other_legacy");
                    }
                }
            } else if let Some(scope) = special_scope(i) {
                found.insert(scope);
            }
            if found.is_empty() {
                continue;
            }
            let opt_in = !ordinary.contains(&id);
            let mut warnings = Vec::new();
            if opt_in {
                warnings.push(WARNING.to_owned());
            }
            if found.contains("other_pilot_services") {
                warnings.push("Pilot services, PLEX and skill injectors have special consumption/account semantics. A synthetic quote does not guarantee support for these actions.".into());
            }
            if found.contains("resources_batch_legacy") || found.contains("other_legacy") {
                warnings.push("Legacy / expired overlay: this type retains its original functional and broad membership.".into());
            }
            for s in &found {
                scopes.get_mut(*s).expect("registered scope").insert(id);
            }
            items.insert(
                id,
                FamilyItem {
                    type_id: id,
                    scopes: found.into_iter().map(str::to_owned).collect(),
                    opt_in,
                    warnings,
                },
            );
        }
        let provenance = json!({"market_groups":{"path":raw_groups,"sha256":sha256_hex(&bytes)},"source_sha256":sha256_hex(include_str!("workbench_families.rs").as_bytes()),"membership":"ordinary facets plus individually explicit published opt-ins"});
        Ok(Self {
            items,
            scopes,
            provenance,
        })
    }
    pub fn schema(&self) -> Value {
        json!({"format_version":1,"provenance":self.provenance,"warning":WARNING,
            "scopes":DEFINITIONS.iter().map(|d|json!({"id":d.id,"parent":d.parent,"label":d.label,"section":d.section,"inherits":d.inherits,"priority_offset":d.rank,"opt_in":d.opt_in,"count":self.scopes[d.id].len()})).collect::<Vec<_>>()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_is_unique_and_opt_ins_are_narrow() {
        let ids: BTreeSet<_> = DEFINITIONS.iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), DEFINITIONS.len());
        assert!(
            DEFINITIONS
                .iter()
                .all(|d| ["resources", "industry", "other"].contains(&d.parent))
        );
        assert!(
            DEFINITIONS
                .iter()
                .filter(|d| d.opt_in)
                .all(|d| d.rank >= 40)
        );
        assert!(!ids.contains("other_all_published"));
    }
}
