//! Additional preset authorities. Reuses NPC acquisition eligibility and the shared
//! price arithmetic; linked meta quotes read the final T1 policy, never fuzzy names.
use crate::classify::Classification;
use crate::funded::FundedPath;
use crate::policy::Source;
use crate::policy_resolve::{Resolution, ResolvedSide};
use crate::staticdata::{StaticData, VariantFamilies};
use crate::tq_snapshot::{Side, TqSnapshot};
use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct References {
    pub npc: BTreeMap<u32, (f64, String)>,
    pub t1_parents: BTreeMap<u32, u32>,
}
pub type Funded = BTreeMap<u32, std::result::Result<FundedPath, String>>;

impl References {
    pub fn from_static(
        data: &StaticData,
        classification: &Classification,
        overlay: &crate::overlay::OverlayImport,
        variants: &VariantFamilies,
    ) -> Self {
        let mut result = Self::default();
        for item in &data.item_types {
            if !item.published {
                continue;
            }
            if let Some(price) = overlay.npc_min_sell(item.type_id) {
                result
                    .npc
                    .insert(item.type_id, (price, "captured_npc_min_sell".into()));
            } else if classification.is_published_reaction_formula(data, item.type_id)
                || (overlay.is_npc_catalog_type(item.type_id)
                    && classification.is_approved_reusable_bpo(data, item.type_id))
            {
                if let Some((price, _)) = crate::seedplan::bpo_reference(
                    item.type_id,
                    item.base_price,
                    Some(overlay),
                    classification.is_published_reaction_formula(data, item.type_id),
                ) {
                    result.npc.insert(
                        item.type_id,
                        (price, "eligible_recipe_base_reference".into()),
                    );
                }
            }
            if !item.is_marketable()
                || item.category_id != Some(7)
                || data.tech_level(item.type_id) >= 2
                || data.meta_group_id(item.type_id).is_some_and(|m| m >= 2)
            {
                continue;
            }
            if let Some(parent) = variants.parent(item.type_id).filter(|p| *p != item.type_id) {
                if data.item_type(parent).is_some_and(|p| {
                    p.is_marketable()
                        && p.category_id == Some(7)
                        && p.group_id == item.group_id
                        && data.tech_level(parent) < 2
                        && data.meta_group_id(parent).is_none_or(|m| m <= 1)
                        && variants.parent(parent).is_none_or(|id| id == parent)
                }) {
                    result.t1_parents.insert(item.type_id, parent);
                }
            }
        }
        result
    }

    pub fn price(
        &self,
        book: &TqSnapshot,
        type_id: u32,
        side: Side,
        rule: &ResolvedSide,
        funded: &Funded,
        resolutions: &BTreeMap<u32, Resolution>,
    ) -> Result<f64> {
        self.price_inner(
            book,
            type_id,
            side,
            rule,
            funded,
            resolutions,
            &mut BTreeSet::new(),
        )
    }

    fn price_inner(
        &self,
        book: &TqSnapshot,
        type_id: u32,
        side: Side,
        rule: &ResolvedSide,
        funded: &Funded,
        resolutions: &BTreeMap<u32, Resolution>,
        stack: &mut BTreeSet<(u32, u8)>,
    ) -> Result<f64> {
        let key = (type_id, if side == Side::Sell { 0 } else { 1 });
        ensure!(
            stack.insert(key),
            "cyclic linked T1 quote at type {type_id}"
        );
        let result = (|| {
            let reference = match rule.source {
                Source::NpcAcquisition => Some(
                    self.npc
                        .get(&type_id)
                        .ok_or_else(|| {
                            anyhow!("type {type_id}: no proven NPC acquisition reference")
                        })?
                        .0,
                ),
                Source::T1VariantSell | Source::T1VariantBuy => {
                    let parent_side = if rule.source == Source::T1VariantSell {
                        Side::Sell
                    } else {
                        Side::Buy
                    };
                    ensure!(
                        side == parent_side,
                        "linked T1 source does not match quote side"
                    );
                    let parent = *self.t1_parents.get(&type_id).ok_or_else(|| {
                        anyhow!("type {type_id}: no authoritative ordinary T1 variant parent")
                    })?;
                    let policy = resolutions
                        .get(&parent)
                        .and_then(|r| r.policy.as_ref())
                        .ok_or_else(|| anyhow!("type {type_id}: T1 parent {parent} is unseeded"))?;
                    let parent_rule = if parent_side == Side::Sell {
                        &policy.sell
                    } else {
                        &policy.buy
                    };
                    let parent_rule = parent_rule.as_ref().ok_or_else(|| {
                        anyhow!("type {type_id}: T1 parent {parent} has no requested side")
                    })?;
                    Some(self.price_inner(
                        book,
                        parent,
                        parent_side,
                        parent_rule,
                        funded,
                        resolutions,
                        stack,
                    )?)
                }
                _ => None,
            };
            if let Some(reference) = reference {
                crate::general_tq::apply_reference(type_id, side, rule, reference)
            } else {
                let cost = funded
                    .get(&type_id)
                    .and_then(|r| r.as_ref().ok())
                    .map(|p| p.unit_cost);
                crate::general_tq::price_with_funded(book, type_id, side, rule, cost)
            }
        })();
        stack.remove(&key);
        result
    }

