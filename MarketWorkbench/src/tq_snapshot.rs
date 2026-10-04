//! Prepared offline TQ references. Current snapshots use Jita best Buy/Sell.
//! Historical five-hub datasets remain readable for existing candidate audits.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::policy::Source;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const JITA_AGGREGATION: &str = "jita_exact_station_best_orders";
const JITA_RANGE: &str = "jita_sell_station_buy_in_range_best_orders";
const JITA_BLEND: &str = "jita_snapshot_history_50_50";
const HISTORICAL_MEDIAN: &str = "median_of_exact_canonical_station_best_orders";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Sell,
    Buy,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Dataset {
    format_version: u32,
    #[serde(rename = "captureID")]
    capture_id: String,
    captured_at: String,
    aggregation: String,
    hubs: BTreeMap<String, (u64, u32)>,
    types: Vec<Record>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    #[serde(rename = "typeID")]
    pub type_id: u32,
    pub name: String,
    pub captured_at: String,
    pub average_price_fallback_candidate: Option<String>,
    pub adjusted_price_industry_only: Option<String>,
    pub sell_reference: Option<String>,
    pub buy_reference: Option<String>,
    pub sell_hub_count: usize,
    pub buy_hub_count: usize,
    pub aggregation: String,
    pub hubs: Vec<Hub>,
    pub state: String,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub history_blend: Option<HistoryBlend>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HistoryBlend {
    #[serde(rename = "regionID")]
    pub region_id: u32,
    pub as_of_date: String,
    pub history_captured_at: String,
    #[serde(rename = "cacheSHA256")]
    pub cache_sha256: String,
    pub statistic: String,
    pub weight: String,
    pub snapshot_buy: Option<String>,
    pub snapshot_sell: Option<String>,
    pub anchor: Option<String>,
    pub window_days: Option<u32>,
    pub trading_days: usize,
    pub total_traded_volume: u64,
    pub last_trade_date: Option<String>,
    pub observations: Vec<HistoryDay>,
    pub mode: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HistoryDay {
    pub date: String,
    pub average: String,
    pub volume: u64,
    pub order_count: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hub {
    pub hub: String,
    #[serde(rename = "stationID")]
    pub station_id: u64,
    #[serde(rename = "regionID")]
    pub region_id: u32,
    pub sell: BookSide,
    pub buy: BookSide,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookSide {
    pub best_price: Option<String>,
    #[serde(rename = "bestOrderID")]
    pub best_order_id: Option<u64>,
    pub order_count: usize,
    pub quoted_volume: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_order: Option<BestOrder>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BestOrder {
    #[serde(rename = "locationID")]
    pub location_id: u64,
    #[serde(rename = "systemID")]
    pub system_id: u32,
    pub range: String,
    pub jumps_to_reference: Option<u32>,
    pub min_volume: u64,
}

#[derive(Debug, Clone)]
pub struct TqSnapshot {
    pub capture_id: String,
    pub captured_at: String,
    pub aggregation: String,
    pub records: BTreeMap<u32, Record>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceTrace {
    pub source: &'static str,
    pub side: Side,
    #[serde(rename = "typeID")]
    pub type_id: u32,
    pub reference: Option<String>,
    pub aggregation: String,
    pub observations: Vec<HubObservation>,
    pub captured_at: String,
    pub capture_id: String,
    pub state: String,
    pub warnings: Vec<String>,
    pub unresolved_reason: Option<&'static str>,
    pub history_blend: Option<HistoryBlend>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HubObservation {
    pub hub: String,
    #[serde(rename = "stationID")]
    pub station_id: u64,
    #[serde(rename = "regionID")]
    pub region_id: u32,
    pub best_price: String,
    #[serde(rename = "bestOrderID")]
    pub best_order_id: Option<u64>,
    pub order_count: usize,
    pub quoted_volume: u64,
    pub best_order: Option<BestOrder>,
}

impl TqSnapshot {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            fs::read(path).with_context(|| format!("reading TQ snapshot {}", path.display()))?;
        Self::parse(&bytes)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let dataset: Dataset =
            serde_json::from_slice(bytes).context("parsing TQ reference dataset")?;
        ensure!(
            dataset.format_version == 1,
            "unsupported TQ dataset formatVersion {}",
            dataset.format_version
        );
        ensure!(
            matches!(
                dataset.aggregation.as_str(),
                JITA_AGGREGATION | JITA_RANGE | JITA_BLEND | HISTORICAL_MEDIAN
            ),
            "unsupported TQ aggregation"
        );
        ensure!(
            dataset.hubs.len() == 5,
            "TQ reference must contain five canonical hubs"
        );
        for (name, station, region) in [
            ("jita", 60_003_760, 10_000_002),
            ("amarr", 60_008_494, 10_000_043),
            ("dodixie", 60_011_866, 10_000_032),
            ("rens", 60_004_588, 10_000_030),
            ("hek", 60_005_686, 10_000_042),
        ] {
            ensure!(
                dataset.hubs.get(name) == Some(&(station, region)),
                "TQ reference hub identity mismatch: {name}"
            );
        }
        let mut records = BTreeMap::new();
        for record in dataset.types {
            ensure!(
                record.captured_at == dataset.captured_at,
                "type {} capture timestamp mismatch",
                record.type_id
            );
            ensure!(
                record.aggregation == dataset.aggregation,
                "type {} aggregation mismatch",
                record.type_id
            );
            for side in [Side::Sell, Side::Buy] {
                let value = match side {
                    Side::Sell => &record.sell_reference,
                    Side::Buy => &record.buy_reference,
                };
                if let Some(text) = value {
                    let price: f64 = text.parse().context("invalid TQ reference decimal")?;
                    ensure!(
                        price.is_finite() && price > 0.0,
                        "type {} invalid TQ reference",
                        record.type_id
                    );
                }
            }
            if dataset.aggregation != HISTORICAL_MEDIAN {
                validate_jita_record(&record)?;
            }
            ensure!(
                records.insert(record.type_id, record).is_none(),
                "duplicate TQ type ID"
            );
        }
        Ok(Self {
            capture_id: dataset.capture_id,
            captured_at: dataset.captured_at,
            aggregation: dataset.aggregation,
            records,
        })
    }

    /// A missing side stays unresolved, regardless of average/adjusted price evidence.
    pub fn reference(&self, type_id: u32, side: Side) -> Option<f64> {
        let record = self.records.get(&type_id)?;
        let value = match side {
            Side::Sell => &record.sell_reference,
            Side::Buy => &record.buy_reference,
        };
        value.as_ref()?.parse().ok()
    }

    /// Explicit preset source contracts. The direct tq_snapshot source above never falls back.
    pub fn policy_reference(&self, type_id: u32, side: Side, source: Source) -> Option<f64> {
        let record = self.records.get(&type_id)?;
        match (source, side, record.state.as_str()) {
            (Source::TqSnapshot, side, _) => self.reference(type_id, side),
            (Source::TqSnapshotSellFallback, Side::Buy, "DIRECT_SELL_ONLY") => {
                record.sell_reference.as_ref()?.parse().ok()
            }
            (Source::TqSnapshotBuyFallback, Side::Sell, "DIRECT_BUY_ONLY") => {
                record.buy_reference.as_ref()?.parse().ok()
            }
            (Source::TqAveragePrice, _, "AVERAGE_ONLY") => record
                .average_price_fallback_candidate
                .as_ref()?
                .parse()
                .ok(),
            _ => None,
        }
    }

    pub fn trace(&self, type_id: u32, side: Side) -> Option<PriceTrace> {
        let record = self.records.get(&type_id)?;
        let reference = match side {
            Side::Sell => record.sell_reference.clone(),
            Side::Buy => record.buy_reference.clone(),
        };
        let observations = record
            .hubs
            .iter()
            .filter(|hub| record.aggregation == HISTORICAL_MEDIAN || hub.hub == "jita")
            .filter_map(|hub| {
                let book = match side {
                    Side::Sell => &hub.sell,
                    Side::Buy => &hub.buy,
                };
                Some(HubObservation {
                    hub: hub.hub.clone(),
                    station_id: hub.station_id,
                    region_id: hub.region_id,
                    best_price: book.best_price.clone()?,
                    best_order_id: book.best_order_id,
                    order_count: book.order_count,
                    quoted_volume: book.quoted_volume,
                    best_order: book.best_order.clone(),
                })
            })
            .collect();
        Some(PriceTrace {
            source: "tq_snapshot",
            side,
            type_id,
            reference: reference.clone(),
            aggregation: record.aggregation.clone(),
            observations,
            captured_at: record.captured_at.clone(),
            capture_id: self.capture_id.clone(),
            state: record.state.clone(),
            warnings: record.warnings.clone(),
            history_blend: record.history_blend.clone(),
            unresolved_reason: if reference.is_none() {
                Some("NO_DIRECT_SIDE_REFERENCE")
            } else {
                None
            },
        })
    }
}

fn validate_jita_record(record: &Record) -> Result<()> {
    let jita: Vec<_> = record.hubs.iter().filter(|hub| hub.hub == "jita").collect();
    ensure!(
        jita.len() == 1,
        "type {} must have exactly one Jita observation",
        record.type_id
    );
    let jita = jita[0];
    ensure!(
        jita.station_id == 60_003_760 && jita.region_id == 10_000_002,
        "type {} Jita observation identity mismatch",
        record.type_id
    );
    let history = record.history_blend.as_ref();
    if record.aggregation == JITA_BLEND {
        validate_history(
            record,
            history.context("blended dataset missing history evidence")?,
        )?;
    } else {
        ensure!(
            history.is_none(),
            "non-blended dataset contains history blend"
        );
    }
    for (side, reference, book, count) in [
        (
            Side::Sell,
            &record.sell_reference,
            &jita.sell,
            record.sell_hub_count,
        ),
        (
            Side::Buy,
            &record.buy_reference,
            &jita.buy,
            record.buy_hub_count,
        ),
    ] {
        let snapshot = if let Some(h) = history {
            match side {
                Side::Sell => &h.snapshot_sell,
                Side::Buy => &h.snapshot_buy,
            }
        } else {
            reference
        };
        ensure!(
            *snapshot == book.best_price && count == usize::from(snapshot.is_some()),
            "type {} reference is not its pinned Jita best order",
            record.type_id
        );
        ensure!(
            reference.is_some() == snapshot.is_some(),
            "history invented a missing snapshot side"
        );
        if let (Some(h), Some(raw), Some(final_price)) = (history, snapshot, reference) {
            let raw: f64 = raw.parse()?;
            let expected = match &h.anchor {
                Some(a) => (raw + a.parse::<f64>()?) / 2.,
                None => raw,
            };
            let actual: f64 = final_price.parse()?;
            ensure!(
                (actual - expected).abs() <= expected.abs() * f64::EPSILON * 4.,
                "invalid 50/50 history blend for type {}",
                record.type_id
            );
        }
        if reference.is_some() {
            ensure!(
                book.order_count > 0 && book.quoted_volume > 0 && book.best_order_id.is_some(),
                "type {} missing Jita order provenance",
                record.type_id
            );
            if record.aggregation != JITA_AGGREGATION {
                let order = book
                    .best_order
                    .as_ref()
                    .context("missing range-aware order provenance")?;
                let eligible = match side {
                    Side::Sell => order.location_id == 60_003_760 && order.system_id == 30_000_142,
                    Side::Buy => match order.range.as_str() {
                        "station" => order.location_id == 60_003_760,
                        "solarsystem" => order.system_id == 30_000_142,
                        "region" => true,
                        "1" | "2" | "3" | "4" | "5" | "10" | "20" | "30" | "40" => order
                            .jumps_to_reference
                            .is_some_and(|j| j <= order.range.parse().unwrap()),
                        _ => false,
                    },
                };
                ensure!(
                    eligible,
                    "best order does not cover Jita for type {}",
                    record.type_id
                );
            }
        }
    }
    let state = match (&record.sell_reference, &record.buy_reference) {
        (Some(sell), Some(buy)) if buy.parse::<f64>()? >= sell.parse::<f64>()? => {
            "CROSSED_REFERENCE"
        }
        (Some(_), Some(_)) => "DIRECT_BOTH",
        (Some(_), None) => "DIRECT_SELL_ONLY",
        (None, Some(_)) => "DIRECT_BUY_ONLY",
        (None, None) if record.average_price_fallback_candidate.is_some() => "AVERAGE_ONLY",
        (None, None) => "NO_TQ_REFERENCE",
    };
    ensure!(
        record.state == state,
        "type {} Jita reference state mismatch",
        record.type_id
    );
    Ok(())
}

fn validate_history(record: &Record, h: &HistoryBlend) -> Result<()> {
    ensure!(
        h.region_id == 10_000_002
            && h.weight == "0.5"
            && h.statistic == "median_of_daily_executed_average",
        "unsupported history blend contract"
    );
    ensure!(
        record.captured_at.starts_with(&h.as_of_date)
            && h.as_of_date.len() == 10
            && !h.history_captured_at.is_empty(),
        "history as-of date mismatch"
    );
    ensure!(
        h.cache_sha256.len() == 64 && h.cache_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "history cache SHA missing"
    );
    let fmt = time::macros::format_description!("[year]-[month]-[day]");
    let as_of = time::Date::parse(&h.as_of_date, fmt)?;
    if let Some(anchor) = &h.anchor {
        let days = h.window_days.context("missing history window")?;
        let minimum = match days {
            7 | 30 => 3,
            180 => 2,
            365 => 1,
            _ => anyhow::bail!("unsupported history window"),
        };
        ensure!(
            h.mode == "BLENDED"
                && h.trading_days == h.observations.len()
                && h.trading_days >= minimum,
            "insufficient history observations"
        );
        let mut averages = Vec::new();
        let mut volume = 0u64;
        let mut previous = None;
        for row in &h.observations {
            let date = time::Date::parse(&row.date, fmt)?;
            ensure!(
                date < as_of
                    && date >= as_of - time::Duration::days(i64::from(days))
                    && previous.is_none_or(|p| date > p),
                "history observation outside window or unsorted"
            );
            previous = Some(date);
            let value: f64 = row.average.parse()?;
            ensure!(
                value.is_finite() && value > 0. && row.volume > 0 && row.order_count > 0,
                "invalid executed history observation"
            );
            averages.push(value);
            volume = volume
                .checked_add(row.volume)
                .context("history volume overflow")?;
        }
        ensure!(
            h.total_traded_volume == volume
                && h.last_trade_date.as_ref() == h.observations.last().map(|r| &r.date),
            "history summary mismatch"
        );
        averages.sort_by(f64::total_cmp);
        let n = averages.len();
        let median = if n % 2 == 0 {
            (averages[n / 2 - 1] + averages[n / 2]) / 2.
        } else {
            averages[n / 2]
        };
        let actual: f64 = anchor.parse()?;
        ensure!(
            (actual - median).abs() <= median.abs() * f64::EPSILON * 4.,
            "history anchor differs from daily median"
        );
    } else {
        ensure!(
            h.mode == "SNAPSHOT_ONLY_NO_HISTORY"
                && h.window_days.is_none()
                && h.trading_days == 0
                && h.observations.is_empty()
                && h.total_traded_volume == 0
                && h.last_trade_date.is_none(),
            "invalid snapshot-only history state"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blend_fixture() -> serde_json::Value {
        let sell = serde_json::json!({"bestPrice":"120","bestOrderID":1,"orderCount":1,"quotedVolume":100,
            "bestOrder":{"locationID":60003760,"systemID":30000142,"range":"station","jumpsToReference":0,"minVolume":1}});
        let buy = serde_json::json!({"bestPrice":"80","bestOrderID":2,"orderCount":2,"quotedVolume":100,
            "bestOrder":{"locationID":1042508032148u64,"systemID":30000144,"range":"1","jumpsToReference":1,"minVolume":1}});
        let history = serde_json::from_str::<serde_json::Value>(r#"{
            "regionID":10000002,"asOfDate":"2026-09-28","historyCapturedAt":"2026-10-03T00:00:00Z",
            "cacheSHA256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","statistic":"median_of_daily_executed_average","weight":"0.5",
            "snapshotBuy":"80","snapshotSell":"120","anchor":"102","windowDays":7,"tradingDays":3,"totalTradedVolume":3,
            "lastTradeDate":"2026-09-27","mode":"BLENDED","observations":[
                {"date":"2026-09-25","average":"100","volume":1,"orderCount":1},
                {"date":"2026-09-26","average":"102","volume":1,"orderCount":1},
                {"date":"2026-09-27","average":"9000","volume":1,"orderCount":1}]}"#).unwrap();
        let mut record = serde_json::from_str::<serde_json::Value>(r#"{
            "typeID":34,"name":"test","capturedAt":"2026-09-28T00:00:00Z","aggregation":"jita_snapshot_history_50_50",
            "sellReference":"111","buyReference":"91","sellHubCount":1,"buyHubCount":1,
            "averagePriceFallbackCandidate":"999","adjustedPriceIndustryOnly":"888","state":"DIRECT_BOTH","warnings":[]}"#).unwrap();
        record["historyBlend"] = history;
        record["hubs"] = serde_json::json!([{"hub":"jita","stationID":60003760,"regionID":10000002,"sell":sell,"buy":buy}]);
        serde_json::json!({"formatVersion":1,"captureID":"pin","capturedAt":"2026-09-28T00:00:00Z",
            "aggregation":JITA_BLEND,"hubs":{"jita":[60003760,10000002],"amarr":[60008494,10000043],"dodixie":[60011866,10000032],"rens":[60004588,10000030],"hek":[60005686,10000042]},"types":[record]})
    }

    #[test]
    fn range_and_history_blend_are_validated_and_explained() {
        let value = blend_fixture();
        let book = TqSnapshot::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(book.reference(34, Side::Buy), Some(91.));
        assert_eq!(book.reference(34, Side::Sell), Some(111.));
        let trace = book.trace(34, Side::Buy).unwrap();
        assert_eq!(trace.history_blend.unwrap().window_days, Some(7));
        assert_eq!(
            trace.observations[0]
                .best_order
                .as_ref()
                .unwrap()
                .jumps_to_reference,
            Some(1)
        );
        assert_eq!(
            book.policy_reference(34, Side::Buy, Source::TqAveragePrice),
            None
        );
        for (pointer, bad) in [
            ("/types/0/buyReference", serde_json::json!("90")),
            ("/types/0/historyBlend/anchor", serde_json::json!("999")),
            ("/types/0/historyBlend/windowDays", serde_json::json!(14)),
            (
                "/types/0/historyBlend/observations/0/volume",
                serde_json::json!(0),
            ),
            (
                "/types/0/historyBlend/observations/0/date",
                serde_json::json!("2026-09-28"),
            ),
            (
                "/types/0/hubs/0/buy/bestOrder/jumpsToReference",
                serde_json::json!(2),
            ),
            (
                "/types/0/hubs/0/sell/bestOrder/locationID",
                serde_json::json!(60008494),
            ),
        ] {
            let mut bad_value = value.clone();
            *bad_value.pointer_mut(pointer).unwrap() = bad;
            assert!(
                TqSnapshot::parse(&serde_json::to_vec(&bad_value).unwrap()).is_err(),
                "accepted {pointer}"
            );
        }
    }

    #[test]
    fn blended_crossed_reference_is_flagged_but_source_still_resolves() {
        let mut value = blend_fixture();
        value["types"][0]["hubs"][0]["buy"]["bestPrice"] = serde_json::json!("140");
        value["types"][0]["historyBlend"]["snapshotBuy"] = serde_json::json!("140");
        value["types"][0]["buyReference"] = serde_json::json!("121");
        value["types"][0]["state"] = serde_json::json!("CROSSED_REFERENCE");
        let book = TqSnapshot::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(book.reference(34, Side::Buy), Some(121.));
        assert_eq!(
            book.trace(34, Side::Buy).unwrap().state,
            "CROSSED_REFERENCE"
        );
    }

    #[test]
    fn blended_missing_side_and_snapshot_only_are_explicit() {
        let mut value = blend_fixture();
        value["types"][0]["hubs"][0]["buy"] = serde_json::json!({"bestPrice":null,"bestOrderID":null,"orderCount":0,"quotedVolume":0});
        value["types"][0]["buyReference"] = serde_json::Value::Null;
        value["types"][0]["historyBlend"]["snapshotBuy"] = serde_json::Value::Null;
        value["types"][0]["buyHubCount"] = serde_json::json!(0);
        value["types"][0]["state"] = serde_json::json!("DIRECT_SELL_ONLY");
        let book = TqSnapshot::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(book.reference(34, Side::Buy), None);
        let h = &mut value["types"][0]["historyBlend"];
        h["mode"] = serde_json::json!("SNAPSHOT_ONLY_NO_HISTORY");
        for field in ["anchor", "windowDays", "lastTradeDate"] {
            h[field] = serde_json::Value::Null;
        }
        h["tradingDays"] = serde_json::json!(0);
        h["totalTradedVolume"] = serde_json::json!(0);
        h["observations"] = serde_json::json!([]);
        value["types"][0]["sellReference"] = serde_json::json!("120");
        let book = TqSnapshot::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(book.reference(34, Side::Sell), Some(120.));
        assert_eq!(book.reference(34, Side::Buy), None);
    }

    fn fixture(
        sell: Option<&str>,
        buy: Option<&str>,
        average: Option<&str>,
        adjusted: Option<&str>,
    ) -> TqSnapshot {
        let state = match (sell, buy, average) {
            (Some(_), Some(_), _) => "DIRECT_BOTH",
            (Some(_), None, _) => "DIRECT_SELL_ONLY",
            (None, Some(_), _) => "DIRECT_BUY_ONLY",
            (None, None, Some(_)) => "AVERAGE_ONLY",
            (None, None, None) => "NO_TQ_REFERENCE",
        };
        let value = serde_json::json!({"formatVersion":1,"captureID":"capture", "capturedAt":"2026-09-28T00:00:00Z",
            "aggregation":"median_of_exact_canonical_station_best_orders", "hubs":{"jita":[60003760,10000002],"amarr":[60008494,10000043],"dodixie":[60011866,10000032],"rens":[60004588,10000030],"hek":[60005686,10000042]},
            "types":[{"typeID":34,"name":"Tritanium","capturedAt":"2026-09-28T00:00:00Z","averagePriceFallbackCandidate":average,
                "adjustedPriceIndustryOnly":adjusted,"sellReference":sell,"buyReference":buy,"sellHubCount":1,"buyHubCount":1,
                "aggregation":"median_of_exact_canonical_station_best_orders","hubs":[],"state":state,"warnings":[]}]});
        TqSnapshot::parse(&serde_json::to_vec(&value).unwrap()).unwrap()
    }

    #[test]
    fn sides_are_independent_and_no_fallback() {
        let book = fixture(Some("42.5"), None, Some("99"), Some("88"));
        assert_eq!(book.reference(34, Side::Sell), Some(42.5));
        assert_eq!(book.reference(34, Side::Buy), None);
        assert_eq!(
            book.trace(34, Side::Buy).unwrap().unresolved_reason,
            Some("NO_DIRECT_SIDE_REFERENCE")
        );
        let industry_only = fixture(None, None, None, Some("88"));
        assert_eq!(industry_only.reference(34, Side::Sell), None);
    }

    #[test]
    fn trace_retains_provenance_and_state() {
        let book = fixture(Some("40"), Some("35"), None, None);
        let trace = book.trace(34, Side::Buy).unwrap();
        assert_eq!(trace.source, "tq_snapshot");
        assert_eq!(trace.reference.as_deref(), Some("35"));
        assert_eq!(trace.capture_id, "capture");
    }

    #[test]
    fn existing_pricing_applies_independent_multipliers_after_tq_reference() {
        let book = fixture(Some("100"), Some("80"), None, None);
        let sell = crate::build::SeedPricing {
            sell_multiplier: 1.25,
            buy_multiplier: 1.0,
            price_floor: 0.0,
        }
        .ask(book.reference(34, Side::Sell).unwrap());
        let buy = crate::build::SeedPricing {
            sell_multiplier: 1.0,
            buy_multiplier: 0.5,
            price_floor: 0.0,
        }
        .bid(book.reference(34, Side::Buy).unwrap());
        assert_eq!((sell, buy), (125.0, 40.0));
    }

    #[test]
    fn explicit_general_tq_fallback_contracts_do_not_change_direct_source() {
        let sell_only = fixture(Some("100"), None, Some("999"), Some("888"));
        assert_eq!(
            sell_only.policy_reference(34, Side::Buy, Source::TqSnapshot),
            None
        );
        assert_eq!(
            sell_only.policy_reference(34, Side::Buy, Source::TqSnapshotSellFallback),
            Some(100.0)
        );
        assert_eq!(
            sell_only.policy_reference(34, Side::Sell, Source::TqSnapshotSellFallback),
            None
        );
        let buy_only = fixture(None, Some("80"), Some("999"), None);
        assert_eq!(
            buy_only.policy_reference(34, Side::Sell, Source::TqSnapshotBuyFallback),
            Some(80.0)
        );
        let average_only = fixture(None, None, Some("50"), Some("888"));
        assert_eq!(
            average_only.policy_reference(34, Side::Buy, Source::TqAveragePrice),
            Some(50.0)
        );
        assert_eq!(
            average_only.policy_reference(34, Side::Sell, Source::TqSnapshot),
            None
        );
        let adjusted_only = fixture(None, None, None, Some("888"));
        assert_eq!(
            adjusted_only.policy_reference(34, Side::Buy, Source::TqAveragePrice),
            None
        );
    }

    #[test]
    fn future_format_fails() {
        let bytes = serde_json::to_vec(&serde_json::json!({"formatVersion":2,"captureID":"x","capturedAt":"x","aggregation":"median_of_exact_canonical_station_best_orders","hubs":{},"types":[]})).unwrap();
        assert!(TqSnapshot::parse(&bytes).is_err());
    }

    #[test]
    fn jita_snapshot_trace_excludes_informational_other_hubs() {
        let mut book = fixture(Some("4496"), Some("3108"), None, None);
        let record = book.records.get_mut(&34).unwrap();
        record.aggregation = JITA_AGGREGATION.into();
        let make_hub = |name: &str, station, region, buy: &str, sell: &str| Hub {
            hub: name.into(),
            station_id: station,
            region_id: region,
            buy: BookSide {
                best_price: Some(buy.into()),
                best_order_id: Some(1),
                order_count: 2,
                quoted_volume: 10,
                best_order: None,
            },
            sell: BookSide {
                best_price: Some(sell.into()),
                best_order_id: Some(2),
                order_count: 3,
                quoted_volume: 20,
                best_order: None,
            },
        };
        record.hubs = vec![
            make_hub("jita", 60_003_760, 10_000_002, "3108", "4496"),
            make_hub("dodixie", 60_011_866, 10_000_032, "32", "1"),
        ];
        validate_jita_record(record).unwrap();
        let trace = book.trace(34, Side::Buy).unwrap();
        assert_eq!(trace.reference.as_deref(), Some("3108"));
        assert_eq!(trace.aggregation, JITA_AGGREGATION);
        assert_eq!(trace.observations.len(), 1);
        assert_eq!(trace.observations[0].station_id, 60_003_760);
        let record = book.records.get_mut(&34).unwrap();
        record.buy_reference = Some("32".into());
        assert!(validate_jita_record(record).is_err());
    }

    #[test]
    fn jita_dataset_parses_and_missing_side_never_borrows_other_region() {
        let empty = serde_json::json!({"bestPrice":null,"bestOrderID":null,"orderCount":0,"quotedVolume":0});
        let sell = serde_json::json!({"bestPrice":"4496","bestOrderID":123,"orderCount":3,"quotedVolume":20});
        let buy_elsewhere =
            serde_json::json!({"bestPrice":"32","bestOrderID":124,"orderCount":1,"quotedVolume":1});
        let mut value = serde_json::json!({"formatVersion":1,"captureID":"capture","capturedAt":"timestamp",
          "aggregation":JITA_AGGREGATION,"hubs":{"jita":[60003760,10000002],"amarr":[60008494,10000043],"dodixie":[60011866,10000032],"rens":[60004588,10000030],"hek":[60005686,10000042]},
          "types":[{"typeID":34,"name":"Tritanium","capturedAt":"timestamp","aggregation":JITA_AGGREGATION,
             "sellReference":"4496","buyReference":null,"sellHubCount":1,"buyHubCount":0,
             "averagePriceFallbackCandidate":"999","adjustedPriceIndustryOnly":"888","state":"DIRECT_SELL_ONLY","warnings":[],
             "hubs":[{"hub":"jita","stationID":60003760,"regionID":10000002,"sell":sell,"buy":empty},
                     {"hub":"dodixie","stationID":60011866,"regionID":10000032,"sell":empty,"buy":buy_elsewhere}]}]});
        let book = TqSnapshot::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(book.reference(34, Side::Buy), None);
        assert!(book.trace(34, Side::Buy).unwrap().observations.is_empty());
        value["types"][0]["buyReference"] = serde_json::json!("32");
        assert!(TqSnapshot::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
}
