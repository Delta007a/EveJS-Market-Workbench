//! Advanced-only blueprint discovery. This does not change pricing or Simple Mode membership.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::policy::{PolicyDocument, Rule, Selector, SidePolicy, Sides};
use crate::policy_preview::sha256_hex;
use crate::staticdata::StaticData;

pub const SEMANTICS_WARNING: &str = "Blueprint originals, copies and special instances may not follow ordinary TQ market mechanics. These settings are allowed; instance semantics are not an eligibility gate. Missing price evidence is reported separately.";
pub const FAMILIES: &[(&str, &str)] = &[
    ("t1", "T1 Blueprints"),
    ("t2", "T2 Blueprints"),
    ("faction", "Faction Blueprints"),
    ("reaction", "Reaction Formulas"),
    ("t3", "T3 Blueprints"),
    ("storyline", "Storyline Blueprints"),
    ("abyssal", "Abyssal Blueprints"),
    ("limited_time", "Limited Time Blueprints"),
    ("officer", "Officer Blueprints"),
    ("other", "Other / Special Blueprints"),
];

#[derive(Debug, Clone, Serialize)]
pub struct Blueprint {
    pub type_id: u32,
    pub name: String,
    pub family: String,
    pub market_group_id: Option<u32>,
    pub product_ids: Vec<u32>,
    pub product_names: Vec<String>,
    pub product_meta_groups: Vec<Option<u32>>,
    pub copying_activity: bool,
    pub max_production_limit: Option<i64>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct Catalog {
    pub items: BTreeMap<u32, Blueprint>,
    pub source_path: PathBuf,
    pub source_sha256: String,
}

#[derive(Deserialize)]
struct RawType {
    #[serde(rename = "_key")]
    id: u32,
    #[serde(rename = "metaGroupID")]
    meta: Option<u32>,
}

/// Family is a product fact, independent of marketGroup and instance BPO/BPC status.
fn family(reaction: bool, products: &[(Option<u32>, i64)], own_meta: Option<u32>) -> &'static str {
    if reaction {
        return "reaction";
    }
    if products.is_empty() {
        return "other";
    }
    let families: BTreeSet<_> = products
        .iter()
        .map(|&(meta, tech)| match meta.or(own_meta) {
            Some(4 | 52) => "faction",
            Some(3) => "storyline",
            Some(15) => "abyssal",
            Some(19) => "limited_time",
            Some(5) => "officer",
            Some(6) => "other",
            Some(14) => "t3",
            Some(2 | 53) => "t2",
            Some(1 | 54) | None => match tech {
                3.. => "t3",
                2 => "t2",
                _ => "t1",
            },
            _ => "other",
        })
        .collect();
    if families.len() == 1 {
        families.first().copied().unwrap()
    } else {
        "other"
    }
}

