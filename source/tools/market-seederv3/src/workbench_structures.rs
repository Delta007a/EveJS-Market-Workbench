//! Quick Setup structure discovery. Published opt-in types never expand Simple membership.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::policy_preview::sha256_hex;
use crate::staticdata::{ItemTypeRecord, StaticData};

pub const WARNING: &str = "Published types without a TQ market group are available only after explicit Apply. Some represent installed, conversion or special objects rather than ordinary traded items. Missing price evidence remains unresolved; these settings do not change gameplay mechanics.";

pub struct Definition {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: &'static str,
    pub rank: i32,
}
macro_rules! definition {
    ($id:literal, $label:literal, $kind:literal, $rank:literal) => {
        Definition {
            id: $id,
            label: $label,
            kind: $kind,
            rank: $rank,
        }
    };
}
pub const DEFINITIONS: &[Definition] = &[
    definition!("structures_upwell", "Upwell Structures", "primary", 10),
    definition!(
        "structures_combat",
        "Structure Combat & Fitting Modules",
        "primary",
        10
    ),
    definition!(
        "structures_services",
        "Structure Service Modules",
        "primary",
        10
    ),
    definition!("structures_rigs", "Structure Rigs", "primary", 10),
    definition!("structures_cores", "Quantum Cores", "primary", 10),
    definition!("structures_deployable_items", "Deployables", "primary", 10),
    definition!("structures_pos", "Starbases / POS", "primary", 10),
    definition!(
        "structures_sovereignty_items",
        "Sovereignty & Infrastructure",
        "primary",
        10
    ),
    definition!(
        "structures_command_centers",
        "Planetary Command Centers",
        "primary",
        10
    ),
    definition!("structures_orbitals", "Orbitals", "primary", 10),
    definition!(
        "structures_tech_t1",
        "Structure modules & rigs: T1",
        "technology",
        20
    ),
    definition!(
        "structures_tech_t2",
        "Structure modules & rigs: T2",
        "technology",
        20
    ),
    definition!(
        "structures_tech_faction",
        "Structure modules & rigs: Faction",
        "technology",
        20
    ),
    definition!(
        "structures_pos_towers",
        "POS Control Towers",
        "pos_function",
        20
    ),
    definition!(
        "structures_pos_defense",
        "POS Weapons & Defense",
        "pos_function",
        20
    ),
    definition!(
        "structures_pos_industry",
        "POS Manufacturing & Research",
        "pos_function",
        20
    ),
    definition!(
        "structures_pos_processing",
        "POS Resource Processing",
        "pos_function",
        20
    ),
    definition!(
        "structures_pos_storage",
        "POS Storage & Maintenance",
        "pos_function",
        20
    ),
    definition!(
        "structures_pos_navigation",
        "POS Navigation & Utility",
        "pos_function",
        20
    ),
    definition!(
        "structures_legacy",
        "Legacy (Deprecated / Defunct)",
        "legacy_overlay",
        30
    ),
    definition!("structures_fuel", "Structure Fuel", "cross_section", 20),
    definition!(
        "structures_planetary_installations",
        "Planetary Installations (published opt-in)",
        "additional_published",
        10
    ),
];
pub const COMPATIBILITY_IDS: &[&str] = &[
    "structures_structures",
    "structures_modules",
    "structures_deployables",
    "structures_starbase",
    "structures_sovereignty",
];

pub fn definition(id: &str) -> Option<&'static Definition> {
    DEFINITIONS.iter().find(|d| d.id == id)
}

#[derive(Debug, Serialize)]
pub struct StructureItem {
    pub type_id: u32,
    pub family: String,
    pub technology: Option<String>,
    pub pos_function: Option<String>,
    pub legacy: bool,
    pub market_group_id: Option<u32>,
    pub warnings: Vec<String>,
}
pub struct Catalog {
    pub items: BTreeMap<u32, StructureItem>,
    pub scopes: BTreeMap<String, BTreeSet<u32>>,
    pub provenance: Value,
}

