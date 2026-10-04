//! Versioned, item-level Market policy. Distribution and pricing evidence live elsewhere.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

pub const FORMAT_VERSION: u32 = 1;
pub const FACT_REGISTRY_VERSION: u32 = 1;

/// A deliberately finite registry. Each tag must be computed from the pinned catalog and
/// associated inputs; absence from a caller's complete true-fact set means false.
pub const FACTS: &[&str] = &[
    "approved_reusable_bpo",
    "authoritative_compressed_ore",
    "core_battlecruiser",
    "core_battleship",
    "core_bucket",
    "core_capital_structure_component",
    "core_cruiser",
    "core_destroyer",
    "core_gas_ice_salvage",
    "core_general",
    "core_ice_ore",
    "core_minerals",
    "core_orca_freighter_structure",
    "core_raw_ore",
    "core_t1_module_ammo_rig",
    "direct_lp_reward",
    "finished_t2",
    "finished_t3",
    "invention_lineage",
    "lp_blueprint_product",
    "marketable",
    "mineable_moon_resource",
    "published_reaction_formula",
    "rare_deadspace_ded",
    "rare_faction_lp",
    "rare_officer",
    "special_reward_placeholder_hull",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDocument {
    pub format_version: u32,
    pub catalog_contract: CatalogContract,
    pub profiles: Vec<Profile>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogContract {
    pub sde_build: u64,
    pub fact_registry_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub sides: Sides,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sell: Option<SidePolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buy: Option<SidePolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sides {
    Unseeded,
    SellOnly,
    BuyOnly,
    BuySell,
}

impl Sides {
    pub fn has_sell(self) -> bool {
        matches!(self, Self::SellOnly | Self::BuySell)
    }
    pub fn has_buy(self) -> bool {
        matches!(self, Self::BuyOnly | Self::BuySell)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    TqSnapshot,
    /// GENERAL_TQ only: the direct Sell reference may price Buy under an explicit multiplier.
    TqSnapshotSellFallback,
    /// GENERAL_TQ only: the direct Buy reference may price Sell under an explicit multiplier.
    TqSnapshotBuyFallback,
    /// GENERAL_TQ only: ESI average_price, never adjusted_price.
    TqAveragePrice,
    FundedCost,
    /// Proven NPC acquisition reference, reusable on either quote side.
    NpcAcquisition,
    /// Ordinary named/meta variant: quote the authoritative T1 parent's final side.
    T1VariantSell,
    T1VariantBuy,
    CoreManifestCost,
    CapturedMarket,
    NpcMinSell,
    BpoNpcOrBase,
    RareReference,
    SkillbookLadder,
    CommandCenterLadder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidePolicy {
    pub source: Source,
    pub multiplier: Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floor: Option<Decimal>,
}

/// Preserve the exact decimal lexeme while validating its finite numeric interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Decimal(pub String);

impl Decimal {
    pub fn value(&self) -> Result<f64> {
        ensure!(
            valid_decimal(&self.0),
            "invalid decimal string {:?}",
            self.0
        );
        let value: f64 = self.0.parse().context("invalid decimal value")?;
        ensure!(value.is_finite(), "nonfinite decimal {:?}", self.0);
        Ok(value)
    }
}

fn valid_decimal(text: &str) -> bool {
    let body = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = body
        .split_once('.')
        .map_or((body, None), |(a, b)| (a, Some(b)));
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.is_none_or(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub selector: Selector,
    pub priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sides: Option<Sides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sell: Option<SidePolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buy: Option<SidePolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selector {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_ids: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_ids: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_ids: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fact: Option<String>,
}

impl Selector {
    pub fn specificity(&self) -> u8 {
        if self.type_ids.is_some() {
            3
        } else if self.group_ids.is_some() {
            2
        } else if self.category_ids.is_some() {
            1
        } else {
            0
        }
    }

    fn validate(&self) -> Result<()> {
        let forms = [
            self.type_ids.is_some(),
            self.group_ids.is_some(),
            self.category_ids.is_some(),
            self.fact.is_some(),
        ];
        ensure!(
            forms.into_iter().filter(|yes| *yes).count() == 1,
            "selector must contain exactly one of type_ids, group_ids, category_ids, fact"
        );
        for ids in [&self.type_ids, &self.group_ids, &self.category_ids]
            .into_iter()
            .flatten()
        {
            ensure!(!ids.is_empty(), "selector ID list cannot be empty");
            let mut seen = BTreeSet::new();
            for id in ids {
                ensure!(
                    *id > 0 && seen.insert(*id),
                    "invalid or duplicate selector ID {id}"
                );
            }
        }
        if let Some(fact) = &self.fact {
            ensure!(
                FACTS.contains(&fact.as_str()),
                "unsupported policy fact {fact}"
            );
        }
        Ok(())
    }
}

impl PolicyDocument {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("reading policy {}", path.display()))?;
        let text = std::str::from_utf8(&bytes).context("policy must be UTF-8")?;
        Self::parse(text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        // Value's ordinary JSON deserializer discards duplicate keys. First pass below rejects
        // them recursively, including keys in objects not represented by a typed field.
        let mut policy: Self =
            serde_json::from_value(strict_json(text).context("invalid policy JSON")?)
                .context("invalid policy schema")?;
        policy.validate()?;
        policy.canonicalize();
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == FORMAT_VERSION,
            "unsupported policy format_version {}",
            self.format_version
        );
        ensure!(
            self.catalog_contract.sde_build > 0,
            "sde_build must be positive"
        );
        ensure!(
            self.catalog_contract.fact_registry_version == FACT_REGISTRY_VERSION,
            "unsupported fact_registry_version {}",
            self.catalog_contract.fact_registry_version
        );
        let mut profiles = BTreeMap::new();
        for profile in &self.profiles {
            validate_id(&profile.id)?;
            ensure!(
                profiles.insert(&profile.id, profile).is_none(),
                "duplicate profile ID {}",
                profile.id
            );
            ensure!(
                profile.sides != Sides::Unseeded,
                "profile {} cannot be unseeded",
                profile.id
            );
            validate_sides(
                profile.sides,
                &profile.sell.as_ref(),
                &profile.buy.as_ref(),
                &profile.id,
            )?;
        }
        let mut rules = BTreeSet::new();
        for rule in &self.rules {
            validate_id(&rule.id)?;
            ensure!(rules.insert(&rule.id), "duplicate rule ID {}", rule.id);
            rule.selector
                .validate()
                .with_context(|| format!("rule {}", rule.id))?;
            if let Some(id) = &rule.profile {
                ensure!(
                    profiles.contains_key(id),
                    "rule {} references missing profile {}",
                    rule.id,
                    id
                );
            }
            if rule.sides == Some(Sides::Unseeded) {
                ensure!(
                    rule.sell.is_none() && rule.buy.is_none(),
                    "unseeded rule {} cannot declare side objects",
                    rule.id
                );
            }
            let base = rule
                .profile
                .as_ref()
                .and_then(|id| profiles.get(id).copied());
            let sides = rule
                .sides
                .or_else(|| base.map(|p| p.sides))
                .ok_or_else(|| anyhow::anyhow!("rule {} needs a profile or sides", rule.id))?;
            let sell = if sides == Sides::Unseeded {
                None
            } else {
                rule.sell
                    .as_ref()
                    .or_else(|| base.and_then(|p| p.sell.as_ref()))
            };
            let buy = if sides == Sides::Unseeded {
                None
            } else {
                rule.buy
                    .as_ref()
                    .or_else(|| base.and_then(|p| p.buy.as_ref()))
            };
            ensure!(
                !(rule.sell.is_some() && !sides.has_sell())
                    && !(rule.buy.is_some() && !sides.has_buy()),
                "rule {} declares a disabled side",
                rule.id
            );
            validate_sides(sides, &sell, &buy, &rule.id)?;
        }
        Ok(())
    }

    pub fn validate_catalog_contract(&self, actual_sde_build: u64) -> Result<()> {
        ensure!(
            self.catalog_contract.sde_build == actual_sde_build,
            "policy SDE build {} differs from catalog build {}",
            self.catalog_contract.sde_build,
            actual_sde_build
        );
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<String> {
        self.validate()?;
        let mut copy = self.clone();
        copy.canonicalize();
        let mut json = serde_json::to_string_pretty(&copy)?;
        json.push('\n');
        Ok(json)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        fs::write(path, self.canonical_json()?)
            .with_context(|| format!("writing policy {}", path.display()))
    }

    fn canonicalize(&mut self) {
        self.profiles.sort_by(|a, b| a.id.cmp(&b.id));
        self.rules.sort_by(|a, b| a.id.cmp(&b.id));
        for rule in &mut self.rules {
            for ids in [
                &mut rule.selector.type_ids,
                &mut rule.selector.group_ids,
                &mut rule.selector.category_ids,
            ]
            .into_iter()
            .flatten()
            {
                ids.sort_unstable();
            }
        }
    }

    pub(crate) fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }
}

fn validate_id(id: &str) -> Result<()> {
    let mut chars = id.bytes();
    ensure!(
        chars.next().is_some_and(|b| b.is_ascii_lowercase())
            && chars.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "invalid stable snake_case ID {id:?}"
    );
    Ok(())
}

fn validate_sides<'a>(
    sides: Sides,
    sell: &Option<&'a SidePolicy>,
    buy: &Option<&'a SidePolicy>,
    context: &str,
) -> Result<()> {
    ensure!(
        sell.is_some() == sides.has_sell(),
        "{context}: Sell declaration disagrees with sides"
    );
    ensure!(
        buy.is_some() == sides.has_buy(),
        "{context}: Buy declaration disagrees with sides"
    );
    for side in [sell, buy].into_iter().flatten() {
        let multiplier = side.multiplier.value()?;
        ensure!(multiplier > 0.0, "{context}: multiplier must be positive");
    }
    if let Some(buy) = buy {
        ensure!(
            matches!(
                buy.source,
                Source::TqSnapshot
                    | Source::TqSnapshotSellFallback
                    | Source::TqAveragePrice
                    | Source::FundedCost
                    | Source::NpcAcquisition
                    | Source::T1VariantBuy
                    | Source::CoreManifestCost
                    | Source::CapturedMarket
                    | Source::RareReference
            ),
            "{context}: source {:?} cannot price Buy",
            buy.source
        );
    }
    if let Some(sell) = sell {
        ensure!(
            !matches!(
                sell.source,
                Source::TqSnapshotSellFallback | Source::T1VariantBuy
            ),
            "{context}: selected source may price Buy only"
        );
        if let Some(floor) = &sell.floor {
            ensure!(
                floor.value()? >= 0.0,
                "{context}: Sell floor must be nonnegative"
            );
        }
    }
    ensure!(
        buy.is_none_or(|side| side.floor.is_none()),
        "{context}: Buy floor is unsupported"
    );
    Ok(())
}

/// Recursive duplicate-detecting JSON tree. Conversion happens only after every map is checked.
#[derive(Debug)]
enum StrictValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<StrictValue>),
    Object(BTreeMap<String, StrictValue>),
}

/// Shared strict JSON reader for policy and portable preset containers.
pub(crate) fn strict_json(text: &str) -> Result<serde_json::Value> {
    let mut de = serde_json::Deserializer::from_str(text);
    let value = StrictValue::deserialize(&mut de)?;
    de.end().context("trailing data after JSON")?;
    Ok(value.into_json())
}

impl StrictValue {
    fn into_json(self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(v) => v.into(),
            Self::Number(v) => v.into(),
            Self::String(v) => v.into(),
            Self::Array(v) => {
                serde_json::Value::Array(v.into_iter().map(Self::into_json).collect())
            }
            Self::Object(v) => {
                serde_json::Value::Object(v.into_iter().map(|(k, v)| (k, v.into_json())).collect())
            }
        }
    }
}

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue::Null)
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue::Bool(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue::Number(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue::Number(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(StrictValue::Number)
                    .ok_or_else(|| E::custom("nonfinite number"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue::String(v.into()))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue::String(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element()? {
                    values.push(value);
                }
                Ok(StrictValue::Array(values))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, StrictValue>()? {
                    if values.insert(key.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON key {key:?}"
                        )));
                    }
                }
                Ok(StrictValue::Object(values))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{"format_version":1,"catalog_contract":{"sde_build":3396210,"fact_registry_version":1},"profiles":[{"id":"alpha","sides":"buy_sell","sell":{"source":"funded_cost","multiplier":"1.250"},"buy":{"source":"funded_cost","multiplier":"0.95"}}],"rules":[{"id":"alpha_types","selector":{"type_ids":[2,1]},"priority":10,"profile":"alpha"}]}"#;

    #[test]
    fn tq_snapshot_parses_for_independent_sides() {
        let json = MINIMAL.replace("funded_cost", "tq_snapshot");
        let policy = PolicyDocument::parse(&json).unwrap();
        let serialized = policy.canonical_json().unwrap();
        assert_eq!(serialized.matches("tq_snapshot").count(), 2);
    }

    #[test]
    fn canonical_round_trip_preserves_decimal_lexeme() {
        let first = PolicyDocument::parse(MINIMAL)
            .unwrap()
            .canonical_json()
            .unwrap();
        let second = PolicyDocument::parse(&first)
            .unwrap()
            .canonical_json()
            .unwrap();
        assert_eq!(first, second);
        assert!(first.contains("\"1.250\""));
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(
            value["rules"][0]["selector"]["type_ids"],
            serde_json::json!([1, 2])
        );
        assert!(first.ends_with('\n'));
    }

    #[test]
    fn rejects_duplicate_nested_and_unknown_keys() {
        let duplicate = MINIMAL.replace("\"priority\":10", "\"priority\":10,\"priority\":20");
        assert!(PolicyDocument::parse(&duplicate).is_err());
        let unknown = MINIMAL.replace(
            "\"source\":\"funded_cost\"",
            "\"source\":\"funded_cost\",\"station_id\":1",
        );
        assert!(PolicyDocument::parse(&unknown).is_err());
    }

    #[test]
    fn rejects_disabled_side_and_future_version() {
        let disabled = MINIMAL.replace("\"sides\":\"buy_sell\"", "\"sides\":\"sell_only\"");
        assert!(PolicyDocument::parse(&disabled).is_err());
        let future = MINIMAL.replace("\"format_version\":1", "\"format_version\":2");
        assert!(PolicyDocument::parse(&future).is_err());
        let malformed = MINIMAL.replace("\"1.250\"", "\"1e3\"");
        assert!(PolicyDocument::parse(&malformed).is_err());
        let impossible_source = MINIMAL.replace(
            "\"source\":\"funded_cost\",\"multiplier\":\"0.95\"",
            "\"source\":\"npc_min_sell\",\"multiplier\":\"0.95\"",
        );
        assert!(PolicyDocument::parse(&impossible_source).is_err());
    }

    #[test]
    fn migrated_policy_canonical_round_trip() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/market-policy-v2.json");
        let policy = PolicyDocument::load(&path).unwrap();
        let canonical = policy.canonical_json().unwrap();
        assert_eq!(
            PolicyDocument::parse(&canonical)
                .unwrap()
                .canonical_json()
                .unwrap(),
            canonical
        );
        assert_eq!(policy.profiles.len(), 24);
    }
}