impl Catalog {
    pub fn load(data: &StaticData, build: u64) -> Result<Self> {
        let raw_dir = data
            .dir
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow!("static data has no adjacent SDE"))?
            .join("sde")
            .join(format!("eve-online-static-data-{build}-jsonl"));
        let header: Value = serde_json::from_str(
            &BufReader::new(fs::File::open(raw_dir.join("_sde.jsonl"))?)
                .lines()
                .next()
                .transpose()?
                .ok_or_else(|| anyhow!("empty SDE header"))?,
        )?;
        ensure!(
            header["buildNumber"].as_u64() == Some(build),
            "Blueprint SDE build mismatch"
        );
        let source_path = raw_dir.join("types.jsonl");
        let source_bytes = fs::read(&source_path)?;
        let mut metadata = BTreeMap::new();
        for line in std::str::from_utf8(&source_bytes)?
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            let row: RawType = serde_json::from_str(line)?;
            ensure!(
                metadata.insert(row.id, row.meta).is_none(),
                "duplicate SDE type metadata"
            );
        }
        let recipes: BTreeMap<_, _> = data
            .blueprints
            .iter()
            .map(|b| (b.blueprint_type_id, b))
            .collect();
        let mut items = BTreeMap::new();
        for item in data
            .item_types
            .iter()
            .filter(|i| i.published && i.category_id == Some(9))
        {
            let recipe = recipes.get(&item.type_id);
            let reaction = recipe.is_some_and(|b| b.activities.reaction.is_some());
            let products: Vec<_> = recipe
                .and_then(|b| b.activities.manufacturing.as_ref())
                .map(|a| a.products.iter().map(|p| p.type_id).collect())
                .unwrap_or_default();
            let evidence: Vec<_> = products
                .iter()
                .map(|id| (metadata.get(id).copied().flatten(), data.tech_level(*id)))
                .collect();
            let mut warnings = vec![SEMANTICS_WARNING.into()];
            if item.market_group_id.is_none() {
                warnings.push("No TQ marketGroupID. Visible and configurable in Advanced; no automatic TQ price fallback.".into());
            }
            let mut kind = family(
                reaction,
                &evidence,
                metadata.get(&item.type_id).copied().flatten(),
            );
            // Known authored Sansha blueprint names lack faction meta evidence. Do not call
            // them ordinary T1 or invent a faction fact from fuzzy text matching.
            if matches!(item.type_id, 2179 | 2211) {
                kind = "other";
                warnings.push("Named Sansha blueprint has no authoritative product faction metaGroup; classified as Other / Special for review.".into());
            }
            if kind == "other" {
                warnings.push("Special, missing or conflicting family evidence; inspect the product/recipe before applying a broad policy.".into());
            }
            items.insert(
                item.type_id,
                Blueprint {
                    type_id: item.type_id,
                    name: item.name.clone(),
                    family: kind.into(),
                    market_group_id: item.market_group_id,
                    product_names: products
                        .iter()
                        .map(|id| {
                            data.item_type(*id)
                                .map(|i| i.name.clone())
                                .unwrap_or_else(|| format!("Type {id}"))
                        })
                        .collect(),
                    product_meta_groups: evidence.iter().map(|e| e.0).collect(),
                    product_ids: products,
                    copying_activity: recipe.is_some_and(|b| b.activities.copying.is_some()),
                    max_production_limit: recipe.and_then(|b| b.max_production_limit),
                    warnings,
                },
            );
        }
        Ok(Self {
            items,
            source_path,
            source_sha256: sha256_hex(&source_bytes),
        })
    }
    pub fn ids(&self, family_id: &str) -> Result<Vec<u32>> {
        ensure!(
            FAMILIES.iter().any(|f| f.0 == family_id),
            "unknown Blueprint family"
        );
        Ok(self
            .items
            .values()
            .filter(|i| i.family == family_id)
            .map(|i| i.type_id)
            .collect())
    }
    pub fn schema(&self) -> Value {
        json!({"total":self.items.len(),"no_market_group":self.items.values().filter(|i|i.market_group_id.is_none()).count(),
            "warning":SEMANTICS_WARNING,"families":FAMILIES.iter().map(|(id,label)| {
                let members:Vec<_>=self.items.values().filter(|i|i.family==*id).collect();
                json!({"id":id,"label":label,"count":members.len(),"market_listed":members.iter().filter(|i|i.market_group_id.is_some()).count(),
                    "no_market_group":members.iter().filter(|i|i.market_group_id.is_none()).count(),"rule_id":rule_id(id)})
            }).collect::<Vec<_>>(),"provenance":{"sde_types_path":self.source_path,"sde_types_sha256":self.source_sha256}})
    }
    pub fn apply(
        &self,
        policy: &PolicyDocument,
        family_id: &str,
        settings: &Value,
    ) -> Result<PolicyDocument> {
        let ids = self.ids(family_id)?;
        ensure!(!ids.is_empty(), "Blueprint family has no published types");
        let sides: Sides = serde_json::from_value(settings["sides"].clone())?;
        let side = |key: &str, enabled: bool| -> Result<Option<SidePolicy>> {
            Ok(if enabled {
                Some(serde_json::from_value(settings[key].clone())?)
            } else {
                None
            })
        };
        let id = rule_id(family_id);
        let old_priority = policy.rules.iter().find(|r| r.id == id).map(|r| r.priority);
        let priority = settings["priority"]
            .as_i64()
            .map(i32::try_from)
            .transpose()?
            .unwrap_or_else(|| {
                old_priority.unwrap_or_else(|| {
                    policy
                        .rules
                        .iter()
                        .map(|r| r.priority)
                        .max()
                        .unwrap_or(0)
                        .saturating_add(10)
                })
            });
        let rule = Rule {
            id: id.clone(),
            priority,
            description: Some(serde_json::to_string(
                &json!({"workbench_blueprint_family":1,"family":family_id,"sde_types_sha256":self.source_sha256}),
            )?),
            selector: Selector {
                type_ids: Some(ids),
                group_ids: None,
                category_ids: None,
                fact: None,
            },
            profile: None,
            sides: Some(sides),
            sell: side("sell", sides.has_sell())?,
            buy: side("buy", sides.has_buy())?,
        };
        let mut result = policy.clone();
        result.rules.retain(|r| r.id != id);
        result.rules.push(rule);
        result.validate()?;
        Ok(result)
    }
}
pub fn rule_id(family: &str) -> String {
    format!("wb_blueprint_family_{family}")
}