fn family(category: u32, group: u32, group_name: &str) -> Option<&'static str> {
    match category {
        65 => Some("structures_upwell"),
        66 if group_name == "Quantum Cores" => Some("structures_cores"),
        66 if group_name.contains("Rig") => Some("structures_rigs"),
        66 if group_name.contains("Service Module") || group == 1717 => Some("structures_services"),
        66 => Some("structures_combat"),
        22 => Some("structures_deployable_items"),
        23 => Some("structures_pos"),
        39 | 40 => Some("structures_sovereignty_items"),
        41 if group == 1027 => Some("structures_command_centers"),
        41 => Some("structures_planetary_installations"),
        46 => Some("structures_orbitals"),
        _ if group == 1136 => Some("structures_fuel"),
        _ => None,
    }
}
fn technology(meta: Option<u32>, tech: i64) -> &'static str {
    match meta {
        Some(4 | 52) => "faction",
        Some(2 | 53) => "t2",
        _ if tech == 2 => "t2",
        _ => "t1",
    }
}
fn pos_function(group: u32) -> Option<&'static str> {
    match group {
        365 => Some("structures_pos_towers"),
        417 | 426 | 430 | 449 | 439 | 440 | 441 | 443 | 444 | 473 | 837 => {
            Some("structures_pos_defense")
        }
        397 | 413 => Some("structures_pos_industry"),
        311 | 416 | 438 | 1282 => Some("structures_pos_processing"),
        363 | 404 | 471 | 1212 => Some("structures_pos_storage"),
        707 | 709 | 838 | 839 => Some("structures_pos_navigation"),
        _ => None,
    }
}
fn legacy(name: &str) -> bool {
    name.starts_with("Deprecated ") || name.starts_with("Defunct ")
}

