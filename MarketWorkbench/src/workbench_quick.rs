//! Friendly, inspectable scope mapping and a compiler to ordinary Policy v2 rules.
//! No pricing, policy precedence or distribution is resolved here.
use crate::policy::{Decimal, PolicyDocument, Rule, Selector, SidePolicy, Sides, Source};
use crate::policy_preview::sha256_hex;
use crate::policy_resolve::{CatalogItem, ResolvedItemPolicy};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const PREFIX: &str = "wb_quick_";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub format_version: u32,
    pub groups: Vec<Group>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub id: String,
    pub label: String,
    pub description: String,
    pub selectors: Vec<Selector>,
    pub children: Vec<Child>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Child {
    pub id: String,
    pub label: String,
    pub selectors: Vec<Selector>,
    #[serde(default = "default_child_priority")]
    pub priority_offset: i32,
}
fn default_child_priority() -> i32 {
    10
}
#[derive(Debug, Clone)]
pub struct Scope {
    pub id: String,
    pub label: String,
    pub parent: Option<String>,
    pub members: BTreeSet<u32>,
    pub priority_offset: i32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub availability: String,
    pub source: String,
    pub buy_percent: Decimal,
    pub sell_percent: Decimal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buy_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sell_source: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Managed {
    workbench_quick: u32,
    scope: String,
    settings: Settings,
    body_sha256: String,
}

fn empty_selector(ids: Vec<u32>) -> Selector {
    Selector {
        type_ids: Some(ids),
        group_ids: None,
        category_ids: None,
        fact: None,
    }
}
fn plain_rule(id: &str, selector: Selector, priority: i32) -> Rule {
    Rule {
        id: id.into(),
        description: None,
        selector,
        priority,
        profile: None,
        sides: Some(Sides::Unseeded),
        sell: None,
        buy: None,
    }
}
fn body_hash(rule: &Rule) -> String {
    let mut r = rule.clone();
    r.description = None;
    sha256_hex(&serde_json::to_vec(&r).expect("rule serializes"))
}
fn managed(rule: &Rule) -> Option<Managed> {
    if !rule.id.starts_with(PREFIX) {
        return None;
    }
    let meta: Managed = serde_json::from_str(rule.description.as_deref()?).ok()?;
    (meta.workbench_quick == 1 && meta.body_sha256 == body_hash(rule)).then_some(meta)
}
pub fn defaults(preset: &str) -> Settings {
    Settings {
        availability: if preset == "GENERAL_TQ" {
            "buy_sell"
        } else {
            "preset"
        }
        .into(),
        source: if preset == "GENERAL_TQ" {
            "tq"
        } else {
            "preset"
        }
        .into(),
        buy_percent: Decimal("100".into()),
        sell_percent: Decimal("100".into()),
        buy_source: None,
        sell_source: None,
    }
}
fn validate_settings(s: &Settings) -> Result<()> {
    ensure!(
        ["preset", "buy_sell", "buy_only", "sell_only", "unseeded"]
            .contains(&s.availability.as_str()),
        "unsupported market availability"
    );
    ensure!(
        ["preset", "tq", "funded_cost"].contains(&s.source.as_str()),
        "unsupported friendly price source"
    );
    for p in [&s.buy_percent, &s.sell_percent] {
        ensure!(p.value()? > 0.0, "price percentage must be positive");
    }
    for source in [&s.buy_source, &s.sell_source].into_iter().flatten() {
        if !["preset", "tq"].contains(&source.as_str()) {
            serde_json::from_value::<Source>(json!(source))?;
        }
    }
    Ok(())
}

fn normalized_settings(mut s: Settings) -> Result<Settings> {
    validate_settings(&s)?;
    s.buy_percent = percent_multiplier(&s.buy_percent, &Decimal("100".into()))?;
    s.sell_percent = percent_multiplier(&s.sell_percent, &Decimal("100".into()))?;
    Ok(s)
}
pub fn percent_multiplier(base: &Decimal, percent: &Decimal) -> Result<Decimal> {
    let v = base.value()? * (percent.value()? / 100.0);
    ensure!(v.is_finite() && v > 0.0, "invalid percentage result");
    // Decimal policy lexemes remain deterministic and never use exponent notation.
    let text = format!("{v:.12}");
    Ok(Decimal(
        text.trim_end_matches('0').trim_end_matches('.').to_owned(),
    ))
}
pub fn scopes(
    mapping: &Mapping,
    catalog: &[CatalogItem],
    ordinary: &BTreeSet<u32>,
) -> Result<Vec<Scope>> {
    ensure!(
        mapping.format_version == 1,
        "unsupported friendly mapping version"
    );
    let mut result = Vec::new();
    let mut assigned = BTreeSet::new();
    let mut names = BTreeSet::new();
    for group in &mapping.groups {
        ensure!(names.insert(group.id.clone()), "duplicate friendly scope");
        let members = if group.selectors.is_empty() {
            ordinary.difference(&assigned).copied().collect()
        } else {
            match_members(&group.selectors, catalog, ordinary)?
        };
        ensure!(
            members.is_disjoint(&assigned),
            "friendly broad groups overlap: {}",
            group.id
        );
        assigned.extend(&members);
        result.push(Scope {
            id: group.id.clone(),
            label: group.label.clone(),
            parent: None,
            members: members.clone(),
            priority_offset: 0,
        });
        for child in &group.children {
            ensure!(names.insert(child.id.clone()), "duplicate friendly scope");
            ensure!(
                (1..=100).contains(&child.priority_offset),
                "invalid subgroup priority offset"
            );
            let children = match_members(&child.selectors, catalog, ordinary)?
                .intersection(&members)
                .copied()
                .collect();
            result.push(Scope {
                id: child.id.clone(),
                label: child.label.clone(),
                parent: Some(group.id.clone()),
                members: children,
                priority_offset: child.priority_offset,
            });
        }
    }
    ensure!(
        &assigned == ordinary,
        "friendly mapping must cover the entire ordinary catalog"
    );
    Ok(result)
}

fn published_blueprint_scope(scope: &Scope) -> bool {
    scope.parent.as_deref() == Some("blueprints")
        && crate::workbench_blueprints::FAMILIES
            .iter()
            .any(|(id, _)| scope.id == format!("blueprints_{id}"))
}

fn structure_advanced_scope(scope: &Scope) -> bool {
    scope.parent.as_deref() == Some("structures")
        && crate::workbench_structures::definition(&scope.id).is_some()
}
fn explicit_advanced_scope(scope: &Scope) -> bool {
    published_blueprint_scope(scope)
        || structure_advanced_scope(scope)
        || crate::workbench_families::definition(&scope.id).is_some()
}

pub fn attach_economic_families(
    mapping: &mut Mapping,
    scopes: &mut Vec<Scope>,
    families: &crate::workbench_families::Catalog,
) -> Result<()> {
    for definition in crate::workbench_families::DEFINITIONS {
        let ids = &families.scopes[definition.id];
        if ids.is_empty() {
            continue;
        }
        let group = mapping
            .groups
            .iter_mut()
            .find(|g| g.id == definition.parent)
            .ok_or_else(|| anyhow!("friendly group missing"))?;
        group.children.push(Child {
            id: definition.id.into(),
            label: definition.label.into(),
            selectors: vec![empty_selector(ids.iter().copied().collect())],
            priority_offset: definition.rank,
        });
        scopes.push(Scope {
            id: definition.id.into(),
            label: definition.label.into(),
            parent: Some(definition.parent.into()),
            members: ids.clone(),
            priority_offset: definition.rank,
        });
    }
    Ok(())
}

fn family_inheritance(
    scope: &Scope,
    settings: &BTreeMap<String, Settings>,
) -> Option<&'static str> {
    let parent = crate::workbench_families::definition(&scope.id).and_then(|d| d.inherits)?;
    // Old saved Reactions still controls its 99 original types, never Molecular-Forged.
    if parent == "industry_reactions_all"
        && !settings.contains_key(parent)
        && settings.contains_key("industry_reactions")
        && scope.id != "industry_reaction_molecular"
    {
        return Some("industry_reactions");
    }
    Some(parent)
}
fn inherited_settings(
    scope: &Scope,
    settings: &BTreeMap<String, Settings>,
    preset: &str,
) -> Settings {
    family_inheritance(scope, settings)
        .and_then(|p| settings.get(p))
        .or_else(|| scope.parent.as_ref().and_then(|p| settings.get(p)))
        .cloned()
        .unwrap_or_else(|| defaults(preset))
}

