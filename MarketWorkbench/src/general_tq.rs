//! Separate GENERAL_TQ preset runner. Shares the V2 resolver, pricing arithmetic,
//! schema and canonical five-hub writers, without invoking LEGACY_V1 economics.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use market_common::{MANIFEST_KEY, MARKET_SCHEMA_VERSION, MarketManifest, now_rfc3339};
use rusqlite::{Connection, params};

use crate::audit::SeedBook;
use crate::build::{
    SeedPricing, SeedRow, SeedStation, build_runtime_indexes, count_crossed_pairs,
    finalize_database, install_built_database, open_build_connection, rebuild_summaries,
    staged_database_path, verify_general_tq_seed_rows, write_canonical_seed_rows,
    write_static_tables,
};
use crate::classify::{MarketRole, PricingProfile};
use crate::config::{PolicyPreset, SeederConfig};
use crate::funded::{FundedPath, FundedResolver, IndustrySemantics, ProductionCatalog};
use crate::manifest::read_sde_build;
use crate::policy::{PolicyDocument, Source};
use crate::policy_resolve::{Resolution, ResolvedSide};
use crate::seedplan::SeedSides;
use crate::staticdata::{StaticData, resolve_static_data_dir};
use crate::tq_snapshot::{Side, TqSnapshot};

fn ordinary_type_ids(path: &Path) -> Result<BTreeSet<u32>> {
    let root: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    let entries = root["types"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("ordinary taxonomy has no types array"))?;
    let mut ordinary = BTreeSet::new();
    for item in entries {
        if item["semanticClass"] == "ORDINARY_MARKET_COMPATIBLE" {
            let id = item["typeID"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("taxonomy typeID missing"))?
                as u32;
            ensure!(ordinary.insert(id), "duplicate ordinary taxonomy type {id}");
        }
    }
    Ok(ordinary)
}

pub(crate) fn price(
    book: &TqSnapshot,
    type_id: u32,
    side: Side,
    rule: &ResolvedSide,
) -> Result<f64> {
    price_with_funded(book, type_id, side, rule, None)
}

pub(crate) fn price_with_funded(
    book: &TqSnapshot,
    type_id: u32,
    side: Side,
    rule: &ResolvedSide,
    funded_cost: Option<f64>,
) -> Result<f64> {
    ensure!(
        rule.floor.is_none(),
        "GENERAL_TQ type {type_id}: policy floor is unsupported"
    );
    let reference = (if rule.source == Source::FundedCost {
        funded_cost
    } else {
        book.policy_reference(type_id, side, rule.source)
    })
    .ok_or_else(|| {
        anyhow::anyhow!(
            "GENERAL_TQ type {type_id}: {:?} {:?} unavailable",
            side,
            rule.source
        )
    })?;
    apply_reference(type_id, side, rule, reference)
}

pub(crate) fn apply_reference(
    type_id: u32,
    side: Side,
    rule: &ResolvedSide,
    reference: f64,
) -> Result<f64> {
    ensure!(
        rule.floor.is_none(),
        "GENERAL_TQ type {type_id}: policy floor is unsupported"
    );
    ensure!(
        reference.is_finite() && reference > 0.0,
        "invalid price reference for type {type_id}"
    );
    let pricing = SeedPricing {
        sell_multiplier: if side == Side::Sell {
            rule.multiplier
        } else {
            1.0
        },
        buy_multiplier: if side == Side::Buy {
            rule.multiplier
        } else {
            1.0
        },
        price_floor: 0.0,
    };
    Ok(if side == Side::Sell {
        pricing.ask(reference)
    } else {
        pricing.bid(reference)
    })
}

/// Use the existing funded production resolver against guaranteed, policy-priced Sell quotes.
/// A synthetic Buy never becomes an acquisition input. Returned errors remain per type so
/// Workbench can show unresolved evidence without aborting an entire draft preview.
pub(crate) fn funded_costs(
    data: &StaticData,
    static_dir: &Path,
    tq: &TqSnapshot,
    resolutions: &BTreeMap<u32, Resolution>,
) -> Result<BTreeMap<u32, std::result::Result<FundedPath, String>>> {
    funded_costs_with_references(
        data,
        static_dir,
        tq,
        resolutions,
        &crate::preset_references::References::default(),
    )
}

