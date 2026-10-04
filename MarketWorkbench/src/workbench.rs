//! Loopback-only Workbench API. Browser input is data, never a path or command.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};

use crate::classify::{Classification, expand_lp_manufacturing_products, load_lp_reward_type_ids};
use crate::config::SeederConfig;
use crate::general_tq;
use crate::manifest::read_sde_build;
use crate::policy::{FACTS, PolicyDocument, Source};
use crate::policy_facts::FactInputs;
use crate::policy_preview::sha256_hex;
use crate::policy_resolve::Resolution;
use crate::staticdata::{StaticData, resolve_static_data_dir};
use crate::tq_snapshot::{Side, TqSnapshot};

const STORAGE: &str = "G:/EVESP/EveJS-CodexLab/work/market-workbench";
const GENERAL_CONFIG: &str = "config/market-policy-general-tq-v1.local.toml";
const LEGACY_CONFIG: &str = "config/market-policy-v2.local.toml";
const MAX_BODY: usize = 8 * 1024 * 1024;

struct App {
    storage: PathBuf,
    install_targets: crate::workbench_install::Targets,
    blueprints: crate::workbench_blueprints::Catalog,
    structures: crate::workbench_structures::Catalog,
    families: crate::workbench_families::Catalog,
    data: StaticData,
    tq: TqSnapshot,
    preset_references: crate::preset_references::References,
    ordinary: BTreeSet<u32>,
    general: PolicyDocument,
    legacy: PolicyDocument,
    general_config: SeederConfig,
    sde_build: u64,
    facts: BTreeMap<u32, BTreeSet<String>>,
    category_names: BTreeMap<u32, String>,
    quick_catalog: Vec<crate::policy_resolve::CatalogItem>,
    quick_mapping: crate::workbench_quick::Mapping,
    quick_scopes: Vec<crate::workbench_quick::Scope>,
    legacy_plan_cache: Mutex<BTreeMap<String, Arc<Value>>>,
    npc_inventory: crate::distribution_stations::Inventory,
    distribution_inputs: Value,
    portable_dependencies: Vec<crate::workbench_portable::VerifiedInput>,
}

pub fn run(port: u16, install_roots: &[PathBuf], storage_dir: Option<&Path>) -> Result<()> {
    let app = App::load(install_roots, storage_dir)?;
    let listener = TcpListener::bind(bind_address(port))?;
    println!("Market Workbench is ready: http://127.0.0.1:{port}/");
    for incoming in listener.incoming() {
        match incoming {
            Ok(mut stream) => {
                if let Err(error) = handle(&app, &mut stream) {
                    eprintln!("Workbench request: {error:#}");
                }
            }
            Err(error) => eprintln!("Workbench connection: {error}"),
        }
    }
    Ok(())
}

fn bind_address(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn storage_path(configured: Option<&Path>) -> Result<PathBuf> {
    let path = configured.unwrap_or_else(|| Path::new(STORAGE));
    ensure!(
        !path.as_os_str().is_empty(),
        "Workbench storage folder is empty"
    );
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    })
}

fn static_asset(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/" | "/index.html" => Some(("index.html", "text/html; charset=utf-8")),
        "/app.js" => Some(("app.js", "application/javascript; charset=utf-8")),
        "/style.css" => Some(("style.css", "text/css; charset=utf-8")),
        _ => None,
    }
}