/// Reuse the existing published catalog; broad Simple membership is left intact.
pub fn attach_blueprint_families(
    mapping: &mut Mapping,
    scopes: &mut Vec<Scope>,
    blueprints: &crate::workbench_blueprints::Catalog,
) -> Result<()> {
    let group = mapping
        .groups
        .iter_mut()
        .find(|g| g.id == "blueprints")
        .ok_or_else(|| anyhow!("Blueprint friendly group missing"))?;
    scopes.retain(|s| s.parent.as_deref() != Some("blueprints") || s.id == "blueprints_reusable");
    let position = scopes
        .iter()
        .position(|s| s.id == "blueprints")
        .ok_or_else(|| anyhow!("Blueprint Simple scope missing"))?
        + 1;
    group.children.clear();
    let mut families = Vec::new();
    for (family, label) in crate::workbench_blueprints::FAMILIES {
        let id = format!("blueprints_{family}");
        let label = label
            .strip_suffix(" Blueprints")
            .unwrap_or(label)
            .to_owned();
        let ids = blueprints.ids(family)?;
        group.children.push(Child {
            id: id.clone(),
            label: label.clone(),
            selectors: vec![empty_selector(ids.clone())],
            priority_offset: 10,
        });
        families.push(Scope {
            id,
            label,
            parent: Some("blueprints".into()),
            members: ids.into_iter().collect(),
            priority_offset: 10,
        });
    }
    scopes.splice(position..position, families);
    Ok(())
}
pub fn attach_structure_scopes(
    mapping: &mut Mapping,
    scopes: &mut Vec<Scope>,
    structures: &crate::workbench_structures::Catalog,
) -> Result<()> {
    let group = mapping
        .groups
        .iter_mut()
        .find(|g| g.id == "structures")
        .ok_or_else(|| anyhow!("Structure friendly group missing"))?;
    // Keep former scopes for existing saved policies; only explicit new edits opt in
    // published non-market types. Compatibility controls are hidden unless customized.
    scopes.retain(|s| {
        s.parent.as_deref() != Some("structures")
            || crate::workbench_structures::COMPATIBILITY_IDS.contains(&s.id.as_str())
    });
    group.children.clear();
    let position = scopes
        .iter()
        .position(|s| s.id == "structures")
        .ok_or_else(|| anyhow!("Structure Simple scope missing"))?
        + 1;
    let mut attached = Vec::new();
    for definition in crate::workbench_structures::DEFINITIONS {
        let ids = &structures.scopes[definition.id];
        if ids.is_empty() {
            continue;
        }
        group.children.push(Child {
            id: definition.id.into(),
            label: definition.label.into(),
            selectors: vec![empty_selector(ids.iter().copied().collect())],
            priority_offset: definition.rank,
        });
        attached.push(Scope {
            id: definition.id.into(),
            label: definition.label.into(),
            parent: Some("structures".into()),
            members: ids.clone(),
            priority_offset: definition.rank,
        });
    }
    scopes.splice(position..position, attached);
    Ok(())
}
fn match_members(
    selectors: &[Selector],
    catalog: &[CatalogItem],
    ordinary: &BTreeSet<u32>,
) -> Result<BTreeSet<u32>> {
    // Discovery uses the authoritative selector matcher through the existing resolver.
    let doc = PolicyDocument {
        format_version: 1,
        catalog_contract: crate::policy::CatalogContract {
            sde_build: 1,
            fact_registry_version: 1,
        },
        profiles: vec![],
        rules: selectors
            .iter()
            .enumerate()
            .map(|(i, s)| plain_rule(&format!("mapping_{i}"), s.clone(), 0))
            .collect(),
    };
    doc.validate()?;
    let mut out = BTreeSet::new();
    for item in catalog.iter().filter(|i| ordinary.contains(&i.type_id)) {
        if doc
            .resolve_validated(item.type_id, item.group_id, item.category_id, &item.facts)?
            .winner_rule_id
            .is_some()
        {
            out.insert(item.type_id);
        }
    }
    Ok(out)
}
fn same_rule(rule: &Rule, baseline: &PolicyDocument, current: &PolicyDocument) -> bool {
    baseline
        .rules
        .iter()
        .any(|b| serde_json::to_value(b).ok() == serde_json::to_value(rule).ok())
        && rule.profile.as_ref().is_none_or(|id| {
            baseline
                .profiles
                .iter()
                .find(|p| &p.id == id)
                .and_then(|p| serde_json::to_value(p).ok())
                == current
                    .profiles
                    .iter()
                    .find(|p| &p.id == id)
                    .and_then(|p| serde_json::to_value(p).ok())
        })
}
fn expert_members(
    policy: &PolicyDocument,
    baseline: &PolicyDocument,
    catalog: &[CatalogItem],
) -> Result<BTreeSet<u32>> {
    let selectors = policy
        .rules
        .iter()
        .filter(|r| managed(r).is_none() && !same_rule(r, baseline, policy))
        .map(|r| r.selector.clone())
        .collect::<Vec<_>>();
    if selectors.is_empty() {
        return Ok(BTreeSet::new());
    }
    match_members(
        &selectors,
        catalog,
        &catalog.iter().map(|i| i.type_id).collect(),
    )
}
fn states(policy: &PolicyDocument) -> Result<BTreeMap<String, Settings>> {
    let mut out = BTreeMap::new();
    for rule in &policy.rules {
        if let Some(meta) = managed(rule) {
            validate_settings(&meta.settings)?;
            if let Some(prev) = out.insert(meta.scope, meta.settings.clone()) {
                ensure!(
                    prev == meta.settings,
                    "inconsistent managed Quick Setup bundle"
                );
            }
        }
    }
    Ok(out)
}
/// A transfer must not silently turn a future/unknown Quick Setup setting into an Expert rule.
pub fn validate_portable_settings(policy: &PolicyDocument, scopes: &[Scope]) -> Result<()> {
    for rule in &policy.rules {
        if rule.id.starts_with(PREFIX) {
            if let Some(description) = &rule.description {
                if let Ok(value) = serde_json::from_str::<Value>(description) {
                    if value.get("workbench_quick").is_some() {
                        let meta: Managed = serde_json::from_value(value)?;
                        ensure!(
                            meta.workbench_quick == 1,
                            "Unsupported Quick Setup metadata version for rule {}",
                            rule.id
                        );
                        // A body edited in Expert mode is intentionally no longer managed.
                        // Preserve that rule verbatim, just as ordinary inspect/apply already do.
                        ensure!(
                            scopes.iter().any(|s| s.id == meta.scope),
                            "Quick Setup scope {} is unavailable in this installation",
                            meta.scope
                        );
                        validate_settings(&meta.settings)?;
                    }
                }
            }
        }
    }
    states(policy)?;
    Ok(())
}