pub(crate) fn funded_costs_with_references(
    data: &StaticData,
    static_dir: &Path,
    tq: &TqSnapshot,
    resolutions: &BTreeMap<u32, Resolution>,
    references: &crate::preset_references::References,
) -> Result<BTreeMap<u32, std::result::Result<FundedPath, String>>> {
    let mut targets = BTreeSet::new();
    let mut guaranteed_sells = BTreeMap::new();
    for (&type_id, decision) in resolutions {
        let Some(policy) = &decision.policy else {
            continue;
        };
        if let Some(sell) = &policy.sell {
            if sell.source == Source::FundedCost {
                targets.insert(type_id);
            } else if let Ok(ask) =
                references.price(tq, type_id, Side::Sell, sell, &BTreeMap::new(), resolutions)
            {
                guaranteed_sells.insert(type_id, ask);
            }
        }
        if policy
            .buy
            .as_ref()
            .is_some_and(|side| side.source == Source::FundedCost)
        {
            targets.insert(type_id);
        }
    }
    if targets.is_empty() {
        return Ok(BTreeMap::new());
    }
    let catalog = ProductionCatalog::from_static(data);
    let semantics = IndustrySemantics::load(static_dir, data)?;
    let mut resolver = FundedResolver::new(data, &catalog, &semantics, &guaranteed_sells);
    let mut costs = BTreeMap::new();
    for type_id in targets {
        costs.insert(
            type_id,
            resolver
                .production_cost(type_id)
                .map_err(|failure| failure.reason),
        );
    }
    Ok(costs)
}

fn write_preview(
    path: &Path,
    rows: &[SeedRow],
    ordinary: usize,
    unresolved: &[(u32, String)],
    policy_path: &Path,
    tq_path: &Path,
) -> Result<()> {
    let payload = serde_json::json!({
        "preset": "GENERAL_TQ", "formatVersion": 1, "ordinaryTypeCount": ordinary,
        "quotedTypeCount": rows.len(),
        "quotedBuySellCount": rows.iter().filter(|r|r.writes_ask() && r.writes_bid()).count(),
        "sellCount": rows.iter().filter(|r|r.writes_ask()).count(),
        "buyCount": rows.iter().filter(|r|r.writes_bid()).count(),
        "unresolvedCount": unresolved.len(),
        "buyGeSellWarnings": rows.iter().filter(|r|r.writes_ask() && r.writes_bid() && r.bid >= r.ask).count(),
        "policySHA256": crate::policy_preview::sha256_hex(&fs::read(policy_path)?),
        "tqDatasetSHA256": crate::policy_preview::sha256_hex(&fs::read(tq_path)?),
        "rows": rows.iter().map(|r| serde_json::json!({"typeID":r.type_id,"name":r.name,"buy":if r.writes_bid(){Some(r.bid)}else{None},"sell":if r.writes_ask(){Some(r.ask)}else{None},"quantity":r.quantity})).collect::<Vec<_>>(),
        "unresolved": unresolved.iter().map(|(id,why)| serde_json::json!({"typeID":id,"reason":why})).collect::<Vec<_>>(),
    });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(&payload)?)?;
    Ok(())
}