impl App {
    fn load(install_roots: &[PathBuf], storage_dir: Option<&Path>) -> Result<Self> {
        let install_targets = crate::workbench_install::Targets::new(install_roots)?;
        let general_config = SeederConfig::load(Path::new(GENERAL_CONFIG))?;
        let legacy_config = SeederConfig::load(Path::new(LEGACY_CONFIG))?;
        let general_policy_path = &general_config
            .policy
            .as_ref()
            .ok_or_else(|| anyhow!("missing GENERAL_TQ policy config"))?
            .path;
        let legacy_policy_path = &legacy_config
            .policy
            .as_ref()
            .ok_or_else(|| anyhow!("missing LEGACY_V1 policy config"))?
            .path;
        let tq_path = general_config
            .policy
            .as_ref()
            .unwrap()
            .tq_snapshot_path
            .as_ref()
            .ok_or_else(|| anyhow!("missing TQ input"))?;
        let expected = general_config
            .policy
            .as_ref()
            .unwrap()
            .tq_snapshot_sha256
            .as_deref()
            .ok_or_else(|| anyhow!("missing pinned TQ SHA"))?;
        ensure!(
            sha256_hex(&fs::read(tq_path)?).eq_ignore_ascii_case(expected),
            "pinned TQ SHA mismatch"
        );
        let taxonomy_path = general_config
            .policy
            .as_ref()
            .unwrap()
            .ordinary_taxonomy_path
            .as_ref()
            .ok_or_else(|| anyhow!("missing ordinary taxonomy"))?;
        let taxonomy: Value = serde_json::from_slice(&fs::read(taxonomy_path)?)?;
        let ordinary: BTreeSet<u32> = taxonomy["types"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid ordinary taxonomy"))?
            .iter()
            .filter(|x| x["semanticClass"] == "ORDINARY_MARKET_COMPATIBLE")
            .filter_map(|x| x["typeID"].as_u64().map(|id| id as u32))
            .collect();
        let (static_dir, _) =
            resolve_static_data_dir(general_config.input.static_data_dir.as_deref());
        let sde_build = read_sde_build(&static_dir)
            .build
            .ok_or_else(|| anyhow!("missing SDE build"))?;
        let data = StaticData::load(&static_dir)?;
        let blueprints = crate::workbench_blueprints::Catalog::load(&data, sde_build)?;
        let structures = crate::workbench_structures::Catalog::load(
            &data,
            &blueprints.source_path,
            &blueprints.source_sha256,
        )?;
        let general = PolicyDocument::load(general_policy_path)?;
        let legacy = PolicyDocument::load(legacy_policy_path)?;
        general.validate_catalog_contract(sde_build)?;
        legacy.validate_catalog_contract(sde_build)?;
        let tq = TqSnapshot::load(tq_path)?;
        let classification = Classification::compute(&data, &legacy_config.junk_seed)?;
        let cached = crate::snapshot::find_cached_snapshot(&legacy_config.source.download_dir)?
            .ok_or_else(|| {
                anyhow!("LEGACY_V1 factual evidence needs cached market-order snapshot")
            })?;
        let overlay = crate::overlay::import_overlay(
            &cached.path,
            &data,
            legacy_config.import.order_filter,
            legacy_config.import.npc_order_duration_threshold_days,
            &BTreeSet::new(),
            legacy_config.price_manifest.reference_solar_system_id,
            true,
        )?;
        let lp_path = &legacy_config.policy.as_ref().unwrap().lp_offer_catalog_path;
        let direct_lp = load_lp_reward_type_ids(lp_path)?;
        let expanded_lp = expand_lp_manufacturing_products(&data, &direct_lp);
        let variants =
            crate::staticdata::VariantFamilies::load_for_generated_data(&data.dir, sde_build)?;
        let preset_references = crate::preset_references::References::from_static(
            &data,
            &classification,
            &overlay,
            &variants,
        );
        let inputs = FactInputs {
            data: &data,
            classification: &classification,
            direct_lp_rewards: &direct_lp,
            expanded_lp_rewards: &expanded_lp,
            npc_catalog_types: &overlay.npc_catalog_type_ids,
        };
        let mut facts: BTreeMap<u32, BTreeSet<String>> = data
            .item_types
            .iter()
            .filter(|i| {
                i.is_marketable()
                    || blueprints.items.contains_key(&i.type_id)
                    || structures.items.contains_key(&i.type_id)
            })
            .map(|i| (i.type_id, inputs.for_type(i.type_id)))
            .collect();
        let mut category_names = BTreeMap::new();
        for row in taxonomy["types"].as_array().unwrap() {
            if let Some(item) = row["typeID"]
                .as_u64()
                .and_then(|id| data.item_type(id as u32))
            {
                if let (Some(category), Some(name)) = (item.category_id, row["family"].as_str()) {
                    if let Some(previous) = category_names.insert(category, name.to_owned()) {
                        ensure!(previous == name, "category display-name conflict");
                    }
                }
            }
        }
        let mut quick_catalog = data
            .item_types
            .iter()
            .filter(|i| {
                i.is_marketable()
                    || blueprints.items.contains_key(&i.type_id)
                    || structures.items.contains_key(&i.type_id)
            })
            .map(|i| crate::policy_resolve::CatalogItem {
                type_id: i.type_id,
                group_id: i.group_id,
                category_id: i.category_id,
                facts: facts[&i.type_id].clone(),
            })
            .collect::<Vec<_>>();
        let mut quick_mapping: crate::workbench_quick::Mapping =
            serde_json::from_slice(&fs::read("config/workbench-friendly-groups-v1.json")?)?;
        let mut quick_scopes =
            crate::workbench_quick::scopes(&quick_mapping, &quick_catalog, &ordinary)?;
        let raw_market_groups = blueprints
            .source_path
            .parent()
            .unwrap()
            .join("marketGroups.jsonl");
        let families = crate::workbench_families::Catalog::load(
            &data,
            &ordinary,
            &quick_scopes,
            &raw_market_groups,
        )?;
        let known: BTreeSet<_> = quick_catalog.iter().map(|i| i.type_id).collect();
        for item in data
            .item_types
            .iter()
            .filter(|i| families.items.contains_key(&i.type_id) && !known.contains(&i.type_id))
        {
            let tags = inputs.for_type(item.type_id);
            facts.insert(item.type_id, tags.clone());
            quick_catalog.push(crate::policy_resolve::CatalogItem {
                type_id: item.type_id,
                group_id: item.group_id,
                category_id: item.category_id,
                facts: tags,
            });
        }
        quick_catalog.sort_by_key(|i| i.type_id);
        crate::workbench_quick::attach_blueprint_families(
            &mut quick_mapping,
            &mut quick_scopes,
            &blueprints,
        )?;
        crate::workbench_quick::attach_structure_scopes(
            &mut quick_mapping,
            &mut quick_scopes,
            &structures,
        )?;
        crate::workbench_quick::attach_economic_families(
            &mut quick_mapping,
            &mut quick_scopes,
            &families,
        )?;
        let storage = storage_path(storage_dir)?;
        for part in ["policies", "drafts", "backups", "candidates", "reports"] {
            fs::create_dir_all(storage.join(part))?;
        }
        let npc_inventory = crate::distribution_stations::Inventory::load(&data, sde_build)?;
        let mut input_files = Vec::new();
        fn gather(dir: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    gather(&entry.path(), paths)?;
                } else if entry.file_type()?.is_file() {
                    paths.push(entry.path());
                }
            }
            Ok(())
        }
        let mut paths = Vec::new();
        gather(&data.dir, &mut paths)?;
        paths.extend([
            PathBuf::from(GENERAL_CONFIG),
            PathBuf::from(LEGACY_CONFIG),
            PathBuf::from("config/workbench-friendly-groups-v1.json"),
            taxonomy_path.clone(),
            raw_market_groups.clone(),
            tq_path.clone(),
            legacy_config.price_manifest.path.clone(),
            lp_path.clone(),
            cached.path.clone(),
        ]);
        paths.sort();
        paths.dedup();
        let mut portable_dependencies = Vec::new();
        for path in paths {
            let sha = sha256_hex(&fs::read(&path)?);
            input_files.push(json!({"path":path,"sha256":sha}));
            portable_dependencies.push(
                crate::workbench_portable::VerifiedInput::remember_verified(&path, &sha)?,
            );
        }
        portable_dependencies.push(crate::workbench_portable::VerifiedInput::remember_verified(
            &blueprints.source_path,
            &blueprints.source_sha256,
        )?);
        for input in npc_inventory.provenance["files"]
            .as_array()
            .ok_or_else(|| anyhow!("Invalid NPC input inventory"))?
        {
            portable_dependencies.push(
                crate::workbench_portable::VerifiedInput::remember_verified(
                    Path::new(
                        input["path"]
                            .as_str()
                            .ok_or_else(|| anyhow!("Missing NPC input path"))?,
                    ),
                    input["sha256"]
                        .as_str()
                        .ok_or_else(|| anyhow!("Missing NPC input hash"))?,
                )?,
            );
        }
        let distribution_inputs = json!({"sde_build":sde_build,"files":input_files,"npc_inventory":npc_inventory.provenance,
            "algorithm":"distribution-v1-fnv1a-splitmix64-basis-points","builder_source_sha256":sha256_hex(concat!(include_str!("distribution.rs"),include_str!("distribution_build.rs"),include_str!("distribution_stations.rs"),include_str!("general_tq.rs"),include_str!("preset_references.rs")).as_bytes())});
        Ok(Self {
            install_targets,
            portable_dependencies,
            npc_inventory,
            distribution_inputs,
            storage,
            blueprints,
            structures,
            families,
            data,
            tq,
            preset_references,
            ordinary,
            general,
            legacy,
            general_config,
            sde_build,
            facts,
            category_names,
            quick_catalog,
            quick_mapping,
            quick_scopes,
            legacy_plan_cache: Mutex::new(BTreeMap::new()),
        })
    }

    fn catalog_items(&self) -> impl Iterator<Item = &crate::staticdata::ItemTypeRecord> {
        self.data.item_types.iter().filter(|i| {
            i.is_marketable()
                || self.blueprints.items.contains_key(&i.type_id)
                || self.structures.items.contains_key(&i.type_id)
                || self.families.items.contains_key(&i.type_id)
        })
    }
    fn ensure_configurable(&self, preview: &Value, preset: &str) -> Result<()> {
        let ids = self
            .blueprints
            .items
            .keys()
            .copied()
            .chain(
                self.structures
                    .items
                    .values()
                    .filter(|i| i.market_group_id.is_none())
                    .map(|i| i.type_id),
            )
            .chain(
                self.families
                    .items
                    .values()
                    .filter(|i| i.opt_in)
                    .map(|i| i.type_id),
            )
            .collect();
        ensure_preview_buildable(
            self,
            &crate::workbench_blueprints::configurable_preview(preview, &ids),
            preset,
        )
    }
    fn policy(&self, id: &str) -> Result<PolicyDocument> {
        match id {
            "GENERAL_TQ" => Ok(self.general.clone()),
            "LEGACY_V1" => Ok(self.legacy.clone()),
            _ => {
                validate_id(id)?;
                PolicyDocument::load(&self.storage.join("policies").join(format!("{id}.json")))
            }
        }
    }

    fn distribution(&self, id: &str) -> Result<crate::distribution::Document> {
        if id == "GENERAL_TQ" || id == "LEGACY_V1" {
            return Ok(crate::distribution::Document::default());
        }
        validate_id(id)?;
        let path = self
            .storage
            .join("policies")
            .join(format!("{id}.meta.json"));
        if !path.exists() {
            return Ok(crate::distribution::Document::default());
        }
        let meta: Value = serde_json::from_slice(&fs::read(path)?)?;
        if meta["distribution"].is_null() {
            Ok(crate::distribution::Document::default())
        } else {
            crate::distribution::Document::parse(&meta["distribution"])
        }
    }
    fn distribution_plan(
        &self,
        policy: &PolicyDocument,
        preset: &str,
        distribution: crate::distribution::Document,
    ) -> Result<(crate::distribution::Plan, Value)> {
        let preview = self.preview(policy, preset)?;
        self.ensure_configurable(&preview, preset)?;
        let quotes = crate::distribution::quotes_from_preview(&preview)?;
        ensure!(
            !quotes.is_empty(),
            "no resolved seedable items in Item Policy"
        );
        let mut provenance = json!({"inputs":self.distribution_inputs,"item_policy_sha256":sha256_hex(policy.canonical_json()?.as_bytes()),"preset":preset});
        if policy
            .rules
            .iter()
            .any(|r| r.id.starts_with("wb_blueprint_family_"))
            || preview["items"].as_array().unwrap().iter().any(|r| {
                !r["blueprint"].is_null()
                    && r["market_group_id"].is_null()
                    && !r["resolution"]["policy"].is_null()
            })
        {
            provenance["advanced_blueprints"] = json!({"catalog":self.blueprints.schema()["provenance"],"source_sha256":sha256_hex(include_str!("workbench_blueprints.rs").as_bytes())});
        }
        if preview["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| !r["structure"].is_null() && !r["resolution"]["policy"].is_null())
        {
            provenance["advanced_structures"] = self.structures.provenance.clone();
        }
        if preview["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| !r["economic_family"].is_null() && !r["resolution"]["policy"].is_null())
        {
            provenance["advanced_economic_families"] = self.families.provenance.clone();
        }
        Ok((
            crate::distribution::Plan::compile(
                distribution,
                quotes,
                &self.npc_inventory,
                &self.quick_scopes,
                provenance,
            )?,
            preview,
        ))
    }
    fn verify_distribution_inputs(&self) -> Result<()> {
        ensure!(
            crate::workbench_install::file_sha256(&self.blueprints.source_path)?
                == self.blueprints.source_sha256,
            "Blueprint SDE input changed; restart Workbench"
        );
        for files in [
            &self.distribution_inputs["files"],
            &self.npc_inventory.provenance["files"],
        ] {
            for file in files
                .as_array()
                .ok_or_else(|| anyhow!("invalid input provenance"))?
            {
                let path = file["path"]
                    .as_str()
                    .ok_or_else(|| anyhow!("missing pinned input path"))?;
                ensure!(
                    file["sha256"] == crate::workbench_install::file_sha256(Path::new(path))?,
                    "Distribution input changed: {path}; restart Workbench and preview again"
                );
            }
        }
        Ok(())
    }
    fn display_name(&self, id: &str) -> String {
        match id {
            "GENERAL_TQ" => "TQ-like Market".into(),
            "LEGACY_V1" => "Legacy Market".into(),
            _ => fs::read(
                self.storage
                    .join("policies")
                    .join(format!("{id}.meta.json")),
            )
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|v| v["display_name"].as_str().map(str::to_owned))
            .unwrap_or_else(|| id.into()),
        }
    }
    fn portable_metadata(&self, id: &str) -> Result<Value> {
        if id == "GENERAL_TQ" || id == "LEGACY_V1" {
            return Ok(json!({}));
        }
        validate_id(id)?;
        let path = self
            .storage
            .join("policies")
            .join(format!("{id}.meta.json"));
        if path.exists() {
            Ok(serde_json::from_slice(&fs::read(path)?)?)
        } else {
            Ok(json!({}))
        }
    }

    fn verify_portable_dependencies(&self) -> Result<()> {
        for input in &self.portable_dependencies {
            input.verify()?;
        }
        Ok(())
    }
    fn category_name(&self, id: Option<u32>) -> Option<&str> {
        id.and_then(|id| self.category_names.get(&id).map(String::as_str))
    }
    fn friendly_group(&self, id: u32) -> Option<&str> {
        self.quick_scopes
            .iter()
            .find(|s| s.parent.is_none() && s.members.contains(&id))
            .map(|s| s.id.as_str())
    }
    fn quick_state(
        &self,
        policy: &PolicyDocument,
        baseline: &PolicyDocument,
        preset: &str,
    ) -> Result<Value> {
        let mut state = crate::workbench_quick::inspect(
            policy,
            baseline,
            &self.quick_catalog,
            &self.quick_scopes,
            preset,
        )?;
        let unlisted: BTreeSet<_> = self
            .catalog_items()
            .filter(|i| i.market_group_id.is_none())
            .map(|i| i.type_id)
            .collect();
        for value in state["scopes"].as_array_mut().unwrap() {
            let scope = self
                .quick_scopes
                .iter()
                .find(|s| Some(s.id.as_str()) == value["id"].as_str())
                .unwrap();
            value["no_market_group"] = json!(scope.members.intersection(&unlisted).count());
        }
        Ok(state)
    }

    fn policy_preset(&self, id: &str) -> Result<String> {
        match id {
            "GENERAL_TQ" | "LEGACY_V1" => Ok(id.to_owned()),
            _ => {
                validate_id(id)?;
                let path = self
                    .storage
                    .join("policies")
                    .join(format!("{id}.meta.json"));
                if !path.exists() {
                    return Ok("GENERAL_TQ".into());
                }
                let value: Value = serde_json::from_slice(&fs::read(path)?)?;
                let preset = value["preset"]
                    .as_str()
                    .ok_or_else(|| anyhow!("saved policy metadata has no preset"))?;
                ensure!(
                    preset == "GENERAL_TQ" || preset == "LEGACY_V1",
                    "saved policy metadata has invalid preset"
                );
                Ok(preset.to_owned())
            }
        }
    }

    fn draft_policy(&self, input: &Value) -> Result<PolicyDocument> {
        let policy = if let Some(value) = input.get("policy") {
            PolicyDocument::parse(&serde_json::to_string(value)?)?
        } else if let Some(id) = input.get("policy_id").and_then(Value::as_str) {
            self.policy(id)?
        } else {
            bail!("provide policy or policy_id")
        };
        policy.validate_catalog_contract(self.sde_build)?;
        Ok(policy)
    }

    fn funded_for(
        &self,
        policy: &PolicyDocument,
    ) -> Result<BTreeMap<u32, std::result::Result<crate::funded::FundedPath, String>>> {
        let resolutions = self.resolutions_for(policy)?;
        general_tq::funded_costs_with_references(
            &self.data,
            &self.data.dir,
            &self.tq,
            &resolutions,
            &self.preset_references,
        )
    }

    fn resolutions_for(&self, policy: &PolicyDocument) -> Result<BTreeMap<u32, Resolution>> {
        let mut resolutions = BTreeMap::<u32, Resolution>::new();
        for item in self.catalog_items().filter(|i| {
            self.ordinary.contains(&i.type_id)
                || self.blueprints.items.contains_key(&i.type_id)
                || self.structures.items.contains_key(&i.type_id)
                || self.families.items.contains_key(&i.type_id)
        }) {
            resolutions.insert(
                item.type_id,
                policy.resolve_validated(
                    item.type_id,
                    item.group_id,
                    item.category_id,
                    self.facts.get(&item.type_id).expect("catalog facts"),
                )?,
            );
        }
        Ok(resolutions)
    }

    fn legacy_plan(&self, policy: &PolicyDocument) -> Result<Arc<Value>> {
        let canonical = policy.canonical_json()?;
        let key = sha256_hex(canonical.as_bytes());
        if let Some(cached) = self
            .legacy_plan_cache
            .lock()
            .map_err(|_| anyhow!("legacy plan cache unavailable"))?
            .get(&key)
        {
            return Ok(Arc::clone(cached));
        }
        let scratch = self
            .storage
            .join("drafts")
            .join(format!("preview-{}", unique_stamp()?));
        fs::create_dir(&scratch)?;
        let policy_path = scratch.join("policy.json");
        let config_path = scratch.join("build.toml");
        let plan_path = scratch.join("plan.json");
        let db_path = scratch.join("market.sqlite");
        let result = (|| -> Result<Value> {
            let mut pricing_policy = policy.clone();
            for rule in &mut pricing_policy.rules {
                if let Some(ids) = &mut rule.selector.type_ids {
                    ids.retain(|id| self.data.item_type(*id).is_some_and(|i| i.is_marketable()));
                }
            }
            pricing_policy.rules.retain(|r| {
                r.selector
                    .type_ids
                    .as_ref()
                    .is_none_or(|ids| !ids.is_empty())
            });
            let pricing_canonical = pricing_policy.canonical_json()?;
            fs::write(&policy_path, &pricing_canonical)?;
            let mut raw: toml::Value = toml::from_str(&fs::read_to_string(LEGACY_CONFIG)?)?;
            raw["policy"]["path"] = toml::Value::String(policy_path.display().to_string());
            raw["policy"]["plan_preview_path"] =
                toml::Value::String(plan_path.display().to_string());
            raw["policy"]
                .as_table_mut()
                .expect("policy table")
                .remove("parity_baseline_path");
            raw["policy"].as_table_mut().expect("policy table").insert(
                "allow_intentional_policy_delta".into(),
                toml::Value::Boolean(true),
            );
            raw["output"]["database_path"] = toml::Value::String(db_path.display().to_string());
            fs::write(&config_path, toml::to_string_pretty(&raw)?)?;
            let config = SeederConfig::load(&config_path)?;
            ensure!(
                config.output.database_path == db_path,
                "legacy preview output escaped workbench"
            );
            ensure!(
                config.policy.as_ref().is_some_and(|p| p.path == policy_path
                    && p.plan_preview_path.as_ref() == Some(&plan_path)
                    && p.allow_intentional_policy_delta
                    && p.parity_baseline_path.is_none()),
                "legacy preview config invalid"
            );
            crate::build::run_build(&config_path, true, true, false)?;
            ensure!(
                !db_path.exists() && !scratch.join("market.sqlite.building").exists(),
                "legacy dry preview unexpectedly wrote SQLite"
            );
            let plan: Value = serde_json::from_slice(&fs::read(&plan_path)?)?;
            ensure!(
                plan["policy_sha256"] == sha256_hex(pricing_canonical.as_bytes()),
                "legacy finalized plan policy SHA mismatch"
            );
            Ok(plan)
        })();
        for path in [&policy_path, &config_path, &plan_path] {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_dir(&scratch);
        let plan = Arc::new(result?);
        let mut cache = self
            .legacy_plan_cache
            .lock()
            .map_err(|_| anyhow!("legacy plan cache unavailable"))?;
        if cache.len() >= 3 {
            cache.clear();
        }
        cache.insert(key, Arc::clone(&plan));
        Ok(plan)
    }

    fn preview(&self, policy: &PolicyDocument, preset: &str) -> Result<Value> {
        ensure!(
            preset == "GENERAL_TQ" || preset == "LEGACY_V1",
            "unsupported preset"
        );
        let resolutions = self.resolutions_for(policy)?;
        let funded = if preset == "GENERAL_TQ" {
            self.funded_for(policy)?
        } else {
            BTreeMap::new()
        };
        let legacy_plan = if preset == "LEGACY_V1" {
            Some(self.legacy_plan(policy)?)
        } else {
            None
        };
        let legacy_entries = legacy_plan
            .as_ref()
            .and_then(|plan| plan["entries"].as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry["type_id"].as_u64().map(|id| (id as u32, entry)))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let legacy_drops = legacy_plan
            .as_ref()
            .and_then(|plan| plan["drops"].as_array())
            .map(|drops| {
                drops
                    .iter()
                    .filter_map(|drop| drop["type_id"].as_u64().map(|id| (id as u32, drop)))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let mut items = Vec::new();
        let mut matched = 0usize;
        let mut seeded = 0usize;
        let mut selected_count = 0usize;
        let mut excluded = 0usize;
        let mut sell = 0usize;
        let mut buy = 0usize;
        let mut both = 0usize;
        let mut unresolved = 0usize;
        let mut unresolved_selected = 0usize;
        let mut profile_usage = BTreeMap::<String, usize>::new();
        let mut source_usage = BTreeMap::<String, usize>::new();
        let mut rule_matches = BTreeMap::<String, usize>::new();
        let mut rule_wins = BTreeMap::<String, usize>::new();
        let mut states = BTreeMap::<String, usize>::new();
        for item in self.catalog_items() {
            if preset == "GENERAL_TQ"
                && !self.ordinary.contains(&item.type_id)
                && !self.blueprints.items.contains_key(&item.type_id)
                && !self.structures.items.contains_key(&item.type_id)
                && !self.families.items.contains_key(&item.type_id)
            {
                continue;
            }
            let resolution = policy.resolve_validated(
                item.type_id,
                item.group_id,
                item.category_id,
                self.facts.get(&item.type_id).expect("catalog facts"),
            )?;
            if preset == "GENERAL_TQ"
                && !self.ordinary.contains(&item.type_id)
                && self.families.items.contains_key(&item.type_id)
                && !self.blueprints.items.contains_key(&item.type_id)
                && !self.structures.items.contains_key(&item.type_id)
                && resolution.winner_rule_id.is_none()
            {
                continue;
            }
            if !item.is_marketable() && resolution.winner_rule_id.is_none() {
                continue;
            }
            if resolution.winner_rule_id.is_some() {
                matched += 1;
            }
            if let Some(ref winner) = resolution.winner_rule_id {
                *rule_wins.entry(winner.clone()).or_default() += 1;
            }
            for rule in &resolution.matched_rules {
                *rule_matches.entry(rule.rule_id.clone()).or_default() += 1;
            }
            let evidence = self.tq.records.get(&item.type_id);
            if let Some(record) = evidence {
                *states.entry(record.state.clone()).or_default() += 1;
            }
            let mut ask = None;
            let mut bid = None;
            let mut problem = None;
            if let Some(ref selected) = resolution.policy {
                selected_count += 1;
                if let Some(ref p) = selected.profile_id {
                    *profile_usage.entry(p.clone()).or_default() += 1;
                }
                if selected.sides.has_sell() {
                    sell += 1;
                }
                if selected.sides.has_buy() {
                    buy += 1;
                }
                if selected.sides.has_sell() && selected.sides.has_buy() {
                    both += 1;
                }
                if preset == "GENERAL_TQ" {
                    let _funded_cost = match funded.get(&item.type_id) {
                        Some(Ok(path)) => Some(path.unit_cost),
                        Some(Err(reason)) => {
                            problem = Some(format!("funded_cost unavailable: {reason}"));
                            None
                        }
                        None => None,
                    };
                    if let Some(ref side) = selected.sell {
                        *source_usage
                            .entry(format!("{:?}", side.source))
                            .or_default() += 1;
                        match self.preset_references.price(
                            &self.tq,
                            item.type_id,
                            Side::Sell,
                            side,
                            &funded,
                            &resolutions,
                        ) {
                            Ok(v) => ask = Some(v),
                            Err(e) => problem = Some(e.to_string()),
                        }
                    }
                    if let Some(ref side) = selected.buy {
                        *source_usage
                            .entry(format!("{:?}", side.source))
                            .or_default() += 1;
                        match self.preset_references.price(
                            &self.tq,
                            item.type_id,
                            Side::Buy,
                            side,
                            &funded,
                            &resolutions,
                        ) {
                            Ok(v) => bid = Some(v),
                            Err(e) => problem = Some(e.to_string()),
                        }
                    }
                    if let Some(Err(reason)) = funded.get(&item.type_id) {
                        problem = Some(format!("funded_cost unavailable: {reason}"));
                    }
                } else if let Some(entry) = legacy_entries.get(&item.type_id) {
                    ask = entry["final_sell_price"].as_f64();
                    bid = entry["final_buy_price"].as_f64();
                    for side in ["sell_reference", "buy_reference"] {
                        if let Some(authority) = entry[side]["authority"].as_str() {
                            *source_usage.entry(authority.to_owned()).or_default() += 1;
                        }
                    }
                } else {
                    problem = Some(
                        legacy_drops
                            .get(&item.type_id)
                            .and_then(|drop| drop["reason"].as_str())
                            .unwrap_or("not in finalized seed plan")
                            .to_owned(),
                    );
                }
                if problem.is_some() {
                    unresolved += 1;
                    unresolved_selected += 1;
                } else {
                    seeded += 1;
                }
            } else if resolution.exclusion_reason.as_deref() == Some("explicit_unseeded") {
                excluded += 1;
                if preset == "GENERAL_TQ" {
                    problem = if resolution.winner_rule_id.as_deref() == Some("rounding_cross") {
                        Some("ROUNDING_CROSS".into())
                    } else {
                        evidence.and_then(|r| {
                            if matches!(r.state.as_str(), "NO_TQ_REFERENCE") {
                                Some(r.state.clone())
                            } else {
                                None
                            }
                        })
                    };
                    if problem.is_some() {
                        unresolved += 1;
                    }
                }
            }
            items.push(json!({
                "type_id": item.type_id, "name": item.name, "group_id": item.group_id,
                "group_name": item.group_name, "category_id": item.category_id,
                "category_name":self.category_name(item.category_id),"friendly_group":self.friendly_group(item.type_id),"facts":self.facts.get(&item.type_id),
                "market_group_id": item.market_group_id, "ordinary": self.ordinary.contains(&item.type_id),
                "blueprint":self.blueprints.items.get(&item.type_id),
                "structure":self.structures.items.get(&item.type_id),
                "economic_family":self.families.items.get(&item.type_id),
                "quick_scope_ids":self.quick_scopes.iter().filter(|s|s.members.contains(&item.type_id)).map(|s|s.id.as_str()).collect::<Vec<_>>(),
                "resolution": resolution, "sell_price": ask, "buy_price": bid,
                "unresolved_reason": problem, "warnings": quote_warnings(ask, bid),
                "quantity":if preset == "LEGACY_V1" {legacy_entries.get(&item.type_id).map(|entry|&entry["quantity_per_side_per_hub"])} else {None},
                "price_provenance":self.preset_references.trace_with_quotes(item.type_id, &resolution, &self.tq, &funded, &resolutions).or_else(||legacy_entries.get(&item.type_id).map(|entry|json!({"sell_reference":entry["sell_reference"],"buy_reference":entry["buy_reference"],"safety_obligations":entry["safety_obligations"]}))),
                "funded":funded.get(&item.type_id).and_then(|value|value.as_ref().ok()).map(funded_trace),
                "tq": evidence.map(|e| json!({"state":e.state,"sell_reference":e.sell_reference,"buy_reference":e.buy_reference,
                    "average_price":e.average_price_fallback_candidate,"captured_at":e.captured_at,"aggregation":e.aggregation,"warnings":e.warnings,"hubs":e.hubs,"history_blend":e.history_blend})),
            }));
        }
        let considered = items.len();
        let unmatched = considered.saturating_sub(selected_count + excluded);
        let finalized_sell = legacy_plan
            .as_ref()
            .and_then(|plan| plan["sell_count_per_hub"].as_u64())
            .map(|x| x as usize)
            .unwrap_or(sell);
        let finalized_buy = legacy_plan
            .as_ref()
            .and_then(|plan| plan["buy_count_per_hub"].as_u64())
            .map(|x| x as usize)
            .unwrap_or(buy);
        let finalized_both = if preset == "LEGACY_V1" {
            legacy_entries
                .values()
                .filter(|entry| {
                    !entry["final_sell_price"].is_null() && !entry["final_buy_price"].is_null()
                })
                .count()
        } else {
            both
        };
        let catalog_ids = self
            .catalog_items()
            .map(|i| i.type_id)
            .collect::<BTreeSet<_>>();
        let mut rule_coverage = Vec::new();
        for rule in &policy.rules {
            if let Some(ids) = &rule.selector.type_ids {
                let missing = ids
                    .iter()
                    .filter(|id| !catalog_ids.contains(id))
                    .collect::<Vec<_>>();
                ensure!(
                    missing.is_empty(),
                    "rule {} contains unreachable type IDs {:?}",
                    rule.id,
                    missing
                );
            }
            let match_count = *rule_matches.get(&rule.id).unwrap_or(&0);
            ensure!(match_count > 0, "rule {} has no catalog match", rule.id);
            rule_coverage.push(json!({"rule_id":rule.id,"match_count":match_count,"winning_count":rule_wins.get(&rule.id).copied().unwrap_or(0)}));
        }
        Ok(
            json!({"summary":{"preset":preset,"marketable_types_considered":considered,"matched":matched,"seeded":seeded,
            "excluded":excluded,"unmatched":unmatched,"selected":selected_count,
            "sell":finalized_sell,"buy":finalized_buy,"both_sides":finalized_both,"unresolved":unresolved,"unresolved_selected":unresolved_selected,
            "profile_usage":profile_usage,"price_source_usage":source_usage,"rule_match_counts":rule_matches,"rule_coverage":rule_coverage,
            "warnings":items.iter().filter(|i| !i["warnings"].as_array().unwrap().is_empty()).count(),"buy_ge_sell_warnings":items.iter().filter(|i| !i["warnings"].as_array().unwrap().is_empty()).count(),"tq_reference_states":states,"price_resolution_status":"computed_from_core"},
            "blueprint_warnings":items.iter().filter(|i| !i["resolution"]["policy"].is_null() && !i["blueprint"].is_null()).map(|i|json!({"type_id":i["type_id"],"family":i["blueprint"]["family"],"warnings":i["blueprint"]["warnings"],"unresolved_reason":i["unresolved_reason"]})).collect::<Vec<_>>(),
            "items":items}),
        )
    }
}

fn quote_warnings(sell: Option<f64>, buy: Option<f64>) -> Vec<&'static str> {
    if matches!((sell, buy), (Some(a), Some(b)) if b >= a) {
        vec!["BUY_AT_OR_ABOVE_SELL"]
    } else {
        vec![]
    }
}

fn ensure_preview_buildable(app: &App, preview: &Value, preset: &str) -> Result<()> {
    let unresolved = preview["items"]
        .as_array()
        .ok_or_else(|| anyhow!("preview items missing"))?
        .iter()
        .filter(|item| {
            !item["resolution"]["policy"].is_null() && !item["unresolved_reason"].is_null()
        })
        .filter_map(|item| item["type_id"].as_u64().map(|id| id as u32))
        .collect::<BTreeSet<_>>();
    if preset == "GENERAL_TQ" {
        ensure!(
            unresolved.is_empty(),
            "{} selected types have unresolved prices",
            unresolved.len()
        );
        ensure!(
            preview["summary"]["unmatched"].as_u64() == Some(0),
            "ordinary types lack a policy rule or explicit exclusion"
        );
    } else if preset == "LEGACY_V1" {
        let baseline = app.legacy_plan(&app.policy("LEGACY_V1")?)?;
        let known = baseline["drops"]
            .as_array()
            .ok_or_else(|| anyhow!("legacy baseline drops missing"))?
            .iter()
            .filter_map(|drop| drop["type_id"].as_u64().map(|id| id as u32))
            .collect::<BTreeSet<_>>();
        let new_drops = unresolved.difference(&known).copied().collect::<Vec<_>>();
        ensure!(
            new_drops.is_empty(),
            "new unresolved legacy types: {:?}",
            new_drops
        );
    } else {
        bail!("unsupported preset");
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "ID must contain only letters, digits, hyphen, underscore (max 64)"
    );
    ensure!(
        id != "GENERAL_TQ" && id != "LEGACY_V1",
        "shipped preset cannot be overwritten"
    );
    Ok(())
}

fn handle(app: &App, stream: &mut TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end;
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        ensure!(buf.len() <= MAX_BODY + 16384, "request too large");
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = i + 4;
            break;
        }
    }
    let header = std::str::from_utf8(&buf[..header_end])?.to_owned();
    let mut lines = header.lines();
    let first = lines.next().ok_or_else(|| anyhow!("empty request"))?;
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let host = headers.get("host").map(String::as_str).unwrap_or("");
    ensure!(
        host.starts_with("127.0.0.1:") || host.starts_with("localhost:"),
        "Host must be loopback"
    );
    if let Some(origin) = headers.get("origin") {
        ensure!(
            origin == &format!("http://{host}"),
            "cross-origin request rejected"
        );
    }
    if method == "POST" {
        ensure!(
            headers
                .get("content-type")
                .is_some_and(|v| v.starts_with("application/json")),
            "POST requires application/json"
        );
    }
    ensure!(content_length <= MAX_BODY, "request body too large");
    while buf.len() - header_end < content_length {
        let n = stream.read(&mut chunk)?;
        ensure!(n > 0, "truncated body");
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = if content_length > 0 {
        serde_json::from_slice(&buf[header_end..header_end + content_length])?
    } else {
        Value::Null
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if method == "GET"
        && let Some((file, mime)) = static_asset(path)
    {
        let content = fs::read(Path::new("workbench-ui").join(file))?;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
            content.len()
        )?;
        stream.write_all(&content)?;
        return Ok(());
    }
    let result = route(app, method, path, query, &body);
    let (status, value) = match result {
        Ok(v) => (200, v),
        Err(e) => (400, json!({"error":format!("{e:#}")})),
    };
    let payload = serde_json::to_vec(&value)?;
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        if status == 200 { "OK" } else { "Bad Request" },
        payload.len()
    )?;
    stream.write_all(&payload)?;
    Ok(())
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
}
fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::new();
    let mut it = input.as_bytes().iter().copied();
    while let Some(b) = it.next() {
        if b == b'%' {
            if let (Some(a), Some(c)) = (it.next(), it.next()) {
                let pair = [a, c];
                if let Ok(hex) = std::str::from_utf8(&pair) {
                    if let Ok(v) = u8::from_str_radix(hex, 16) {
                        bytes.push(v);
                        continue;
                    }
                }
                bytes.extend_from_slice(&[b, a, c]);
            } else {
                bytes.push(b);
            }
        } else {
            bytes.push(if b == b'+' { b' ' } else { b });
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn route(app: &App, method: &str, path: &str, query: &str, body: &Value) -> Result<Value> {
    if method == "GET" && path == "/api/v1/distribution/schema" {
        let regions: BTreeMap<_, _> = app
            .npc_inventory
            .stations
            .values()
            .map(|s| (s.region_id, s.region_name.clone()))
            .collect();
        return Ok(
            json!({"regions":regions.iter().map(|(id,name)|json!({"id":id,"name":name})).collect::<Vec<_>>(),"default":crate::distribution::Document::default(),"eligible_npc_stations":app.npc_inventory.stations.len(),
            "topology_available":app.npc_inventory.topology_available,"canonical_hubs":crate::config::CANONICAL_HUB_STATION_IDS.iter().map(|id|&app.npc_inventory.stations[id]).collect::<Vec<_>>(),
            "scopes":app.quick_scopes.iter().map(|s|json!({"id":s.id,"label":s.label,"parent":s.parent,"count":s.members.len()})).collect::<Vec<_>>(),"provenance":app.npc_inventory.provenance}),
        );
    }
    if method == "GET" && path == "/api/v1/distribution/stations" {
        let q = query_param(query, "q").unwrap_or_default().to_lowercase();
        let region = query_param(query, "region").and_then(|r| r.parse::<u32>().ok());
        let limit = query_param(query, "limit")
            .and_then(|r| r.parse::<usize>().ok())
            .unwrap_or(50)
            .clamp(1, 200);
        let stations: Vec<_> = app
            .npc_inventory
            .stations
            .values()
            .filter(|s| !crate::config::CANONICAL_HUB_STATION_IDS.contains(&s.station_id))
            .filter(|s| region.is_none_or(|r| s.region_id == r))
            .filter(|s| {
                q.is_empty()
                    || s.name.to_lowercase().contains(&q)
                    || s.region_name.to_lowercase().contains(&q)
            })
            .take(limit)
            .collect();
        return Ok(json!({"stations":stations}));
    }
    if method == "POST" && path == "/api/v1/distribution/preview" {
        let policy = app.draft_policy(body)?;
        let doc = crate::distribution::Document::parse(&body["distribution"])?;
        let preset = body["preset"].as_str().unwrap_or("GENERAL_TQ");
        let (plan, item_preview) = app.distribution_plan(&policy, preset, doc)?;
        let mut result = plan.preview(app.npc_inventory.stations.len())?;
        result["item_summary"] = item_preview["summary"].clone();
        result["item_policy_buildable"] =
            json!(ensure_preview_buildable(app, &item_preview, preset).is_ok());
        if result["item_policy_buildable"] == false {
            result["warnings"].as_array_mut().unwrap().push(json!("Configuration preview only: selected blueprint prices are unresolved. Save is allowed; market database build remains blocked until prices resolve."));
        }
        return Ok(result);
    }
    if method == "POST" && path == "/api/v1/quick-setup/item" {
        let mut policy = app.draft_policy(body)?;
        let preset = body["preset"].as_str().unwrap_or("GENERAL_TQ");
        ensure!(
            ["GENERAL_TQ", "LEGACY_V1"].contains(&preset),
            "unsupported base preset"
        );
        let baseline = app.policy(preset)?;
        let settings: crate::workbench_quick::Settings =
            serde_json::from_value(body["settings"].clone())?;
        let id = body["type_id"]
            .as_u64()
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| anyhow!("invalid item"))?;
        ensure!(
            app.ordinary.contains(&id),
            "Simple item edits support ordinary-compatible items only"
        );
        let item = app
            .data
            .item_type(id)
            .ok_or_else(|| anyhow!("unknown item"))?;
        let source = if settings.source == "tq" {
            &app.general
        } else {
            &baseline
        };
        let selected = source
            .resolve_validated(id, item.group_id, item.category_id, &app.facts[&id])?
            .policy
            .unwrap_or(crate::policy_resolve::ResolvedItemPolicy {
                profile_id: None,
                sides: crate::policy::Sides::Unseeded,
                sell: None,
                buy: None,
            });
        let priority = policy
            .rules
            .iter()
            .map(|r| r.priority)
            .max()
            .unwrap_or(0)
            .checked_add(20)
            .ok_or_else(|| anyhow!("priority exhausted"))?;
        let rule = crate::workbench_quick::exact_rule(id, priority, &selected, &settings)?;
        policy.rules.retain(|r| r.id != rule.id);
        policy.rules.push(rule);
        policy.validate()?;
        let result = crate::workbench_quick::apply(
            &policy,
            &baseline,
            &app.general,
            &app.quick_catalog,
            &app.quick_scopes,
            preset,
            None,
            None,
        )?;
        return Ok(
            json!({"policy":result,"mapping":app.quick_mapping,"state":app.quick_state(&result,&baseline,preset)?}),
        );
    }
    if method == "POST"
        && [
            "/api/v1/quick-setup/inspect",
            "/api/v1/quick-setup/apply",
            "/api/v1/quick-setup/sync",
        ]
        .contains(&path)
    {
        let policy = app.draft_policy(body)?;
        let preset = body["preset"].as_str().unwrap_or("GENERAL_TQ");
        ensure!(
            ["GENERAL_TQ", "LEGACY_V1"].contains(&preset),
            "unsupported base preset"
        );
        let baseline = app.policy(preset)?;
        let result = if path.ends_with("inspect") {
            policy
        } else {
            let scope = if path.ends_with("sync") {
                None
            } else {
                Some(
                    body["scope_id"]
                        .as_str()
                        .ok_or_else(|| anyhow!("missing friendly scope"))?,
                )
            };
            let settings = body
                .get("settings")
                .filter(|v| !v.is_null())
                .map(|v| serde_json::from_value::<crate::workbench_quick::Settings>(v.clone()))
                .transpose()?;
            crate::workbench_quick::apply(
                &policy,
                &baseline,
                &app.general,
                &app.quick_catalog,
                &app.quick_scopes,
                preset,
                scope,
                settings,
            )?
        };
        let state = app.quick_state(&result, &baseline, preset)?;
        return Ok(json!({"policy":result,"mapping":app.quick_mapping,"state":state}));
    }
    if method == "GET" && path == "/api/v1/health" {
        return Ok(
            json!({"ok":true,"bind":"127.0.0.1","storage":app.storage,"workbench_version":"1.3",
                "tq_snapshot":{"capture_id":app.tq.capture_id,"captured_at":app.tq.captured_at,"aggregation":app.tq.aggregation}}),
        );
    }
    if method == "POST" && path == "/api/v1/presets/export" {
        let policy = app.draft_policy(body)?;
        let distribution = crate::distribution::Document::parse(&body["distribution"])?;
        let package = crate::workbench_portable::Preset::new(
            crate::workbench_portable::Metadata {
                name: body["name"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Missing preset name"))?
                    .into(),
                base_preset: body["preset"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Missing base preset"))?
                    .into(),
                description: crate::workbench_portable::optional_text(
                    body["description"].as_str(),
                    2000,
                    "Description",
                )?,
                author: crate::workbench_portable::optional_text(
                    body["author"].as_str(),
                    160,
                    "Author",
                )?,
            },
            policy,
            distribution,
        )?;
        let (package, summary) = prepare_portable(app, package)?;
        let text = package.canonical_json()?;
        return Ok(
            json!({"filename":"market-workbench-preset.json","json":text,"sha256":sha256_hex(text.as_bytes()),"summary":summary}),
        );
    }
    if method == "POST"
        && (path == "/api/v1/presets/import/preview" || path == "/api/v1/presets/import")
    {
        let text = body["json"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing portable preset JSON text"))?;
        let package = crate::workbench_portable::Preset::parse(text)?;
        let (package, summary) = prepare_portable(app, package)?;
        let hash = sha256_hex(package.canonical_json()?.as_bytes());
        if path.ends_with("/preview") {
            return Ok(json!({"valid":true,"sha256":hash,"summary":summary}));
        }
        ensure!(
            body["preview_sha256"].as_str() == Some(hash.as_str()),
            "Preset changed since import preview; review the summary again"
        );
        // Browser-provided identifiers/paths are never used. Import always creates a fresh copy.
        let id = format!("imported-{}", unique_stamp()?);
        let mut saved = save_policy(
            app,
            &json!({"id":id,"preset":package.metadata.base_preset,
            "display_name":package.metadata.name,"description":package.metadata.description,"author":package.metadata.author,
            "policy":package.item_policy,"distribution":package.distribution_policy,"overwrite":false}),
        )?;
        saved["summary"] = summary;
        saved["portable_sha256"] = json!(hash);
        return Ok(saved);
    }
    if method == "GET" && path == "/api/v1/presets" {
        return Ok(
            json!({"presets":[{"id":"LEGACY_V1","label":"LEGACY_V1","editable":false},{"id":"GENERAL_TQ","label":"GENERAL_TQ","editable":false}]}),
        );
    }
    if method == "GET" && path == "/api/v1/blueprints/schema" {
        return Ok(app.blueprints.schema());
    }
    if method == "GET" && path == "/api/v1/economic-families/schema" {
        return Ok(app.families.schema());
    }
    if method == "GET" && path == "/api/v1/structures/schema" {
        return Ok(app.structures.schema());
    }
    if method == "POST" && path == "/api/v1/blueprints/apply" {
        let policy = app.draft_policy(body)?;
        let family = body["family"]
            .as_str()
            .ok_or_else(|| anyhow!("missing Blueprint family"))?;
        let policy = app.blueprints.apply(&policy, family, &body["settings"])?;
        return Ok(
            json!({"policy":policy,"family":family,"matched":app.blueprints.ids(family)?.len(),"warnings":[crate::workbench_blueprints::SEMANTICS_WARNING]}),
        );
    }
    if method == "POST" && path == "/api/v1/blueprints/catalog" {
        return blueprint_catalog(app, body);
    }
    if method == "GET" && path == "/api/v1/policies/schema" {
        let sources = [
            Source::TqSnapshot,
            Source::TqSnapshotSellFallback,
            Source::TqSnapshotBuyFallback,
            Source::TqAveragePrice,
            Source::FundedCost,
            Source::CoreManifestCost,
            Source::CapturedMarket,
            Source::NpcMinSell,
            Source::BpoNpcOrBase,
            Source::RareReference,
            Source::SkillbookLadder,
            Source::CommandCenterLadder,
        ];
        return Ok(
            json!({"selector_fields":["type_ids","group_ids","category_ids","fact"],"facts":FACTS,
            "sides":["unseeded","sell_only","buy_only","buy_sell"],"sources":sources,
            "general_tq_price_sources":["tq_snapshot","tq_snapshot_sell_fallback","tq_snapshot_buy_fallback","tq_average_price","funded_cost","npc_acquisition","t1_variant_sell","t1_variant_buy"]}),
        );
    }
    if method == "GET" && path == "/api/v1/policies" {
        let mut policies = vec![
            json!({"id":"LEGACY_V1","preset":true}),
            json!({"id":"GENERAL_TQ","preset":true}),
        ];
        for entry in fs::read_dir(app.storage.join("policies"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(id) = entry.path().file_stem().and_then(|s| s.to_str()) {
                    if validate_id(id).is_ok() {
                        policies.push(
                            json!({"id":id,"display_name":app.display_name(id),"preset":false,"base_preset":app.policy_preset(id)?}),
                        );
                    }
                }
            }
        }
        return Ok(json!({"policies":policies}));
    }
    if method == "GET" && path == "/api/v1/catalog" {
        return catalog(app, query);
    }
    if method == "GET" && path == "/api/v1/catalog/tree" {
        return catalog_tree(app);
    }
    if method == "GET" && path.starts_with("/api/v1/catalog/") {
        let id: u32 = path.trim_start_matches("/api/v1/catalog/").parse()?;
        return type_detail(
            app,
            id,
            query_param(query, "preset")
                .as_deref()
                .unwrap_or("GENERAL_TQ"),
        );
    }
    if method == "GET" && path.starts_with("/api/v1/policies/") {
        let id = path.trim_start_matches("/api/v1/policies/");
        let metadata = app.portable_metadata(id)?;
        return Ok(
            json!({"id":id,"display_name":app.display_name(id),"description":metadata["description"],"author":metadata["author"],"preset":app.policy_preset(id)?,"policy":app.policy(id)?,"distribution":app.distribution(id)?}),
        );
    }
    if method == "POST" && path == "/api/v1/validate" {
        let preset = body
            .get("preset")
            .and_then(Value::as_str)
            .unwrap_or("GENERAL_TQ");
        return Ok(
            match app.draft_policy(body).and_then(|p| {
                let preview = app.preview(&p, preset)?;
                app.ensure_configurable(&preview, preset)?;
                Ok((p, preview))
            }) {
                Ok((p, preview)) => {
                    let summary = preview["summary"].clone();
                    let mut warnings = Vec::new();
                    if !preview["blueprint_warnings"].as_array().unwrap().is_empty() {
                        warnings.push(crate::workbench_blueprints::SEMANTICS_WARNING.into());
                    }
                    if preview["items"].as_array().unwrap().iter().any(|i| {
                        !i["blueprint"].is_null()
                            && !i["resolution"]["policy"].is_null()
                            && !i["unresolved_reason"].is_null()
                    }) {
                        warnings.push("Selected blueprint prices are unresolved. The configuration can be saved; building a market database requires every selected price to resolve.".into());
                    }
                    if preview["items"].as_array().unwrap().iter().any(|i| {
                        !i["structure"].is_null()
                            && !i["market_group_id"].is_number()
                            && !i["resolution"]["policy"].is_null()
                    }) {
                        warnings.push(crate::workbench_structures::WARNING.into());
                    }
                    if preview["items"].as_array().unwrap().iter().any(|i| {
                        i["economic_family"]["opt_in"] == true
                            && !i["resolution"]["policy"].is_null()
                    }) {
                        warnings.push(crate::workbench_families::WARNING.into());
                    }
                    if preset == "GENERAL_TQ" && summary["unresolved"].as_u64().unwrap_or(0) > 0 {
                        warnings.push(format!(
                            "{} excluded types need review",
                            summary["unresolved"]
                        ));
                    }
                    if summary["buy_ge_sell_warnings"].as_u64().unwrap_or(0) > 0 {
                        warnings.push(format!("{} item quotes have Buy >= Sell. Quotes retained as configured; building is allowed.", summary["buy_ge_sell_warnings"]));
                    }
                    if preset == "LEGACY_V1"
                        && summary["unresolved_selected"].as_u64().unwrap_or(0) > 0
                    {
                        warnings.push(format!("{} selected types were dropped by the finalized legacy seed plan; review the preview",summary["unresolved_selected"]));
                    }
                    if let Some(rules) = summary["rule_coverage"].as_array() {
                        for rule in rules {
                            if rule["match_count"].as_u64().unwrap_or(0) > 1000 {
                                warnings.push(format!(
                                    "rule {} affects {} types",
                                    rule["rule_id"], rule["match_count"]
                                ));
                            }
                        }
                    }
                    json!({"valid":true,"errors":[],"warnings":warnings,"summary":summary,"policy":p})
                }
                Err(e) => json!({"valid":false,"errors":[format!("{e:#}")],"warnings":[]}),
            },
        );
    }
    if method == "POST" && path == "/api/v1/preview" {
        let policy = app.draft_policy(body)?;
        let preset = body
            .get("preset")
            .and_then(Value::as_str)
            .unwrap_or("GENERAL_TQ");
        return app.preview(&policy, preset);
    }
    if method == "POST" && path == "/api/v1/compare" {
        let policy = app.draft_policy(body)?;
        let baseline = body
            .get("baseline")
            .and_then(Value::as_str)
            .unwrap_or("GENERAL_TQ");
        let current = app.preview(&policy, baseline)?;
        let old = app.preview(&app.policy(baseline)?, baseline)?;
        let mut changes = Vec::new();
        let (
            mut added,
            mut removed,
            mut side_changes,
            mut price_changes,
            mut profile_changes,
            mut unresolved_changes,
        ) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        let before: BTreeMap<_, _> = old["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| (i["type_id"].as_u64().unwrap(), i))
            .collect();
        let after: BTreeMap<_, _> = current["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| (i["type_id"].as_u64().unwrap(), i))
            .collect();
        let ids: BTreeSet<_> = before.keys().chain(after.keys()).copied().collect();
        let absent = Value::Null;
        for id in ids {
            let a = after.get(&id).copied().unwrap_or(&absent);
            let b = before.get(&id).copied().unwrap_or(&absent);
            if a["resolution"]["policy"] != b["resolution"]["policy"]
                || a["sell_price"] != b["sell_price"]
                || a["buy_price"] != b["buy_price"]
                || a["unresolved_reason"] != b["unresolved_reason"]
            {
                let after_policy = &a["resolution"]["policy"];
                let before_policy = &b["resolution"]["policy"];
                if before_policy.is_null() && !after_policy.is_null() {
                    added += 1;
                }
                if !before_policy.is_null() && after_policy.is_null() {
                    removed += 1;
                }
                if after_policy["sides"] != before_policy["sides"] {
                    side_changes += 1;
                }
                if a["sell_price"] != b["sell_price"] || a["buy_price"] != b["buy_price"] {
                    price_changes += 1;
                }
                if after_policy["profile_id"] != before_policy["profile_id"] {
                    profile_changes += 1;
                }
                if a["unresolved_reason"] != b["unresolved_reason"] {
                    unresolved_changes += 1;
                }
                let delta = |key: &str| -> Value {
                    match (a[key].as_f64(), b[key].as_f64()) {
                        (Some(new), Some(base)) => {
                            json!({"absolute":new-base,"percent":if base != 0.0 {Some((new/base-1.0)*100.0)} else {None}})
                        }
                        _ => Value::Null,
                    }
                };
                changes.push(json!({"type_id":id,"name":if a.is_null(){&b["name"]}else{&a["name"]},"before":b,"after":a,"sell_delta":delta("sell_price"),"buy_delta":delta("buy_price"),
                    "added":before_policy.is_null() && !after_policy.is_null(),"removed":!before_policy.is_null() && after_policy.is_null(),
                    "side_changed":after_policy["sides"] != before_policy["sides"],"profile_changed":after_policy["profile_id"] != before_policy["profile_id"]}));
            }
        }
        return Ok(
            json!({"summary":{"baseline":baseline,"changed":changes.len(),"added":added,"removed":removed,"side_changes":side_changes,
                "price_changes":price_changes,"profile_changes":profile_changes,"unresolved_changes":unresolved_changes},"changes":changes}),
        );
    }
    if method == "POST" && path == "/api/v1/save" {
        return save_policy(app, body);
    }
    if method == "GET" && path == "/api/v1/candidates" {
        return list_candidates(app);
    }
    if method == "GET" && path == "/api/v1/installation/targets" {
        return Ok(app.install_targets.list());
    }
    if method == "POST" && path == "/api/v1/candidates/build" {
        return build_candidate(app, body);
    }
    if path.starts_with("/api/v1/candidates/") {
        let tail = path.trim_start_matches("/api/v1/candidates/");
        if method == "POST" && tail.ends_with("/install-preview") {
            let id = tail.trim_end_matches("/install-preview");
            let request: crate::workbench_install::PreviewRequest =
                serde_json::from_value(body.clone())?;
            // The existing core audits the actual persisted candidate before offering installation.
            let audit = audit_candidate(app, id)?;
            ensure!(
                audit["passed"] == true,
                "Candidate audit failed; installation is blocked. Open the audit report."
            );
            return app
                .install_targets
                .preview(&candidate_dir(app, id)?, id, &request.target_id);
        }
        if method == "POST" && tail.ends_with("/install") {
            let id = tail.trim_end_matches("/install");
            let request: crate::workbench_install::InstallRequest =
                serde_json::from_value(body.clone())?;
            return app
                .install_targets
                .install(&candidate_dir(app, id)?, id, &request);
        }
        if method == "POST" && tail.ends_with("/finalize") {
            return finalize_candidate(app, tail.trim_end_matches("/finalize"), None);
        }
        if method == "POST" && tail.ends_with("/audit") {
            return audit_candidate(app, tail.trim_end_matches("/audit"));
        }
        if method == "GET" {
            return candidate_detail(app, tail);
        }
    }
    bail!("unknown API route")
}

fn blueprint_catalog(app: &App, body: &Value) -> Result<Value> {
    let policy = app.draft_policy(body)?;
    let preset = body["preset"].as_str().unwrap_or("GENERAL_TQ");
    let family = body["family"].as_str().unwrap_or("");
    if !family.is_empty() {
        app.blueprints.ids(family)?;
    }
    let query = body["q"].as_str().unwrap_or("").to_lowercase();
    let limit = body["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
    let offset = body["offset"].as_u64().unwrap_or(0) as usize;
    let preview = app.preview(&policy, preset)?;
    let prices: BTreeMap<_, _> = preview["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["type_id"].as_u64().unwrap() as u32, i))
        .collect();
    let matched: Vec<_> = app
        .blueprints
        .items
        .values()
        .filter(|b| {
            (family.is_empty() || b.family == family)
                && (query.is_empty()
                    || b.name.to_lowercase().contains(&query)
                    || b.type_id.to_string().contains(&query))
        })
        .collect();
    let mut rows = Vec::new();
    for b in matched.iter().skip(offset).take(limit) {
        let item = app.data.item_type(b.type_id).expect("published blueprint");
        let resolution = policy.resolve_validated(
            b.type_id,
            item.group_id,
            item.category_id,
            &app.facts[&b.type_id],
        )?;
        let mut row = serde_json::to_value(b)?;
        row["resolution"] = json!(resolution);
        row["preview"] = prices
            .get(&b.type_id)
            .map(|i| (*i).clone())
            .unwrap_or(Value::Null);
        rows.push(row);
    }
    let rule = policy
        .rules
        .iter()
        .find(|r| r.id == crate::workbench_blueprints::rule_id(family));
    Ok(
        json!({"total":matched.len(),"limit":limit,"offset":offset,"items":rows,"rule":rule,
        "warning":crate::workbench_blueprints::SEMANTICS_WARNING}),
    )
}

fn catalog(app: &App, query: &str) -> Result<Value> {
    let q = query_param(query, "q").unwrap_or_default().to_lowercase();
    let category = query_param(query, "category").and_then(|x| x.parse::<u32>().ok());
    let group = query_param(query, "group").and_then(|x| x.parse::<u32>().ok());
    let fact = query_param(query, "fact");
    let profile = query_param(query, "profile");
    let side = query_param(query, "side");
    let seeded = query_param(query, "seeded").and_then(|x| x.parse::<bool>().ok());
    let unresolved = query_param(query, "unresolved").and_then(|x| x.parse::<bool>().ok());
    let tq_state = query_param(query, "tq_state");
    let ordinary = query_param(query, "ordinary").and_then(|x| x.parse::<bool>().ok());
    let scope_id = query_param(query, "scope_id");
    let scope = scope_id
        .as_ref()
        .map(|id| {
            app.quick_scopes
                .iter()
                .find(|s| &s.id == id)
                .ok_or_else(|| anyhow!("unknown Quick Setup scope"))
        })
        .transpose()?;
    let preset = query_param(query, "preset").unwrap_or_else(|| "GENERAL_TQ".into());
    let policy = app.policy(&preset)?;
    let limit = query_param(query, "limit")
        .and_then(|x| x.parse::<usize>().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    let offset = query_param(query, "offset")
        .and_then(|x| x.parse::<usize>().ok())
        .unwrap_or(0);
    let mut items = Vec::new();
    let mut total = 0usize;
    for item in app
        .catalog_items()
        .filter(|i| scope.is_some() || i.is_marketable())
    {
        if scope.is_some_and(|s| !s.members.contains(&item.type_id)) {
            continue;
        }
        if !q.is_empty()
            && !item.name.to_lowercase().contains(&q)
            && !item.type_id.to_string().contains(&q)
        {
            continue;
        }
        if category.is_some_and(|x| item.category_id != Some(x))
            || group.is_some_and(|x| item.group_id != Some(x))
        {
            continue;
        }
        let facts = app.facts.get(&item.type_id).expect("catalog facts");
        if fact.as_ref().is_some_and(|f| !facts.contains(f)) {
            continue;
        }
        if ordinary.is_some_and(|o| app.ordinary.contains(&item.type_id) != o) {
            continue;
        }
        let tq = app.tq.records.get(&item.type_id);
        if tq_state
            .as_ref()
            .is_some_and(|s| tq.is_none_or(|r| &r.state != s))
        {
            continue;
        }
        let resolution =
            policy.resolve_validated(item.type_id, item.group_id, item.category_id, facts)?;
        let current_profile = resolution
            .policy
            .as_ref()
            .and_then(|p| p.profile_id.as_deref());
        if profile
            .as_ref()
            .is_some_and(|p| current_profile != Some(p.as_str()))
        {
            continue;
        }
        let current_seeded = resolution.policy.is_some();
        if seeded.is_some_and(|value| value != current_seeded) {
            continue;
        }
        let current_side = resolution
            .policy
            .as_ref()
            .map(|p| match (p.sides.has_sell(), p.sides.has_buy()) {
                (true, true) => "buy_sell",
                (true, false) => "sell_only",
                (false, true) => "buy_only",
                _ => "unseeded",
            })
            .unwrap_or("unseeded");
        if side.as_ref().is_some_and(|s| s != current_side) {
            continue;
        }
        let review_reason = if resolution.winner_rule_id.as_deref() == Some("rounding_cross") {
            Some("ROUNDING_CROSS")
        } else {
            tq.and_then(|r| {
                if matches!(r.state.as_str(), "NO_TQ_REFERENCE") {
                    Some(r.state.as_str())
                } else {
                    None
                }
            })
        };
        if unresolved.is_some_and(|u| u != review_reason.is_some()) {
            continue;
        }
        if total >= offset && items.len() < limit {
            items.push(json!({"type_id":item.type_id,"name":item.name,"category_id":item.category_id,"group_id":item.group_id,"group_name":item.group_name,
                "category_name":app.category_name(item.category_id),"friendly_group":app.friendly_group(item.type_id),
                "market_group_id":item.market_group_id,"published":item.published,"ordinary":app.ordinary.contains(&item.type_id),"tq_state":tq.map(|r| &r.state),
                "facts":facts,"profile":current_profile,"side":current_side,"seeded":current_seeded,"unresolved_reason":review_reason}));
        }
        total += 1;
    }
    Ok(json!({"total":total,"limit":limit,"offset":offset,"items":items}))
}

fn catalog_tree(app: &App) -> Result<Value> {
    let mut groups = BTreeMap::<u32, BTreeMap<u32, (String, usize)>>::new();
    for item in app.data.marketable_item_types() {
        if let (Some(category), Some(group)) = (item.category_id, item.group_id) {
            let entry = groups.entry(category).or_default().entry(group).or_insert((
                item.group_name
                    .clone()
                    .unwrap_or_else(|| format!("Group {group}")),
                0,
            ));
            entry.1 += 1;
        }
    }
    let categories = groups.into_iter().map(|(category,items)| {
        let count = items.values().map(|(_,n)|n).sum::<usize>();
        json!({"id":category,"label":app.category_name(Some(category)).unwrap_or("Other"),"count":count,
            "groups":items.into_iter().map(|(id,(name,count))|json!({"id":id,"name":name,"count":count})).collect::<Vec<_>>()})
    }).collect::<Vec<_>>();
    Ok(json!({"categories":categories}))
}

fn funded_trace(path: &crate::funded::FundedPath) -> Value {
    json!({"source":"funded_cost","unit_cost":path.unit_cost,"activity":path.activity.key(),
        "blueprint_type_id":path.blueprint_type_id,
        "choices":path.choices.iter().map(|choice|json!({"type_id":choice.type_id,
            "kind":format!("{:?}",choice.kind).to_lowercase(),"unit_cost":choice.unit_cost})).collect::<Vec<_>>()})
}

fn type_detail(app: &App, id: u32, preset: &str) -> Result<Value> {
    let base_preset = app.policy_preset(preset)?;
    let item = app
        .data
        .item_type(id)
        .filter(|i| {
            i.is_marketable()
                || app.blueprints.items.contains_key(&i.type_id)
                || app.structures.items.contains_key(&i.type_id)
                || app.families.items.contains_key(&i.type_id)
        })
        .ok_or_else(|| anyhow!("unknown catalog type"))?;
    let policy = app.policy(preset)?;
    let resolution = policy.resolve(
        id,
        item.group_id,
        item.category_id,
        app.facts.get(&id).expect("catalog facts"),
    )?;
    let evidence = app.tq.records.get(&id);
    let mut sell_price = None;
    let mut buy_price = None;
    let mut unresolved_reason = None;
    let mut funded_detail = None;
    let mut legacy_price_provenance = None;
    let mut quantity = json!(app.general_config.canonical_market.quantity);
    if base_preset == "GENERAL_TQ" {
        let resolutions = app.resolutions_for(&policy)?;
        if let Some(selected) = &resolution.policy {
            let needs_funded = selected
                .sell
                .as_ref()
                .is_some_and(|s| s.source == Source::FundedCost)
                || selected
                    .buy
                    .as_ref()
                    .is_some_and(|s| s.source == Source::FundedCost);
            let funded = if needs_funded
                || selected
                    .sell
                    .as_ref()
                    .is_some_and(|s| s.source == Source::T1VariantSell)
                || selected
                    .buy
                    .as_ref()
                    .is_some_and(|s| s.source == Source::T1VariantBuy)
            {
                app.funded_for(&policy)?
            } else {
                BTreeMap::new()
            };
            legacy_price_provenance = app.preset_references.trace_with_quotes(
                id,
                &resolution,
                &app.tq,
                &funded,
                &resolutions,
            );
            funded_detail = funded
                .get(&id)
                .and_then(|value| value.as_ref().ok())
                .map(funded_trace);
            let _funded_cost = match funded.get(&id) {
                Some(Ok(path)) => Some(path.unit_cost),
                Some(Err(reason)) => {
                    unresolved_reason = Some(format!("funded_cost unavailable: {reason}"));
                    None
                }
                None => None,
            };
            if let Some(side) = &selected.sell {
                match app.preset_references.price(
                    &app.tq,
                    id,
                    Side::Sell,
                    side,
                    &funded,
                    &resolutions,
                ) {
                    Ok(value) => sell_price = Some(value),
                    Err(error) => unresolved_reason = Some(error.to_string()),
                }
            }
            if let Some(side) = &selected.buy {
                match app.preset_references.price(
                    &app.tq,
                    id,
                    Side::Buy,
                    side,
                    &funded,
                    &resolutions,
                ) {
                    Ok(value) => buy_price = Some(value),
                    Err(error) => unresolved_reason = Some(error.to_string()),
                }
            }
            if let Some(Err(reason)) = funded.get(&id) {
                unresolved_reason = Some(format!("funded_cost unavailable: {reason}"));
            }
        } else if resolution.winner_rule_id.as_deref() == Some("rounding_cross") {
            unresolved_reason = Some("ROUNDING_CROSS".into());
        } else if let Some(record) = evidence {
            if matches!(record.state.as_str(), "NO_TQ_REFERENCE") {
                unresolved_reason = Some(record.state.clone());
            }
        }
    } else if base_preset == "LEGACY_V1" {
        let plan = app.legacy_plan(&policy)?;
        if let Some(entry) = plan["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["type_id"].as_u64() == Some(id as u64))
        {
            sell_price = entry["final_sell_price"].as_f64();
            buy_price = entry["final_buy_price"].as_f64();
            quantity = entry["quantity_per_side_per_hub"].clone();
            legacy_price_provenance = Some(json!({"sell_reference":entry["sell_reference"],
                "buy_reference":entry["buy_reference"],"safety_obligations":entry["safety_obligations"]}));
        } else if resolution.policy.is_some() {
            unresolved_reason = Some(
                plan["drops"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|entry| entry["type_id"].as_u64() == Some(id as u64))
                    .and_then(|entry| entry["reason"].as_str())
                    .unwrap_or("not in finalized seed plan")
                    .to_owned(),
            );
        }
    }
    legacy_price_provenance =
        legacy_price_provenance.or_else(|| app.preset_references.trace(id, &resolution));
    let preview = json!({"type_id":id,"name":item.name,"group_id":item.group_id,"group_name":item.group_name,
        "category_id":item.category_id,"market_group_id":item.market_group_id,"facts":app.facts.get(&id),"resolution":resolution,"blueprint":app.blueprints.items.get(&id),
        "structure":app.structures.items.get(&id),
        "economic_family":app.families.items.get(&id),
        "sell_price":sell_price,"buy_price":buy_price,"quantity":quantity,"unresolved_reason":unresolved_reason,"warnings":quote_warnings(sell_price,buy_price),
        "funded":funded_detail,
        "tq":evidence.map(|e|json!({"state":e.state,"sell_reference":e.sell_reference,"buy_reference":e.buy_reference,
            "average_price":e.average_price_fallback_candidate,"captured_at":e.captured_at,"aggregation":e.aggregation,"warnings":e.warnings,"hubs":e.hubs,"history_blend":e.history_blend}))});
    Ok(
        json!({"type_id":id,"name":item.name,"category_id":item.category_id,"group_id":item.group_id,"group_name":item.group_name,
        "market_group_id":item.market_group_id,"published":item.published,"blueprint":app.blueprints.items.get(&id),"resolution":resolution,"preview":preview,
        "structure":app.structures.items.get(&id),
        "economic_family":app.families.items.get(&id),
        "distribution":{"resolved_separately":true,"mode":app.distribution(preset)?.mode,"canonical_item_prices":true},
        "price_provenance":legacy_price_provenance.or(funded_detail),
        "tq_sell_trace":app.tq.trace(id,Side::Sell),"tq_buy_trace":app.tq.trace(id,Side::Buy)}),
    )
}

fn prepare_portable(
    app: &App,
    mut package: crate::workbench_portable::Preset,
) -> Result<(crate::workbench_portable::Preset, Value)> {
    package.validate()?;
    package
        .item_policy
        .validate_catalog_contract(app.sde_build)?;
    crate::workbench_quick::validate_portable_settings(&package.item_policy, &app.quick_scopes)?;
    app.verify_portable_dependencies()?;
    package.distribution_policy = package
        .distribution_policy
        .normalized(&app.quick_scopes, &app.npc_inventory)?;
    let preset = &package.metadata.base_preset;
    let (plan, preview) = app.distribution_plan(
        &package.item_policy,
        preset,
        package.distribution_policy.clone(),
    )?;
    let distribution_preview = plan.preview(app.npc_inventory.stations.len())?;
    ensure!(
        distribution_preview["valid"] == true,
        "Distribution has price safety errors; correct these before import/export"
    );
    let baseline = if preset == "LEGACY_V1" {
        &app.legacy
    } else {
        &app.general
    };
    let quick = app.quick_state(&package.item_policy, baseline, preset)?;
    let custom_scopes: Vec<_> = quick["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["custom"] == true)
        .map(|s| json!({"id":s["id"],"label":s["label"]}))
        .collect();
    let special_count = preview["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["market_group_id"].is_null() && !i["resolution"]["policy"].is_null())
        .count();
    let expert_ids = quick["expert_rule_ids"].as_array().unwrap();
    let exact_overrides = package
        .item_policy
        .rules
        .iter()
        .filter(|r| {
            r.selector.type_ids.is_some()
                && expert_ids
                    .iter()
                    .any(|id| id.as_str() == Some(r.id.as_str()))
        })
        .count();
    let mut warnings = vec![
        "Import creates a new user preset. Gameplay and built-in presets remain unchanged."
            .to_owned(),
    ];
    if special_count > 0 {
        warnings.push(format!("{special_count} special/non-market types are configured. These settings are preserved; unresolved individual prices still prevent a candidate build."));
    }
    if preview["summary"]["unresolved"].as_u64().unwrap_or(0) > 0 {
        warnings.push("Some item quotes need review. Import does not invent missing prices or apply additional fallbacks.".into());
    }
    Ok((
        package.clone(),
        json!({"name":package.metadata.name,"description":package.metadata.description,"author":package.metadata.author,
        "base_preset":preset,"format_version":package.format_version,"profiles":package.item_policy.profiles.len(),"rules":package.item_policy.rules.len(),
        "quick_setup_scopes":custom_scopes,"expert_exact_overrides":exact_overrides,"configured_nonmarket_types":special_count,
        "required_price_sources":package.required_price_sources,"item_policy":preview["summary"],
        "distribution_mode":package.distribution_policy.mode,"distribution_seed":package.distribution_policy.seed,
        "distribution":distribution_preview,"warnings":warnings}),
    ))
}

fn save_policy(app: &App, input: &Value) -> Result<Value> {
    let id = input
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing policy id"))?;
    validate_id(id)?;
    let policy = app.draft_policy(input)?;
    let preset = input
        .get("preset")
        .and_then(Value::as_str)
        .unwrap_or("GENERAL_TQ");
    ensure!(
        preset == "GENERAL_TQ" || preset == "LEGACY_V1",
        "unsupported preset"
    );
    let preview = app.preview(&policy, preset)?;
    app.ensure_configurable(&preview, preset)?;
    let doc = if input["distribution"].is_null() {
        app.distribution(id)?
    } else {
        crate::distribution::Document::parse(&input["distribution"])?
    };
    let doc = doc.normalized(&app.quick_scopes, &app.npc_inventory)?;
    let (distribution_plan, _) = app.distribution_plan(&policy, preset, doc.clone())?;
    let distribution_preview = distribution_plan.preview(app.npc_inventory.stations.len())?;
    ensure!(
        distribution_preview["valid"] == true,
        "Distribution has price safety errors; preview before saving"
    );
    let serialized = policy.canonical_json()?;
    let dir = app.storage.join("policies");
    let target = dir.join(format!("{id}.json"));
    let meta = dir.join(format!("{id}.meta.json"));
    if meta.exists() {
        ensure!(
            app.policy_preset(id)? == preset,
            "saved policy metadata uses a different base preset"
        );
    }
    let overwrite = input
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if target.exists() {
        ensure!(overwrite, "policy exists; set overwrite=true explicitly");
        ensure!(
            app.policy_preset(id)? == preset,
            "cannot change a saved policy's base preset while overwriting"
        );
        let prior = fs::read(&target)?;
        let backup = app
            .storage
            .join("backups")
            .join(format!("{}-{}.json", id, unique_stamp()?));
        fs::write(backup, prior)?;
        if meta.exists() {
            fs::write(
                app.storage
                    .join("backups")
                    .join(format!("{}-{}.meta.json", id, unique_stamp()?)),
                fs::read(&meta)?,
            )?;
        }
    }
    let display_name = input["display_name"].as_str().unwrap_or(id).trim();
    ensure!(
        !display_name.is_empty()
            && display_name.chars().count() <= 80
            && !display_name.chars().any(char::is_control),
        "Preset name must be 1–80 readable characters"
    );
    let prior_metadata = app.portable_metadata(id)?;
    let description = crate::workbench_portable::optional_text(
        input
            .get("description")
            .unwrap_or(&prior_metadata["description"])
            .as_str(),
        2000,
        "Description",
    )?;
    let author = crate::workbench_portable::optional_text(
        input
            .get("author")
            .unwrap_or(&prior_metadata["author"])
            .as_str(),
        160,
        "Author",
    )?;
    let temp = dir.join(format!(".{id}-{}.tmp", unique_stamp()?));
    let meta_temp = dir.join(format!(".{id}-{}.meta.tmp", unique_stamp()?));
    let metadata = serde_json::to_vec_pretty(
        &json!({"id":id,"preset":preset,"display_name":display_name,"description":description,"author":author,"distribution":doc}),
    )?;
    for (path, bytes) in [
        (&temp, serialized.as_bytes()),
        (&meta_temp, metadata.as_slice()),
    ] {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    let rollback = if meta.exists() {
        let path = dir.join(format!(".{id}-{}.rollback.tmp", unique_stamp()?));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&fs::read(&meta)?)?;
        file.sync_all()?;
        Some(path)
    } else {
        None
    };
    atomic_install(&meta_temp, &meta)?;
    if let Err(error) = atomic_install(&temp, &target) {
        if let Some(path) = rollback {
            atomic_install(&path, &meta)?;
        } else {
            fs::rename(
                &meta,
                dir.join(format!(".{id}-{}.failed-meta.tmp", unique_stamp()?)),
            )?;
        }
        return Err(error);
    }
    if let Some(path) = rollback {
        let _ = fs::remove_file(path);
    }
    Ok(
        json!({"id":id,"preset":preset,"path":target,"sha256":sha256_hex(serialized.as_bytes()),"distribution":doc}),
    )
}

pub(crate) fn unique_stamp() -> Result<String> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!("{nanos}"))
}

#[cfg(windows)]
pub(crate) fn atomic_install(temp: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from = temp
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let to = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    ensure!(
        ok != 0,
        "atomic file replacement failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
#[cfg(not(windows))]
pub(crate) fn atomic_install(temp: &Path, target: &Path) -> Result<()> {
    fs::rename(temp, target)?;
    Ok(())
}

fn new_candidate_id(policy_id: &str) -> Result<String> {
    validate_id(policy_id)?;
    Ok(format!(
        "{}-{}",
        &policy_id[..policy_id.len().min(40)],
        unique_stamp()?
    ))
}

fn candidate_dir(app: &App, id: &str) -> Result<PathBuf> {
    confined_candidate_dir(&app.storage, id)
}
fn confined_candidate_dir(storage: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(storage.join("candidates").join(id))
}
fn build_candidate(app: &App, body: &Value) -> Result<Value> {
    let policy_id = body["policy_id"]
        .as_str()
        .ok_or_else(|| anyhow!("save a policy before building"))?;
    validate_id(policy_id)?;
    let receipt = body["distribution_preview_sha256"]
        .as_str()
        .ok_or_else(|| anyhow!("Distribution Preview is required before building"))?;
    app.verify_distribution_inputs()?;
    let policy = app.policy(policy_id)?;
    let preset = app.policy_preset(policy_id)?;
    let (plan, item_preview) =
        app.distribution_plan(&policy, &preset, app.distribution(policy_id)?)?;
    ensure_preview_buildable(app, &item_preview, &preset)?;
    let preview = plan.preview(app.npc_inventory.stations.len())?;
    ensure!(
        preview["valid"] == true,
        "Distribution has price safety errors"
    );
    ensure!(
        preview["plan_sha256"] == receipt,
        "Distribution Preview is stale; preview the saved preset again"
    );
    let has_advanced_quotes = item_preview["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| !i["resolution"]["policy"].is_null() && i["market_group_id"].is_null());
    let mut manifest = if plan.distribution.mode == crate::distribution::Mode::FiveHubs
        && !has_advanced_quotes
    {
        build_five_hub_candidate(app, body)?
    } else {
        let id = new_candidate_id(policy_id)?;
        let dir = candidate_dir(app, &id)?;
        fs::create_dir(&dir)?;
        let policy_path = dir.join("policy.json");
        fs::write(&policy_path, policy.canonical_json()?)?;
        let config_path = dir.join("build.toml");
        let mut raw: toml::Value =
            toml::from_str(&fs::read_to_string(if preset == "GENERAL_TQ" {
                GENERAL_CONFIG
            } else {
                LEGACY_CONFIG
            })?)?;
        raw["policy"]["path"] = toml::Value::String(policy_path.display().to_string());
        raw["policy"]["plan_preview_path"] =
            toml::Value::String(dir.join("plan.json").display().to_string());
        raw["policy"]
            .as_table_mut()
            .unwrap()
            .remove("parity_baseline_path");
        raw["policy"].as_table_mut().unwrap().insert(
            "allow_intentional_policy_delta".into(),
            toml::Value::Boolean(true),
        );
        raw["output"]["database_path"] =
            toml::Value::String(dir.join("market.sqlite").display().to_string());
        fs::write(&config_path, toml::to_string_pretty(&raw)?)?;
        let config = SeederConfig::load(&config_path)?;
        crate::distribution_build::run(&plan, &preview, &config, &app.data)?;
        let summary = json!({"preset":preset,"policy_preview":item_preview["summary"],"sell_rows":preview["sell_rows"],"buy_rows":preview["buy_rows"]});
        fs::write(
            dir.join("summary.json"),
            serde_json::to_vec_pretty(&summary)?,
        )?;
        let mut m = json!({"id":id,"preset":preset,"policy_id":policy_id,"policy_sha256":sha256_hex(&fs::read(policy_path)?),
            "config_sha256":sha256_hex(&fs::read(config_path)?),"market_sha256":sha256_hex(&fs::read(dir.join("market.sqlite"))?),"summary":summary});
        if preset == "GENERAL_TQ" {
            m["tq_dataset_sha256"] = json!(sha256_hex(&fs::read(
                config
                    .policy
                    .as_ref()
                    .unwrap()
                    .tq_snapshot_path
                    .as_ref()
                    .unwrap()
            )?));
        } else {
            m["price_manifest_sha256"] = json!(sha256_hex(&fs::read(&config.price_manifest.path)?));
        }
        m
    };
    manifest["policy_id"] = json!(policy_id);
    let id = manifest["id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing candidate id"))?
        .to_owned();
    let dir = candidate_dir(app, &id)?;
    let conn = rusqlite::Connection::open_with_flags(
        dir.join("market.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    plan.verify_database(&conn)?;
    drop(conn);
    fs::write(
        dir.join("distribution.json"),
        serde_json::to_vec_pretty(&plan.distribution)?,
    )?;
    fs::write(
        dir.join("distribution-preview.json"),
        serde_json::to_vec_pretty(&preview)?,
    )?;
    let plan_bytes = serde_json::to_vec(&plan)?;
    fs::write(dir.join("distribution-plan.json"), &plan_bytes)?;
    manifest["distribution"] = json!({"format_version":1,"plan_sha256":sha256_hex(&plan_bytes),"policy_sha256":sha256_hex(&fs::read(dir.join("distribution.json"))?),
        "mode":plan.distribution.mode,"seed":plan.distribution.seed,"station_ids":plan.stations.iter().filter(|s|!plan.rows(s).is_empty()).map(|s|s.npc.station_id).collect::<Vec<_>>(),
        "selected_stations":plan.stations.len(),"sell_rows":preview["sell_rows"],"buy_rows":preview["buy_rows"],"provenance":plan.provenance});
    fs::write(
        dir.join("build-report.json"),
        serde_json::to_vec_pretty(
            &json!({"candidate_id":id,"result":"built","distribution_plan_sha256":preview["plan_sha256"],"exact_plan_verification":true}),
        )?,
    )?;
    let temp = dir.join(format!(".manifest-{}.tmp", unique_stamp()?));
    fs::write(&temp, serde_json::to_vec_pretty(&manifest)?)?;
    atomic_install(&temp, &dir.join("manifest.json"))?;
    Ok(manifest)
}

fn build_five_hub_candidate(app: &App, body: &Value) -> Result<Value> {
    let policy_id = body
        .get("policy_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("save a policy before building; provide policy_id"))?;
    validate_id(policy_id)?;
    if app.policy_preset(policy_id)? == "LEGACY_V1" {
        return build_legacy_candidate(app, policy_id);
    }
    let policy = app.policy(policy_id)?;
    let preview = app.preview(&policy, "GENERAL_TQ")?;
    ensure!(
        preview["summary"]["unresolved_selected"].as_u64() == Some(0),
        "candidate has unresolved enabled prices"
    );
    ensure!(
        preview["summary"]["unmatched"].as_u64() == Some(0),
        "candidate has unmatched ordinary types"
    );
    let id = new_candidate_id(policy_id)?;
    let dir = candidate_dir(app, &id)?;
    fs::create_dir(&dir)?;
    let policy_path = dir.join("policy.json");
    fs::write(&policy_path, policy.canonical_json()?)?;
    let mut config = app.general_config.clone();
    config.policy.as_mut().unwrap().path = policy_path.clone();
    config.policy.as_mut().unwrap().plan_preview_path = Some(dir.join("plan.json"));
    config.output.database_path = dir.join("market.sqlite");
    let config_path = dir.join("build.toml");
    // Preserve all operational inputs from the pinned config, changing only confined outputs.
    let mut raw: toml::Value = toml::from_str(&fs::read_to_string(GENERAL_CONFIG)?)?;
    raw["policy"]["path"] = toml::Value::String(policy_path.display().to_string());
    raw["policy"]["plan_preview_path"] =
        toml::Value::String(dir.join("plan.json").display().to_string());
    raw["output"]["database_path"] =
        toml::Value::String(config.output.database_path.display().to_string());
    fs::write(&config_path, toml::to_string_pretty(&raw)?)?;
    general_tq::run_with_facts(&config_path, &config, false, Some(&app.facts))?;
    finalize_candidate(app, &id, Some(preview["summary"].clone()))
}

fn build_legacy_candidate(app: &App, policy_id: &str) -> Result<Value> {
    let policy = app.policy(policy_id)?;
    let preview = app.preview(&policy, "LEGACY_V1")?;
    ensure_preview_buildable(app, &preview, "LEGACY_V1")?;
    let id = new_candidate_id(policy_id)?;
    let dir = candidate_dir(app, &id)?;
    fs::create_dir(&dir)?;
    let policy_path = dir.join("policy.json");
    fs::write(&policy_path, policy.canonical_json()?)?;
    let config_path = dir.join("build.toml");
    let mut raw: toml::Value = toml::from_str(&fs::read_to_string(LEGACY_CONFIG)?)?;
    raw["policy"]["path"] = toml::Value::String(policy_path.display().to_string());
    raw["policy"]["plan_preview_path"] =
        toml::Value::String(dir.join("plan.json").display().to_string());
    raw["policy"]
        .as_table_mut()
        .expect("policy table")
        .remove("parity_baseline_path");
    raw["policy"].as_table_mut().expect("policy table").insert(
        "allow_intentional_policy_delta".into(),
        toml::Value::Boolean(true),
    );
    raw["output"]["database_path"] =
        toml::Value::String(dir.join("market.sqlite").display().to_string());
    fs::write(&config_path, toml::to_string_pretty(&raw)?)?;
    let config = SeederConfig::load(&config_path)?;
    ensure!(
        config.output.database_path == dir.join("market.sqlite"),
        "legacy candidate output escaped workbench"
    );
    ensure!(
        config.policy.as_ref().is_some_and(|p| p.path == policy_path
            && p.allow_intentional_policy_delta
            && p.parity_baseline_path.is_none()),
        "legacy candidate policy config invalid"
    );
    crate::build::run_build(&config_path, true, false, false)?;
    let db = dir.join("market.sqlite");
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let sell: i64 = conn.query_row("SELECT count(*) FROM seed_stock", [], |r| r.get(0))?;
    let buy: i64 = conn.query_row("SELECT count(*) FROM seed_buy_orders", [], |r| r.get(0))?;
    drop(conn);
    ensure!(
        sell % 5 == 0 && buy % 5 == 0,
        "legacy candidate side counts are not replicated five ways"
    );
    let summary = json!({"preset":"LEGACY_V1","policy_preview":preview["summary"],"sell":sell/5,"buy":buy/5,"sell_rows":sell,"buy_rows":buy});
    let manifest = json!({"id":id,"preset":"LEGACY_V1","policy_id":policy_id,"policy_sha256":sha256_hex(&fs::read(&policy_path)?),
        "config_sha256":sha256_hex(&fs::read(&config_path)?),"price_manifest_sha256":sha256_hex(&fs::read(&config.price_manifest.path)?),
        "market_sha256":sha256_hex(&fs::read(&db)?),"summary":summary});
    fs::write(
        dir.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    fs::write(
        dir.join("build-report.json"),
        serde_json::to_vec_pretty(
            &json!({"candidate_id":id,"result":"built","database":"market.sqlite","economic_audit":"pending"}),
        )?,
    )?;
    let temp = dir.join(format!(".manifest-{}.tmp", unique_stamp()?));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    file.sync_all()?;
    atomic_install(&temp, &dir.join("manifest.json"))?;
    Ok(manifest)
}

fn finalize_candidate(app: &App, id: &str, summary: Option<Value>) -> Result<Value> {
    let dir = candidate_dir(app, id)?;
    ensure!(
        !dir.join("manifest.json").exists(),
        "candidate is already finalized"
    );
    for name in ["policy.json", "build.toml", "plan.json", "market.sqlite"] {
        ensure!(dir.join(name).is_file(), "candidate missing {name}");
    }
    let policy = PolicyDocument::load(&dir.join("policy.json"))?;
    policy.validate_catalog_contract(app.sde_build)?;
    let summary = match summary {
        Some(v) => v,
        None => app.preview(&policy, "GENERAL_TQ")?["summary"].clone(),
    };
    ensure!(
        summary["unresolved_selected"].as_u64() == Some(0),
        "candidate policy has unresolved enabled prices"
    );
    let plan: Value = serde_json::from_slice(&fs::read(dir.join("plan.json"))?)?;
    let quoted_types = plan
        .get("quotedTypeCount")
        .unwrap_or(&plan["quotedBuySellCount"]);
    ensure!(
        quoted_types == &summary["seeded"],
        "candidate plan/preview count differs"
    );
    let db = dir.join("market.sqlite");
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let sell: i64 = conn.query_row("SELECT count(*) FROM seed_stock", [], |r| r.get(0))?;
    let buy: i64 = conn.query_row("SELECT count(*) FROM seed_buy_orders", [], |r| r.get(0))?;
    let expected_sell = summary["sell"]
        .as_i64()
        .ok_or_else(|| anyhow!("missing Sell count"))?
        * 5;
    let expected_buy = summary["buy"]
        .as_i64()
        .ok_or_else(|| anyhow!("missing Buy count"))?
        * 5;
    ensure!(
        sell == expected_sell && buy == expected_buy,
        "candidate seed row count differs"
    );
    drop(conn);
    let config: SeederConfig = SeederConfig::load(&dir.join("build.toml"))?;
    ensure!(
        config.output.database_path == db,
        "candidate output path differs"
    );
    ensure!(
        config
            .policy
            .as_ref()
            .is_some_and(|p| p.path == dir.join("policy.json")),
        "candidate policy path differs"
    );
    let policy_id = id
        .rsplit_once('-')
        .map(|(head, _)| head)
        .ok_or_else(|| anyhow!("invalid candidate ID"))?;
    let manifest = json!({"id":id,"preset":"GENERAL_TQ","policy_id":policy_id,"policy_sha256":sha256_hex(&fs::read(dir.join("policy.json"))?),
        "tq_dataset_sha256":sha256_hex(&fs::read(config.policy.as_ref().unwrap().tq_snapshot_path.as_ref().unwrap())?),
        "config_sha256":sha256_hex(&fs::read(dir.join("build.toml"))?),"market_sha256":sha256_hex(&fs::read(&db)?),
        "summary":summary});
    fs::write(
        dir.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    fs::write(
        dir.join("build-report.json"),
        serde_json::to_vec_pretty(
            &json!({"candidate_id":id,"result":"built","database":"market.sqlite","plan":"plan.json"}),
        )?,
    )?;
    let manifest_tmp = dir.join(format!(".manifest-{}.tmp", unique_stamp()?));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest_tmp)?;
    output.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    output.sync_all()?;
    atomic_install(&manifest_tmp, &dir.join("manifest.json"))?;
    Ok(manifest)
}

fn list_candidates(app: &App) -> Result<Value> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(app.storage.join("candidates"))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let id = entry.file_name().to_string_lossy().to_string();
            if validate_id(&id).is_ok() && entry.path().join("manifest.json").exists() {
                let mut candidate: Value =
                    serde_json::from_slice(&fs::read(entry.path().join("manifest.json"))?)?;
                candidate["database_path"] =
                    json!(entry.path().join("market.sqlite").display().to_string());
                candidates.push(candidate);
            }
        }
    }
    Ok(json!({"candidates":candidates}))
}
fn candidate_detail(app: &App, id: &str) -> Result<Value> {
    let dir = candidate_dir(app, id)?;
    let mut candidate: Value = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
    candidate["database_path"] = json!(dir.join("market.sqlite").display().to_string());
    Ok(candidate)
}
fn audit_candidate(app: &App, id: &str) -> Result<Value> {
    let dir = candidate_dir(app, id)?;
    let db = dir.join("market.sqlite");
    ensure!(
        db.exists() && dir.join("manifest.json").exists(),
        "candidate is incomplete"
    );
    let connection =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    let fk: i64 =
        connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    let mut station_query = connection.prepare("SELECT station_id FROM seed_stock UNION SELECT station_id FROM seed_buy_orders ORDER BY station_id")?;
    let station_ids = station_query
        .query_map([], |r| r.get::<_, u64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut station_ok = station_ids
        == crate::config::CANONICAL_HUB_STATION_IDS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
    let sell: i64 = connection.query_row("SELECT count(*) FROM seed_stock", [], |r| r.get(0))?;
    let buy: i64 =
        connection.query_row("SELECT count(*) FROM seed_buy_orders", [], |r| r.get(0))?;
    let crossed: i64 = connection.query_row("SELECT count(*) FROM seed_buy_orders b JOIN seed_stock s ON s.station_id=b.station_id AND s.type_id=b.type_id WHERE b.price >= s.price",[],|r|r.get(0))?;
    let player_orders: i64 =
        connection.query_row("SELECT count(*) FROM market_orders", [], |r| r.get(0))?;
    let player_events: i64 =
        connection.query_row("SELECT count(*) FROM market_order_events", [], |r| r.get(0))?;
    let manifest = candidate_detail(app, id)?;
    let file_sha = crate::workbench_install::file_sha256(&db)?;
    let policy_sha = sha256_hex(&fs::read(dir.join("policy.json"))?);
    let config_sha = sha256_hex(&fs::read(dir.join("build.toml"))?);
    let config = SeederConfig::load(&dir.join("build.toml"))?;
    let preset = manifest["preset"]
        .as_str()
        .ok_or_else(|| anyhow!("candidate manifest has no preset"))?;
    let input_sha_ok = if preset == "GENERAL_TQ" {
        let tq_path = config
            .policy
            .as_ref()
            .and_then(|p| p.tq_snapshot_path.as_ref())
            .ok_or_else(|| anyhow!("candidate has no TQ path"))?;
        manifest["tq_dataset_sha256"] == sha256_hex(&fs::read(tq_path)?)
    } else if preset == "LEGACY_V1" {
        manifest["price_manifest_sha256"] == sha256_hex(&fs::read(&config.price_manifest.path)?)
    } else {
        bail!("unknown candidate preset")
    };
    let mut provenance_ok = manifest["market_sha256"] == file_sha
        && manifest["policy_sha256"] == policy_sha
        && manifest["config_sha256"] == config_sha
        && input_sha_ok;
    let mut expected_sell_rows = manifest["summary"]["sell"].as_i64().unwrap_or(-1) * 5;
    let mut expected_buy_rows = manifest["summary"]["buy"].as_i64().unwrap_or(-1) * 5;
    let mut exact_distribution = true;
    if !manifest["distribution"].is_null() {
        let plan_bytes = fs::read(dir.join("distribution-plan.json"))?;
        let plan: crate::distribution::Plan = serde_json::from_slice(&plan_bytes)?;
        let preview = plan.preview(app.npc_inventory.stations.len())?;
        app.verify_distribution_inputs()?;
        let (replayed, _) = app.distribution_plan(
            &PolicyDocument::load(&dir.join("policy.json"))?,
            preset,
            plan.distribution.clone(),
        )?;
        ensure!(
            serde_json::to_vec(&replayed)? == plan_bytes,
            "candidate plan differs from saved Item Policy / Distribution / pinned inputs"
        );
        for s in &plan.stations {
            ensure!(
                app.npc_inventory.require(s.npc.station_id)? == &s.npc,
                "candidate station eligibility/metadata changed"
            );
        }
        let expected: Vec<u64> = plan
            .stations
            .iter()
            .filter(|s| !plan.rows(s).is_empty())
            .map(|s| s.npc.station_id)
            .collect();
        station_ok = station_ids == expected;
        expected_sell_rows = preview["sell_rows"].as_i64().unwrap_or(-1);
        expected_buy_rows = preview["buy_rows"].as_i64().unwrap_or(-1);
        exact_distribution = plan.verify_database(&connection).is_ok() && preview["valid"] == true;
        provenance_ok = provenance_ok
            && manifest["distribution"]["plan_sha256"] == sha256_hex(&plan_bytes)
            && manifest["distribution"]["policy_sha256"]
                == sha256_hex(&fs::read(dir.join("distribution.json"))?)
            && plan.provenance["item_policy_sha256"]
                == sha256_hex(
                    PolicyDocument::load(&dir.join("policy.json"))?
                        .canonical_json()?
                        .as_bytes(),
                );
    }
    let global_crossed:i64=connection.query_row("SELECT count(*) FROM (SELECT type_id,max(price) AS bid FROM seed_buy_orders GROUP BY type_id) b JOIN (SELECT type_id,min(price) AS ask FROM seed_stock GROUP BY type_id) s ON s.type_id=b.type_id WHERE b.bid>=s.ask",[],|r|r.get(0))?;
    let structural_passed = integrity == "ok"
        && fk == 0
        && station_ok
        && sell == expected_sell_rows
        && buy == expected_buy_rows
        && player_orders == 0
        && player_events == 0
        && provenance_ok
        && exact_distribution;
    let economic = if preset == "LEGACY_V1" && structural_passed {
        let audit_stations =
            (!manifest["distribution"].is_null()).then_some(station_ids.as_slice());
        let passed = crate::audit::run_audit_with_stations(
            &dir.join("build.toml"),
            crate::audit::PERFECT_YIELD,
            20,
            false,
            false,
            Some(&db),
            audit_stations,
        );
        match passed {
            Ok(passed) => {
                json!({"applicable":true,"passed":passed,"source":"candidate_bound_market_seeder_audit"})
            }
            Err(error) => {
                json!({"applicable":true,"passed":false,"error":format!("{error:#}"),"source":"candidate_bound_market_seeder_audit"})
            }
        }
    } else if preset == "LEGACY_V1" {
        json!({"applicable":true,"passed":false,"reason":"structural audit failed"})
    } else {
        json!({"applicable":false,"reason":"GENERAL_TQ has no legacy production economy gate"})
    };
    let passed = structural_passed && economic["passed"].as_bool().unwrap_or(true);
    let result = json!({"id":id,"preset":preset,"market_sha256":file_sha,"integrity":integrity,"foreign_key_errors":fk,"station_ids":station_ids,"canonical_stations":station_ok,"sell_rows":sell,"buy_rows":buy,
        "buy_ge_sell":crossed,"global_buy_ge_sell":global_crossed,"exact_distribution_plan":exact_distribution,"player_orders":player_orders,"player_events":player_events,"provenance_ok":provenance_ok,
        "passed":passed,"economic_audit":economic,"warnings":if crossed > 0 || global_crossed > 0 {vec![format!("Buy >= Sell: {crossed} local pairs, {global_crossed} types across stations. Quotes retained as configured.")]} else {vec![]}});
    fs::write(dir.join("audit.json"), serde_json::to_vec_pretty(&result)?)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossed_quotes_are_review_warnings_not_unresolved_sources() {
        assert!(quote_warnings(Some(120.), Some(80.)).is_empty());
        assert_eq!(
            quote_warnings(Some(80.), Some(120.)),
            vec!["BUY_AT_OR_ABOVE_SELL"]
        );
        assert_eq!(
            quote_warnings(Some(100.), Some(100.)),
            vec!["BUY_AT_OR_ABOVE_SELL"]
        );
        assert!(quote_warnings(None, Some(100.)).is_empty());
    }

    #[test]
    fn bind_is_loopback_only() {
        assert_eq!(bind_address(8765), SocketAddr::from(([127, 0, 0, 1], 8765)));
        assert!(bind_address(8765).ip().is_loopback());
    }

    #[test]
    fn browser_identifiers_cannot_be_paths_or_presets() {
        for bad in [
            "",
            "../market.sqlite",
            "C:\\gameplay\\market.sqlite",
            "/tmp/out",
            "GENERAL_TQ",
            "LEGACY_V1",
            "a/b",
            "a\\b",
        ] {
            assert!(validate_id(bad).is_err(), "accepted {bad}");
        }
        assert!(validate_id("custom_20260929").is_ok());
    }

    #[test]
    fn static_assets_are_explicit_and_never_expose_database_or_service_control() {
        assert_eq!(
            static_asset("/"),
            Some(("index.html", "text/html; charset=utf-8"))
        );
        for path in [
            "/../market.sqlite",
            "/api/v1/deploy",
            "/api/v1/restart",
            "/market.sqlite",
            "/other.js",
        ] {
            assert!(static_asset(path).is_none(), "served {path}");
        }
    }

    #[test]
    fn candidate_paths_remain_under_workbench_storage() {
        let root = PathBuf::from(STORAGE);
        let path = confined_candidate_dir(&root, "custom-20260929").unwrap();
        assert!(path.starts_with(root.join("candidates")));
        assert!(confined_candidate_dir(&root, "../market.sqlite").is_err());
        assert!(confined_candidate_dir(&root, "C:\\gameplay\\market.sqlite").is_err());
    }

    #[test]
    fn portable_storage_is_explicit_and_does_not_change_development_default() {
        assert_eq!(storage_path(None).unwrap(), PathBuf::from(STORAGE));
        let root = std::env::current_dir().unwrap().join("user-data");
        assert_eq!(storage_path(Some(Path::new("user-data"))).unwrap(), root);
        assert_eq!(storage_path(Some(&root)).unwrap(), root);
        assert!(storage_path(Some(Path::new(""))).is_err());
    }
}
