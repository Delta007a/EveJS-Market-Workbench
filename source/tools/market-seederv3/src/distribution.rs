//! Distribution consumes finished item quotes. It cannot select a price authority,
//! invent an absent side, or repair an unresolved item.
use crate::config::CANONICAL_HUB_STATION_IDS as HUBS;
use crate::distribution_stations::{Inventory, NpcStation};
use crate::policy_preview::sha256_hex;
use crate::workbench_quick::Scope;
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_STOCK: i64 = i32::MAX as i64;
// Match the existing seed writer's technical ISK minimum. This is applied only
// after secondary station modifiers, never to canonical item quotes.
fn local_price(price: f64, modifier: f64, plan_version: u32) -> f64 {
    let adjusted = price * modifier / 100.;
    let rounded = market_common::round_isk(adjusted);
    if plan_version >= 2 && adjusted.is_finite() && adjusted > 0. {
        rounded.max(crate::build::MIN_SEED_PRICE)
    } else {
        // Preserve version 1 replay and let preview reject invalid arithmetic.
        rounded
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub min: f64,
    pub max: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Stock {
    pub min: i64,
    pub max: i64,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    JitaOnly,
    FiveHubs,
    HubsRegional,
    AllNpc,
    Custom,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Remoteness {
    Off,
    Light,
    Medium,
    Strong,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Secondary {
    pub assortment: Range,
    pub stock: Stock,
    pub buy_modifier: Range,
    pub sell_modifier: Range,
    pub remoteness: Remoteness,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Regional {
    pub min: u32,
    pub max: u32,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    Regional,
    All,
    Explicit,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Custom {
    pub canonical_hubs: Vec<u64>,
    pub selection: Selection,
    pub station_ids: Vec<u64>,
    pub region_ids: Vec<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Override {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assortment: Option<Range>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stock: Option<Stock>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub distribution_format_version: u32,
    pub mode: Mode,
    pub seed: String,
    pub secondary: Secondary,
    pub regional: Regional,
    pub custom: Custom,
    pub overrides: BTreeMap<String, Override>,
}
impl Default for Document {
    fn default() -> Self {
        Self {
            distribution_format_version: 1,
            mode: Mode::FiveHubs,
            seed: "12345".into(),
            secondary: Secondary {
                assortment: Range { min: 5., max: 20. },
                stock: Stock { min: 1, max: 500 },
                buy_modifier: Range {
                    min: 90.,
                    max: 100.,
                },
                sell_modifier: Range {
                    min: 100.,
                    max: 115.,
                },
                remoteness: Remoteness::Off,
            },
            regional: Regional { min: 1, max: 2 },
            custom: Custom {
                canonical_hubs: HUBS.to_vec(),
                selection: Selection::Explicit,
                station_ids: vec![],
                region_ids: vec![],
            },
            overrides: BTreeMap::new(),
        }
    }
}
fn validate_range(r: &Range, ceiling: f64, positive: bool) -> Result<()> {
    ensure!(
        r.min.is_finite() && r.max.is_finite() && r.min >= 0. && r.max >= r.min && r.max <= ceiling,
        "range must be finite, ordered and within 0–{ceiling}"
    );
    ensure!(!positive || r.min > 0., "price modifier must be positive");
    ensure!(
        (r.min * 100. - (r.min * 100.).round()).abs() < 1e-7
            && (r.max * 100. - (r.max * 100.).round()).abs() < 1e-7,
        "percentages support at most two decimal places"
    );
    Ok(())
}
fn validate_stock(r: &Stock) -> Result<()> {
    ensure!(
        r.min >= 1 && r.max >= r.min && r.max <= MAX_STOCK && r.min < MAX_STOCK,
        "secondary stock must be positive, ordered, <= 2,147,483,647; all rows cannot have maximum stock"
    );
    Ok(())
}
impl Document {
    pub fn parse(v: &Value) -> Result<Self> {
        Ok(serde_json::from_value(v.clone())?)
    }
    pub fn normalized(mut self, scopes: &[Scope], inventory: &Inventory) -> Result<Self> {
        ensure!(
            self.distribution_format_version == 1,
            "unsupported Distribution format version"
        );
        ensure!(
            !self.seed.is_empty()
                && self.seed.len() <= 128
                && !self.seed.chars().any(char::is_control),
            "Distribution seed must be 1–128 readable characters"
        );
        validate_range(&self.secondary.assortment, 100., false)?;
        validate_stock(&self.secondary.stock)?;
        validate_range(&self.secondary.buy_modifier, 1000., true)?;
        validate_range(&self.secondary.sell_modifier, 1000., true)?;
        ensure!(
            self.regional.max >= self.regional.min && self.regional.max <= 1000,
            "regional station count must be ordered and <= 1,000 per region"
        );
        ensure!(
            self.secondary.remoteness == Remoteness::Off || inventory.topology_available,
            "authoritative stargate topology is unavailable; choose remoteness Off"
        );
        self.custom.canonical_hubs.sort_unstable();
        self.custom.canonical_hubs.dedup();
        self.custom.station_ids.sort_unstable();
        self.custom.station_ids.dedup();
        self.custom.region_ids.sort_unstable();
        self.custom.region_ids.dedup();
        for id in &self.custom.canonical_hubs {
            ensure!(
                HUBS.contains(id),
                "Custom canonical hubs must be from the five protected hubs"
            );
            inventory.require(*id)?;
        }
        for id in &self.custom.station_ids {
            inventory.require(*id)?;
        }
        for id in &self.custom.region_ids {
            ensure!(
                inventory.stations.values().any(|s| s.region_id == *id),
                "unknown NPC region {id}"
            );
        }
        for (id, o) in &self.overrides {
            ensure!(
                scopes.iter().any(|s| s.id == *id),
                "unknown friendly Distribution group {id}"
            );
            if let Some(r) = &o.assortment {
                validate_range(r, 100., false)?;
            }
            if let Some(r) = &o.stock {
                validate_stock(r)?;
            }
        }
        // Remove redundant values in parent-before-child order, including subgroup inheritance.
        for child in [false, true] {
            for scope in scopes.iter().filter(|s| s.parent.is_some() == child) {
                let parent = scope
                    .parent
                    .as_ref()
                    .and_then(|p| self.overrides.get(p))
                    .cloned()
                    .unwrap_or_default();
                if let Some(o) = self.overrides.get_mut(&scope.id) {
                    if o.assortment.as_ref()
                        == Some(
                            parent
                                .assortment
                                .as_ref()
                                .unwrap_or(&self.secondary.assortment),
                        )
                    {
                        o.assortment = None;
                    }
                    if o.stock.as_ref()
                        == Some(parent.stock.as_ref().unwrap_or(&self.secondary.stock))
                    {
                        o.stock = None;
                    }
                }
            }
        }
        self.overrides
            .retain(|_, o| o.assortment.is_some() || o.stock.is_some());
        // Different sibling scopes may overlap factually. An active collision has no silent winner.
        let active: Vec<_> = scopes
            .iter()
            .filter(|s| self.overrides.contains_key(&s.id))
            .collect();
        for (i, a) in active.iter().enumerate() {
            for b in &active[i + 1..] {
                if a.parent.as_deref() == Some(&b.id) || b.parent.as_deref() == Some(&a.id) {
                    continue;
                }
                let oa = &self.overrides[&a.id];
                let ob = &self.overrides[&b.id];
                let collision = (oa.assortment.is_some() && ob.assortment.is_some())
                    || (oa.stock.is_some() && ob.stock.is_some());
                ensure!(
                    !collision || a.members.is_disjoint(&b.members),
                    "overlapping Distribution overrides: {} and {}; reset one",
                    a.label,
                    b.label
                );
            }
        }
        Ok(self)
    }
}

/// FNV-1a followed by SplitMix64 avalanche, with explicitly separated byte fields.
/// Stable across processes, compilers and platforms; this is selection, not cryptography.
fn rank(seed: &str, purpose: &str, station: u64, item: u32) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for part in [
        seed.as_bytes(),
        purpose.as_bytes(),
        &station.to_le_bytes(),
        &item.to_le_bytes(),
    ] {
        for &b in part {
            h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
        }
        h = (h ^ 255).wrapping_mul(0x100000001b3);
    }
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d049bb133111eb);
    h ^ (h >> 31)
}
fn bias(r: f64, remoteness: Remoteness, jumps: Option<u32>, high: bool) -> f64 {
    let strength = match remoteness {
        Remoteness::Off => 0.,
        Remoteness::Light => 0.25,
        Remoteness::Medium => 0.5,
        Remoteness::Strong => 0.75,
    };
    let d = f64::from(jumps.unwrap_or(0));
    let b = strength * d / (d + 10.);
    if high {
        1. - (1. - r) * (1. - b)
    } else {
        r * (1. - b)
    }
}
fn draw(
    seed: &str,
    purpose: &str,
    station: &NpcStation,
    item: u32,
    min: i64,
    max: i64,
    remote: Remoteness,
    high: bool,
) -> i64 {
    let n = rank(seed, purpose, station.station_id, item);
    // 53 deterministic bits, inclusive endpoints, then quantized to the selected range.
    let r = (n >> 11) as f64 / ((1u64 << 53) - 1) as f64;
    let r = bias(r, remote, station.nearest_hub_jumps, high);
    (min + (r * (max - min) as f64).round() as i64).clamp(min, max)
}
fn percent(doc: &Document, purpose: &str, station: &NpcStation, r: &Range, high: bool) -> f64 {
    draw(
        &doc.seed,
        purpose,
        station,
        0,
        (r.min * 100.).round() as i64,
        (r.max * 100.).round() as i64,
        doc.secondary.remoteness,
        high,
    ) as f64
        / 100.
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Quote {
    pub type_id: u32,
    pub name: String,
    pub buy: Option<f64>,
    pub sell: Option<f64>,
}
pub fn quotes_from_preview(preview: &Value) -> Result<Vec<Quote>> {
    let mut result = Vec::new();
    for item in preview["items"]
        .as_array()
        .ok_or_else(|| anyhow!("missing Item preview"))?
    {
        if !item["unresolved_reason"].is_null() || item["resolution"]["policy"].is_null() {
            continue;
        }
        let q = Quote {
            type_id: item["type_id"]
                .as_u64()
                .ok_or_else(|| anyhow!("missing type ID"))? as u32,
            name: item["name"].as_str().unwrap_or("").into(),
            buy: item["buy_price"].as_f64(),
            sell: item["sell_price"].as_f64(),
        };
        if q.buy.is_some() || q.sell.is_some() {
            result.push(q);
        }
    }
    result.sort_by_key(|q| q.type_id);
    Ok(result)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Row {
    pub type_id: u32,
    pub buy: Option<f64>,
    pub sell: Option<f64>,
    pub quantity: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Station {
    pub npc: NpcStation,
    pub canonical: bool,
    pub assortment_percent: f64,
    pub buy_modifier: f64,
    pub sell_modifier: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Bucket {
    id: String,
    types: Vec<u32>,
    range: Range,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub plan_format_version: u32,
    pub distribution: Document,
    pub quotes: Vec<Quote>,
    pub stations: Vec<Station>,
    pub provenance: Value,
    buckets: Vec<Bucket>,
    stock_ranges: BTreeMap<u32, Stock>,
}
impl Plan {
    pub fn compile(
        doc: Document,
        quotes: Vec<Quote>,
        inventory: &Inventory,
        scopes: &[Scope],
        provenance: Value,
    ) -> Result<Self> {
        let doc = doc.normalized(scopes, inventory)?;
        let ids: BTreeSet<_> = quotes.iter().map(|q| q.type_id).collect();
        ensure!(ids.len() == quotes.len(), "duplicate canonical item quote");
        for q in &quotes {
            ensure!(q.buy.is_some() || q.sell.is_some(), "empty item quote");
            for p in [q.buy, q.sell].into_iter().flatten() {
                ensure!(p.is_finite() && p > 0., "invalid canonical item price");
            }
        }
        let mut selected = BTreeSet::new();
        let canonical: BTreeSet<u64> = match doc.mode {
            Mode::JitaOnly => [HUBS[0]].into_iter().collect(),
            Mode::Custom => doc.custom.canonical_hubs.iter().copied().collect(),
            _ => HUBS.into_iter().collect(),
        };
        selected.extend(&canonical);
        let selection = match doc.mode {
            Mode::HubsRegional => Some(Selection::Regional),
            Mode::AllNpc => Some(Selection::All),
            Mode::Custom => Some(doc.custom.selection),
            _ => None,
        };
        if let Some(selection) = selection {
            let mut regions = BTreeMap::<u32, Vec<u64>>::new();
            for s in inventory
                .stations
                .values()
                .filter(|s| !HUBS.contains(&s.station_id))
            {
                if doc.mode == Mode::Custom
                    && !doc.custom.region_ids.is_empty()
                    && !doc.custom.region_ids.contains(&s.region_id)
                {
                    continue;
                }
                regions.entry(s.region_id).or_default().push(s.station_id);
            }
            match selection {
                Selection::All => selected.extend(regions.values().flatten()),
                Selection::Explicit => {
                    for id in &doc.custom.station_ids {
                        ensure!(
                            !HUBS.contains(id),
                            "enable canonical hubs using their protected checkboxes"
                        );
                        ensure!(
                            doc.custom.region_ids.is_empty()
                                || doc
                                    .custom
                                    .region_ids
                                    .contains(&inventory.require(*id)?.region_id),
                            "selected station is outside the Custom region filter"
                        );
                        selected.insert(*id);
                    }
                }
                Selection::Regional => {
                    for (region, mut stations) in regions {
                        let width = doc.regional.max - doc.regional.min + 1;
                        let n = doc.regional.min
                            + (rank(&doc.seed, "region-count", u64::from(region), 0)
                                % u64::from(width)) as u32;
                        stations
                            .sort_by_key(|&id| (rank(&doc.seed, "region-station", id, region), id));
                        selected.extend(stations.into_iter().take(n as usize));
                    }
                }
            }
        }
        ensure!(
            !selected.is_empty(),
            "Distribution must select at least one NPC station"
        );
        let mut stations = Vec::new();
        for id in selected {
            let npc = inventory.require(id)?.clone();
            let hub = canonical.contains(&id);
            stations.push(Station {
                assortment_percent: if hub {
                    100.
                } else {
                    percent(&doc, "assortment", &npc, &doc.secondary.assortment, false)
                },
                buy_modifier: if hub {
                    100.
                } else {
                    percent(&doc, "buy", &npc, &doc.secondary.buy_modifier, false)
                },
                sell_modifier: if hub {
                    100.
                } else {
                    percent(&doc, "sell", &npc, &doc.secondary.sell_modifier, true)
                },
                npc,
                canonical: hub,
            });
        }
        let mut buckets = Vec::new();
        let mut remaining = ids.clone();
        let mut stock_ranges = BTreeMap::new();
        // Deepest applicable layer owns its types. Stock-only overrides do not reshuffle assortment.
        for child in [true, false] {
            for s in scopes.iter().filter(|s| s.parent.is_some() == child) {
                if let Some(o) = doc.overrides.get(&s.id) {
                    if let Some(range) = &o.assortment {
                        let types: Vec<u32> = s.members.intersection(&remaining).copied().collect();
                        for id in &types {
                            remaining.remove(id);
                        }
                        buckets.push(Bucket {
                            id: s.id.clone(),
                            types,
                            range: range.clone(),
                        });
                    }
                    if let Some(range) = &o.stock {
                        for &id in s.members.intersection(&ids) {
                            stock_ranges.entry(id).or_insert_with(|| range.clone());
                        }
                    }
                }
            }
        }
        buckets.push(Bucket {
            id: "default".into(),
            types: remaining.into_iter().collect(),
            range: doc.secondary.assortment.clone(),
        });
        buckets.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Self {
            plan_format_version: 3,
            distribution: doc,
            quotes,
            stations,
            provenance,
            buckets,
            stock_ranges,
        })
    }
    pub fn rows(&self, station: &Station) -> Vec<Row> {
        let mut selected = BTreeSet::new();
        if station.canonical {
            selected.extend(self.quotes.iter().map(|q| q.type_id));
        } else {
            for bucket in &self.buckets {
                let p = if bucket.id == "default" {
                    station.assortment_percent
                } else {
                    percent(
                        &self.distribution,
                        &format!("assortment:{}", bucket.id),
                        &station.npc,
                        &bucket.range,
                        false,
                    )
                };
                let n = ((bucket.types.len() as f64 * p / 100.).round() as usize)
                    .min(bucket.types.len());
                let purpose = format!("items:{}", bucket.id);
                if n == 0 {
                    continue;
                }
                if n == bucket.types.len() {
                    selected.extend(&bucket.types);
                    continue;
                }
                let mut ranked: Vec<_> = bucket
                    .types
                    .iter()
                    .map(|&id| {
                        (
                            rank(
                                &self.distribution.seed,
                                &purpose,
                                station.npc.station_id,
                                id,
                            ),
                            id,
                        )
                    })
                    .collect();
                if n < ranked.len() {
                    ranked.select_nth_unstable(n);
                }
                selected.extend(ranked.into_iter().take(n).map(|(_, id)| id));
            }
        }
        self.quotes
            .iter()
            .filter(|q| selected.contains(&q.type_id))
            .map(|q| {
                let stock = self
                    .stock_ranges
                    .get(&q.type_id)
                    .unwrap_or(&self.distribution.secondary.stock);
                Row {
                    type_id: q.type_id,
                    buy: q.buy.map(|p| {
                        if station.canonical {
                            p
                        } else {
                            local_price(p, station.buy_modifier, self.plan_format_version)
                        }
                    }),
                    sell: q.sell.map(|p| {
                        if station.canonical {
                            p
                        } else {
                            local_price(p, station.sell_modifier, self.plan_format_version)
                        }
                    }),
                    quantity: if station.canonical {
                        MAX_STOCK
                    } else {
                        draw(
                            &self.distribution.seed,
                            "stock",
                            &station.npc,
                            q.type_id,
                            stock.min,
                            stock.max,
                            self.distribution.secondary.remoteness,
                            false,
                        )
                    },
                }
            })
            .collect()
    }
    pub fn preview(&self, eligible: usize) -> Result<Value> {
        ensure!(
            (1..=3).contains(&self.plan_format_version),
            "unsupported distribution plan version {}",
            self.plan_format_version
        );
        let mut stats = Vec::new();
        let mut errors = Vec::new();
        let mut error_count = 0usize;
        let mut crossing_samples = Vec::new();
        let quotes: BTreeMap<_, _> = self.quotes.iter().map(|q| (q.type_id, q)).collect();
        let (mut floored_buy, mut floored_sell) = (0u64, 0u64);
        let mut floor_samples = Vec::new();
        let mut best = BTreeMap::<u32, (Option<(f64, u64)>, Option<(f64, u64)>)>::new();
        let (mut sell, mut buy) = (0u64, 0u64);
        let mut local_cross = 0usize;
        let mut digest = 0xcbf29ce484222325u64;
        let mut record_error = |v: Value| {
            error_count += 1;
            if errors.len() < 100 {
                errors.push(v);
            }
        };
        for s in &self.stations {
            let rows = self.rows(s);
            let (mut sc, mut bc) = (0, 0);
            let (mut qmin, mut qmax, mut qsum) = (MAX_STOCK, 0i64, 0u64);
            for row in &rows {
                qmin = qmin.min(row.quantity);
                qmax = qmax.max(row.quantity);
                qsum += row.quantity as u64;
                let entry = best.entry(row.type_id).or_default();
                for (is_buy, price) in [(false, row.sell), (true, row.buy)] {
                    if let Some(p) = price {
                        let modifier = if is_buy {
                            s.buy_modifier
                        } else {
                            s.sell_modifier
                        };
                        let side = if is_buy { "buy" } else { "sell" };
                        if !s.canonical && p == crate::build::MIN_SEED_PRICE {
                            let q = quotes[&row.type_id];
                            let base = if is_buy { q.buy } else { q.sell }.unwrap();
                            let adjusted = base * modifier / 100.;
                            if adjusted.is_finite()
                                && adjusted > 0.
                                && market_common::round_isk(adjusted) < p
                            {
                                if is_buy {
                                    floored_buy += 1;
                                } else {
                                    floored_sell += 1;
                                }
                                if floor_samples.len() < 20 {
                                    floor_samples.push(json!({"type_id":row.type_id,"name":q.name,
                                        "station_id":s.npc.station_id,"station_name":s.npc.name,"side":side,
                                        "canonical_price":base,"modifier_percent":modifier,
                                        "unrounded_price":adjusted,"price":p}));
                                }
                            }
                        }
                        if !p.is_finite() || p <= 0. {
                            let q = quotes[&row.type_id];
                            let base = if is_buy { q.buy } else { q.sell };
                            record_error(
                                json!({"reason":"INVALID_ROUNDED_PRICE","type_id":row.type_id,
                                    "station_id":s.npc.station_id,"side":side,"price":p,
                                    "canonical_price":base,"modifier_percent":modifier}),
                            );
                        }
                        if is_buy {
                            bc += 1;
                            if entry.0.is_none_or(|(b, _)| p > b) {
                                entry.0 = Some((p, s.npc.station_id));
                            }
                        } else {
                            sc += 1;
                            if entry.1.is_none_or(|(a, _)| p < a) {
                                entry.1 = Some((p, s.npc.station_id));
                            }
                        }
                        for b in s
                            .npc
                            .station_id
                            .to_le_bytes()
                            .into_iter()
                            .chain(row.type_id.to_le_bytes())
                            .chain([u8::from(is_buy)])
                            .chain(p.to_bits().to_le_bytes())
                            .chain(row.quantity.to_le_bytes())
                        {
                            digest = (digest ^ u64::from(b)).wrapping_mul(0x100000001b3);
                        }
                    }
                }
                if let (Some(b), Some(a)) = (row.buy, row.sell) {
                    if b >= a {
                        local_cross += 1;
                        let crossing = json!({"reason":"LOCAL_CROSSING","type_id":row.type_id,"station_id":s.npc.station_id,"buy":b,"sell":a});
                        if self.plan_format_version < 3 {
                            record_error(crossing);
                        } else if crossing_samples.len() < 100 {
                            crossing_samples.push(crossing);
                        }
                    }
                }
            }
            sell += sc;
            buy += bc;
            stats.push(json!({"station_id":s.npc.station_id,"name":s.npc.name,"region":s.npc.region_name,"region_id":s.npc.region_id,
                "tier":if s.canonical {"canonical"}else{"secondary"},"nearest_hub_jumps":s.npc.nearest_hub_jumps,
                "assortment_target_percent":s.assortment_percent,"actual_assortment_percent":if self.quotes.is_empty(){0.}else{rows.len() as f64*100./self.quotes.len() as f64},
                "types":rows.len(),"sell_rows":sc,"buy_rows":bc,"stock_min":if rows.is_empty(){0}else{qmin},"stock_max":qmax,
                "stock_sum":qsum,"stock_average":if rows.is_empty(){0.}else{qsum as f64/rows.len() as f64},"buy_modifier":s.buy_modifier,"sell_modifier":s.sell_modifier}));
        }
        let mut global_cross = 0usize;
        for (id, (bid, ask)) in best {
            if let (Some((b, bs)), Some((a, ss))) = (bid, ask) {
                if b >= a {
                    global_cross += 1;
                    let crossing = json!({"reason":"GLOBAL_SAME_ITEM_CROSSING","type_id":id,"highest_buy":b,"buy_station_id":bs,"lowest_sell":a,"sell_station_id":ss});
                    if self.plan_format_version < 3 {
                        record_error(crossing);
                    } else if crossing_samples.len() < 100 {
                        crossing_samples.push(crossing);
                    }
                }
            }
        }
        let mut tiers = Vec::new();
        for tier in ["canonical", "secondary"] {
            let xs: Vec<_> = stats.iter().filter(|s| s["tier"] == tier).collect();
            if xs.is_empty() {
                continue;
            }
            let range = |key: &str| -> Value {
                let vs: Vec<f64> = xs.iter().filter_map(|s| s[key].as_f64()).collect();
                json!({"min":vs.iter().copied().fold(f64::INFINITY,f64::min),"max":vs.iter().copied().fold(f64::NEG_INFINITY,f64::max),"average":vs.iter().sum::<f64>()/vs.len() as f64})
            };
            let types: u64 = xs.iter().map(|s| s["types"].as_u64().unwrap()).sum();
            tiers.push(json!({"tier":tier,"stations":xs.len(),"assortment":range("actual_assortment_percent"),"buy_modifier":range("buy_modifier"),"sell_modifier":range("sell_modifier"),
                "stock":{"min":xs.iter().map(|s|s["stock_min"].as_i64().unwrap()).min(),"max":xs.iter().map(|s|s["stock_max"].as_i64().unwrap()).max(),
                    "average":if types==0{0.}else{xs.iter().map(|s|s["stock_sum"].as_u64().unwrap()).sum::<u64>() as f64/types as f64}}}));
        }
        let mut sorted = stats.clone();
        sorted.sort_by_key(|s| {
            (
                s["types"].as_u64().unwrap(),
                s["station_id"].as_u64().unwrap(),
            )
        });
        let mut warnings: Vec<String> = Vec::new();
        if sell + buy >= 10_000_000 {
            warnings.push("Very large market: 10+ million seed rows. Build may take substantial time and disk space.".into());
        } else if sell + buy >= 1_000_000 {
            warnings.push(
                "Large market: 1+ million seed rows. Review the exact row count before building."
                    .into(),
            );
        }
        if self.distribution.mode == Mode::AllNpc {
            warnings.push("All NPC Stations can produce a large database. Preview counts are exact; no database was written.".into());
        }
        if self
            .stations
            .iter()
            .any(|s| !s.canonical && s.npc.nearest_hub_jumps.is_none())
        {
            warnings.push("Some secondary systems have no stargate path to a canonical hub. Their remoteness bias is neutral; no distance is invented.".into());
        }
        if floored_buy + floored_sell > 0 {
            warnings.push(format!("{} secondary quotes ({} Buy, {} Sell) would round below 0.01 ISK after local modifiers. The existing 0.01 ISK minimum is applied. Canonical hub prices are unchanged.",
                floored_buy + floored_sell, floored_buy, floored_sell));
        }
        if self.plan_format_version >= 3 && local_cross + global_cross > 0 {
            warnings.push(format!("Buy >= Sell: {local_cross} local pairs and {global_cross} types across stations. Quotes are retained as configured; these are review warnings, not build errors."));
        }
        let bytes = serde_json::to_vec(self)?;
        Ok(
            json!({"valid":error_count==0,"distribution":self.distribution,"plan_sha256":sha256_hex(&bytes),"row_fingerprint":format!("{digest:016x}"),
            "eligible_npc_stations":eligible,"canonical_hubs":self.stations.iter().filter(|s|s.canonical).count(),"selected_secondary_stations":self.stations.iter().filter(|s|!s.canonical).count(),
            "selected_stations":self.stations.len(),"stations_with_rows":stats.iter().filter(|s|s["types"].as_u64().unwrap()>0).count(),"resolved_item_types":self.quotes.len(),
            "sell_rows":sell,"buy_rows":buy,"total_seed_rows":sell+buy,"tiers":tiers,"stations":stats,
            "smallest_books":sorted.iter().take(5).collect::<Vec<_>>(),"largest_books":sorted.iter().rev().take(5).collect::<Vec<_>>(),
            "price_floor":{"minimum_isk":crate::build::MIN_SEED_PRICE,"adjusted_quotes":floored_buy+floored_sell,
                "buy_quotes":floored_buy,"sell_quotes":floored_sell,"samples":floor_samples},
            "local_crossings":local_cross,"global_crossings":global_cross,"crossing_warnings":crossing_samples,"error_count":error_count,"errors":errors,"warnings":warnings} ),
        )
    }
    /// Exact persisted-row verification, including absent sides, IEEE price bits,
    /// quantities, initial quantities, version and authoritative station geometry.
    pub fn verify_database(&self, conn: &rusqlite::Connection) -> Result<()> {
        let selected: BTreeSet<_> = self.stations.iter().map(|s| s.npc.station_id).collect();
        for (table, is_buy) in [("seed_stock", false), ("seed_buy_orders", true)] {
            let mut query=conn.prepare(&format!("SELECT station_id,type_id,price,quantity,initial_quantity,price_version,solar_system_id,constellation_id,region_id FROM {table} ORDER BY station_id,type_id"))?;
            let mut stored = query.query([])?;
            for station in &self.stations {
                for row in self.rows(station) {
                    let price = if is_buy { row.buy } else { row.sell };
                    let Some(price) = price else { continue };
                    let actual = stored.next()?.ok_or_else(|| {
                        anyhow!(
                            "missing {table} row at station {} type {}",
                            station.npc.station_id,
                            row.type_id
                        )
                    })?;
                    ensure!(
                        actual.get::<_, u64>(0)? == station.npc.station_id
                            && actual.get::<_, u32>(1)? == row.type_id,
                        "{table} type/station differs from Distribution plan"
                    );
                    ensure!(
                        actual.get::<_, f64>(2)?.to_bits() == price.to_bits()
                            && actual.get::<_, i64>(3)? == row.quantity
                            && actual.get::<_, i64>(4)? == row.quantity
                            && actual.get::<_, i64>(5)? == 1,
                        "{table} price/quantity/version differs from Distribution plan"
                    );
                    ensure!(
                        actual.get::<_, u32>(6)? == station.npc.solar_system_id
                            && actual.get::<_, u32>(7)? == station.npc.constellation_id
                            && actual.get::<_, u32>(8)? == station.npc.region_id,
                        "{table} station geometry differs"
                    );
                }
            }
            ensure!(
                stored.next()?.is_none(),
                "unexpected {table} row outside Distribution plan"
            );
        }
        ensure!(!selected.is_empty(), "empty Distribution selection");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Inventory, Vec<Quote>, Vec<Scope>) {
        let stations = HUBS
            .into_iter()
            .chain(700..720)
            .enumerate()
            .map(|(i, id)| {
                (
                    id,
                    NpcStation {
                        station_id: id,
                        name: format!("Station {id}"),
                        owner_id: 1000001,
                        solar_system_id: i as u32 + 1,
                        constellation_id: 1,
                        region_id: if id < 710 { 1 } else { 2 },
                        region_name: "Region".into(),
                        nearest_hub_jumps: Some(i as u32),
                    },
                )
            })
            .collect();
        let quotes = (1..=100)
            .map(|id| Quote {
                type_id: id,
                name: id.to_string(),
                buy: Some(10.),
                sell: Some(12.),
            })
            .collect();
        let scopes = vec![
            Scope {
                id: "resources".into(),
                label: "Resources".into(),
                priority_offset: 0,
                parent: None,
                members: (1..=50).collect(),
            },
            Scope {
                id: "ore".into(),
                label: "Ore".into(),
                priority_offset: 10,
                parent: Some("resources".into()),
                members: (1..=20).collect(),
            },
        ];
        (
            Inventory {
                stations,
                provenance: json!({}),
                topology_available: true,
            },
            quotes,
            scopes,
        )
    }
    fn plan(doc: Document) -> Plan {
        let (i, q, s) = fixture();
        Plan::compile(doc, q, &i, &s, json!({"input":"fixed"})).unwrap()
    }
    #[test]
    fn strict_schema_and_ranges() {
        let mut v = serde_json::to_value(Document::default()).unwrap();
        v["unknown"] = json!(true);
        assert!(Document::parse(&v).is_err());
        let (i, _, s) = fixture();
        for mutate in 0..4 {
            let mut d = Document::default();
            match mutate {
                0 => d.distribution_format_version = 2,
                1 => d.secondary.assortment.min = 101.,
                2 => d.secondary.stock.max = MAX_STOCK + 1,
                _ => d.custom.station_ids = vec![1000000000001],
            };
            assert!(d.normalized(&s, &i).is_err());
        }
    }
    #[test]
    fn jita_and_five_hubs_preserve_all_quotes_sides_and_maximum_stock() {
        for (mode, n) in [(Mode::JitaOnly, 1), (Mode::FiveHubs, 5)] {
            let mut d = Document::default();
            d.mode = mode;
            d.secondary.buy_modifier = Range {
                min: 500.,
                max: 600.,
            };
            let p = plan(d);
            assert_eq!(p.stations.len(), n);
            for s in &p.stations {
                assert!(s.canonical);
                assert_eq!(
                    (s.assortment_percent, s.buy_modifier, s.sell_modifier),
                    (100., 100., 100.)
                );
                let rows = p.rows(s);
                assert_eq!(rows.len(), 100);
                assert!(
                    rows.iter().all(|r| r.quantity == MAX_STOCK
                        && r.buy == Some(10.)
                        && r.sell == Some(12.))
                );
            }
            let v = p.preview(25).unwrap();
            assert!(v["valid"].as_bool().unwrap());
            assert_eq!(v["buy_rows"], json!(100 * n));
        }
    }
    #[test]
    fn secondary_subcent_quotes_use_seed_minimum_without_changing_canonical_prices() {
        let (i, _, s) = fixture();
        let q = vec![Quote {
            type_id: 1,
            name: "Thin quote".into(),
            buy: Some(0.01),
            sell: Some(0.02),
        }];
        let mut d = Document::default();
        d.mode = Mode::AllNpc;
        d.secondary.assortment = Range {
            min: 100.,
            max: 100.,
        };
        d.secondary.buy_modifier = Range { min: 40., max: 40. };
        d.secondary.sell_modifier = Range {
            min: 115.,
            max: 115.,
        };
        let p = Plan::compile(d, q, &i, &s, json!({})).unwrap();
        assert_eq!(p.plan_format_version, 3);
        for station in &p.stations {
            let rows = p.rows(station);
            assert_eq!((rows[0].buy, rows[0].sell), (Some(0.01), Some(0.02)));
        }
        let v = p.preview(25).unwrap();
        assert_eq!(v["valid"], true);
        assert_eq!(v["error_count"], 0);
        assert_eq!(v["price_floor"]["buy_quotes"], 20);
        assert_eq!(v["price_floor"]["sell_quotes"], 0);
        assert_eq!(v["price_floor"]["samples"][0]["unrounded_price"], 0.004);
        assert_eq!(v["price_floor"]["samples"][0]["side"], "buy");
        assert!(
            v["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w.as_str().unwrap().contains("0.01 ISK minimum"))
        );
        assert_eq!(v, p.preview(25).unwrap());
        // Saved v1 plans retain their old row replay rather than silently changing.
        let mut old = p;
        old.plan_format_version = 1;
        let v = old.preview(25).unwrap();
        assert_eq!(v["valid"], false);
        assert_eq!(v["errors"][0]["side"], "buy");
        assert_eq!(v["errors"][0]["price"], 0.);
        assert_eq!(v["errors"][0]["canonical_price"], 0.01);
        assert_eq!(v["errors"][0]["modifier_percent"], 40.);
    }
    #[test]
    fn minimum_price_never_hides_crossings_or_invalid_arithmetic() {
        assert_eq!(local_price(0.01, 40., 2), 0.01);
        assert_eq!(local_price(0.01, 40., 1), 0.);
        assert_eq!(local_price(12.34, 115., 2), 14.19);
        assert_eq!(local_price(0., 40., 2), 0.);
        assert!(local_price(f64::NAN, 100., 2).is_nan());
        assert!(local_price(1e308, 1000., 2).is_infinite());
        let (i, _, s) = fixture();
        let mut d = Document::default();
        d.mode = Mode::AllNpc;
        d.secondary.assortment = Range {
            min: 100.,
            max: 100.,
        };
        d.secondary.buy_modifier = Range { min: 40., max: 40. };
        d.secondary.sell_modifier = Range { min: 1., max: 1. };
        let q = vec![Quote {
            type_id: 1,
            name: "Rounded cross".into(),
            buy: Some(0.01),
            sell: Some(0.02),
        }];
        let p = Plan::compile(d, q, &i, &s, json!({})).unwrap();
        let v = p.preview(25).unwrap();
        assert_eq!(v["valid"], true);
        assert_eq!(v["local_crossings"], 20);
        assert_eq!(v["global_crossings"], 1);
        assert_eq!(v["price_floor"]["sell_quotes"], 20);
        assert_eq!(v["crossing_warnings"][0]["reason"], "LOCAL_CROSSING");
        let mut historical = p.clone();
        historical.plan_format_version = 2;
        assert_eq!(historical.preview(25).unwrap()["valid"], false);
        historical.plan_format_version = 4;
        assert!(historical.preview(25).is_err());
    }
    #[test]
    fn stable_selection_ranges_variation_and_plan_bytes() {
        let mut d = Document::default();
        d.mode = Mode::HubsRegional;
        d.regional = Regional { min: 3, max: 4 };
        let p = plan(d.clone());
        let again = plan(d.clone());
        assert_eq!(
            serde_json::to_vec(&p).unwrap(),
            serde_json::to_vec(&again).unwrap()
        );
        assert_eq!(p.preview(25).unwrap(), again.preview(25).unwrap());
        let secondary: Vec<_> = p.stations.iter().filter(|s| !s.canonical).collect();
        assert!(secondary.len() >= 6);
        assert!(
            secondary
                .windows(2)
                .any(|s| s[0].assortment_percent != s[1].assortment_percent)
        );
        for s in secondary {
            assert!((5. ..=20.).contains(&s.assortment_percent));
            assert!((90. ..=100.).contains(&s.buy_modifier));
            assert!((100. ..=115.).contains(&s.sell_modifier));
            let rows = p.rows(s);
            assert_eq!(rows, again.rows(s));
            assert!(rows.iter().all(|r| (1..=500).contains(&r.quantity)));
            assert_eq!(
                rows.len(),
                (100. * s.assortment_percent / 100.).round() as usize
            );
        }
        d.seed = "different".into();
        let other = plan(d);
        assert_ne!(
            p.preview(25).unwrap()["row_fingerprint"],
            other.preview(25).unwrap()["row_fingerprint"]
        );
    }
    #[test]
    fn exact_sides_disabled_and_unresolved_are_not_invented() {
        let v = json!({"items":[{"type_id":1,"name":"sell","resolution":{"policy":{}},"sell_price":5.,"buy_price":null,"unresolved_reason":null},
            {"type_id":2,"name":"off","resolution":{"policy":null},"sell_price":5.,"unresolved_reason":null},
            {"type_id":3,"name":"missing","resolution":{"policy":{}},"sell_price":5.,"unresolved_reason":"missing buy"}]});
        let q = quotes_from_preview(&v).unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].buy, None);
        let (i, _, s) = fixture();
        let mut q2 = q.clone();
        q2.push(Quote {
            type_id: 4,
            name: "buy".into(),
            buy: Some(1.),
            sell: None,
        });
        let p = Plan::compile(Document::default(), q2, &i, &s, json!({})).unwrap();
        assert_eq!(p.rows(&p.stations[0])[0].buy, None);
        assert_eq!(p.rows(&p.stations[0])[1].sell, None);
    }
    #[test]
    fn inheritance_normalizes_and_reset_only_removes_requested_layer() {
        let (i, _, s) = fixture();
        let mut d = Document::default();
        d.overrides.insert(
            "resources".into(),
            Override {
                assortment: Some(Range { min: 50., max: 50. }),
                stock: Some(Stock { min: 10, max: 10 }),
            },
        );
        d.overrides.insert(
            "ore".into(),
            Override {
                assortment: Some(Range { min: 50., max: 50. }),
                stock: Some(Stock { min: 1, max: 2 }),
            },
        );
        let normalized = d.normalized(&s, &i).unwrap();
        assert!(normalized.overrides["ore"].assortment.is_none());
        assert!(normalized.overrides["ore"].stock.is_some());
        let mut reset = normalized.clone();
        reset.overrides.remove("ore");
        assert_eq!(
            reset.overrides["resources"],
            normalized.overrides["resources"]
        );
        assert_ne!(reset, normalized);
    }
    #[test]
    fn group_and_subgroup_assortment_stock_and_parent_inheritance() {
        let mut d = Document::default();
        d.mode = Mode::AllNpc;
        d.secondary.assortment = Range { min: 0., max: 0. };
        d.overrides.insert(
            "resources".into(),
            Override {
                assortment: Some(Range {
                    min: 100.,
                    max: 100.,
                }),
                stock: Some(Stock { min: 10, max: 10 }),
            },
        );
        d.overrides.insert(
            "ore".into(),
            Override {
                assortment: Some(Range { min: 50., max: 50. }),
                stock: Some(Stock { min: 1, max: 1 }),
            },
        );
        let p = plan(d);
        let s = p.stations.iter().find(|s| !s.canonical).unwrap();
        let rows = p.rows(s);
        assert_eq!(rows.len(), 40);
        assert_eq!(rows.iter().filter(|r| r.type_id <= 20).count(), 10);
        assert!(
            rows.iter()
                .all(|r| r.quantity == if r.type_id <= 20 { 1 } else { 10 })
        );
    }
    #[test]
    fn stock_only_override_does_not_change_assortment() {
        let mut d = Document::default();
        d.mode = Mode::AllNpc;
        let a = plan(d.clone());
        d.overrides.insert(
            "ore".into(),
            Override {
                assortment: None,
                stock: Some(Stock { min: 2, max: 3 }),
            },
        );
        let b = plan(d);
        for (s, t) in a.stations.iter().zip(&b.stations) {
            assert_eq!(
                a.rows(s).iter().map(|r| r.type_id).collect::<Vec<_>>(),
                b.rows(t).iter().map(|r| r.type_id).collect::<Vec<_>>()
            );
        }
    }
    #[test]
    fn local_and_cross_station_crossings_warn_after_rounding() {
        let mut d = Document::default();
        d.mode = Mode::AllNpc;
        d.secondary.assortment = Range {
            min: 100.,
            max: 100.,
        };
        d.secondary.buy_modifier = Range {
            min: 125.,
            max: 125.,
        };
        let v = plan(d).preview(25).unwrap();
        assert_eq!(v["valid"], true);
        assert!(v["local_crossings"].as_u64().unwrap() > 0);
        assert!(v["global_crossings"].as_u64().unwrap() > 0);
        let (i, mut q, s) = fixture();
        q[0].buy = Some(0.01);
        q[0].sell = Some(0.02);
        let mut d = Document::default();
        d.mode = Mode::AllNpc;
        d.secondary.assortment = Range {
            min: 100.,
            max: 100.,
        };
        d.secondary.sell_modifier = Range { min: 50., max: 50. };
        let p = Plan::compile(d, q, &i, &s, json!({})).unwrap();
        assert!(p.preview(25).unwrap()["local_crossings"].as_u64().unwrap() > 0);
    }
    #[test]
    fn farther_remoteness_is_bounded_and_directional() {
        for strength in [
            Remoteness::Off,
            Remoteness::Light,
            Remoteness::Medium,
            Remoteness::Strong,
        ] {
            for r in [0., 0.01, 0.5, 0.99, 1.] {
                let near = bias(r, strength, Some(1), false);
                let far = bias(r, strength, Some(100), false);
                assert!((0. ..=1.).contains(&far));
                assert!(far <= near);
                let near = bias(r, strength, Some(1), true);
                let far = bias(r, strength, Some(100), true);
                assert!((0. ..=1.).contains(&far));
                assert!(far >= near);
            }
        }
        let mut d = Document::default();
        d.mode = Mode::AllNpc;
        d.secondary.remoteness = Remoteness::Strong;
        let p = plan(d);
        for s in p.stations.iter().filter(|s| !s.canonical) {
            assert!((5. ..=20.).contains(&s.assortment_percent));
            assert!((90. ..=100.).contains(&s.buy_modifier));
            assert!((100. ..=115.).contains(&s.sell_modifier));
            assert!(p.rows(s).iter().all(|r| (1..=500).contains(&r.quantity)));
        }
    }
    #[test]
    fn seed_is_separate_roundtrippable_and_custom_is_npc_only() {
        let d = Document::default();
        assert_eq!(
            d,
            Document::parse(&serde_json::to_value(&d).unwrap()).unwrap()
        );
        let mut d = d;
        d.mode = Mode::Custom;
        d.custom.canonical_hubs = vec![HUBS[0]];
        d.custom.station_ids = vec![700, 701];
        let p = plan(d.clone());
        assert_eq!(p.stations.len(), 3);
        d.custom.station_ids = vec![999999999999];
        let (i, q, s) = fixture();
        assert!(Plan::compile(d, q, &i, &s, json!({})).is_err());
    }

    #[test]
    fn global_crossing_warns_even_when_every_local_book_is_uncrossed() {
        let mut d = Document::default();
        d.mode = Mode::AllNpc;
        d.secondary.assortment = Range {
            min: 100.,
            max: 100.,
        };
        d.secondary.buy_modifier = Range {
            min: 120.,
            max: 120.,
        };
        d.secondary.sell_modifier = Range {
            min: 150.,
            max: 150.,
        };
        let v = plan(d).preview(25).unwrap();
        assert_eq!(v["local_crossings"], 0);
        assert_eq!(v["global_crossings"], 100);
        assert_eq!(v["valid"], true);
        assert_eq!(v["error_count"], 0);
        assert_eq!(v["crossing_warnings"].as_array().unwrap().len(), 100);
    }

    #[test]
    fn persisted_rows_match_preview_and_tampering_fails() {
        let mut d = Document::default();
        d.mode = Mode::Custom;
        d.custom.station_ids = vec![700, 701];
        d.secondary.buy_modifier = Range { min: 40., max: 40. };
        let (i, mut q, s) = fixture();
        q[0].buy = Some(0.01);
        let p = Plan::compile(d, q, &i, &s, json!({})).unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(market_common::SCHEMA_SQL).unwrap();
        for station in &p.stations {
            for row in p.rows(station) {
                for (table, price) in [("seed_stock", row.sell), ("seed_buy_orders", row.buy)] {
                    if let Some(price) = price {
                        conn.execute(&format!("INSERT INTO {table} (station_id,solar_system_id,constellation_id,region_id,type_id,price,quantity,initial_quantity,price_version,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?7,1,'fixture')"),
                            rusqlite::params![station.npc.station_id,station.npc.solar_system_id,station.npc.constellation_id,station.npc.region_id,row.type_id,price,row.quantity]).unwrap();
                    }
                }
            }
        }
        p.verify_database(&conn).unwrap();
        let preview = p.preview(25).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM seed_stock)+(SELECT count(*) FROM seed_buy_orders)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(preview["total_seed_rows"], count);
        conn.execute(
            "UPDATE seed_stock SET price=price+0.01 WHERE station_id=700",
            [],
        )
        .unwrap();
        assert!(p.verify_database(&conn).is_err());
    }

    #[test]
    fn overlapping_sibling_distribution_overrides_fail_closed() {
        let (i, q, mut scopes) = fixture();
        scopes.push(Scope {
            id: "other_fact".into(),
            label: "Another fact".into(),
            priority_offset: 10,
            parent: Some("resources".into()),
            members: (10..=30).collect(),
        });
        let mut d = Document::default();
        for id in ["ore", "other_fact"] {
            d.overrides.insert(
                id.into(),
                Override {
                    assortment: Some(Range { min: 40., max: 50. }),
                    stock: None,
                },
            );
        }
        assert!(Plan::compile(d, q, &i, &scopes, json!({})).is_err());
    }
}