fn scope_states(policy: &PolicyDocument, scopes: &[Scope]) -> Result<BTreeMap<String, Settings>> {
    let mut settings = states(policy)?;
    // Older user policies applied this scope to implants AND boosters. Preserve both
    // settings when the former combined group is split; new implant-only edits do not migrate.
    for (former, split) in [
        ("skills_implants", "skills_boosters"),
        ("equipment_faction", "equipment_storyline"),
    ] {
        if let Some(split_scope) = scopes.iter().find(|s| s.id == split) {
            if !settings.contains_key(&split_scope.id)
                && policy.rules.iter().any(|r| {
                    managed(r).is_some_and(|m| m.scope == former)
                        && r.selector.type_ids.as_ref().is_some_and(|ids| {
                            ids.iter().any(|id| split_scope.members.contains(id))
                        })
                })
            {
                if let Some(old) = settings.get(former).cloned() {
                    settings.insert(split_scope.id.clone(), old);
                }
            }
        }
    }
    Ok(settings)
}
pub fn inspect(
    policy: &PolicyDocument,
    baseline: &PolicyDocument,
    catalog: &[CatalogItem],
    scopes: &[Scope],
    preset: &str,
) -> Result<Value> {
    let settings = scope_states(policy, scopes)?;
    let experts = expert_members(policy, baseline, catalog)?;
    let expert_ids: BTreeSet<_> = policy
        .rules
        .iter()
        .filter(|r| managed(r).is_none() && !same_rule(r, baseline, policy))
        .map(|r| r.id.as_str())
        .collect();
    let values=scopes.iter().map(|scope| {
        let inherited=inherited_settings(scope,&settings,preset);
        json!({"id":scope.id,"label":scope.label,"parent":scope.parent,"count":scope.members.len(),"priority_offset":scope.priority_offset,
            "published_blueprint_family":published_blueprint_scope(scope),
            "explicit_advanced":explicit_advanced_scope(scope),
            "structure_kind":crate::workbench_structures::definition(&scope.id).map(|d|d.kind),
            "family_section":crate::workbench_families::definition(&scope.id).map(|d|d.section),
            "family_opt_in":crate::workbench_families::definition(&scope.id).is_some_and(|d|d.opt_in),
            "inherits_from":family_inheritance(scope,&settings),
            "warning":if published_blueprint_scope(scope){Some(crate::workbench_blueprints::SEMANTICS_WARNING)}else if structure_advanced_scope(scope){Some(crate::workbench_structures::WARNING)}else if crate::workbench_families::definition(&scope.id).is_some_and(|d|d.opt_in){Some(crate::workbench_families::WARNING)}else if scope.id=="other_pilot_services"{Some("Pilot services have special account/consumption semantics. A synthetic market quote does not implement those actions.")}else{None},
            "hidden":((scope.id=="blueprints_reusable"||scope.id=="industry_reactions")||crate::workbench_structures::COMPATIBILITY_IDS.contains(&scope.id.as_str()))&&!settings.contains_key(&scope.id),
            "higher_priority_overlaps":scopes.iter().filter(|s|s.parent==scope.parent&&s.id!=scope.id&&s.priority_offset>scope.priority_offset&&!s.members.is_disjoint(&scope.members)).map(|s|s.label.clone()).collect::<Vec<_>>(),"custom":settings.contains_key(&scope.id),
            "settings":settings.get(&scope.id).unwrap_or(&inherited),"expert_count":scope.members.intersection(&experts).count(),
            "contains_advanced_overrides":preset=="LEGACY_V1"||!scope.members.is_disjoint(&experts),
            "custom_children":scopes.iter().filter(|s|s.parent.as_ref()==Some(&scope.id)&&settings.contains_key(&s.id)).count()})
    }).collect::<Vec<_>>();
    Ok(json!({"format_version":1,"scopes":values,"changes":settings,"expert_rule_ids":expert_ids}))
}
pub fn apply(
    policy: &PolicyDocument,
    baseline: &PolicyDocument,
    tq_baseline: &PolicyDocument,
    catalog: &[CatalogItem],
    scopes: &[Scope],
    preset: &str,
    scope_id: Option<&str>,
    setting: Option<Settings>,
) -> Result<PolicyDocument> {
    policy.validate()?;
    let mut settings = scope_states(policy, scopes)?;
    if let Some(id) = scope_id {
        let scope = scopes
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| anyhow!("unknown friendly scope"))?;
        if let Some(s) = setting {
            let s = normalized_settings(s)?;
            ensure!(
                explicit_advanced_scope(scope)
                    || (s.buy_source.is_none() && s.sell_source.is_none()),
                "Independent side sources in Quick Setup require a published Advanced subgroup"
            );
            let inherited = inherited_settings(scope, &settings, preset);
            if s == inherited && !explicit_advanced_scope(scope) {
                settings.remove(id);
            } else {
                settings.insert(id.into(), s);
            }
        } else {
            settings.remove(id);
        }
    }
    // Remove redundant children when their settings become equal to their parent.
    for scope in scopes.iter().filter(|s| s.parent.is_some()) {
        if explicit_advanced_scope(scope) {
            continue;
        }
        let inherited = settings
            .get(scope.parent.as_ref().unwrap())
            .cloned()
            .unwrap_or_else(|| defaults(preset));
        if settings.get(&scope.id) == Some(&inherited) {
            settings.remove(&scope.id);
        }
    }
    let mut out = policy.clone();
    out.rules.retain(|r| managed(r).is_none());
    ensure!(
        settings.keys().all(|id| scopes.iter().any(|s| &s.id == id)),
        "saved Quick Setup scope is no longer available"
    );
    if settings.is_empty() {
        out.validate()?;
        return Ok(out);
    }
    let reserved = expert_members(&out, baseline, catalog)?;
    let needed: BTreeSet<u32> = scopes
        .iter()
        .filter(|s| settings.contains_key(&s.id))
        .flat_map(|s| s.members.iter().copied())
        .collect();
    let mut base = BTreeMap::new();
    let mut tq = BTreeMap::new();
    for item in catalog
        .iter()
        .filter(|i| needed.contains(&i.type_id) && !reserved.contains(&i.type_id))
    {
        base.insert(
            item.type_id,
            baseline
                .resolve_validated(item.type_id, item.group_id, item.category_id, &item.facts)?
                .policy,
        );
        tq.insert(
            item.type_id,
            tq_baseline
                .resolve_validated(item.type_id, item.group_id, item.category_id, &item.facts)?
                .policy,
        );
    }
    let priority = baseline
        .rules
        .iter()
        .map(|r| r.priority)
        .max()
        .unwrap_or(0)
        .checked_add(10)
        .ok_or_else(|| anyhow!("priority exhausted"))?;
    let mut claimed_children = BTreeMap::<u32, BTreeSet<i32>>::new();
    for scope in scopes {
        let Some(s) = settings.get(&scope.id) else {
            continue;
        };
        let mut bundles = BTreeMap::<String, (Rule, Vec<u32>)>::new();
        for id in scope.members.difference(&reserved) {
            // Keep baseline exclusions. Quick Setup never invents missing evidence or re-enables controls.
            let original = base.get(id).and_then(|p| p.as_ref());
            if original.is_none() && !explicit_advanced_scope(scope) {
                continue;
            }
            if scope.parent.is_some() {
                ensure!(
                    claimed_children
                        .entry(*id)
                        .or_default()
                        .insert(scope.priority_offset),
                    "custom subgroups have equal-priority overlap at type {id}; use an exact Advanced override"
                );
            }
            let sides = match s.availability.as_str() {
                "preset" => original.map_or(Sides::Unseeded, |p| p.sides),
                "buy_sell" => Sides::BuySell,
                "sell_only" => Sides::SellOnly,
                "buy_only" => Sides::BuyOnly,
                _ => Sides::Unseeded,
            };
            let (sell, buy) = if explicit_advanced_scope(scope) {
                let tq = tq.get(id).and_then(|p| p.as_ref());
                (
                    blueprint_side(sides.has_sell(), true, original, tq, s)?,
                    blueprint_side(sides.has_buy(), false, original, tq, s)?,
                )
            } else {
                let original = original.unwrap();
                let selected=match s.source.as_str() {"tq"=>tq.get(id).and_then(|p|p.as_ref()).ok_or_else(||anyhow!("No approved TQ price for type {id}; leave it off the market or inspect Advanced"))?,_=>original};
                (
                    side_policy(sides.has_sell(), true, selected, s)?,
                    side_policy(sides.has_buy(), false, selected, s)?,
                )
            };
            let mut rule = plain_rule(
                "",
                empty_selector(vec![*id]),
                priority
                    .checked_add(scope.priority_offset)
                    .ok_or_else(|| anyhow!("priority exhausted"))?,
            );
            rule.sides = Some(sides);
            rule.sell = sell;
            rule.buy = buy;
            let key = serde_json::to_string(&(rule.sides, &rule.sell, &rule.buy))?;
            bundles
                .entry(key)
                .or_insert_with(|| (rule, Vec::new()))
                .1
                .push(*id);
        }
        for (i, (_, (mut rule, ids))) in bundles.into_iter().enumerate() {
            rule.id = format!("{PREFIX}{}_{i}", scope.id);
            rule.selector = empty_selector(ids);
            ensure!(
                !out.rules.iter().any(|r| r.id == rule.id),
                "Quick Setup rule was edited in Advanced; rename it before regenerating this scope"
            );
            rule.description = Some(serde_json::to_string(&Managed {
                workbench_quick: 1,
                scope: scope.id.clone(),
                settings: s.clone(),
                body_sha256: body_hash(&rule),
            })?);
            out.rules.push(rule);
        }
    }
    out.validate()?;
    // Fail closed through the existing resolver for any generated overlap/conflict.
    for item in catalog {
        out.resolve_validated(item.type_id, item.group_id, item.category_id, &item.facts)?;
    }
    Ok(out)
}

