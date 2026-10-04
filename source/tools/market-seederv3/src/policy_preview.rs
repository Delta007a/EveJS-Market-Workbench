//! Read-only V2 policy preview. It resolves item policy but does not compute final prices or
//! touch SQLite; the pricing/build path must enrich these traces from its finalized plan.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::classify::{Classification, expand_lp_manufacturing_products, load_lp_reward_type_ids};
use crate::config::SeederConfig;
use crate::manifest::read_sde_build;
use crate::policy::PolicyDocument;
use crate::policy_facts::FactInputs;
use crate::policy_resolve::{CatalogItem, Resolution, RuleCoverage};
use crate::staticdata::{StaticData, resolve_static_data_dir};

#[derive(Debug, Serialize)]
pub struct PolicyPreviewSummary {
    pub format_version: u32,
    pub policy_path: String,
    pub policy_sha256: String,
    pub sde_build: u64,
    pub fact_registry_version: u32,
    pub marketable_count: usize,
    pub matched_count: usize,
    /// Enabled by a V2 item-policy rule, before source eligibility and price resolution.
    pub selected_count: usize,
    /// Populated only by the finalized plan preview, not by this policy-only pass.
    pub finalized_seeded_count: Option<usize>,
    pub explicit_excluded_count: usize,
    pub unmatched_count: usize,
    pub sell_selected_count: usize,
    pub buy_selected_count: usize,
    pub overlap_count: usize,
    pub v1_policy_delta_count: usize,
    pub v1_policy_deltas: Vec<PolicyDelta>,
    /// Final pricing is performed by the shared build plan, outside this policy-only preview.
    pub price_resolution_status: &'static str,
    pub profile_usage: BTreeMap<String, usize>,
    pub rule_coverage: Vec<RuleCoverage>,
    pub distribution: DistributionMetadata,
    pub explanations: Vec<ItemExplanation>,
}

#[derive(Debug, Serialize)]
pub struct DistributionMetadata {
    pub station_ids: Vec<u64>,
    pub quantity_per_enabled_side: i64,
}

#[derive(Debug, Serialize)]
pub struct ItemExplanation {
    pub type_id: u32,
    pub name: String,
    pub group_id: Option<u32>,
    pub category_id: Option<u32>,
    pub market_group_id: Option<u32>,
    pub facts: BTreeSet<String>,
    pub resolution: Resolution,
}

#[derive(Debug, Serialize)]
pub struct PolicyDelta {
    pub type_id: u32,
    pub name: String,
    pub v1_profile: Option<String>,
    pub v2_profile: Option<String>,
    pub v1_sides: Option<String>,
    pub v2_sides: Option<String>,
}