impl Catalog {
    pub fn load(data: &StaticData, raw_types: &Path, expected_sha: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "_key")]
            id: u32,
            #[serde(rename = "metaGroupID")]
            meta: Option<u32>,
        }
        let bytes = fs::read(raw_types)?;
        ensure!(
            sha256_hex(&bytes) == expected_sha,
            "Structure SDE metadata identity changed"
        );
        let mut metadata = BTreeMap::new();
        for line in std::str::from_utf8(&bytes)?
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            let row: Raw = serde_json::from_str(line)?;
            ensure!(
                metadata.insert(row.id, row.meta).is_none(),
                "duplicate structure metadata"
            );
        }
        let mut catalog = Self {
            items: BTreeMap::new(),
            scopes: DEFINITIONS
                .iter()
                .map(|d| (d.id.to_owned(), BTreeSet::new()))
                .collect(),
            provenance: json!({"sde_types_path":raw_types,"sde_types_sha256":expected_sha,
                "source_sha256":sha256_hex(include_str!("workbench_structures.rs").as_bytes())}),
        };
        for item in data.item_types.iter().filter(|i| i.published) {
            let Some(kind) = family(
                item.category_id.unwrap_or(0),
                item.group_id.unwrap_or(0),
                item.group_name.as_deref().unwrap_or(""),
            ) else {
                continue;
            };
            let tech = (item.category_id == Some(66) && kind != "structures_cores").then(|| {
                technology(
                    metadata.get(&item.type_id).copied().flatten(),
                    data.tech_level(item.type_id),
                )
                .to_owned()
            });
            let pos = if item.category_id == Some(23) {
                Some(
                    pos_function(item.group_id.unwrap_or(0))
                        .ok_or_else(|| {
                            anyhow::anyhow!("Unclassified published POS group: {:?}", item.group_id)
                        })?
                        .to_owned(),
                )
            } else {
                None
            };
            catalog.insert(item, kind, tech, pos);
        }
        Ok(catalog)
    }
    fn insert(
        &mut self,
        item: &ItemTypeRecord,
        kind: &str,
        tech: Option<String>,
        pos: Option<String>,
    ) {
        self.scopes.get_mut(kind).unwrap().insert(item.type_id);
        if let Some(tech) = &tech {
            self.scopes
                .get_mut(&format!("structures_tech_{tech}"))
                .unwrap()
                .insert(item.type_id);
        }
        if let Some(pos) = &pos {
            self.scopes.get_mut(pos).unwrap().insert(item.type_id);
        }
        if legacy(&item.name) {
            self.scopes
                .get_mut("structures_legacy")
                .unwrap()
                .insert(item.type_id);
        }
        let mut warnings = Vec::new();
        if item.market_group_id.is_none() {
            warnings.push(WARNING.into());
        }
        if kind == "structures_fuel" {
            warnings.push("Fuel Blocks remain in Other Market Items. Only explicitly applying this cross-section filter overrides their policy. Strontium Clathrates is not included.".into());
        }
        if legacy(&item.name) {
            warnings.push("Published name is marked Deprecated / Defunct. Legacy is an overlay; the item retains its functional family.".into());
        }
        self.items.insert(
            item.type_id,
            StructureItem {
                type_id: item.type_id,
                family: kind.into(),
                technology: tech,
                pos_function: pos,
                legacy: legacy(&item.name),
                market_group_id: item.market_group_id,
                warnings,
            },
        );
    }
    pub fn schema(&self) -> Value {
        json!({"published_structure_types":self.items.values().filter(|i|i.family!="structures_fuel").count(),
            "no_market_group":self.items.values().filter(|i|i.market_group_id.is_none()).count(),"fuel_types":self.scopes["structures_fuel"].len(),
            "warning":WARNING,"provenance":self.provenance,"scopes":DEFINITIONS.iter().map(|d| {
                let ids=&self.scopes[d.id]; json!({"id":d.id,"label":d.label,"kind":d.kind,"priority_offset":d.rank,"count":ids.len(),
                    "market_listed":ids.iter().filter(|id|self.items[id].market_group_id.is_some()).count(),
                    "no_market_group":ids.iter().filter(|id|self.items[id].market_group_id.is_none()).count()})
            }).collect::<Vec<_>>()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn functional_families_do_not_mislabel_cores_rigs_or_planetary_installations() {
        assert_eq!(
            family(66, 1984, "Outpost Conversion Rigs"),
            Some("structures_rigs")
        );
        assert_eq!(
            family(66, 1717, "Unpublished Structure Modules"),
            Some("structures_services")
        );
        assert_eq!(family(66, 0, "Quantum Cores"), Some("structures_cores"));
        assert_eq!(
            family(66, 0, "Structure Energy Neutralizer"),
            Some("structures_combat")
        );
        assert_eq!(
            family(41, 1027, "Command Centers"),
            Some("structures_command_centers")
        );
        assert_eq!(
            family(41, 1026, "Extractors"),
            Some("structures_planetary_installations")
        );
        assert_eq!(family(4, 1136, "Fuel Block"), Some("structures_fuel"));
        assert_eq!(family(4, 423, "Ice Product"), None);
    }
    #[test]
    fn technology_uses_structure_meta_groups_and_legacy_is_an_overlay() {
        assert_eq!(technology(Some(52), 2), "faction");
        assert_eq!(technology(Some(53), 1), "t2");
        assert_eq!(technology(Some(54), 1), "t1");
        assert!(legacy("Deprecated Pirate Detection Array 1"));
        assert!(legacy("Defunct Amarr Encounter Surveillance System"));
        assert!(!legacy("Upwell Standard Outpost Rig"));
        assert_eq!(pos_function(473), Some("structures_pos_defense"));
        assert_eq!(pos_function(999999), None);
        assert_eq!(
            definition("structures_legacy").unwrap().kind,
            "legacy_overlay"
        );
        assert!(
            definition("structures_legacy").unwrap().rank
                > definition("structures_tech_t2").unwrap().rank
        );
    }
}