fn blueprint_side(
    enabled: bool,
    sell: bool,
    original: Option<&ResolvedItemPolicy>,
    tq: Option<&ResolvedItemPolicy>,
    s: &Settings,
) -> Result<Option<SidePolicy>> {
    if !enabled {
        return Ok(None);
    }
    let source = if sell { &s.sell_source } else { &s.buy_source };
    let source = source.as_deref().unwrap_or(&s.source);
    let percent = if sell {
        &s.sell_percent
    } else {
        &s.buy_percent
    };
    let base = match source {
        "preset" => original,
        "tq" => tq,
        _ => None,
    };
    if let Some(base) = base {
        let mut inherited = s.clone();
        inherited.source = source.into();
        return side_policy(true, sell, base, &inherited);
    }
    ensure!(
        source != "preset",
        "Published Advanced type has no preset pricing for this side. Choose a price source explicitly."
    );
    // A source declaration can be configured without evidence. Native preview reports
    // the missing price; this does not create a price or change any source contract.
    let source = if source == "tq" {
        Source::TqSnapshot
    } else {
        serde_json::from_value(json!(source))?
    };
    Ok(Some(SidePolicy {
        source,
        multiplier: percent_multiplier(&Decimal("1".into()), percent)?,
        floor: None,
    }))
}
fn side_policy(
    enabled: bool,
    sell: bool,
    base: &ResolvedItemPolicy,
    s: &Settings,
) -> Result<Option<SidePolicy>> {
    if !enabled {
        return Ok(None);
    }
    let percent = if sell {
        &s.sell_percent
    } else {
        &s.buy_percent
    };
    if s.source == "funded_cost" {
        return Ok(Some(SidePolicy {
            source: Source::FundedCost,
            multiplier: percent_multiplier(&Decimal("1".into()), percent)?,
            floor: None,
        }));
    }
    let side = if sell { &base.sell } else { &base.buy };
    let side=side.as_ref().ok_or_else(||anyhow!("Preset has no {} pricing source. Choose TQ Market Prices or Production Cost explicitly.",if sell{"Sell"}else{"Buy"}))?;
    Ok(Some(SidePolicy {
        source: side.source,
        multiplier: percent_multiplier(&Decimal(side.multiplier_text.clone()), percent)?,
        floor: side.floor_text.clone().map(Decimal),
    }))
}

