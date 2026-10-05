//! Additional preset authorities. Reuses NPC acquisition eligibility and the shared
//! price arithmetic; linked meta quotes read the final T1 policy, never fuzzy names.
use crate::classify::Classification;
use crate::funded::FundedPath;
use crate::policy::{Aggregate, Source};
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
    /// `Source::SiblingFamily` index: type -> the same-name-family siblings that may
    /// supply a quote, nearest family first.
    pub families: BTreeMap<u32, FamilySiblings>,
}
pub type Funded = BTreeMap<u32, std::result::Result<FundedPath, String>>;

/// Candidate quote sources for one type: the exact family (same base name **and** same
/// groupID) and the relaxed one (same base name and marketGroupID).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FamilySiblings {
    pub exact: Vec<u32>,
    pub relaxed: Vec<u32>,
}

/// The family key of a variant name: the English name with a trailing ` (variant)`
/// suffix removed. CCP's SDE carries no `variationParentTypeID` for apparel colours
/// (verified against build 3396210), so for those the name is the only authoritative
/// grouping; requiring a shared groupID keeps unrelated items that merely share words
/// apart, and the caller relaxes to the marketGroupID only when the exact family has
/// nothing priceable.
pub(crate) fn base_name(name: &str) -> Option<&str> {
    let trimmed = name.trim_end();
    if !trimmed.ends_with(')') {
        return None;
    }
    let open = trimmed.rfind(" (")?;
    let base = trimmed[..open].trim_end();
    (!base.is_empty() && base.len() >= 4).then_some(base)
}

/// Build the [`Source::SiblingFamily`] index from the published, marketable catalog.
fn name_families(data: &StaticData) -> BTreeMap<u32, FamilySiblings> {
    let mut exact: BTreeMap<(&str, u32), Vec<u32>> = BTreeMap::new();
    let mut relaxed: BTreeMap<(&str, u32), Vec<u32>> = BTreeMap::new();
    for item in &data.item_types {
        if !item.is_marketable() {
            continue;
        }
        let Some(base) = base_name(&item.name) else {
            continue;
        };
        if let Some(group) = item.group_id {
            exact.entry((base, group)).or_default().push(item.type_id);
        }
        if let Some(market_group) = item.market_group_id {
            relaxed
                .entry((base, market_group))
                .or_default()
                .push(item.type_id);
        }
    }
    let mut families = BTreeMap::new();
    for item in &data.item_types {
        if !item.is_marketable() {
            continue;
        }
        let Some(base) = base_name(&item.name) else {
            continue;
        };
        let mut siblings = FamilySiblings::default();
        if let Some(group) = item.group_id {
            if let Some(members) = exact.get(&(base, group)) {
                siblings.exact = members
                    .iter()
                    .copied()
                    .filter(|id| *id != item.type_id)
                    .collect();
            }
        }
        if let Some(market_group) = item.market_group_id {
            if let Some(members) = relaxed.get(&(base, market_group)) {
                siblings.relaxed = members
                    .iter()
                    .copied()
                    .filter(|id| *id != item.type_id)
                    .collect();
            }
        }
        if !siblings.exact.is_empty() || !siblings.relaxed.is_empty() {
            families.insert(item.type_id, siblings);
        }
    }
    families
}