/// Resolve the entire marketable catalog, validate every rule, and write deterministic JSON.
/// `explain_type` limits only the emitted explanation array; the aggregate counts still cover
/// the full catalog. All input paths are read-only and the cached snapshot is never downloaded.
pub fn run(
    config_path: &Path,
    output_path: &Path,
    explain_type: Option<u32>,
) -> Result<PolicyPreviewSummary> {
    let config = SeederConfig::load(config_path)?;
    let policy_config = config.policy.as_ref().ok_or_else(|| {
        anyhow::anyhow!("[policy] path and LP catalog are required for V2 preview")
    })?;
    ensure!(
        output_path != config_path && output_path != policy_config.path,
        "preview output cannot overwrite policy or operational config"
    );
    let policy_bytes = fs::read(&policy_config.path)
        .with_context(|| format!("reading policy {}", policy_config.path.display()))?;
    let policy_sha256 = sha256_hex(&policy_bytes);
    let policy =
        PolicyDocument::parse(std::str::from_utf8(&policy_bytes).context("policy must be UTF-8")?)?;
    let (static_data_dir, _) = resolve_static_data_dir(config.input.static_data_dir.as_deref());
    let sde = read_sde_build(&static_data_dir);
    let actual_build = sde.build.ok_or_else(|| {
        anyhow::anyhow!(
            "SDE build identity unavailable: {}",
            sde.warning
                .unwrap_or_else(|| sde.path.display().to_string())
        )
    })?;
    policy.validate_catalog_contract(actual_build)?;
    let data = StaticData::load(&static_data_dir)?;
    let classification = Classification::compute(&data, &config.junk_seed)?;
    let cached =
        crate::snapshot::find_cached_snapshot(&config.source.download_dir)?.ok_or_else(|| {
            anyhow::anyhow!(
                "V2 policy preview requires a cached market-order snapshot in {}",
                config.source.download_dir.display()
            )
        })?;
    if config.import.order_filter.uses_market_scope() {
        bail!("V2 policy preview cannot use market-scope order_filter without scope systems");
    }
    let overlay = crate::overlay::import_overlay(
        &cached.path,
        &data,
        config.import.order_filter,
        config.import.npc_order_duration_threshold_days,
        &BTreeSet::new(),
        config.price_manifest.reference_solar_system_id,
        true,
    )?;
    let direct_lp = load_lp_reward_type_ids(&policy_config.lp_offer_catalog_path)?;
    let expanded_lp = expand_lp_manufacturing_products(&data, &direct_lp);
    let facts = FactInputs {
        data: &data,
        classification: &classification,
        direct_lp_rewards: &direct_lp,
        expanded_lp_rewards: &expanded_lp,
        npc_catalog_types: &overlay.npc_catalog_type_ids,
    };

    let mut catalog = Vec::new();
    for item in data.item_types.iter().filter(|item| item.is_marketable()) {
        catalog.push(CatalogItem {
            type_id: item.type_id,
            group_id: item.group_id,
            category_id: item.category_id,
            facts: facts.for_type(item.type_id),
        });
    }
    let coverage = policy.validate_reachability(&catalog)?;
    if let Some(type_id) = explain_type {
        ensure!(
            catalog.iter().any(|item| item.type_id == type_id),
            "type {type_id} is absent or not marketable in the pinned catalog"
        );
    }

    let mut report = PolicyPreviewSummary {
        format_version: policy.format_version,
        policy_path: policy_config.path.display().to_string(),
        policy_sha256,
        sde_build: actual_build,
        fact_registry_version: policy.catalog_contract.fact_registry_version,
        marketable_count: catalog.len(),
        matched_count: 0,
        selected_count: 0,
        finalized_seeded_count: None,
        explicit_excluded_count: 0,
        unmatched_count: 0,
        sell_selected_count: 0,
        buy_selected_count: 0,
        overlap_count: 0,
        v1_policy_delta_count: 0,
        v1_policy_deltas: Vec::new(),
        price_resolution_status: "not_computed_by_policy_resolver",
        profile_usage: BTreeMap::new(),
        rule_coverage: coverage,
        distribution: DistributionMetadata {
            station_ids: config.canonical_market.station_ids.clone(),
            quantity_per_enabled_side: config.canonical_market.quantity,
        },
        explanations: Vec::new(),
    };
    for item in &catalog {
        let resolution =
            policy.resolve(item.type_id, item.group_id, item.category_id, &item.facts)?;
        let v1 = classification.market_assignment_checked_with_supply_ladder(
            &data,
            item.type_id,
            overlay.npc_catalog_type_ids.contains(&item.type_id),
            expanded_lp.contains(&item.type_id),
            &config
                .market_roles
                .bridge_group_ids
                .iter()
                .copied()
                .collect(),
            &config
                .market_roles
                .bridge_type_ids
                .iter()
                .copied()
                .collect(),
            &config
                .market_roles
                .progression_npc_group_ids
                .iter()
                .copied()
                .collect(),
            &config
                .market_roles
                .progression_npc_type_ids
                .iter()
                .copied()
                .collect(),
            config.market_roles.moon_mining_available,
        )?;
        let v1_profile = v1.map(|assignment| assignment.profile.key().to_owned());
        let v2_profile = resolution
            .policy
            .as_ref()
            .and_then(|selected| selected.profile_id.clone());
        let v1_sides = v1.map(|assignment| side_name(assignment.sides.sell, assignment.sides.buy));
        let v2_sides = resolution
            .policy
            .as_ref()
            .map(|selected| side_name(selected.sides.has_sell(), selected.sides.has_buy()));
        if v1_profile != v2_profile || v1_sides != v2_sides {
            let record = data.item_type(item.type_id).expect("catalog item exists");
            report.v1_policy_deltas.push(PolicyDelta {
                type_id: item.type_id,
                name: record.name.clone(),
                v1_profile,
                v2_profile,
                v1_sides,
                v2_sides,
            });
        }
        if resolution.winner_rule_id.is_some() {
            report.matched_count += 1;
        }
        if resolution.matched_rules.len() > 1 {
            report.overlap_count += 1;
        }
        if let Some(selected) = &resolution.policy {
            report.selected_count += 1;
            if selected.sides.has_sell() {
                report.sell_selected_count += 1;
            }
            if selected.sides.has_buy() {
                report.buy_selected_count += 1;
            }
            if let Some(profile) = &selected.profile_id {
                *report.profile_usage.entry(profile.clone()).or_default() += 1;
            }
        } else if resolution.exclusion_reason.as_deref() == Some("explicit_unseeded") {
            report.explicit_excluded_count += 1;
        } else {
            report.unmatched_count += 1;
        }
        if explain_type.is_none_or(|type_id| type_id == item.type_id) {
            let record = data.item_type(item.type_id).expect("catalog item exists");
            report.explanations.push(ItemExplanation {
                type_id: item.type_id,
                name: record.name.clone(),
                group_id: item.group_id,
                category_id: item.category_id,
                market_group_id: record.market_group_id,
                facts: item.facts.clone(),
                resolution,
            });
        }
    }
    report.v1_policy_delta_count = report.v1_policy_deltas.len();
    let mut json = serde_json::to_string_pretty(&report)?;
    json.push('\n');
    fs::write(output_path, json)
        .with_context(|| format!("writing V2 preview {}", output_path.display()))?;
    ensure!(
        report.v1_policy_delta_count == 0,
        "V2 policy differs from V1 on {} marketable types; details written to {}",
        report.v1_policy_delta_count,
        output_path.display()
    );
    Ok(report)
}

fn side_name(sell: bool, buy: bool) -> String {
    match (sell, buy) {
        (false, false) => "unseeded",
        (true, false) => "sell_only",
        (false, true) => "buy_only",
        (true, true) => "buy_sell",
    }
    .to_owned()
}

/// SHA-256 over the exact input bytes, used to bind a preview to its policy document.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend(bit_len.to_be_bytes());
    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, part) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(part.try_into().expect("four bytes"));
        }
        for i in 16..64 {
            let a = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let b = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(a)
                .wrapping_add(w[i - 7])
                .wrapping_add(b);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut q] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let t1 = q
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            q = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (state, value) in h.iter_mut().zip([a, b, c, d, e, f, g, q]) {
            *state = state.wrapping_add(value);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