pub fn exact_rule(
    id: u32,
    priority: i32,
    base: &ResolvedItemPolicy,
    settings: &Settings,
) -> Result<Rule> {
    validate_settings(settings)?;
    let sides = match settings.availability.as_str() {
        "preset" => base.sides,
        "buy_sell" => Sides::BuySell,
        "buy_only" => Sides::BuyOnly,
        "sell_only" => Sides::SellOnly,
        _ => Sides::Unseeded,
    };
    let mut rule = plain_rule(
        &format!("override_type_{id}"),
        empty_selector(vec![id]),
        priority,
    );
    rule.description = Some(format!(
        "Item settings: {}",
        serde_json::to_string(settings)?
    ));
    rule.sides = Some(sides);
    rule.sell = side_policy(sides.has_sell(), true, base, settings)?;
    rule.buy = side_policy(sides.has_buy(), false, base, settings)?;
    Ok(rule)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::FACTS;
    fn fixture() -> (PolicyDocument, Vec<CatalogItem>, Vec<Scope>) {
        let p=PolicyDocument::parse(r#"{"format_version":1,"catalog_contract":{"sde_build":1,"fact_registry_version":1},"profiles":[],"rules":[{"id":"base","selector":{"category_ids":[4]},"priority":10,"sides":"buy_sell","sell":{"source":"tq_snapshot","multiplier":"1"},"buy":{"source":"tq_snapshot","multiplier":"1"}}]}"#).unwrap();
        let c = vec![
            CatalogItem {
                type_id: 34,
                category_id: Some(4),
                group_id: Some(18),
                facts: BTreeSet::new(),
            },
            CatalogItem {
                type_id: 35,
                category_id: Some(4),
                group_id: Some(19),
                facts: BTreeSet::new(),
            },
        ];
        let m = Mapping {
            format_version: 1,
            groups: vec![Group {
                id: "resources".into(),
                label: "Resources".into(),
                description: "".into(),
                selectors: vec![Selector {
                    type_ids: None,
                    group_ids: None,
                    category_ids: Some(vec![4]),
                    fact: None,
                }],
                children: vec![Child {
                    id: "minerals".into(),
                    label: "Minerals".into(),
                    priority_offset: 10,
                    selectors: vec![Selector {
                        type_ids: None,
                        group_ids: Some(vec![18]),
                        category_ids: None,
                        fact: None,
                    }],
                }],
            }],
        };
        let s = scopes(&m, &c, &BTreeSet::from([34, 35])).unwrap();
        (p, c, s)
    }
    fn edited(
        p: &PolicyDocument,
        c: &[CatalogItem],
        s: &[Scope],
        scope: &str,
        buy: &str,
    ) -> PolicyDocument {
        let mut set = defaults("GENERAL_TQ");
        set.buy_percent = Decimal(buy.into());
        apply(
            p,
            &fixture().0,
            &fixture().0,
            c,
            s,
            "GENERAL_TQ",
            Some(scope),
            Some(set),
        )
        .unwrap()
    }
    #[test]
    fn portable_quick_settings_preserve_expert_edits_and_reject_future_scopes() {
        let (base, catalog, scopes) = fixture();
        let mut p = edited(&base, &catalog, &scopes, "minerals", "95");
        validate_portable_settings(&p, &scopes).unwrap();
        let generated = p
            .rules
            .iter_mut()
            .find(|r| r.id.starts_with(PREFIX))
            .unwrap();
        generated.priority += 1; // Expert mode: stale ownership hash is deliberately preserved.
        let before = p.canonical_json().unwrap();
        validate_portable_settings(&p, &scopes).unwrap();
        assert_eq!(before, p.canonical_json().unwrap());
        let generated = p
            .rules
            .iter_mut()
            .find(|r| r.id.starts_with(PREFIX))
            .unwrap();
        let mut metadata: Value =
            serde_json::from_str(generated.description.as_ref().unwrap()).unwrap();
        metadata["workbench_quick"] = json!(2);
        generated.description = Some(metadata.to_string());
        assert!(validate_portable_settings(&p, &scopes).is_err());
        let generated = p
            .rules
            .iter_mut()
            .find(|r| r.id.starts_with(PREFIX))
            .unwrap();
        metadata["workbench_quick"] = json!(1);
        metadata["scope"] = json!("missing_installation_scope");
        generated.description = Some(metadata.to_string());
        assert!(validate_portable_settings(&p, &scopes).is_err());
    }
    #[test]
    fn deterministic_mapping_and_supported_selectors() {
        let (p, c, s) = fixture();
        let m: Mapping =
            serde_json::from_str(include_str!("../config/workbench-friendly-groups-v1.json"))
                .unwrap();
        assert!(
            m.groups
                .iter()
                .flat_map(|g| g
                    .selectors
                    .iter()
                    .chain(g.children.iter().flat_map(|c| c.selectors.iter())))
                .all(|s| s.fact.as_ref().is_none_or(|f| FACTS.contains(&f.as_str())))
        );
        assert_eq!(format!("{:?}", s), format!("{:?}", fixture().2));
        assert!(scopes(&m, &c, &BTreeSet::from([34, 35])).is_ok());
        assert!(p.validate().is_ok());
    }
    #[test]
    fn unchanged_load_and_inherited_child_generate_nothing() {
        let (p, c, s) = fixture();
        let out = apply(
            &p,
            &p,
            &p,
            &c,
            &s,
            "GENERAL_TQ",
            Some("minerals"),
            Some(defaults("GENERAL_TQ")),
        )
        .unwrap();
        assert_eq!(p.canonical_json().unwrap(), out.canonical_json().unwrap());
    }
    #[test]
    fn valid_deterministic_base_and_child_bundle() {
        let (p, c, s) = fixture();
        let base = edited(&p, &c, &s, "resources", "90");
        let child = edited(&base, &c, &s, "minerals", "95");
        assert_eq!(states(&child).unwrap().len(), 2);
        assert_eq!(
            child
                .rules
                .iter()
                .filter(|r| managed(r).is_some_and(|m| m.scope == "minerals"))
                .count(),
            1
        );
        assert_eq!(
            edited(&base, &c, &s, "minerals", "95")
                .canonical_json()
                .unwrap(),
            child.canonical_json().unwrap()
        );
        assert_eq!(
            child
                .resolve(34, Some(18), Some(4), &BTreeSet::new())
                .unwrap()
                .policy
                .unwrap()
                .buy
                .unwrap()
                .multiplier,
            0.95
        );
    }
    #[test]
    fn reset_only_owned_subgroup_preserves_expert_and_exact() {
        let (p, c, s) = fixture();
        let mut child = edited(&p, &c, &s, "minerals", "95");
        let mut expert = plain_rule("expert", empty_selector(vec![34]), 999);
        expert.sides = Some(Sides::Unseeded);
        child.rules.push(expert.clone());
        let out = apply(&child, &p, &p, &c, &s, "GENERAL_TQ", Some("minerals"), None).unwrap();
        assert!(states(&out).unwrap().is_empty());
        assert_eq!(
            serde_json::to_value(out.rules.last().unwrap()).unwrap(),
            serde_json::to_value(expert).unwrap()
        );
    }
    #[test]
    fn expert_and_exact_winners_survive_broad_and_child_edits() {
        let (p, c, s) = fixture();
        let mut draft = p.clone();
        let mut expert = p.rules[0].clone();
        expert.id = "expert".into();
        expert.selector = empty_selector(vec![34]);
        expert.priority = 999;
        expert.buy.as_mut().unwrap().multiplier = Decimal("0.75".into());
        draft.rules.push(expert);
        let broad = edited(&draft, &c, &s, "resources", "90");
        let child = edited(&broad, &c, &s, "minerals", "95");
        assert_eq!(
            child
                .resolve(34, Some(18), Some(4), &BTreeSet::new())
                .unwrap()
                .winner_rule_id
                .as_deref(),
            Some("expert")
        );
        assert_eq!(
            inspect(&child, &p, &c, &s, "GENERAL_TQ").unwrap()["scopes"][0]["expert_count"],
            1
        );
    }
    #[test]
    fn percent_conversion() {
        for (p, want) in [("100", 1.), ("125", 1.25), ("80", 0.8)] {
            assert_eq!(
                percent_multiplier(&Decimal("1".into()), &Decimal(p.into()))
                    .unwrap()
                    .value()
                    .unwrap(),
                want
            );
        }
        assert!(percent_multiplier(&Decimal("1".into()), &Decimal("NaN".into())).is_err());
    }
    #[test]
    fn unchanged_decimal_percent_does_not_create_a_child_override() {
        let (p, c, s) = fixture();
        let mut setting = defaults("GENERAL_TQ");
        setting.buy_percent = Decimal("100.00".into());
        setting.sell_percent = Decimal("100.0".into());
        let out = apply(
            &p,
            &p,
            &p,
            &c,
            &s,
            "GENERAL_TQ",
            Some("minerals"),
            Some(setting),
        )
        .unwrap();
        assert!(states(&out).unwrap().is_empty());
        assert_eq!(p.canonical_json().unwrap(), out.canonical_json().unwrap());
    }
    #[test]
    fn source_fallback_and_baseline_exclusion_survive_quick_edits() {
        let (mut p, c, s) = fixture();
        p.rules[0].buy.as_mut().unwrap().source = Source::TqSnapshotSellFallback;
        p.rules[0].buy.as_mut().unwrap().multiplier = Decimal("0.80".into());
        p.rules
            .push(plain_rule("no_reference", empty_selector(vec![35]), 100));
        let mut setting = defaults("GENERAL_TQ");
        setting.sell_percent = Decimal("125".into());
        let out = apply(
            &p,
            &p,
            &p,
            &c,
            &s,
            "GENERAL_TQ",
            Some("resources"),
            Some(setting),
        )
        .unwrap();
        let r = out
            .resolve(34, Some(18), Some(4), &BTreeSet::new())
            .unwrap()
            .policy
            .unwrap();
        assert_eq!(
            r.buy.as_ref().unwrap().source,
            Source::TqSnapshotSellFallback
        );
        assert_eq!(r.buy.unwrap().multiplier, 0.8);
        assert_eq!(r.sell.unwrap().multiplier, 1.25);
        assert!(
            out.resolve(35, Some(19), Some(4), &BTreeSet::new())
                .unwrap()
                .policy
                .is_none()
        );
    }
    #[test]
    fn exact_item_uses_normal_rules_and_existing_guard() {
        let (p, c, s) = fixture();
        let selected = p
            .resolve(34, Some(18), Some(4), &BTreeSet::new())
            .unwrap()
            .policy
            .unwrap();
        let mut settings = defaults("GENERAL_TQ");
        settings.availability = "unseeded".into();
        let mut draft = edited(&p, &c, &s, "resources", "90");
        draft
            .rules
            .push(exact_rule(34, 1000, &selected, &settings).unwrap());
        let out = apply(&draft, &p, &p, &c, &s, "GENERAL_TQ", None, None).unwrap();
        assert_eq!(
            out.resolve(34, Some(18), Some(4), &BTreeSet::new())
                .unwrap()
                .winner_rule_id
                .as_deref(),
            Some("override_type_34")
        );
        assert!(
            out.resolve(34, Some(18), Some(4), &BTreeSet::new())
                .unwrap()
                .policy
                .is_none()
        );
    }
    #[test]
    fn all_availability_mappings() {
        let (p, c, s) = fixture();
        for (a, sell, buy) in [
            ("buy_sell", true, true),
            ("sell_only", true, false),
            ("buy_only", false, true),
            ("unseeded", false, false),
        ] {
            let mut set = defaults("GENERAL_TQ");
            set.availability = a.into();
            set.sell_percent = Decimal("110".into());
            let out = apply(
                &p,
                &p,
                &p,
                &c,
                &s,
                "GENERAL_TQ",
                Some("resources"),
                Some(set),
            )
            .unwrap();
            let r = out
                .resolve(34, Some(18), Some(4), &BTreeSet::new())
                .unwrap();
            assert_eq!(r.policy.as_ref().is_some_and(|p| p.sell.is_some()), sell);
            assert_eq!(r.policy.as_ref().is_some_and(|p| p.buy.is_some()), buy);
        }
    }
    #[test]
    fn edited_generated_rule_becomes_expert_not_deleted() {
        let (p, c, s) = fixture();
        let mut d = edited(&p, &c, &s, "resources", "90");
        d.rules.last_mut().unwrap().buy.as_mut().unwrap().multiplier = Decimal("0.7".into());
        let out = apply(&d, &p, &p, &c, &s, "GENERAL_TQ", Some("resources"), None).unwrap();
        assert_eq!(out.rules.len(), d.rules.len());
        assert_eq!(
            out.resolve(34, Some(18), Some(4), &BTreeSet::new())
                .unwrap()
                .policy
                .unwrap()
                .buy
                .unwrap()
                .multiplier,
            0.7
        );
    }

    #[test]
    fn overlapping_facets_use_explicit_priority_and_are_edit_order_independent() {
        let (p, c, mut s) = fixture();
        s[1].priority_offset = 5;
        let mut facet = s[1].clone();
        facet.id = "rigs".into();
        facet.label = "Rigs".into();
        facet.priority_offset = 10;
        s.push(facet);
        let a = edited(&edited(&p, &c, &s, "minerals", "90"), &c, &s, "rigs", "95");
        let b = edited(&edited(&p, &c, &s, "rigs", "95"), &c, &s, "minerals", "90");
        assert_eq!(a.canonical_json().unwrap(), b.canonical_json().unwrap());
        let resolved = a.resolve(34, Some(18), Some(4), &BTreeSet::new()).unwrap();
        assert!(
            resolved
                .winner_rule_id
                .unwrap()
                .starts_with("wb_quick_rigs_")
        );
        assert_eq!(resolved.policy.unwrap().buy.unwrap().multiplier, 0.95);
        assert_eq!(
            inspect(&a, &p, &c, &s, "GENERAL_TQ").unwrap()["scopes"][1]["higher_priority_overlaps"],
            json!(["Rigs"])
        );
    }

    #[test]
    fn overlapping_subgroups_with_equal_priority_fail_closed() {
        let (p, c, mut s) = fixture();
        let mut second = s[1].clone();
        second.id = "other_facet".into();
        s.push(second);
        let first = edited(&p, &c, &s, "minerals", "90");
        let mut setting = defaults("GENERAL_TQ");
        setting.buy_percent = Decimal("95".into());
        assert!(
            apply(
                &first,
                &p,
                &p,
                &c,
                &s,
                "GENERAL_TQ",
                Some("other_facet"),
                Some(setting)
            )
            .is_err()
        );
    }

    #[test]
    fn splitting_older_combined_scopes_preserves_settings_until_explicit_reset() {
        for (former, split) in [
            ("skills_implants", "skills_boosters"),
            ("equipment_faction", "equipment_storyline"),
        ] {
            let (p, c, mut old) = fixture();
            old[1].id = former.into();
            old[1].members.insert(35);
            let previous = edited(&p, &c, &old, former, "90");
            let mut new = old.clone();
            new[1].members.remove(&35);
            let mut separate = new[1].clone();
            separate.id = split.into();
            separate.members = BTreeSet::from([35]);
            new.push(separate);
            let before = previous.canonical_json().unwrap();
            let inspection = inspect(&previous, &p, &c, &new, "GENERAL_TQ").unwrap();
            assert_eq!(inspection["scopes"][2]["custom"], true);
            assert_eq!(
                previous.canonical_json().unwrap(),
                before,
                "inspection must not rewrite saved policy"
            );
            let synced = apply(&previous, &p, &p, &c, &new, "GENERAL_TQ", None, None).unwrap();
            assert_eq!(
                synced
                    .resolve(35, Some(19), Some(4), &BTreeSet::new())
                    .unwrap()
                    .policy
                    .unwrap()
                    .buy
                    .unwrap()
                    .multiplier,
                0.9
            );
            let reset = apply(&synced, &p, &p, &c, &new, "GENERAL_TQ", Some(split), None).unwrap();
            assert_eq!(
                reset
                    .resolve(34, Some(18), Some(4), &BTreeSet::new())
                    .unwrap()
                    .policy
                    .unwrap()
                    .buy
                    .unwrap()
                    .multiplier,
                0.9
            );
            assert_eq!(
                reset
                    .resolve(35, Some(19), Some(4), &BTreeSet::new())
                    .unwrap()
                    .policy
                    .unwrap()
                    .buy
                    .unwrap()
                    .multiplier,
                1.0
            );
        }
    }

    fn blueprint_fixture() -> (PolicyDocument, Vec<CatalogItem>, Vec<Scope>) {
        let (p, mut c, mut s) = fixture();
        c[1].category_id = Some(9); // no baseline rule or TQ preset decision for this special type
        s[0].id = "blueprints".into();
        s[0].members = BTreeSet::from([34]);
        s[1].id = "blueprints_t1".into();
        s[1].parent = Some("blueprints".into());
        s[1].members = BTreeSet::from([34, 35]);
        (p, c, s)
    }

    #[test]
    fn blueprint_special_members_require_explicit_family_apply_even_at_inherited_settings() {
        let (p, c, s) = blueprint_fixture();
        let broad = edited(&p, &c, &s, "blueprints", "90");
        assert!(
            broad
                .resolve(35, Some(19), Some(9), &BTreeSet::new())
                .unwrap()
                .winner_rule_id
                .is_none()
        );
        let selected = apply(
            &p,
            &p,
            &p,
            &c,
            &s,
            "GENERAL_TQ",
            Some("blueprints_t1"),
            Some(defaults("GENERAL_TQ")),
        )
        .unwrap();
        let inspection = inspect(&selected, &p, &c, &s, "GENERAL_TQ").unwrap();
        assert_eq!(inspection["scopes"][1]["custom"], true);
        assert_eq!(
            selected.rules.last().unwrap().selector.type_ids,
            Some(vec![34, 35])
        );
        let special = selected
            .resolve(35, Some(19), Some(9), &BTreeSet::new())
            .unwrap()
            .policy
            .unwrap();
        assert_eq!(special.buy.unwrap().source, Source::TqSnapshot);
        assert_eq!(special.sell.unwrap().source, Source::TqSnapshot);
        let reset = apply(
            &selected,
            &p,
            &p,
            &c,
            &s,
            "GENERAL_TQ",
            Some("blueprints_t1"),
            None,
        )
        .unwrap();
        assert_eq!(reset.canonical_json().unwrap(), p.canonical_json().unwrap());
    }

    #[test]
    fn blueprint_family_has_independent_native_sources_multipliers_and_all_sides() {
        let (p, c, s) = blueprint_fixture();
        for availability in ["buy_sell", "buy_only", "sell_only", "unseeded"] {
            let mut setting = defaults("GENERAL_TQ");
            setting.availability = availability.into();
            setting.buy_source = Some("funded_cost".into());
            setting.sell_source = Some("npc_min_sell".into());
            setting.buy_percent = Decimal("80".into());
            setting.sell_percent = Decimal("125".into());
            let out = apply(
                &p,
                &p,
                &p,
                &c,
                &s,
                "GENERAL_TQ",
                Some("blueprints_t1"),
                Some(setting.clone()),
            )
            .unwrap();
            let r = out.rules.last().unwrap();
            assert_eq!(r.selector.type_ids, Some(vec![34, 35]));
            assert_eq!(
                r.buy.is_some(),
                ["buy_sell", "buy_only"].contains(&availability)
            );
            assert_eq!(
                r.sell.is_some(),
                ["buy_sell", "sell_only"].contains(&availability)
            );
            if let Some(buy) = &r.buy {
                assert_eq!(buy.source, Source::FundedCost);
                assert_eq!(buy.multiplier.value().unwrap(), 0.8);
            }
            if let Some(sell) = &r.sell {
                assert_eq!(sell.source, Source::NpcMinSell);
                assert_eq!(sell.multiplier.value().unwrap(), 1.25);
            }
            assert_eq!(
                out.canonical_json().unwrap(),
                apply(&out, &p, &p, &c, &s, "GENERAL_TQ", None, None)
                    .unwrap()
                    .canonical_json()
                    .unwrap()
            );
        }
        let mut bad = defaults("GENERAL_TQ");
        bad.buy_source = Some("invented_source".into());
        assert!(
            apply(
                &p,
                &p,
                &p,
                &c,
                &s,
                "GENERAL_TQ",
                Some("blueprints_t1"),
                Some(bad)
            )
            .is_err()
        );
    }

    #[test]
    fn quick_blueprint_family_preserves_expert_override_and_cannot_invent_preset_source() {
        let (p, c, s) = blueprint_fixture();
        let mut d = p.clone();
        let expert = plain_rule("expert_exclusion", empty_selector(vec![35]), 999);
        d.rules.push(expert.clone());
        let out = edited(&d, &c, &s, "blueprints_t1", "90");
        assert_eq!(
            serde_json::to_value(&out.rules[1]).unwrap(),
            serde_json::to_value(expert).unwrap()
        );
        assert_eq!(
            out.resolve(35, Some(19), Some(9), &BTreeSet::new())
                .unwrap()
                .winner_rule_id
                .as_deref(),
            Some("expert_exclusion")
        );
        let mut setting = defaults("GENERAL_TQ");
        setting.source = "preset".into();
        assert!(
            apply(
                &p,
                &p,
                &p,
                &c,
                &s,
                "GENERAL_TQ",
                Some("blueprints_t1"),
                Some(setting)
            )
            .is_err()
        );
    }
}
