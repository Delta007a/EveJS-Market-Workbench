//! Runtime-only ore-family reference normalization.
//!
//! The price manifest remains an immutable record of exact captures and the older local
//! pricing rules. This module consumes those captures plus authoritative SDE reprocessing
//! vectors and overlays a family-consistent reference on the canonical seed plan. Existing
//! row-level reprocessing floors and refining Buy ceilings run afterwards, unchanged.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};

use crate::build::SeedRow;
use crate::classify::{MarketRole, PricingProfile};
use crate::manifest::{CapturePolicy, PriceManifest, PriceSource, normalize_price};
use crate::seedplan::SeedPlan;
use crate::staticdata::{ReprocessingStatic, ReprocessingType, StaticData};

const ORE_FAMILY: &str = "ore";
const MOON_ORE_FAMILY: &str = "moon_ore";
// SDE outputs are integral per refine batch, so a nominal scalar grade increase can round
// individual low-quantity materials by one unit in different directions. The least-squares
// scalar below admits up to two percent normalized vector residual (for example Vanadinite's
// 10 -> 12 and 40 -> 46 outputs at one nominal grade) while the exact output material set and
// authoritative group boundary still reject unrelated recipes.
const SCALAR_RELATIVE_TOLERANCE: f64 = 2.0e-2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OreMemberKind {
    Raw,
    ModernCompressed,
    LegacyBatch,
}

