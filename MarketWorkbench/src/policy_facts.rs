//! Factual catalog evidence for the finite V2 selector registry.
//! These tags do not select sides, prices, or a winning policy rule.

use std::collections::BTreeSet;

use crate::classify::{
    CHARGE_CATEGORY_ID, COMMODITY_CATEGORY_ID, Classification, MINERAL_GROUP_ID,
    MODULE_CATEGORY_ID, MOON_RESOURCE_GROUP_IDS, NATURAL_SALVAGE_GROUP_ID, SHIP_CATEGORY_ID,
};
use crate::staticdata::StaticData;

pub struct FactInputs<'a> {
    pub data: &'a StaticData,
    pub classification: &'a Classification,
    pub direct_lp_rewards: &'a BTreeSet<u32>,
    pub expanded_lp_rewards: &'a BTreeSet<u32>,
    pub npc_catalog_types: &'a BTreeSet<u32>,
}

impl FactInputs<'_> {
    pub fn for_type(&self, type_id: u32) -> BTreeSet<String> {
        let mut tags = BTreeSet::new();
        let Some(item) = self.data.item_type(type_id) else {
            return tags;
        };
        if !item.is_marketable() {
            return tags;
        }
        let mut add = |name: &str, enabled: bool| {
            if enabled {
                tags.insert(name.to_owned());
            }
        };
        let group = item.group_name.as_deref().unwrap_or_default();
        let lower = group.to_ascii_lowercase();
        let moon = item
            .group_id
            .is_some_and(|id| MOON_RESOURCE_GROUP_IDS.contains(&id));
        let core = moon
            || (item.category_id != Some(COMMODITY_CATEGORY_ID)
                && self.classification.flags(type_id).is_classified());
        let tech = self.data.tech_level(type_id);
        let meta = self.data.meta_group_id(type_id);
        let finished_good = matches!(
            item.category_id,
            Some(6 | 7 | 8 | 18 | 20 | 22 | 23 | 32 | 65 | 66 | 87)
        );
        let producer_invention =
            self.classification
                .producers
                .get(type_id)
                .is_some_and(|producer| {
                    self.classification
                        .invention_lineage
                        .contains(&producer.blueprint_type_id)
                });
        add("finished_t3", finished_good && tech >= 3);
        add(
            "finished_t2",
            finished_good
                && tech < 3
                && (tech == 2
                    || meta == Some(2)
                    || self.classification.invention_lineage.contains(&type_id)
                    || producer_invention),
        );
        add(
            "published_reaction_formula",
            self.classification
                .is_published_reaction_formula(self.data, type_id),
        );
        add(
            "approved_reusable_bpo",
            self.npc_catalog_types.contains(&type_id)
                && self
                    .classification
                    .is_approved_reusable_bpo(self.data, type_id),
        );
        add(
            "special_reward_placeholder_hull",
            self.classification
                .is_special_reward_placeholder_hull(self.data, type_id),
        );
        add("rare_officer", meta == Some(5));
        add("rare_deadspace_ded", meta == Some(6));
        add(
            "direct_lp_reward",
            self.direct_lp_rewards.contains(&type_id),
        );
        add(
            "lp_blueprint_product",
            self.expanded_lp_rewards.contains(&type_id)
                && !self.direct_lp_rewards.contains(&type_id),
        );
        add("rare_faction_lp", matches!(meta, Some(3 | 4)));
        add("mineable_moon_resource", moon);
        add(
            "authoritative_compressed_ore",
            item.category_id == Some(25) && self.data.is_authoritative_compressed_type(type_id),
        );
        add("core_bucket", core);
        add(
            "core_ice_ore",
            core && item.category_id == Some(25) && lower.contains("ice"),
        );
        add(
            "core_raw_ore",
            core && item.category_id == Some(25)
                && !lower.contains("ice")
                && !self.data.is_authoritative_compressed_type(type_id),
        );
        add(
            "core_minerals",
            core && item.group_id == Some(MINERAL_GROUP_ID),
        );
        add(
            "core_gas_ice_salvage",
            core && (item.group_id == Some(NATURAL_SALVAGE_GROUP_ID)
                || matches!(group, "Harvestable Cloud" | "Compressed Gas")
                || lower.contains("ice product")),
        );
        add(
            "core_battlecruiser",
            core && item.category_id == Some(SHIP_CATEGORY_ID) && lower.contains("battlecruiser"),
        );
        add(
            "core_battleship",
            core && item.category_id == Some(SHIP_CATEGORY_ID) && lower.contains("battleship"),
        );
        add(
            "core_destroyer",
            core && item.category_id == Some(SHIP_CATEGORY_ID) && lower.contains("destroyer"),
        );
        add(
            "core_cruiser",
            core && item.category_id == Some(SHIP_CATEGORY_ID) && lower.contains("cruiser"),
        );
        add(
            "core_orca_freighter_structure",
            core && (item.category_id == Some(65)
                || (item.category_id == Some(SHIP_CATEGORY_ID)
                    && (matches!(item.group_id, Some(513 | 902 | 941)) || item.name == "Orca"))),
        );
        add(
            "core_capital_structure_component",
            core && matches!(item.group_id, Some(334 | 536 | 873 | 913)),
        );
        add(
            "core_t1_module_ammo_rig",
            core && (matches!(
                item.category_id,
                Some(MODULE_CATEGORY_ID | CHARGE_CATEGORY_ID | 66)
            ) || lower.contains("rig")),
        );
        tags
    }
}