    pub fn trace(&self, id: u32, resolution: &Resolution) -> Option<Value> {
        let policy = resolution.policy.as_ref()?;
        if [&policy.sell, &policy.buy]
            .into_iter()
            .flatten()
            .any(|s| matches!(s.source, Source::T1VariantSell | Source::T1VariantBuy))
        {
            Some(json!({"authority":"sde_variation_parent_final_quote",
                "parent_type_id":self.t1_parents.get(&id),"sell_contract":"parent final Sell times policy multiplier",
                "buy_contract":"parent final Buy times policy multiplier","no_name_matching":true}))
        } else if [&policy.sell, &policy.buy]
            .into_iter()
            .flatten()
            .any(|s| s.source == Source::NpcAcquisition)
        {
            Some(
                json!({"authority":"npc_acquisition","reference":self.npc.get(&id).map(|p|p.0),
                "provenance":self.npc.get(&id).map(|p|&p.1)}),
            )
        } else {
            None
        }
    }

    pub fn trace_with_quotes(
        &self,
        id: u32,
        resolution: &Resolution,
        book: &TqSnapshot,
        funded: &Funded,
        resolutions: &BTreeMap<u32, Resolution>,
    ) -> Option<Value> {
        let mut trace = self.trace(id, resolution)?;
        if let Some(parent) = self.t1_parents.get(&id) {
            if let Some(policy) = resolutions.get(parent).and_then(|r| r.policy.as_ref()) {
                trace["parent_policy"] = json!(policy);
                for (key, side, rule) in [
                    ("reference_sell", Side::Sell, &policy.sell),
                    ("reference_buy", Side::Buy, &policy.buy),
                ] {
                    if let Some(rule) = rule {
                        match self.price(book, *parent, side, rule, funded, resolutions) {
                            Ok(value) => trace[key] = json!(value),
                            Err(error) => {
                                trace[format!("{key}_unresolved")] = json!(error.to_string())
                            }
                        }
                    }
                }
            }
        }
        Some(trace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyDocument;
    #[test]
    fn side_contracts_parse_and_reject_wrong_linked_side() {
        let text = r#"{"format_version":1,"catalog_contract":{"sde_build":1,"fact_registry_version":1},"profiles":[],"rules":[{"id":"test","priority":1,"selector":{"type_ids":[1]},"sides":"buy_sell","sell":{"source":"t1_variant_sell","multiplier":"2"},"buy":{"source":"t1_variant_buy","multiplier":"1"}}]}"#;
        assert!(PolicyDocument::parse(text).is_ok());
        assert!(PolicyDocument::parse(&text.replace("t1_variant_buy", "t1_variant_sell")).is_err());
        assert!(PolicyDocument::parse(&text.replace("t1_variant_sell", "t1_variant_buy")).is_err());
        assert!(
            PolicyDocument::parse(
                &text
                    .replace("t1_variant_sell", "npc_acquisition")
                    .replace("t1_variant_buy", "npc_acquisition")
            )
            .is_ok()
        );
    }
    fn rule(source: Source, multiplier: f64) -> ResolvedSide {
        ResolvedSide {
            source,
            multiplier,
            multiplier_text: multiplier.to_string(),
            floor: None,
            floor_text: None,
        }
    }
    fn book() -> TqSnapshot {
        TqSnapshot {
            capture_id: "test".into(),
            captured_at: "test".into(),
            aggregation: "fixture".into(),
            records: BTreeMap::new(),
        }
    }
    #[test]
    fn npc_eligibility_no_trade_fallback() {
        let mut refs = References::default();
        refs.npc.insert(1, (1000.0, "fixture".into()));
        assert_eq!(
            refs.price(
                &book(),
                1,
                Side::Sell,
                &rule(Source::NpcAcquisition, 0.1),
                &Funded::new(),
                &BTreeMap::new()
            )
            .unwrap(),
            100.0
        );
        assert_eq!(
            refs.price(
                &book(),
                1,
                Side::Buy,
                &rule(Source::NpcAcquisition, 0.05),
                &Funded::new(),
                &BTreeMap::new()
            )
            .unwrap(),
            50.0
        );
        assert!(
            refs.price(
                &book(),
                2,
                Side::Buy,
                &rule(Source::NpcAcquisition, 1.0),
                &Funded::new(),
                &BTreeMap::new()
            )
            .is_err()
        );
    }
    #[test]
    fn meta_quotes_follow_parent_policy_and_cycles_fail() {
        let p=PolicyDocument::parse(r#"{"format_version":1,"catalog_contract":{"sde_build":1,"fact_registry_version":1},"profiles":[],"rules":[{"id":"parent","priority":1,"selector":{"type_ids":[1]},"sides":"buy_sell","sell":{"source":"npc_acquisition","multiplier":"1.2"},"buy":{"source":"npc_acquisition","multiplier":"0.8"}}]}"#).unwrap();
        let mut resolutions = BTreeMap::new();
        resolutions.insert(1, p.resolve(1, None, None, &BTreeSet::new()).unwrap());
        let mut refs = References::default();
        refs.npc.insert(1, (100.0, "fixture".into()));
        refs.t1_parents.insert(2, 1);
        assert_eq!(
            refs.price(
                &book(),
                2,
                Side::Sell,
                &rule(Source::T1VariantSell, 2.0),
                &Funded::new(),
                &resolutions
            )
            .unwrap(),
            240.0
        );
        assert_eq!(
            refs.price(
                &book(),
                2,
                Side::Buy,
                &rule(Source::T1VariantBuy, 1.0),
                &Funded::new(),
                &resolutions
            )
            .unwrap(),
            80.0
        );
        let variant_resolution = resolutions[&1].clone();
        let trace = refs
            .trace_with_quotes(
                2,
                &Resolution {
                    type_id: 2,
                    policy: Some(crate::policy_resolve::ResolvedItemPolicy {
                        profile_id: Some("renamed_profile".into()),
                        sides: crate::policy::Sides::BuySell,
                        sell: Some(rule(Source::T1VariantSell, 2.0)),
                        buy: Some(rule(Source::T1VariantBuy, 1.0)),
                    }),
                    ..variant_resolution
                },
                &book(),
                &Funded::new(),
                &resolutions,
            )
            .unwrap();
        assert_eq!(trace["reference_sell"], 120.0);
        assert_eq!(trace["reference_buy"], 80.0);
        resolutions
            .get_mut(&1)
            .unwrap()
            .policy
            .as_mut()
            .unwrap()
            .sell
            .as_mut()
            .unwrap()
            .multiplier = 3.0;
        assert_eq!(
            refs.price(
                &book(),
                2,
                Side::Sell,
                &rule(Source::T1VariantSell, 2.0),
                &Funded::new(),
                &resolutions
            )
            .unwrap(),
            600.0
        );
        refs.t1_parents.insert(1, 2);
        let mut r = resolutions[&1].clone();
        r.type_id = 2;
        r.policy.as_mut().unwrap().sell = Some(rule(Source::T1VariantSell, 1.0));
        resolutions.insert(2, r);
        resolutions
            .get_mut(&1)
            .unwrap()
            .policy
            .as_mut()
            .unwrap()
            .sell = Some(rule(Source::T1VariantSell, 1.0));
        assert!(
            refs.price(
                &book(),
                2,
                Side::Sell,
                &rule(Source::T1VariantSell, 2.0),
                &Funded::new(),
                &resolutions
            )
            .unwrap_err()
            .to_string()
            .contains("cyclic")
        );
    }
}
