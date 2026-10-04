//! Writes an already validated, bounded-memory Distribution plan using the
//! existing SQLite schema, summaries, history and finalization functions.
use crate::build::{self, SeedRow, SeedStation};
use crate::config::SeederConfig;
use crate::distribution::Plan;
use crate::staticdata::StaticData;
use anyhow::{Result, ensure};
use market_common::{MANIFEST_KEY, MARKET_SCHEMA_VERSION, MarketManifest, now_rfc3339};
use rusqlite::{Connection, params};

pub fn run(
    plan: &Plan,
    preview: &serde_json::Value,
    config: &SeederConfig,
    data: &StaticData,
) -> Result<()> {
    ensure!(
        preview["valid"] == true,
        "Distribution safety errors prevent build"
    );
    let target = &config.output.database_path;
    let staging = build::staged_database_path(target);
    ensure!(
        !target.exists() && !staging.exists(),
        "candidate already exists; refusing overwrite"
    );
    let mut conn = build::open_build_connection(
        &staging,
        config.build.sqlite_cache_size_kib,
        config.build.sqlite_page_size_bytes,
        config.build.sqlite_worker_threads,
    )?;
    let now = now_rfc3339();
    let counts = build::write_static_tables(&mut conn, data)?;
    let tx = conn.transaction()?;
    {
        let sql = |table: &str| {
            format!(
                "INSERT INTO {table} (station_id,solar_system_id,constellation_id,region_id,type_id,price,quantity,initial_quantity,price_version,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?7,1,?8)"
            )
        };
        let mut sell = tx.prepare(&sql("seed_stock"))?;
        let mut buy = tx.prepare(&sql("seed_buy_orders"))?;
        for station in &plan.stations {
            for row in plan.rows(station) {
                let s = &station.npc;
                if let Some(p) = row.sell {
                    sell.execute(params![
                        s.station_id,
                        s.solar_system_id,
                        s.constellation_id,
                        s.region_id,
                        row.type_id,
                        p,
                        row.quantity,
                        now
                    ])?;
                }
                if let Some(p) = row.buy {
                    buy.execute(params![
                        s.station_id,
                        s.solar_system_id,
                        s.constellation_id,
                        s.region_id,
                        row.type_id,
                        p,
                        row.quantity,
                        now
                    ])?;
                }
            }
        }
    }
    tx.commit()?;
    plan.verify_database(&conn)?;
    build::rebuild_summaries(&mut conn, "region_summaries", "region_id")?;
    build::rebuild_summaries(&mut conn, "system_seed_summaries", "solar_system_id")?;
    // Release the exclusive writer before the existing candidate-bound book reader.
    drop(conn);
    let stations: Vec<SeedStation> = plan
        .stations
        .iter()
        .filter(|s| !plan.rows(s).is_empty())
        .map(|s| {
            SeedStation::from(
                data.station(s.npc.station_id)
                    .expect("eligible static station"),
            )
        })
        .collect();
    let labels: Vec<SeedRow> = plan
        .quotes
        .iter()
        .map(|q| SeedRow {
            type_id: q.type_id,
            name: q.name.clone(),
            bucket: "C",
            junk: false,
            role: crate::classify::MarketRole::Core,
            profile: crate::classify::PricingProfile::CoreGeneral,
            cost: 0.,
            ask: q.sell.unwrap_or(0.),
            bid: q.buy.unwrap_or(0.),
            sides: crate::seedplan::SeedSides {
                ask: q.sell.is_some(),
                bid: q.buy.is_some(),
            },
            quantity: crate::distribution::MAX_STOCK,
        })
        .collect();
    let book = crate::audit::SeedBook::from_candidate_db(&staging, &labels, &stations, data)?;
    let history = crate::history::plan_canonical_history(
        &book,
        config.history.effective_days(),
        crate::history::today_utc(),
    );
    let mut conn = Connection::open(&staging)?;
    if !history.rows.is_empty() {
        crate::history::write_history_rows(
            &mut conn,
            &history.rows,
            history.types,
            history.days,
            None,
        )?;
    }
    let manifest = MarketManifest {
        schema_version: MARKET_SCHEMA_VERSION,
        generated_at: now.clone(),
        static_data_dir: data.dir.display().to_string(),
        database_path: target.display().to_string(),
        selection_mode: "workbench_distribution_v1".into(),
        selection_label: format!(
            "Distribution: {} selected NPC stations",
            plan.stations.len()
        ),
        selected_solar_system_ids: plan
            .stations
            .iter()
            .map(|s| s.npc.solar_system_id)
            .collect(),
        selected_solar_system_names: plan
            .stations
            .iter()
            .map(|s| {
                data.solar_system(s.npc.solar_system_id)
                    .expect("eligible system")
                    .solar_system_name
                    .clone()
            })
            .collect(),
        region_count: counts.regions as u32,
        solar_system_count: counts.solar_systems as u32,
        station_count: counts.stations as u32,
        market_type_count: counts.market_types as u32,
        seed_row_count: preview["total_seed_rows"].as_u64().unwrap(),
        default_quantity_per_station_type: crate::distribution::MAX_STOCK as u32,
        seed_buy_orders_enabled: preview["buy_rows"].as_u64().unwrap() > 0,
        history_days_seeded: history.days,
        seed_markup_percent: 0.,
        station_jitter_percent: 0.,
        region_jitter_percent: 0.,
    };
    for (key, value) in [
        (MANIFEST_KEY, serde_json::to_string_pretty(&manifest)?),
        ("seeder", "market-seederv3".into()),
        ("built_at", now),
        (
            "distribution_plan_sha256",
            preview["plan_sha256"].as_str().unwrap().into(),
        ),
        (
            "distribution_policy",
            serde_json::to_string(&plan.distribution)?,
        ),
        ("input_provenance", serde_json::to_string(&plan.provenance)?),
    ] {
        conn.execute(
            "INSERT INTO manifest (key,value) VALUES (?1,?2)",
            params![key, value],
        )?;
    }
    build::build_runtime_indexes(&conn)?;
    build::finalize_database(&conn)?;
    plan.verify_database(&conn)?;
    ensure!(
        conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))? == "ok",
        "candidate integrity failed"
    );
    ensure!(
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))? == 0,
        "candidate foreign keys failed"
    );
    drop(conn);
    build::install_built_database(&staging, target)?;
    Ok(())
}