/// Combine the sibling quotes a [`Source::SiblingFamily`] side found.
fn combine_quotes(mode: Aggregate, mut quotes: Vec<f64>) -> f64 {
    quotes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let middle = quotes.len() / 2;
    match mode {
        Aggregate::Min => quotes[0],
        Aggregate::Mean => quotes.iter().sum::<f64>() / quotes.len() as f64,
        Aggregate::Median => {
            if quotes.len() % 2 == 1 {
                quotes[middle]
            } else {
                (quotes[middle - 1] + quotes[middle]) / 2.0
            }
        }
    }
}

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
        result.families = name_families(data);
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
                Source::SiblingFamily => Some(self.sibling_reference(
                    book,
                    type_id,
                    side,
                    rule,
                    funded,
                    resolutions,
                    stack,
                )?),
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

    /// Combine the final same-side quotes of the siblings in this type's name family.
    /// The exact family (same base name and groupID) is tried first; when nothing in it
    /// resolves, the relaxed family (same base name and marketGroupID) is used. Siblings
    /// already on the recursion stack are skipped, so a family whose every member asks
    /// the family for a price fails as unresolved instead of looping.
    fn sibling_reference(
        &self,
        book: &TqSnapshot,
        type_id: u32,
        side: Side,
        rule: &ResolvedSide,
        funded: &Funded,
        resolutions: &BTreeMap<u32, Resolution>,
        stack: &mut BTreeSet<(u32, u8)>,
    ) -> Result<f64> {
        Ok(self
            .sibling_quotes(book, type_id, side, rule, funded, resolutions, stack)?
            .1)
    }

    /// The sibling quotes that fed the aggregate, as `(type_id, price)`, plus the combined
    /// value. Kept separate from [`Self::sibling_reference`] so the traces can show exactly
    /// which siblings were used - with prices this far apart the operator needs to see them.
    fn sibling_quotes(
        &self,
        book: &TqSnapshot,
        type_id: u32,
        side: Side,
        rule: &ResolvedSide,
        funded: &Funded,
        resolutions: &BTreeMap<u32, Resolution>,
        stack: &mut BTreeSet<(u32, u8)>,
    ) -> Result<(Vec<(u32, f64)>, f64)> {
        let mode = rule.aggregate.ok_or_else(|| {
            anyhow!("type {type_id}: sibling_family side carries no aggregate mode")
        })?;
        let family = self.families.get(&type_id).ok_or_else(|| {
            anyhow!("type {type_id}: no sibling item shares its name family")
        })?;
        let side_slot = if side == Side::Sell { 0 } else { 1 };
        let mut used = Vec::new();
        for candidates in [&family.exact, &family.relaxed] {
            let mut values = Vec::new();
            for sibling in candidates {
                if *sibling == type_id || stack.contains(&(*sibling, side_slot)) {
                    continue;
                }
                let Some(policy) = resolutions
                    .get(sibling)
                    .and_then(|resolution| resolution.policy.as_ref())
                else {
                    continue;
                };
                let sibling_rule = if side == Side::Sell {
                    policy.sell.as_ref()
                } else {
                    policy.buy.as_ref()
                };
                let Some(sibling_rule) = sibling_rule else {
                    continue;
                };
                if let Ok(value) = self.price_inner(
                    book,
                    *sibling,
                    side,
                    sibling_rule,
                    funded,
                    resolutions,
                    stack,
                ) {
                    if value.is_finite() && value > 0.0 {
                        values.push((*sibling, value));
                    }
                }
            }
            if !values.is_empty() {
                used = values;
                break;
            }
        }
        ensure!(
            !used.is_empty(),
            "type {type_id}: no sibling in its name family has a usable {side:?} quote"
        );
        let combined = combine_quotes(mode, used.iter().map(|(_, value)| *value).collect());
        Ok((used, combined))
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
        } else if let Some(side) = [&policy.sell, &policy.buy]
            .into_iter()
            .flatten()
            .find(|side| side.source == Source::SiblingFamily)
        {
            Some(json!({"authority":"sibling_name_family","aggregate":side.aggregate,
                "exact_siblings":self.families.get(&id).map(|f|f.exact.clone()),
                "relaxed_siblings":self.families.get(&id).map(|f|f.relaxed.clone())}))
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
        if let Some(sibling) = self.sibling_trace(id, resolution, book, funded, resolutions) {
            trace["sibling_family"] = sibling;
        }
        Some(trace)
    }

    /// Sibling provenance for the detail view: which siblings were quoted, what each was
    /// worth, and what the chosen aggregate produced. Sibling prices can differ by orders of
    /// magnitude, so the operator must be able to see the numbers behind the quote.
    pub fn sibling_trace(
        &self,
        id: u32,
        resolution: &Resolution,
        book: &TqSnapshot,
        funded: &Funded,
        resolutions: &BTreeMap<u32, Resolution>,
    ) -> Option<Value> {
        let policy = resolution.policy.as_ref()?;
        for (key, side, rule) in [
            ("sell", Side::Sell, policy.sell.as_ref()),
            ("buy", Side::Buy, policy.buy.as_ref()),
        ] {
            let Some(rule) = rule else { continue };
            if rule.source != Source::SiblingFamily {
                continue;
            }
            let mut stack = BTreeSet::new();
            return Some(match self.sibling_quotes(book, id, side, rule, funded, resolutions, &mut stack)
            {
                Ok((quotes, combined)) => json!({
                    "side": key,
                    "aggregate": rule.aggregate,
                    // The combined sibling reference, before this side's multiplier and the
                    // shared cent rounding that produces the final quote.
                    "reference": combined,
                    "sibling_count": quotes.len(),
                    "siblings": quotes.iter().map(|(sibling, value)| json!({
                        "type_id": sibling,
                        "price": value,
                    })).collect::<Vec<_>>(),
                }),
                Err(error) => json!({"side": key, "aggregate": rule.aggregate, "unresolved": error.to_string()}),
            });
        }
        None
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
            price: None,
            price_text: None,
            aggregate: None,
        }
    }

    /// A `sibling_family` side with the given aggregation mode.
    fn family_rule(aggregate: Aggregate, multiplier: f64) -> ResolvedSide {
        ResolvedSide {
            aggregate: Some(aggregate),
            ..rule(Source::SiblingFamily, multiplier)
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

    /// Resolve a policy whose rules price the listed types with explicit manual prices.
    fn resolutions_from_rules(rules: &str) -> BTreeMap<u32, Resolution> {
        let text = format!(
            r#"{{"format_version":1,"catalog_contract":{{"sde_build":1,"fact_registry_version":1}},"profiles":[],"rules":[{rules}]}}"#
        );
        let policy = PolicyDocument::parse(&text).unwrap();
        let mut out = BTreeMap::new();
        for rule in &policy.rules {
            for id in rule.selector.type_ids.clone().unwrap_or_default() {
                out.insert(
                    id,
                    policy.resolve_validated(id, None, None, &BTreeSet::new()).unwrap(),
                );
            }
        }
        out
    }

    fn manual_rules(prices: &[(u32, &str)]) -> String {
        prices
            .iter()
            .map(|(id, price)| {
                format!(
                    r#"{{"id":"m{id}","priority":1,"selector":{{"type_ids":[{id}]}},"sides":"sell_only","sell":{{"source":"manual_fixed","multiplier":"1","price":"{price}"}}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    #[test]
    fn manual_fixed_prices_the_side_and_still_applies_the_multiplier() {
        let resolutions = resolutions_from_rules(&manual_rules(&[(1, "1000")]));
        let refs = References::default();
        let mut half = rule(Source::ManualFixed, 0.5);
        half.price = Some(1000.0);
        half.price_text = Some("1000".into());
        assert_eq!(
            refs.price(&book(), 1, Side::Sell, &half, &Funded::new(), &resolutions)
                .unwrap(),
            500.0
        );
        // A missing price is a policy defect; the error must say so rather than blaming the
        // market for having no reference.
        let error = refs
            .price(
                &book(),
                1,
                Side::Sell,
                &rule(Source::ManualFixed, 1.0),
                &Funded::new(),
                &resolutions,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("manual_fixed"), "{error}");
    }

    #[test]
    fn sibling_family_combines_the_final_quotes_of_its_family() {
        // Type 9's exact family holds three priced colours; its relaxed family holds a fourth
        // that is cheaper, which must NOT leak into the exact result.
        let resolutions = resolutions_from_rules(&manual_rules(&[
            (1, "100"),
            (2, "200"),
            (3, "400"),
            (4, "1"),
        ]));
        let mut refs = References::default();
        refs.families.insert(
            9,
            FamilySiblings {
                exact: vec![1, 2, 3],
                relaxed: vec![4],
            },
        );
        refs.families.insert(
            10,
            FamilySiblings {
                exact: vec![1, 2],
                relaxed: vec![],
            },
        );
        let price = |id: u32, mode: Aggregate| {
            refs.price(
                &book(),
                id,
                Side::Sell,
                &family_rule(mode, 1.0),
                &Funded::new(),
                &resolutions,
            )
            .unwrap()
        };
        assert_eq!(price(9, Aggregate::Min), 100.0);
        assert_eq!(price(9, Aggregate::Median), 200.0);
        // 700/3 rounded to the cent by the shared quote arithmetic.
        assert_eq!(price(9, Aggregate::Mean), 233.33);
        // An even count takes the mean of the two middle quotes.
        assert_eq!(price(10, Aggregate::Median), 150.0);
        // The side's own multiplier still applies to the combined reference.
        assert_eq!(
            refs.price(
                &book(),
                9,
                Side::Sell,
                &family_rule(Aggregate::Min, 2.0),
                &Funded::new(),
                &resolutions,
            )
            .unwrap(),
            200.0
        );
    }

    #[test]
    fn sibling_family_reports_an_unpriceable_family_instead_of_looping() {
        // Two types that can only price each other: the recursion guard must turn this into a
        // plain "no sibling" error rather than overflowing the stack.
        let resolutions = resolutions_from_rules(
            r#"{"id":"a","priority":1,"selector":{"type_ids":[5]},"sides":"sell_only","sell":{"source":"sibling_family","multiplier":"1","aggregate":"min"}},
               {"id":"b","priority":1,"selector":{"type_ids":[6]},"sides":"sell_only","sell":{"source":"sibling_family","multiplier":"1","aggregate":"min"}}"#,
        );
        let mut refs = References::default();
        refs.families.insert(
            5,
            FamilySiblings {
                exact: vec![6],
                relaxed: vec![],
            },
        );
        refs.families.insert(
            6,
            FamilySiblings {
                exact: vec![5],
                relaxed: vec![],
            },
        );
        let error = refs
            .price(
                &book(),
                5,
                Side::Sell,
                &family_rule(Aggregate::Min, 1.0),
                &Funded::new(),
                &resolutions,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("no sibling"), "{error}");

        // No family at all is also an error, never a zero price.
        let lonely = refs
            .price(
                &book(),
                5,
                Side::Sell,
                &family_rule(Aggregate::Min, 1.0),
                &Funded::new(),
                &resolutions_from_rules(&manual_rules(&[])),
            )
            .unwrap_err()
            .to_string();
        assert!(lonely.contains("no sibling"), "{lonely}");
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
