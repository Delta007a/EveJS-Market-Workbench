//! Deterministic Market policy selection over versioned, externally supplied catalog facts.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail, ensure};
use serde::Serialize;

use crate::policy::{Aggregate, PolicyDocument, Rule, Selector, SidePolicy, Sides, Source};

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedSide {
    pub source: Source,
    /// V1-compatible parsed f64, consumed by the existing cent-rounding path.
    pub multiplier: f64,
    pub multiplier_text: String,
    pub floor: Option<f64>,
    pub floor_text: Option<String>,
    /// The operator's absolute quote for `Source::ManualFixed`, before multiplier/floor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<f64>,
    pub price_text: Option<String>,
    /// How `Source::SiblingFamily` combines its sibling quotes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<Aggregate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedItemPolicy {
    pub profile_id: Option<String>,
    pub sides: Sides,
    pub sell: Option<ResolvedSide>,
    pub buy: Option<ResolvedSide>,
}

impl ResolvedItemPolicy {
    fn semantically_eq(&self, other: &Self) -> bool {
        self.sides == other.sides
            && sides_equal(&self.sell, &other.sell)
            && sides_equal(&self.buy, &other.buy)
    }
}

fn sides_equal(a: &Option<ResolvedSide>, b: &Option<ResolvedSide>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.source == b.source
                && a.multiplier.to_bits() == b.multiplier.to_bits()
                && a.floor.map(f64::to_bits) == b.floor.map(f64::to_bits)
                // A manual price and a sibling aggregation mode are part of the side's
                // meaning: two equal-rank rules differing only there are ambiguous, not equal.
                && a.price.map(f64::to_bits) == b.price.map(f64::to_bits)
                && a.aggregate == b.aggregate
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleMatch {
    pub rule_id: String,
    pub priority: i32,
    pub specificity: u8,
    pub profile_id: Option<String>,
    pub output: ResolvedItemPolicy,
    pub disposition: MatchDisposition,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchDisposition {
    Winner,
    EqualWinner,
    Overridden,
}

#[derive(Debug, Clone, Serialize)]
pub struct Resolution {
    pub type_id: u32,
    pub group_id: Option<u32>,
    pub category_id: Option<u32>,
    pub matched_rules: Vec<RuleMatch>,
    pub winner_rule_id: Option<String>,
    pub equal_winner_rule_ids: Vec<String>,
    pub overridden_rule_ids: Vec<String>,
    pub priority_overrode_specificity: bool,
    pub exclusion_reason: Option<String>,
    /// None for explicit unseeded exclusion or unmatched type.
    pub policy: Option<ResolvedItemPolicy>,
}

#[derive(Debug, Clone)]
pub struct CatalogItem {
    pub type_id: u32,
    pub group_id: Option<u32>,
    pub category_id: Option<u32>,
    /// All true facts from a complete fact-registry evaluation for this item.
    pub facts: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleCoverage {
    pub rule_id: String,
    pub match_count: usize,
    pub winning_count: usize,
}

impl PolicyDocument {
    /// Resolve a complete true-fact set. The caller must compute every registered fact for
    /// this catalog item; use `resolve_with_evidence` when a fact can be unknown.
    pub fn resolve(
        &self,
        type_id: u32,
        group_id: Option<u32>,
        category_id: Option<u32>,
        facts: &BTreeSet<String>,
    ) -> Result<Resolution> {
        self.resolve_internal(type_id, group_id, category_id, true, &|fact| {
            Ok(facts.contains(fact))
        })
    }

    /// Use only after a complete document-level validate() in a bulk offline preview/build.
    pub fn resolve_validated(
        &self,
        type_id: u32,
        group_id: Option<u32>,
        category_id: Option<u32>,
        facts: &BTreeSet<String>,
    ) -> Result<Resolution> {
        self.resolve_internal(type_id, group_id, category_id, false, &|fact| {
            Ok(facts.contains(fact))
        })
    }

    /// Nullable evidence path: a selected fact without evidence is an error, never false.
    pub fn resolve_with_evidence(
        &self,
        type_id: u32,
        group_id: Option<u32>,
        category_id: Option<u32>,
        facts: &BTreeMap<String, Option<bool>>,
    ) -> Result<Resolution> {
        self.resolve_internal(type_id, group_id, category_id, true, &|fact| {
            facts
                .get(fact)
                .copied()
                .flatten()
                .ok_or_else(|| anyhow::anyhow!("type {type_id}: fact {fact} has no known evidence"))
        })
    }

    fn resolve_internal(
        &self,
        type_id: u32,
        group_id: Option<u32>,
        category_id: Option<u32>,
        validate_before: bool,
        fact_value: &impl Fn(&str) -> Result<bool>,
    ) -> Result<Resolution> {
        ensure!(type_id > 0, "type ID must be positive");
        if validate_before {
            self.validate()?;
        }
        let mut matches = Vec::new();
        for rule in &self.rules {
            if selector_matches(&rule.selector, type_id, group_id, category_id, fact_value)? {
                matches.push(RuleMatch {
                    rule_id: rule.id.clone(),
                    priority: rule.priority,
                    specificity: rule.selector.specificity(),
                    profile_id: rule.profile.clone(),
                    output: self.expand(rule)?,
                    disposition: MatchDisposition::Overridden,
                });
            }
        }
        matches.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| b.specificity.cmp(&a.specificity))
                .then_with(|| a.rule_id.cmp(&b.rule_id))
        });

        // An exact-type exclusion is a guardrail over broader positive selectors. Explicit
        // contradictory exact-type rules at the same priority are rejected even if another
        // higher-ranked rule would otherwise hide the collision.
        for exclusion in matches
            .iter()
            .filter(|m| m.specificity == 3 && m.output.sides == Sides::Unseeded)
        {
            let conflicts: Vec<_> = matches
                .iter()
                .filter(|m| {
                    m.specificity == 3
                        && m.priority == exclusion.priority
                        && m.output.sides != Sides::Unseeded
                })
                .map(|m| m.rule_id.as_str())
                .collect();
            if !conflicts.is_empty() {
                bail!(
                    "type {type_id}: contradictory exact-type exclusion {} and seeded rules {}",
                    exclusion.rule_id,
                    conflicts.join(", ")
                );
            }
        }
        let guard_rank = matches
            .iter()
            .enumerate()
            .filter(|(_, m)| m.specificity == 3 && m.output.sides == Sides::Unseeded)
            .map(|(i, _)| i)
            .next();
        if let Some(index) = guard_rank {
            let guarded_priority = matches[index].priority;
            let mut guarded: Vec<_> = matches
                .drain(..)
                .filter(|m| {
                    m.specificity == 3
                        && m.output.sides == Sides::Unseeded
                        && m.priority == guarded_priority
                })
                .collect();
            guarded.sort_by(|a, b| a.rule_id.cmp(&b.rule_id));
            // Keep all non-guarded matches in the trace, ordered after the guard.
            let mut remaining = Vec::new();
            for rule in &self.rules {
                if let Some(m) =
                    self.match_rule(rule, type_id, group_id, category_id, fact_value)?
                {
                    if !(m.specificity == 3
                        && m.output.sides == Sides::Unseeded
                        && m.priority == guarded_priority)
                    {
                        remaining.push(m);
                    }
                }
            }
            remaining.sort_by(|a, b| {
                b.priority
                    .cmp(&a.priority)
                    .then_with(|| b.specificity.cmp(&a.specificity))
                    .then_with(|| a.rule_id.cmp(&b.rule_id))
            });
            guarded.extend(remaining);
            matches = guarded;
        }

        let Some(first) = matches.first() else {
            return Ok(Resolution {
                type_id,
                group_id,
                category_id,
                matched_rules: vec![],
                winner_rule_id: None,
                equal_winner_rule_ids: vec![],
                overridden_rule_ids: vec![],
                priority_overrode_specificity: false,
                exclusion_reason: Some("no_matching_rule".into()),
                policy: None,
            });
        };
        let top_priority = first.priority;
        let top_specificity = first.specificity;
        let guard = first.specificity == 3 && first.output.sides == Sides::Unseeded;
        let top: Vec<_> = matches
            .iter()
            .filter(|m| {
                m.priority == top_priority
                    && m.specificity == top_specificity
                    && (!guard || m.output.sides == Sides::Unseeded)
            })
            .collect();
        let conflicting: Vec<_> = top
            .iter()
            .filter(|m| !m.output.semantically_eq(&first.output))
            .map(|m| m.rule_id.as_str())
            .collect();
        if !conflicting.is_empty() {
            let ids = top
                .iter()
                .map(|m| m.rule_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("type {type_id}: ambiguous equal-rank policy among rules {ids}");
        }
        let equal_ids = top.iter().map(|m| m.rule_id.clone()).collect::<Vec<_>>();
        let winner_id = equal_ids.iter().min().cloned().expect("nonempty top rank");
        let priority_overrode_specificity = !guard
            && matches
                .iter()
                .any(|m| m.priority < top_priority && m.specificity > top_specificity);
        let overridden = matches
            .iter()
            .filter(|m| !equal_ids.contains(&m.rule_id))
            .map(|m| m.rule_id.clone())
            .collect();
        let output = first.output.clone();
        for matched in &mut matches {
            matched.disposition = if matched.rule_id == winner_id {
                MatchDisposition::Winner
            } else if equal_ids.contains(&matched.rule_id) {
                MatchDisposition::EqualWinner
            } else {
                MatchDisposition::Overridden
            };
        }
        Ok(Resolution {
            type_id,
            group_id,
            category_id,
            matched_rules: matches,
            winner_rule_id: Some(winner_id),
            equal_winner_rule_ids: equal_ids,
            overridden_rule_ids: overridden,
            priority_overrode_specificity,
            exclusion_reason: (output.sides == Sides::Unseeded).then(|| "explicit_unseeded".into()),
            policy: (output.sides != Sides::Unseeded).then_some(output),
        })
    }

    fn match_rule(
        &self,
        rule: &Rule,
        type_id: u32,
        group_id: Option<u32>,
        category_id: Option<u32>,
        fact_value: &impl Fn(&str) -> Result<bool>,
    ) -> Result<Option<RuleMatch>> {
        if !selector_matches(&rule.selector, type_id, group_id, category_id, fact_value)? {
            return Ok(None);
        }
        Ok(Some(RuleMatch {
            rule_id: rule.id.clone(),
            priority: rule.priority,
            specificity: rule.selector.specificity(),
            profile_id: rule.profile.clone(),
            output: self.expand(rule)?,
            disposition: MatchDisposition::Overridden,
        }))
    }

    fn expand(&self, rule: &Rule) -> Result<ResolvedItemPolicy> {
        let profile = rule.profile.as_deref().and_then(|id| self.profile(id));
        let sides = rule
            .sides
            .or_else(|| profile.map(|p| p.sides))
            .ok_or_else(|| anyhow::anyhow!("rule {} has no sides", rule.id))?;
        if sides == Sides::Unseeded {
            return Ok(ResolvedItemPolicy {
                profile_id: rule.profile.clone(),
                sides,
                sell: None,
                buy: None,
            });
        }
        let sell = if sides.has_sell() {
            let side = rule
                .sell
                .as_ref()
                .or_else(|| profile.and_then(|p| p.sell.as_ref()))
                .ok_or_else(|| anyhow::anyhow!("rule {} has no Sell policy", rule.id))?;
            Some(resolve_side(side)?)
        } else {
            None
        };
        let buy = if sides.has_buy() {
            let side = rule
                .buy
                .as_ref()
                .or_else(|| profile.and_then(|p| p.buy.as_ref()))
                .ok_or_else(|| anyhow::anyhow!("rule {} has no Buy policy", rule.id))?;
            Some(resolve_side(side)?)
        } else {
            None
        };
        Ok(ResolvedItemPolicy {
            profile_id: rule.profile.clone(),
            sides,
            sell,
            buy,
        })
    }

    /// Fail if any rule or referenced exact ID is unreachable in the pinned catalog.
    /// Returns per-rule counts for the preview and migration parity report.
    pub fn validate_reachability(&self, catalog: &[CatalogItem]) -> Result<Vec<RuleCoverage>> {
        self.validate()?;
        let mut ids = BTreeSet::new();
        let mut counts: BTreeMap<&str, (usize, usize)> =
            self.rules.iter().map(|r| (r.id.as_str(), (0, 0))).collect();
        for item in catalog {
            ensure!(
                ids.insert(item.type_id),
                "duplicate catalog type {}",
                item.type_id
            );
            let resolution =
                self.resolve(item.type_id, item.group_id, item.category_id, &item.facts)?;
            for matched in &resolution.matched_rules {
                let count = counts
                    .get_mut(matched.rule_id.as_str())
                    .expect("known rule");
                count.0 += 1;
                if resolution.equal_winner_rule_ids.contains(&matched.rule_id) {
                    count.1 += 1;
                }
            }
        }
        for rule in &self.rules {
            if let Some(exacts) = &rule.selector.type_ids {
                let absent: Vec<_> = exacts.iter().filter(|id| !ids.contains(id)).collect();
                ensure!(
                    absent.is_empty(),
                    "rule {} contains unreachable type IDs {:?}",
                    rule.id,
                    absent
                );
            }
            ensure!(
                counts[rule.id.as_str()].0 > 0,
                "rule {} has no catalog match",
                rule.id
            );
        }
        Ok(self
            .rules
            .iter()
            .map(|r| {
                let (match_count, winning_count) = counts[r.id.as_str()];
                RuleCoverage {
                    rule_id: r.id.clone(),
                    match_count,
                    winning_count,
                }
            })
            .collect())
    }
}

fn selector_matches(
    selector: &Selector,
    type_id: u32,
    group_id: Option<u32>,
    category_id: Option<u32>,
    fact_value: &impl Fn(&str) -> Result<bool>,
) -> Result<bool> {
    if let Some(ids) = &selector.type_ids {
        return Ok(ids.contains(&type_id));
    }
    if let Some(ids) = &selector.group_ids {
        return Ok(group_id.is_some_and(|id| ids.contains(&id)));
    }
    if let Some(ids) = &selector.category_ids {
        return Ok(category_id.is_some_and(|id| ids.contains(&id)));
    }
    if let Some(fact) = &selector.fact {
        return fact_value(fact);
    }
    bail!("selector has no predicate")
}

fn resolve_side(side: &SidePolicy) -> Result<ResolvedSide> {
    Ok(ResolvedSide {
        source: side.source,
        multiplier: side.multiplier.value()?,
        multiplier_text: side.multiplier.0.clone(),
        floor: side.floor.as_ref().map(|v| v.value()).transpose()?,
        floor_text: side.floor.as_ref().map(|v| v.0.clone()),
        price: side.price.as_ref().map(|v| v.value()).transpose()?,
        price_text: side.price.as_ref().map(|v| v.0.clone()),
        aggregate: side.aggregate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(rules: &str) -> PolicyDocument {
        let json = format!(
            r#"{{"format_version":1,"catalog_contract":{{"sde_build":1,"fact_registry_version":1}},"profiles":[{{"id":"seeded","sides":"sell_only","sell":{{"source":"captured_market","multiplier":"1.0"}}}},{{"id":"other","sides":"buy_only","buy":{{"source":"rare_reference","multiplier":"0.5"}}}}],"rules":[{rules}]}}"#
        );
        PolicyDocument::parse(&json).unwrap()
    }

    #[test]
    fn equal_rank_rules_differing_only_by_manual_price_are_ambiguous() {
        // Two exact-type rules at the same priority that disagree about the price must not
        // silently pick one: the second would otherwise be dropped from the comparison.
        let ambiguous = document(
            r#"{"id":"a","selector":{"type_ids":[42]},"priority":5,"sides":"sell_only","sell":{"source":"manual_fixed","multiplier":"1","price":"100"}},{"id":"b","selector":{"type_ids":[42]},"priority":5,"sides":"sell_only","sell":{"source":"manual_fixed","multiplier":"1","price":"200"}}"#,
        );
        assert!(
            ambiguous.resolve(42, None, None, &BTreeSet::new()).is_err(),
            "equal-rank rules with different manual prices must be rejected as ambiguous"
        );

        // The same two rules agreeing on the price are fine.
        let agreed = document(
            r#"{"id":"a","selector":{"type_ids":[42]},"priority":5,"sides":"sell_only","sell":{"source":"manual_fixed","multiplier":"1","price":"100"}},{"id":"b","selector":{"type_ids":[42]},"priority":5,"sides":"sell_only","sell":{"source":"manual_fixed","multiplier":"1","price":"100"}}"#,
        );
        let resolution = agreed.resolve(42, None, None, &BTreeSet::new()).unwrap();
        assert_eq!(resolution.policy.unwrap().sell.unwrap().price, Some(100.0));
    }

    #[test]
    fn priority_then_specificity_is_independent_of_array_order() {
        let rules = r#"{"id":"type","selector":{"type_ids":[42]},"priority":5,"profile":"seeded"},{"id":"fact","selector":{"fact":"core_bucket"},"priority":6,"profile":"other"}"#;
        let p = document(rules);
        let resolution = p
            .resolve(42, None, None, &BTreeSet::from(["core_bucket".into()]))
            .unwrap();
        assert_eq!(resolution.winner_rule_id.as_deref(), Some("fact"));
        assert!(resolution.priority_overrode_specificity);
        let mut reversed = p.clone();
        reversed.rules.reverse();
        assert_eq!(
            reversed
                .resolve(42, None, None, &BTreeSet::from(["core_bucket".into()]))
                .unwrap()
                .winner_rule_id,
            resolution.winner_rule_id
        );
    }

    #[test]
    fn equal_rank_conflict_fails_and_equal_output_is_visible() {
        let conflict = document(
            r#"{"id":"a","selector":{"type_ids":[42]},"priority":5,"profile":"seeded"},{"id":"b","selector":{"type_ids":[42]},"priority":5,"profile":"other"}"#,
        );
        assert!(
            conflict
                .resolve(42, None, None, &BTreeSet::new())
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        let equal = document(
            r#"{"id":"b","selector":{"type_ids":[42]},"priority":5,"profile":"seeded"},{"id":"a","selector":{"type_ids":[42]},"priority":5,"profile":"seeded"}"#,
        );
        let trace = equal.resolve(42, None, None, &BTreeSet::new()).unwrap();
        assert_eq!(trace.winner_rule_id.as_deref(), Some("a"));
        assert_eq!(trace.equal_winner_rule_ids, ["a", "b"]);
    }

    #[test]
    fn exact_unseeded_guard_and_unknown_fact() {
        let p = document(
            r#"{"id":"broad","selector":{"group_ids":[536]},"priority":999,"profile":"seeded"},{"id":"exert","selector":{"type_ids":[49720]},"priority":1,"sides":"unseeded"}"#,
        );
        let trace = p.resolve(49720, Some(536), None, &BTreeSet::new()).unwrap();
        assert_eq!(trace.winner_rule_id.as_deref(), Some("exert"));
        assert!(trace.policy.is_none());
        let facts = document(
            r#"{"id":"fact","selector":{"fact":"core_bucket"},"priority":1,"profile":"seeded"}"#,
        );
        assert!(
            facts
                .resolve_with_evidence(1, None, None, &BTreeMap::new())
                .is_err()
        );
    }

    #[test]
    fn exact_group_category_and_independent_side_override() {
        let p = document(
            r#"{"id":"category","selector":{"category_ids":[25]},"priority":1,"profile":"seeded","sides":"buy_sell","buy":{"source":"rare_reference","multiplier":"0.42"}},{"id":"group","selector":{"group_ids":[7]},"priority":2,"profile":"other"},{"id":"exact","selector":{"type_ids":[42]},"priority":2,"profile":"seeded"}"#,
        );
        let category = p.resolve(1, Some(8), Some(25), &BTreeSet::new()).unwrap();
        let policy = category.policy.unwrap();
        assert_eq!(policy.sides, Sides::BuySell);
        assert!(matches!(
            policy.sell.unwrap().source,
            Source::CapturedMarket
        ));
        let buy = policy.buy.unwrap();
        assert!(matches!(buy.source, Source::RareReference));
        assert_eq!(buy.multiplier, 0.42);
        assert_eq!(buy.multiplier_text, "0.42");
        assert_eq!(
            p.resolve(2, Some(7), Some(25), &BTreeSet::new())
                .unwrap()
                .winner_rule_id
                .as_deref(),
            Some("group")
        );
        assert_eq!(
            p.resolve(42, Some(7), Some(25), &BTreeSet::new())
                .unwrap()
                .winner_rule_id
                .as_deref(),
            Some("exact")
        );
        let absent = p.resolve(99, None, None, &BTreeSet::new()).unwrap();
        assert_eq!(absent.exclusion_reason.as_deref(), Some("no_matching_rule"));
        assert!(absent.policy.is_none());
    }

    #[test]
    fn unreachable_exact_id_fails_catalog_validation() {
        let p = document(
            r#"{"id":"exact","selector":{"type_ids":[42,43]},"priority":1,"profile":"seeded"}"#,
        );
        let catalog = [CatalogItem {
            type_id: 42,
            group_id: None,
            category_id: None,
            facts: BTreeSet::new(),
        }];
        assert!(
            p.validate_reachability(&catalog)
                .unwrap_err()
                .to_string()
                .contains("43")
        );
    }
}