/// Configuration can be saved with missing blueprint evidence, but a build remains strict.
pub fn configurable_preview(preview: &Value, ids: &BTreeSet<u32>) -> Value {
    let mut value = preview.clone();
    if let Some(items) = value["items"].as_array_mut() {
        for item in items {
            if item["type_id"]
                .as_u64()
                .is_some_and(|id| ids.contains(&(id as u32)))
                && item["unresolved_reason"]
                    .as_str()
                    .is_some_and(|reason| reason != "ROUNDING_CROSS")
            {
                item["unresolved_reason"] = Value::Null;
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn families_use_product_evidence_and_do_not_hide_special_types() {
        for (meta, tech, expected) in [
            (Some(1), 1, "t1"),
            (Some(2), 2, "t2"),
            (Some(4), 2, "faction"),
            (Some(52), 1, "faction"),
            (Some(53), 2, "t2"),
            (Some(54), 1, "t1"),
            (Some(14), 3, "t3"),
            (Some(3), 1, "storyline"),
            (Some(15), 1, "abyssal"),
            (Some(19), 1, "limited_time"),
            (Some(5), 1, "officer"),
            (Some(6), 1, "other"),
        ] {
            assert_eq!(family(false, &[(meta, tech)], None), expected);
        }
        assert_eq!(family(true, &[(Some(2), 2)], None), "reaction");
        assert_eq!(family(false, &[], Some(4)), "other");
        assert_eq!(family(false, &[(Some(1), 1), (Some(4), 1)], None), "other");
        assert_eq!(family(false, &[(None, 1)], Some(4)), "faction");
    }
    #[test]
    fn configuration_warning_never_repairs_preview_or_allows_a_rounded_crossing() {
        let original = json!({"items":[{"type_id":1,"unresolved_reason":"no trade evidence"},{"type_id":2,"unresolved_reason":"ROUNDING_CROSS"},{"type_id":3,"unresolved_reason":"unavailable"}]});
        let filtered = configurable_preview(&original, &[1, 2].into_iter().collect());
        assert!(filtered["items"][0]["unresolved_reason"].is_null());
        assert_eq!(filtered["items"][1]["unresolved_reason"], "ROUNDING_CROSS");
        assert_eq!(filtered["items"][2]["unresolved_reason"], "unavailable");
        assert_eq!(
            original["items"][0]["unresolved_reason"],
            "no trade evidence"
        );
    }
    #[test]
    fn family_rules_are_normal_policy_selectors_with_independent_sources_and_sides() {
        let mut items = BTreeMap::new();
        for (n, (id, _)) in FAMILIES.iter().enumerate() {
            let type_id = n as u32 + 1;
            items.insert(
                type_id,
                Blueprint {
                    type_id,
                    name: id.to_string(),
                    family: id.to_string(),
                    market_group_id: None,
                    product_ids: vec![],
                    product_names: vec![],
                    product_meta_groups: vec![],
                    copying_activity: false,
                    max_production_limit: Some(1),
                    warnings: vec![SEMANTICS_WARNING.into()],
                },
            );
        }
        let catalog = Catalog {
            items,
            source_path: PathBuf::new(),
            source_sha256: "fixture".into(),
        };
        let policy = PolicyDocument::parse(r#"{"format_version":1,"catalog_contract":{"sde_build":1,"fact_registry_version":1},"profiles":[],"rules":[]}"#).unwrap();
        let settings = json!({"sides":"buy_sell","sell":{"source":"tq_snapshot","multiplier":"1.25"},"buy":{"source":"funded_cost","multiplier":"0.80"}});
        for (id, _) in FAMILIES {
            let changed = catalog.apply(&policy, id, &settings).unwrap();
            let rule = &changed.rules[0];
            assert_eq!(rule.selector.type_ids.as_ref().unwrap().len(), 1);
            assert_eq!(
                rule.sell.as_ref().unwrap().source,
                crate::policy::Source::TqSnapshot
            );
            assert_eq!(
                rule.buy.as_ref().unwrap().source,
                crate::policy::Source::FundedCost
            );
            assert_eq!(rule.sell.as_ref().unwrap().multiplier.0, "1.25");
            assert_eq!(rule.buy.as_ref().unwrap().multiplier.0, "0.80");
            let repeated = catalog.apply(&changed, id, &settings).unwrap();
            assert_eq!(
                changed.canonical_json().unwrap(),
                repeated.canonical_json().unwrap()
            );
            for sides in ["buy_only", "sell_only", "unseeded"] {
                let mut setting = settings.clone();
                setting["sides"] = json!(sides);
                let changed = catalog.apply(&policy, id, &setting).unwrap();
                assert_eq!(changed.rules[0].buy.is_some(), sides == "buy_only");
                assert_eq!(changed.rules[0].sell.is_some(), sides == "sell_only");
            }
        }
        let mut invalid = settings;
        invalid["sell"]["multiplier"] = json!("-1");
        assert!(catalog.apply(&policy, "faction", &invalid).is_err());
        assert!(catalog.apply(&policy, "unknown", &invalid).is_err());
        assert!(policy.rules.is_empty());
        assert_eq!(catalog.schema()["no_market_group"], 10);
    }
}