fn write_general_manifest(
    connection: &Connection,
    config: &SeederConfig,
    config_path: &Path,
    static_dir: &Path,
    policy_path: &Path,
    tq_path: &Path,
    taxonomy_path: &Path,
    static_data: &StaticData,
    stations: &[SeedStation],
    rows: &[SeedRow],
    history_days: u32,
    built_at: &str,
    static_counts: crate::build::StaticCounts,
) -> Result<()> {
    let selected_names = stations
        .iter()
        .map(|station| {
            static_data
                .solar_system(station.solar_system_id)
                .map(|system| system.solar_system_name.clone())
                .unwrap_or_else(|| "<unknown>".into())
        })
        .collect::<Vec<_>>();
    let market_manifest = MarketManifest {
        schema_version: MARKET_SCHEMA_VERSION,
        generated_at: built_at.to_string(),
        static_data_dir: static_dir.display().to_string(),
        database_path: config.output.database_path.display().to_string(),
        selection_mode: "general_tq_v1".into(),
        selection_label: format!(
            "GENERAL_TQ: {} ordinary types, direct TQ references and explicit preset fallbacks at five canonical hubs",
            rows.len()
        ),
        selected_solar_system_ids: stations.iter().map(|s| s.solar_system_id).collect(),
        selected_solar_system_names: selected_names,
        region_count: static_counts.regions as u32,
        solar_system_count: static_counts.solar_systems as u32,
        station_count: static_counts.stations as u32,
        market_type_count: static_counts.market_types as u32,
        seed_row_count: (rows.iter().map(SeedRow::row_count).sum::<usize>() * stations.len())
            as u64,
        default_quantity_per_station_type: config.canonical_market.quantity as u32,
        seed_buy_orders_enabled: rows.iter().any(SeedRow::writes_bid),
        history_days_seeded: history_days,
        seed_markup_percent: 0.0,
        station_jitter_percent: 0.0,
        region_jitter_percent: 0.0,
    };
    for (key, value) in [
        (MANIFEST_KEY, serde_json::to_string_pretty(&market_manifest)?),
        ("seeder", "market-seederv3".into()),
        ("selection_mode", "general_tq_v1".into()),
        ("built_at", built_at.into()),
        ("v2_policy_path", policy_path.display().to_string()),
        ("v2_policy_sha256", crate::policy_preview::sha256_hex(&fs::read(policy_path)?)),
        ("tq_reference_path", tq_path.display().to_string()),
        ("tq_reference_sha256", crate::policy_preview::sha256_hex(&fs::read(tq_path)?)),
        ("ordinary_taxonomy_path", taxonomy_path.display().to_string()),
        ("ordinary_taxonomy_sha256", crate::policy_preview::sha256_hex(&fs::read(taxonomy_path)?)),
        ("operational_config_path", config_path.display().to_string()),
        ("operational_config_sha256", crate::policy_preview::sha256_hex(&fs::read(config_path)?)),
        ("quote_parameters", "direct=1.00/1.00;sell_only_buy=0.80;buy_only_sell=1.25;average_buy=0.80;average_sell=1.25".into()),
    ] {
        connection.execute("INSERT INTO manifest (key,value) VALUES (?1,?2)", params![key,value])?;
    }
    Ok(())
}

pub fn run(config_path: &Path, config: &SeederConfig, dry_run: bool) -> Result<()> {
    run_with_facts(config_path, config, dry_run, None)
}