impl OreMemberKind {
    fn key(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::ModernCompressed => "modern-compressed",
            Self::LegacyBatch => "legacy-batch",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OreCapturedAnchor {
    pub type_id: u32,
    pub name: String,
    pub captured_reference: f64,
    pub source: PriceSource,
    pub output_ratio: f64,
    pub normalized_capture: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OreNormalizedMember {
    pub type_id: u32,
    pub name: String,
    pub kind: OreMemberKind,
    /// Actual per-unit output scale relative to the lowest comparable raw member.
    pub output_ratio: f64,
    /// Raw grade supplying the grade identity. Equal to `type_id` for raw members.
    pub raw_type_id: u32,
    /// Actual output equivalence to `raw_type_id` (1 for raw; normally 1 for modern
    /// compressed; discovered from SDE quantities for legacy batches).
    pub raw_equivalence: f64,
    pub reconstructed_reference: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OreAnchoredFamily {
    pub group_id: u32,
    pub reprocessing_family: String,
    pub output_material_type_ids: Vec<u32>,
    pub family_anchor: f64,
    pub captures: Vec<OreCapturedAnchor>,
    pub members: Vec<OreNormalizedMember>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OreUnanchoredFamily {
    pub group_id: u32,
    pub reprocessing_family: String,
    pub output_material_type_ids: Vec<u32>,
    pub raw_type_ids: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OreAmbiguousMember {
    pub type_id: u32,
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ReferenceDecision {
    family_index: usize,
    reference: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OreFamilyNormalization {
    pub anchored: Vec<OreAnchoredFamily>,
    pub unanchored: Vec<OreUnanchoredFamily>,
    pub ambiguous: Vec<OreAmbiguousMember>,
    references: BTreeMap<u32, ReferenceDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ComparableFamilyKey {
    group_id: u32,
    reprocessing_family: String,
    output_material_type_ids: Vec<u32>,
}

type OutputVector = BTreeMap<u32, f64>;

impl OreFamilyNormalization {
    pub fn derive(
        data: &StaticData,
        reprocessing: &ReprocessingStatic,
        manifest: &PriceManifest,
        policy: CapturePolicy,
        moon_mining_available: bool,
    ) -> Self {
        let mut captures = BTreeMap::new();
        for entry in &reprocessing.types {
            if let Some(capture) = manifest.market_capture(entry.type_id, policy) {
                captures.insert(entry.type_id, (capture.price, capture.source));
            }
        }
        Self::derive_inner(
            reprocessing,
            &captures,
            moon_mining_available,
            |type_id| {
                data.item_type(type_id)
                    .is_some_and(|item| item.is_marketable())
            },
            |type_id| data.item_type(type_id).and_then(|item| item.group_id),
        )
    }

    fn derive_inner(
        reprocessing: &ReprocessingStatic,
        captures: &BTreeMap<u32, (f64, PriceSource)>,
        moon_mining_available: bool,
        marketable: impl Fn(u32) -> bool,
        group_id: impl Fn(u32) -> Option<u32>,
    ) -> Self {
        let modern_types = reprocessing
            .compressed_by_source
            .values()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut vectors = BTreeMap::new();
        let mut entries = BTreeMap::new();
        let mut comparable: BTreeMap<ComparableFamilyKey, Vec<u32>> = BTreeMap::new();

        for entry in &reprocessing.types {
            if !marketable(entry.type_id)
                || !matches!(entry.family.as_str(), ORE_FAMILY | MOON_ORE_FAMILY)
                || (entry.family == MOON_ORE_FAMILY && !moon_mining_available)
            {
                continue;
            }
            let Some(vector) = per_unit_output(entry) else {
                continue;
            };
            let Some(group_id) = group_id(entry.type_id) else {
                continue;
            };
            let key = ComparableFamilyKey {
                group_id,
                reprocessing_family: entry.family.clone(),
                output_material_type_ids: vector.keys().copied().collect(),
            };
            vectors.insert(entry.type_id, vector);
            entries.insert(entry.type_id, entry);
            comparable.entry(key).or_default().push(entry.type_id);
        }

        let mut result = Self::default();
        for (key, member_ids) in comparable {
            let mut raw_ids = member_ids
                .iter()
                .copied()
                .filter(|type_id| {
                    reprocessing.compressed_by_source.contains_key(type_id)
                        || (!modern_types.contains(type_id)
                            && entries
                                .get(type_id)
                                .is_some_and(|entry| entry.portion_size > 1))
                })
                .collect::<Vec<_>>();
            if raw_ids.is_empty() {
                for type_id in member_ids {
                    let entry = entries[&type_id];
                    result.ambiguous.push(OreAmbiguousMember {
                        type_id,
                        name: entry.name.clone(),
                        reason: "no authoritative/raw per-unit member in comparable output family"
                            .to_string(),
                    });
                }
                continue;
            }

            raw_ids.sort_unstable_by(|left, right| {
                vector_scale(&vectors[left])
                    .total_cmp(&vector_scale(&vectors[right]))
                    .then_with(|| left.cmp(right))
            });
            let base_id = raw_ids[0];
            let base = &vectors[&base_id];
            let mut raw_ratios = BTreeMap::new();
            let mut scalar_raw_ids = Vec::new();
            for type_id in raw_ids {
                if let Some(ratio) = scalar_ratio(&vectors[&type_id], base) {
                    raw_ratios.insert(type_id, ratio);
                    scalar_raw_ids.push(type_id);
                } else {
                    let entry = entries[&type_id];
                    result.ambiguous.push(OreAmbiguousMember {
                        type_id,
                        name: entry.name.clone(),
                        reason: format!(
                            "output vector is not scalar-comparable with family base type {base_id}"
                        ),
                    });
                }
            }

            let mut captured = scalar_raw_ids
                .iter()
                .filter_map(|type_id| {
                    let (price, source) = captures.get(type_id).copied()?;
                    if !(price.is_finite() && price > 0.0 && source.is_market_price()) {
                        return None;
                    }
                    let output_ratio = raw_ratios[type_id];
                    Some(OreCapturedAnchor {
                        type_id: *type_id,
                        name: entries[type_id].name.clone(),
                        captured_reference: price,
                        source,
                        output_ratio,
                        normalized_capture: price / output_ratio,
                    })
                })
                .collect::<Vec<_>>();
            captured.sort_unstable_by_key(|entry| entry.type_id);

            if captured.is_empty() {
                result.unanchored.push(OreUnanchoredFamily {
                    group_id: key.group_id,
                    reprocessing_family: key.reprocessing_family,
                    output_material_type_ids: key.output_material_type_ids,
                    raw_type_ids: scalar_raw_ids,
                });
                continue;
            }

            let anchor = normalize_price(median(
                captured
                    .iter()
                    .map(|entry| entry.normalized_capture)
                    .collect(),
            ));
            let family_index = result.anchored.len();
            let mut family = OreAnchoredFamily {
                group_id: key.group_id,
                reprocessing_family: key.reprocessing_family,
                output_material_type_ids: key.output_material_type_ids,
                family_anchor: anchor,
                captures: captured,
                members: Vec::new(),
            };

            for raw_id in &scalar_raw_ids {
                let ratio = raw_ratios[raw_id];
                add_member(
                    &mut result.references,
                    &mut family,
                    family_index,
                    entries[raw_id],
                    OreMemberKind::Raw,
                    *raw_id,
                    ratio,
                    1.0,
                    anchor * ratio,
                );

                if let Some(compressed_id) = reprocessing.compressed_by_source.get(raw_id)
                    && let (Some(compressed), Some(compressed_vector)) =
                        (entries.get(compressed_id), vectors.get(compressed_id))
                    && let Some(equivalence) = scalar_ratio(compressed_vector, &vectors[raw_id])
                {
                    add_member(
                        &mut result.references,
                        &mut family,
                        family_index,
                        compressed,
                        OreMemberKind::ModernCompressed,
                        *raw_id,
                        ratio * equivalence,
                        equivalence,
                        anchor * ratio * equivalence,
                    );
                }
            }

            let legacy_ids = member_ids
                .iter()
                .copied()
                .filter(|type_id| {
                    !raw_ratios.contains_key(type_id)
                        && !modern_types.contains(type_id)
                        && entries
                            .get(type_id)
                            .is_some_and(|entry| entry.portion_size == 1)
                })
                .collect::<Vec<_>>();
            let (legacy_matches, unmatched) =
                match_legacy_batches(&legacy_ids, &scalar_raw_ids, &vectors);
            for (legacy_id, raw_id, equivalence) in legacy_matches {
                let grade_ratio = raw_ratios[&raw_id];
                add_member(
                    &mut result.references,
                    &mut family,
                    family_index,
                    entries[&legacy_id],
                    OreMemberKind::LegacyBatch,
                    raw_id,
                    grade_ratio * equivalence,
                    equivalence,
                    anchor * grade_ratio * equivalence,
                );
            }
            for type_id in unmatched {
                result.ambiguous.push(OreAmbiguousMember {
                    type_id,
                    name: entries[&type_id].name.clone(),
                    reason: "no unique constant SDE output-equivalence mapping to a raw grade"
                        .to_string(),
                });
            }

            family.members.sort_unstable_by_key(|member| member.type_id);
            result.anchored.push(family);
        }
        result
            .unanchored
            .sort_by_key(|family| (family.group_id, family.output_material_type_ids.clone()));
        result.ambiguous.sort_unstable_by_key(|entry| entry.type_id);
        result
    }

    /// Applies the runtime references to an already classified canonical plan. Role, profile,
    /// side ownership and source manifest entries are not changed.
    pub fn apply_to_plan(&self, plan: &mut SeedPlan) -> usize {
        let mut changed = 0;
        for (type_id, decision) in &self.references {
            let Some(seedable) = plan.seedable.get_mut(type_id) else {
                continue;
            };
            let eligible = if let Some(v2) = plan.v2_sides.get(type_id) {
                v2.policy
                    .sell
                    .as_ref()
                    .or(v2.policy.buy.as_ref())
                    .is_some_and(|side| side.source == crate::policy::Source::CoreManifestCost)
            } else {
                seedable.role == MarketRole::Core
                    && matches!(
                        seedable.profile,
                        PricingProfile::RawOre | PricingProfile::CompressedOre
                    )
            };
            if !eligible {
                continue;
            }
            seedable.price = decision.reference;
            seedable.source = PriceSource::OreFamilyNormalizedReference;
            if let Some(v2) = plan.v2_sides.get_mut(type_id) {
                if let Some((price, source)) = v2.sell.as_mut() {
                    *price = decision.reference;
                    *source = PriceSource::OreFamilyNormalizedReference;
                }
                if let Some((price, source)) = v2.buy.as_mut() {
                    *price = decision.reference;
                    *source = PriceSource::OreFamilyNormalizedReference;
                }
            }
            changed += 1;
        }
        changed
    }

    pub fn represented_group_count(&self) -> usize {
        self.anchored
            .iter()
            .map(|family| family.group_id)
            .collect::<BTreeSet<_>>()
            .len()
    }

    pub fn normalized_type_count(&self) -> usize {
        self.references.len()
    }

    pub fn reference(&self, type_id: u32) -> Option<f64> {
        self.references.get(&type_id).map(|entry| entry.reference)
    }

    pub fn family_for(&self, type_id: u32) -> Option<&OreAnchoredFamily> {
        let index = self.references.get(&type_id)?.family_index;
        self.anchored.get(index)
    }
}

fn add_member(
    references: &mut BTreeMap<u32, ReferenceDecision>,
    family: &mut OreAnchoredFamily,
    family_index: usize,
    entry: &ReprocessingType,
    kind: OreMemberKind,
    raw_type_id: u32,
    output_ratio: f64,
    raw_equivalence: f64,
    reference: f64,
) {
    let reference = normalize_price(reference);
    references.insert(
        entry.type_id,
        ReferenceDecision {
            family_index,
            reference,
        },
    );
    family.members.push(OreNormalizedMember {
        type_id: entry.type_id,
        name: entry.name.clone(),
        kind,
        output_ratio,
        raw_type_id,
        raw_equivalence,
        reconstructed_reference: reference,
    });
}

fn per_unit_output(entry: &ReprocessingType) -> Option<OutputVector> {
    if entry.portion_size <= 0 || entry.materials.is_empty() {
        return None;
    }
    let mut output = OutputVector::new();
    for material in &entry.materials {
        if material.quantity <= 0 {
            continue;
        }
        output.insert(
            material.type_id,
            material.quantity as f64 / entry.portion_size as f64,
        );
    }
    (!output.is_empty()).then_some(output)
}

fn vector_scale(vector: &OutputVector) -> f64 {
    vector.values().sum()
}

fn close_ratio(left: f64, right: f64) -> bool {
    (left - right).abs() <= SCALAR_RELATIVE_TOLERANCE * left.abs().max(right.abs()).max(1.0)
}

/// `candidate = base * ratio` across every output material, or `None` when the vectors are
/// not scalar-comparable. Material names and ore names never participate.
fn scalar_ratio(candidate: &OutputVector, base: &OutputVector) -> Option<f64> {
    if candidate.len() != base.len() || candidate.keys().ne(base.keys()) {
        return None;
    }
    let denominator = base
        .values()
        .map(|quantity| quantity * quantity)
        .sum::<f64>();
    if !(denominator.is_finite() && denominator > 0.0) {
        return None;
    }
    let ratio = candidate
        .iter()
        .map(|(material, quantity)| quantity * base[material])
        .sum::<f64>()
        / denominator;
    let residual = candidate
        .iter()
        .map(|(material, quantity)| {
            let difference = quantity - ratio * base[material];
            difference * difference
        })
        .sum::<f64>()
        .sqrt();
    let magnitude = candidate
        .values()
        .map(|quantity| quantity * quantity)
        .sum::<f64>()
        .sqrt();
    (ratio.is_finite()
        && ratio > 0.0
        && magnitude > 0.0
        && residual / magnitude <= SCALAR_RELATIVE_TOLERANCE)
        .then_some(ratio)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

/// Finds the equivalence factor supported by the largest unique one-to-one set of actual
/// output-vector comparisons. A tie is ambiguous and maps nothing. This discovers the legacy
/// batch scale (rather than assuming 100) and also handles families with an extra raw grade.
fn match_legacy_batches(
    legacy_ids: &[u32],
    raw_ids: &[u32],
    vectors: &BTreeMap<u32, OutputVector>,
) -> (Vec<(u32, u32, f64)>, Vec<u32>) {
    if legacy_ids.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut factors = Vec::new();
    for legacy_id in legacy_ids {
        for raw_id in raw_ids {
            if let Some(factor) = scalar_ratio(&vectors[legacy_id], &vectors[raw_id])
                && !factors
                    .iter()
                    .any(|existing| close_ratio(*existing, factor))
            {
                factors.push(factor);
            }
        }
    }

    let mut candidates = Vec::new();
    for factor in factors {
        let mut used_raw = BTreeSet::new();
        let mut matches = Vec::new();
        for legacy_id in legacy_ids {
            let possible = raw_ids
                .iter()
                .filter_map(|raw_id| {
                    let actual = scalar_ratio(&vectors[legacy_id], &vectors[raw_id])?;
                    close_ratio(actual, factor).then_some((*raw_id, actual))
                })
                .collect::<Vec<_>>();
            if let [(raw_id, actual)] = possible.as_slice()
                && used_raw.insert(*raw_id)
            {
                matches.push((*legacy_id, *raw_id, *actual));
            }
        }
        if !matches.is_empty() {
            candidates.push(matches);
        }
    }
    candidates.sort_by_key(|matches| std::cmp::Reverse(matches.len()));
    let Some(best) = candidates.first().cloned() else {
        return (Vec::new(), legacy_ids.to_vec());
    };
    let tied = candidates
        .get(1)
        .is_some_and(|candidate| candidate.len() == best.len());
    // One isolated batch against multiple proportional raw grades has no independently
    // provable counterpart; requiring at least two corroborating grades avoids guessing.
    if tied || (best.len() == 1 && raw_ids.len() > 1) {
        return (Vec::new(), legacy_ids.to_vec());
    }
    let matched = best
        .iter()
        .map(|(legacy, _, _)| *legacy)
        .collect::<BTreeSet<_>>();
    let unmatched = legacy_ids
        .iter()
        .copied()
        .filter(|type_id| !matched.contains(type_id))
        .collect();
    (best, unmatched)
}

pub fn verify_ore_family_monotonicity(
    normalization: &OreFamilyNormalization,
    rows: &[SeedRow],
) -> Result<usize> {
    let book = rows
        .iter()
        .map(|row| (row.type_id, row))
        .collect::<BTreeMap<_, _>>();
    let mut checked = 0;
    for family in &normalization.anchored {
        for kind in [
            OreMemberKind::Raw,
            OreMemberKind::ModernCompressed,
            OreMemberKind::LegacyBatch,
        ] {
            let members = family
                .members
                .iter()
                .filter(|member| member.kind == kind)
                .collect::<Vec<_>>();
            for lower in &members {
                for higher in &members {
                    if higher.output_ratio <= lower.output_ratio {
                        continue;
                    }
                    let (Some(lower_row), Some(higher_row)) =
                        (book.get(&lower.type_id), book.get(&higher.type_id))
                    else {
                        continue;
                    };
                    if lower_row.writes_ask() && higher_row.writes_ask() {
                        checked += 1;
                        if higher_row.ask < lower_row.ask {
                            bail!(
                                "ore-family Sell inversion after safety: {} {} ({:.2}) has greater SDE output than {} {} ({:.2}) but a lower Sell ({:.2} < {:.2})",
                                higher.type_id,
                                higher.name,
                                higher.output_ratio,
                                lower.type_id,
                                lower.name,
                                lower.output_ratio,
                                higher_row.ask,
                                lower_row.ask,
                            );
                        }
                    }
                    if lower_row.writes_bid() && higher_row.writes_bid() {
                        checked += 1;
                        if higher_row.bid < lower_row.bid {
                            bail!(
                                "ore-family Buy inversion after safety: {} {} ({:.2}) has greater SDE output than {} {} ({:.2}) but a lower Buy ({:.2} < {:.2})",
                                higher.type_id,
                                higher.name,
                                higher.output_ratio,
                                lower.type_id,
                                lower.name,
                                lower.output_ratio,
                                higher_row.bid,
                                lower_row.bid,
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(checked)
}

pub fn print_ore_family_normalization_report(
    normalization: &OreFamilyNormalization,
    applied_types: usize,
    monotonic_checks: usize,
) {
    println!("Ore-family runtime reference normalization:");
    println!(
        "  anchored comparable families: {} across {} SDE ore groups; {} normalized references; {} applied canonical types",
        normalization.anchored.len(),
        normalization.represented_group_count(),
        normalization.normalized_type_count(),
        applied_types,
    );
    println!(
        "  unanchored comparable families: {}; ambiguous/unmapped members: {}; final monotonic side comparisons: {}",
        normalization.unanchored.len(),
        normalization.ambiguous.len(),
        monotonic_checks,
    );
    for family in &normalization.anchored {
        println!(
            "    group {} [{} outputs {:?}]: median anchor {:.6} from {} exact raw capture(s)",
            family.group_id,
            family.reprocessing_family,
            family.output_material_type_ids,
            family.family_anchor,
            family.captures.len(),
        );
        for capture in &family.captures {
            println!(
                "      capture {} {}: {:.6} ({}) / output ratio {:.6} = {:.6}",
                capture.type_id,
                capture.name,
                capture.captured_reference,
                capture.source.key(),
                capture.output_ratio,
                capture.normalized_capture,
            );
        }
    }
    for family in &normalization.unanchored {
        println!(
            "    unanchored group {} [{} outputs {:?}] raw types {:?}: existing manifest behavior preserved",
            family.group_id,
            family.reprocessing_family,
            family.output_material_type_ids,
            family.raw_type_ids,
        );
    }
    for entry in &normalization.ambiguous {
        println!(
            "    unchanged ambiguous {} {}: {}",
            entry.type_id, entry.name, entry.reason
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::BucketFlags;
    use crate::seedplan::{SeedSides, SeedableType};
    use crate::staticdata::ReprocessingMaterial;
    use std::path::Path;

    const MINERAL: u32 = 34;

    fn ore(
        type_id: u32,
        name: &str,
        portion_size: i64,
        per_unit: i64,
        family: &str,
    ) -> ReprocessingType {
        ReprocessingType {
            type_id,
            name: name.to_string(),
            portion_size,
            is_refinable: true,
            is_recyclable: true,
            family: family.to_string(),
            materials: vec![ReprocessingMaterial {
                type_id: MINERAL,
                quantity: per_unit * portion_size,
            }],
        }
    }

    fn fixture_family(
        raw: &[(u32, &str, i64)],
        modern: &[(u32, &str, i64)],
        legacy: &[(u32, &str, i64)],
        family: &str,
    ) -> ReprocessingStatic {
        let mut types = Vec::new();
        let mut pairs = BTreeMap::new();
        for ((raw_id, raw_name, output), (compressed_id, compressed_name, compressed_output)) in
            raw.iter().zip(modern)
        {
            types.push(ore(*raw_id, raw_name, 100, *output, family));
            types.push(ore(
                *compressed_id,
                compressed_name,
                100,
                *compressed_output,
                family,
            ));
            pairs.insert(*raw_id, *compressed_id);
        }
        for (type_id, name, output) in legacy {
            types.push(ore(*type_id, name, 1, *output, family));
        }
        ReprocessingStatic::new(types, pairs)
    }

    fn derive(
        reprocessing: &ReprocessingStatic,
        captures: &[(u32, f64, PriceSource)],
        moon_mining_available: bool,
    ) -> OreFamilyNormalization {
        OreFamilyNormalization::derive_inner(
            reprocessing,
            &captures
                .iter()
                .map(|(type_id, price, source)| (*type_id, (*price, *source)))
                .collect(),
            moon_mining_available,
            |_| true,
            |_| Some(450),
        )
    }

    fn three_grade_fixture(family: &str) -> ReprocessingStatic {
        fixture_family(
            &[(1, "Base", 100), (2, "Higher", 105), (3, "Highest", 110)],
            &[
                (11, "Packed Base", 100),
                (12, "Packed Higher", 105),
                (13, "Packed Highest", 110),
            ],
            &[
                (21, "Batch Base", 10_000),
                (22, "Batch Higher", 10_500),
                (23, "Batch Highest", 11_000),
            ],
            family,
        )
    }

    #[test]
    fn actual_output_vectors_define_family_ratios_and_odd_median_anchor() {
        let reprocessing = three_grade_fixture(ORE_FAMILY);
        // Normalized captures are 100, 200 and 90; the ordinary median is 100.
        let normalized = derive(
            &reprocessing,
            &[
                (1, 100.0, PriceSource::CcpEsiAverage),
                (2, 210.0, PriceSource::CcpSnapshotJitaSplit),
                (3, 99.0, PriceSource::CcpEsiAverage),
            ],
            true,
        );
        assert_eq!(normalized.anchored.len(), 1);
        assert_eq!(normalized.anchored[0].family_anchor, 100.0);
        assert_eq!(normalized.reference(1), Some(100.0));
        assert_eq!(normalized.reference(2), Some(105.0));
        assert_eq!(normalized.reference(3), Some(110.0));
    }

    #[test]
    fn even_median_one_capture_zero_capture_and_derived_exclusion_are_fail_closed() {
        let reprocessing = three_grade_fixture(ORE_FAMILY);
        let even = derive(
            &reprocessing,
            &[
                (1, 80.0, PriceSource::CcpEsiAverage),
                (3, 132.0, PriceSource::CcpSnapshotJitaSplit),
            ],
            true,
        );
        assert_eq!(even.anchored[0].family_anchor, 100.0);

        let one = derive(
            &reprocessing,
            &[(2, 126.0, PriceSource::CcpEsiAverage)],
            true,
        );
        assert_eq!(one.anchored[0].family_anchor, 120.0);

        let derived_only = derive(
            &reprocessing,
            &[
                (1, 9_999.0, PriceSource::ReprocessingFloor),
                (2, 8_888.0, PriceSource::DerivedCompressedTwin),
            ],
            true,
        );
        assert!(derived_only.anchored.is_empty());
        assert_eq!(derived_only.unanchored.len(), 1);
        assert!(derived_only.references.is_empty());

        let none = derive(&reprocessing, &[], true);
        assert!(none.anchored.is_empty());
        assert_eq!(none.unanchored.len(), 1);
    }

    #[test]
    fn modern_twins_and_legacy_batches_derive_from_normalized_raw_by_actual_equivalence() {
        let reprocessing = three_grade_fixture(ORE_FAMILY);
        let normalized = derive(
            &reprocessing,
            &[(1, 100.0, PriceSource::CcpEsiAverage)],
            true,
        );
        assert_eq!(normalized.reference(11), Some(100.0));
        assert_eq!(normalized.reference(12), Some(105.0));
        assert_eq!(normalized.reference(13), Some(110.0));
        assert_eq!(normalized.reference(21), Some(10_000.0));
        assert_eq!(normalized.reference(22), Some(10_500.0));
        assert_eq!(normalized.reference(23), Some(11_000.0));
        let batch = normalized.anchored[0]
            .members
            .iter()
            .find(|member| member.type_id == 22)
            .unwrap();
        assert_eq!(batch.raw_type_id, 2);
        assert_eq!(batch.raw_equivalence, 100.0);
    }

    fn seedable(type_id: u32, profile: PricingProfile, price: f64) -> SeedableType {
        SeedableType {
            type_id,
            price,
            source: PriceSource::BasePrice,
            bucket: BucketFlags {
                material: true,
                ..BucketFlags::default()
            },
            role: MarketRole::Core,
            profile,
            sides: SeedSides::BOTH,
        }
    }

    fn row(type_id: u32, kind: OreMemberKind, ask: f64, bid: f64) -> SeedRow {
        SeedRow {
            type_id,
            name: format!("type {type_id}"),
            bucket: "B",
            junk: false,
            role: MarketRole::Core,
            profile: if kind == OreMemberKind::ModernCompressed {
                PricingProfile::CompressedOre
            } else {
                PricingProfile::RawOre
            },
            cost: 1.0,
            ask,
            bid,
            sides: SeedSides::BOTH,
            quantity: 1,
        }
    }

    #[test]
    fn plan_overlay_changes_only_core_ore_and_final_side_invariant_is_independent() {
        let normalized = derive(
            &three_grade_fixture(ORE_FAMILY),
            &[(1, 100.0, PriceSource::CcpEsiAverage)],
            true,
        );
        let mut plan = SeedPlan::default();
        for type_id in [1, 2, 3] {
            plan.seedable
                .insert(type_id, seedable(type_id, PricingProfile::RawOre, 999.0));
        }
        // Unrelated manufacturing output is deliberately outside the normalization map.
        plan.seedable
            .insert(9_000, seedable(9_000, PricingProfile::CoreGeneral, 777.0));
        assert_eq!(normalized.apply_to_plan(&mut plan), 3);
        assert_eq!(plan.seedable[&2].price, 105.0);
        assert_eq!(
            plan.seedable[&2].source,
            PriceSource::OreFamilyNormalizedReference
        );
        assert_eq!(plan.seedable[&9_000].price, 777.0);
        assert_eq!(plan.seedable[&9_000].source, PriceSource::BasePrice);

        let rows = vec![
            row(1, OreMemberKind::Raw, 105.0, 58.0),
            row(2, OreMemberKind::Raw, 110.25, 60.9),
            row(3, OreMemberKind::Raw, 115.5, 63.8),
        ];
        assert_eq!(
            verify_ore_family_monotonicity(&normalized, &rows).unwrap(),
            6
        );
        let mut inverted = rows;
        inverted[2].bid = 50.0;
        assert!(verify_ore_family_monotonicity(&normalized, &inverted).is_err());
    }

    #[test]
    fn moon_mining_false_preserves_compatibility_and_true_normalizes_moon_grades() {
        let reprocessing = three_grade_fixture(MOON_ORE_FAMILY);
        let captures = [(1, 100.0, PriceSource::CcpEsiAverage)];
        let disabled = derive(&reprocessing, &captures, false);
        assert!(disabled.references.is_empty());
        assert!(disabled.anchored.is_empty());
        let enabled = derive(&reprocessing, &captures, true);
        assert_eq!(enabled.reference(1), Some(100.0));
        assert_eq!(enabled.reference(2), Some(105.0));
        assert_eq!(enabled.reference(3), Some(110.0));
    }

    #[test]
    fn full_spodumain_ids_share_one_anchor_across_raw_modern_and_batch_forms() {
        let reprocessing = fixture_family(
            &[
                (19, "Spodumain", 100),
                (17_466, "Spodumain II", 105),
                (17_467, "Spodumain III", 110),
                (46_688, "Spodumain IV", 115),
            ],
            &[
                (62_572, "Compressed Spodumain", 100),
                (62_573, "Compressed Spodumain II", 105),
                (62_574, "Compressed Spodumain III", 110),
                (62_575, "Compressed Spodumain IV", 115),
            ],
            &[
                (28_420, "Batch Spodumain", 10_000),
                (28_418, "Batch Spodumain II", 10_500),
                (28_419, "Batch Spodumain III", 11_000),
                (46_704, "Batch Spodumain IV", 11_500),
            ],
            ORE_FAMILY,
        );
        // Deliberately inverted independent captures. They may anchor the family, but they
        // cannot survive as independent grade references.
        let normalized = derive(
            &reprocessing,
            &[
                (19, 1_000.0, PriceSource::CcpEsiAverage),
                (17_466, 945.0, PriceSource::CcpSnapshotJitaSplit),
                (17_467, 1_320.0, PriceSource::CcpEsiAverage),
                (46_688, 920.0, PriceSource::CcpSnapshotJitaSplit),
            ],
            true,
        );
        let family = &normalized.anchored[0];
        assert_eq!(family.captures.len(), 4);
        for ids in [
            [19, 17_466, 17_467, 46_688],
            [62_572, 62_573, 62_574, 62_575],
            [28_420, 28_418, 28_419, 46_704],
        ] {
            let prices = ids.map(|type_id| normalized.reference(type_id).unwrap());
            assert!(prices.windows(2).all(|pair| pair[1] >= pair[0]));
        }
        assert_eq!(normalized.reference(62_574), normalized.reference(17_467));
        assert_eq!(
            normalized.reference(28_419),
            normalized.reference(17_467).map(|price| price * 100.0)
        );
    }

    #[test]
    fn unsupported_single_legacy_mapping_is_reported_and_left_unchanged() {
        let mut reprocessing = three_grade_fixture(ORE_FAMILY);
        reprocessing
            .types
            .retain(|entry| entry.type_id != 22 && entry.type_id != 23);
        reprocessing =
            ReprocessingStatic::new(reprocessing.types, reprocessing.compressed_by_source);
        let normalized = derive(
            &reprocessing,
            &[(1, 100.0, PriceSource::CcpEsiAverage)],
            true,
        );
        assert_eq!(normalized.reference(21), None);
        assert!(normalized.ambiguous.iter().any(|entry| entry.type_id == 21));
    }

    #[test]
    fn real_sde_controls_when_fixture_paths_are_supplied() {
        let (Ok(data_dir), Ok(manifest_path)) = (
            std::env::var("MARKET_SEEDERV3_TEST_STATIC_DATA"),
            std::env::var("MARKET_SEEDERV3_TEST_PRICE_MANIFEST"),
        ) else {
            // Portable unit-test runs do not require an adjacent generated-data checkout.
            return;
        };
        let data = StaticData::load(Path::new(&data_dir)).expect("load accepted generated data");
        let reprocessing =
            ReprocessingStatic::load(Path::new(&data_dir)).expect("load reprocessing data");
        let manifest = PriceManifest::load(Path::new(&manifest_path))
            .expect("load manifest")
            .expect("manifest exists");
        let normalized = OreFamilyNormalization::derive(
            &data,
            &reprocessing,
            &manifest,
            CapturePolicy {
                accept_adjusted: false,
            },
            true,
        );

        let spodumain = [
            19, 17_466, 17_467, 46_688, 62_572, 62_573, 62_574, 62_575, 28_420, 28_418, 28_419,
            46_704,
        ];
        for type_id in spodumain {
            assert!(
                normalized.reference(type_id).is_some(),
                "Spodumain type {type_id} must derive from the common raw anchor"
            );
        }
        let mut reference_comparisons = 0;
        for family in &normalized.anchored {
            for kind in [
                OreMemberKind::Raw,
                OreMemberKind::ModernCompressed,
                OreMemberKind::LegacyBatch,
            ] {
                let members = family
                    .members
                    .iter()
                    .filter(|member| member.kind == kind)
                    .collect::<Vec<_>>();
                for lower in &members {
                    for higher in &members {
                        if higher.output_ratio <= lower.output_ratio {
                            continue;
                        }
                        reference_comparisons += 1;
                        assert!(
                            higher.reconstructed_reference >= lower.reconstructed_reference,
                            "normalized reference inversion: {} {} < {} {}",
                            higher.type_id,
                            higher.name,
                            lower.type_id,
                            lower.name,
                        );
                    }
                }
            }
        }

        let by_name = data
            .marketable_item_types()
            .map(|item| (item.name.as_str(), item.type_id))
            .collect::<BTreeMap<_, _>>();
        for (lower_name, higher_name) in [
            ("Arkonor II-Grade", "Arkonor III-Grade"),
            ("Bistot III-Grade", "Bistot IV-Grade"),
            ("Plagioclase III-Grade", "Plagioclase IV-Grade"),
            ("Veldspar III-Grade", "Veldspar IV-Grade"),
            ("Talassonite", "Talassonite II-Grade"),
            ("Euxenite", "Copious Euxenite"),
            ("Vanadinite", "Lavish Vanadinite"),
            ("Sylvite", "Brimful Sylvite"),
            ("Ytterbite", "Bountiful Ytterbite"),
            ("Ytirium III-Grade", "Ytirium IV-Grade"),
            ("Hezorime", "Hezorime II-Grade"),
        ] {
            let lower_id = by_name[lower_name];
            let higher_id = by_name[higher_name];
            let lower = normalized
                .reference(lower_id)
                .unwrap_or_else(|| panic!("{lower_name} must be anchored"));
            let higher = normalized
                .reference(higher_id)
                .unwrap_or_else(|| panic!("{higher_name} must be anchored"));
            assert!(
                higher >= lower,
                "{higher_name} normalized reference {higher} < {lower_name} {lower}"
            );
            println!(
                "control {lower_id} {lower_name}: manifest {:?} -> normalized {:.6}; {higher_id} {higher_name}: manifest {:?} -> normalized {:.6}",
                manifest.entry(lower_id).map(|entry| entry.price),
                lower,
                manifest.entry(higher_id).map(|entry| entry.price),
                higher,
            );
        }
        println!(
            "real SDE normalization: {} anchored comparable families across {} groups, {} references, {} reference comparisons, {} unanchored, {} ambiguous",
            normalized.anchored.len(),
            normalized.represented_group_count(),
            normalized.normalized_type_count(),
            reference_comparisons,
            normalized.unanchored.len(),
            normalized.ambiguous.len(),
        );
        for type_id in spodumain {
            println!(
                "Spodumain {type_id}: manifest {:?} -> normalized {:.6}",
                manifest
                    .entry(type_id)
                    .map(|entry| (entry.price, entry.source.key())),
                normalized.reference(type_id).unwrap(),
            );
        }
    }
}