pub(crate) fn run_with_facts(
    config_path: &Path,
    config: &SeederConfig,
    dry_run: bool,
    facts: Option<&BTreeMap<u32, BTreeSet<String>>>,
) -> Result<()> {
    let policy_config = config
        .policy
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing [policy]"))?;
    ensure!(
        policy_config.preset == PolicyPreset::GeneralTq,
        "wrong preset runner"
    );
    let tq_path = policy_config
        .tq_snapshot_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("GENERAL_TQ requires tq_snapshot_path"))?;
    let expected_tq_sha = policy_config
        .tq_snapshot_sha256
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("GENERAL_TQ requires tq_snapshot_sha256"))?;
    let actual_tq_sha = crate::policy_preview::sha256_hex(&fs::read(tq_path)?);
    ensure!(
        actual_tq_sha.eq_ignore_ascii_case(expected_tq_sha),
        "GENERAL_TQ pinned TQ dataset SHA256 mismatch: expected {expected_tq_sha}, got {actual_tq_sha}"
    );
    let taxonomy_path = policy_config
        .ordinary_taxonomy_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("GENERAL_TQ requires ordinary_taxonomy_path"))?;
    let (static_dir, _) = resolve_static_data_dir(config.input.static_data_dir.as_deref());
    let static_data = StaticData::load(&static_dir)?;
    let sde_build = read_sde_build(&static_dir)
        .build
        .ok_or_else(|| anyhow::anyhow!("missing SDE build identity"))?;
    let policy = PolicyDocument::load(&policy_config.path)?;
    policy.validate_catalog_contract(sde_build)?;
    policy.validate()?;
    let tq = TqSnapshot::load(tq_path)?;
    let ordinary = ordinary_type_ids(taxonomy_path)?;
    let catalog_ids = static_data
        .marketable_item_types()
        .map(|i| i.type_id)
        .collect::<BTreeSet<_>>();
    ensure!(
        ordinary.is_subset(&catalog_ids),
        "ordinary taxonomy contains nonmarketable type"
    );
    ensure!(
        tq.records.keys().copied().collect::<BTreeSet<_>>() == catalog_ids,
        "TQ dataset/SDE marketable type sets differ"
    );
    let empty_facts = BTreeSet::new();
    let mut resolutions = BTreeMap::new();
    for item in static_data.marketable_item_types() {
        let item_facts = facts
            .and_then(|all| all.get(&item.type_id))
            .unwrap_or(&empty_facts);
        resolutions.insert(
            item.type_id,
            policy.resolve_validated(item.type_id, item.group_id, item.category_id, item_facts)?,
        );
    }
    let needs_preset_references = resolutions
        .values()
        .filter_map(|r| r.policy.as_ref())
        .flat_map(|p| [&p.sell, &p.buy])
        .flatten()
        .any(|s| {
            matches!(
                s.source,
                Source::NpcAcquisition | Source::T1VariantSell | Source::T1VariantBuy
            )
        });
    let references = if needs_preset_references {
        let cached = crate::snapshot::find_cached_snapshot(&config.source.download_dir)?
            .ok_or_else(|| {
                anyhow::anyhow!("preset NPC reference requires a pinned cached public NPC snapshot")
            })?;
        let overlay = crate::overlay::import_overlay(
            &cached.path,
            &static_data,
            config.import.order_filter,
            config.import.npc_order_duration_threshold_days,
            &BTreeSet::new(),
            config.price_manifest.reference_solar_system_id,
            true,
        )?;
        let classification =
            crate::classify::Classification::compute(&static_data, &config.junk_seed)?;
        let variants =
            crate::staticdata::VariantFamilies::load_for_generated_data(&static_dir, sde_build)?;
        crate::preset_references::References::from_static(
            &static_data,
            &classification,
            &overlay,
            &variants,
        )
    } else {
        crate::preset_references::References::default()
    };
    let funded =
        funded_costs_with_references(&static_data, &static_dir, &tq, &resolutions, &references)?;
    let mut rows = Vec::new();
    let mut unresolved = Vec::new();
    for item in static_data.marketable_item_types() {
        let decision = resolutions
            .get(&item.type_id)
            .expect("marketable resolution");
        if !ordinary.contains(&item.type_id) {
            ensure!(
                decision.policy.is_none(),
                "GENERAL_TQ selects nonordinary type {}",
                item.type_id
            );
            continue;
        }
        let evidence = tq
            .records
            .get(&item.type_id)
            .expect("marketable set verified");
        if let Some(resolved) = &decision.policy {
            let _funded_cost = match funded.get(&item.type_id) {
                Some(Ok(path)) => Some(path.unit_cost),
                Some(Err(reason)) => anyhow::bail!(
                    "GENERAL_TQ type {} funded_cost unavailable: {}",
                    item.type_id,
                    reason
                ),
                None => None,
            };
            let ask = if let Some(side) = resolved.sell.as_ref() {
                references.price(&tq, item.type_id, Side::Sell, side, &funded, &resolutions)?
            } else {
                0.0
            };
            let bid = if let Some(side) = resolved.buy.as_ref() {
                references.price(&tq, item.type_id, Side::Buy, side, &funded, &resolutions)?
            } else {
                0.0
            };
            ensure!(
                (!resolved.sides.has_sell() || (ask.is_finite() && ask > 0.0))
                    && (!resolved.sides.has_buy() || (bid.is_finite() && bid > 0.0)),
                "GENERAL_TQ type {} invalid rounded Buy/Sell: {bid}/{ask}",
                item.type_id
            );
            rows.push(SeedRow {
                type_id: item.type_id,
                name: item.name.clone(),
                bucket: "D",
                junk: false,
                role: MarketRole::Core,
                profile: PricingProfile::CoreGeneral,
                cost: if resolved.sides.has_sell() { ask } else { bid },
                ask,
                bid,
                sides: SeedSides {
                    ask: resolved.sides.has_sell(),
                    bid: resolved.sides.has_buy(),
                },
                quantity: config.canonical_market.quantity,
            });
        } else {
            ensure!(
                decision.exclusion_reason.as_deref() == Some("explicit_unseeded"),
                "ordinary type {} lacks an explicit exclusion",
                item.type_id
            );
            let reason = if decision.winner_rule_id.as_deref() == Some("rounding_cross") {
                "ROUNDING_CROSS".to_string()
            } else {
                evidence.state.clone()
            };
            unresolved.push((item.type_id, reason));
        }
    }
    ensure!(
        rows.len() + unresolved.len() == ordinary.len(),
        "GENERAL_TQ coverage partition failed"
    );
    verify_general_tq_seed_rows(
        &rows,
        rows.len(),
        config.canonical_market.quantity,
        config.canonical_market.quantity,
    )?;
    let stations = config
        .canonical_market
        .station_ids
        .iter()
        .map(|id| {
            static_data
                .station(*id)
                .map(SeedStation::from)
                .ok_or_else(|| anyhow::anyhow!("canonical station {id} absent from SDE"))
        })
        .collect::<Result<Vec<_>>>()?;
    let preview_path = policy_config
        .plan_preview_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("GENERAL_TQ requires plan_preview_path"))?;
    write_preview(
        preview_path,
        &rows,
        ordinary.len(),
        &unresolved,
        &policy_config.path,
        tq_path,
    )?;
    println!(
        "GENERAL_TQ preview: {} ordinary, {} quoted types, {} unresolved; {} total seed rows across {} hubs",
        ordinary.len(),
        rows.len(),
        unresolved.len(),
        rows.iter().map(SeedRow::row_count).sum::<usize>() * stations.len(),
        stations.len()
    );
    println!("Final plan: {}", preview_path.display());
    if dry_run {
        return Ok(());
    }

    let database_path = &config.output.database_path;
    let staging_path = staged_database_path(database_path);
    ensure!(
        !database_path.exists() && !staging_path.exists(),
        "GENERAL_TQ candidate or staging database already exists; refusing a second build"
    );
    if let Some(parent) = database_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut connection = open_build_connection(
        &staging_path,
        config.build.sqlite_cache_size_kib,
        config.build.sqlite_page_size_bytes,
        config.build.sqlite_worker_threads,
    )?;
    let built_at = now_rfc3339();
    let static_counts = write_static_tables(&mut connection, &static_data)?;
    write_canonical_seed_rows(&mut connection, &stations, &rows, &built_at, None)?;
    let crossings = count_crossed_pairs(&connection)?;
    if crossings > 0 {
        println!(
            "Warning: {crossings} persisted GENERAL_TQ Buy >= Sell pairs; quotes retained as configured."
        );
    }
    rebuild_summaries(&mut connection, "region_summaries", "region_id")?;
    rebuild_summaries(&mut connection, "system_seed_summaries", "solar_system_id")?;
    let book = SeedBook::from_rows_at_stations(&rows, &stations);
    let history = crate::history::plan_canonical_history(
        &book,
        config.history.effective_days(),
        crate::history::today_utc(),
    );
    if !history.rows.is_empty() {
        crate::history::write_history_rows(
            &mut connection,
            &history.rows,
            history.types,
            history.days,
            None,
        )?;
    }
    write_general_manifest(
        &connection,
        config,
        config_path,
        &static_dir,
        &policy_config.path,
        tq_path,
        taxonomy_path,
        &static_data,
        &stations,
        &rows,
        history.days,
        &built_at,
        static_counts,
    )?;
    build_runtime_indexes(&connection)?;
    finalize_database(&connection)?;
    drop(connection);
    install_built_database(&staging_path, database_path).with_context(|| {
        format!(
            "installing isolated GENERAL_TQ candidate {}",
            database_path.display()
        )
    })?;
    println!(
        "Built isolated GENERAL_TQ candidate: {}",
        database_path.display()
    );
    Ok(())
}
