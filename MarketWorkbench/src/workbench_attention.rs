//! "Needs attention" spreadsheet round-trip.
//!
//! The GENERAL_TQ preset pins items that have no Tranquility reference off-market with
//! `{"id":"no_tq_reference","selector":{"type_ids":[…]},"sides":"unseeded"}`. The resolver
//! hoists *any* exact-type unseeded match ahead of the priority sort (`policy_resolve.rs`),
//! so adding a higher-priority rule alone cannot bring one of those items back: the
//! exclusion has to be lifted as well. This module exports the affected items with empty
//! price columns, and turns the edited file back into exact, explicitly priced overrides -
//! lifting the exclusion for exactly the types the operator priced.
//!
//! Nothing here invents a price: a row only takes effect when the operator typed one.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::policy::{Aggregate, Decimal, PolicyDocument, Rule, Selector, SidePolicy, Sides, Source};

/// Export columns, in order. The first eight describe the item; `set_*` is what the
/// operator fills in; `note` is free text carried into the generated rule description.
pub const COLUMNS: &[&str] = &[
    "type_id",
    "name",
    "group_name",
    "market_group_id",
    "tq_state",
    "reason",
    "current_sides",
    "current_sell_price",
    "current_buy_price",
    "set_sell_price",
    "set_buy_price",
    "note",
];

/// What one side of an import row asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum PriceInput {
    /// An absolute ISK quote typed by the operator.
    Manual { text: String, value: f64 },
    /// `family:<min|median|mean>`: borrow the same-side quote of the item's name family.
    Family(Aggregate),
}

impl PriceInput {
    /// What the operator wrote, for the preview report.
    pub fn text(&self) -> String {
        match self {
            Self::Manual { text, .. } => text.clone(),
            Self::Family(mode) => format!("family:{}", aggregate_name(*mode)),
        }
    }
}

fn aggregate_name(mode: Aggregate) -> &'static str {
    match mode {
        Aggregate::Min => "min",
        Aggregate::Median => "median",
        Aggregate::Mean => "mean",
    }
}

fn parse_aggregate(text: &str) -> Option<Aggregate> {
    match text.trim().to_ascii_lowercase().as_str() {
        "min" | "minimum" | "cheapest" => Some(Aggregate::Min),
        "median" => Some(Aggregate::Median),
        "mean" | "average" => Some(Aggregate::Mean),
        _ => None,
    }
}

/// One parsed import row. `None` means "this side stays untouched".
#[derive(Debug)]
pub struct ImportRow {
    pub type_id: u32,
    pub sell: Option<PriceInput>,
    pub buy: Option<PriceInput>,
    pub note: String,
    pub line: usize,
}

impl ImportRow {
    /// A row the operator left alone: no price typed, so nothing is applied.
    pub fn is_empty(&self) -> bool {
        self.sell.is_none() && self.buy.is_none()
    }

    pub fn summary(&self) -> Value {
        json!({
            "type_id": self.type_id,
            "line": self.line,
            "set_sell_price": self.sell.as_ref().map(PriceInput::text),
            "set_buy_price": self.buy.as_ref().map(PriceInput::text),
            "note": self.note,
            "action": if self.is_empty() { "skip" } else { "apply" },
        })
    }
}

/// What an import changed, for the workbench to report back to the operator.
#[derive(Debug)]
pub struct ImportReport {
    pub applied: usize,
    pub skipped: usize,
    pub lifted: Vec<Value>,
    pub warnings: Vec<String>,
}

impl ImportReport {
    pub fn to_json(&self) -> Value {
        json!({
            "applied": self.applied,
            "skipped": self.skipped,
            "exclusions_lifted": self.lifted,
            "warnings": self.warnings,
        })
    }
}

fn text(item: &Value, key: &str) -> String {
    match &item[key] {
        Value::String(value) => value.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn price_text(item: &Value, key: &str) -> String {
    item[key]
        .as_f64()
        .map(|value| format!("{value:.2}"))
        .unwrap_or_default()
}

/// Build the CSV from a `/api/v1/preview` payload: every item the current draft leaves
/// unresolved, with the price columns ready to fill. With `auto_family`, a row whose name
/// family already has a priced member gets `family:median` written for it, so the file can be
/// imported back as-is (the operator can still edit any row first).
pub fn export_csv(preview: &Value, auto_family: bool) -> Result<(String, usize, usize)> {
    let items = preview["items"]
        .as_array()
        .context("preview payload has no items array")?;
    let (exact, relaxed) = if auto_family {
        priced_families(items)
    } else {
        Default::default()
    };
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(COLUMNS)?;
    let mut rows = 0usize;
    let mut auto_filled = 0usize;
    for item in items {
        if item["unresolved_reason"].is_null() {
            continue;
        }
        rows += 1;
        let by_design = is_no_price_by_design(item);
        let suggestion = (auto_family && !by_design)
            .then(|| family_offer(item, &exact, &relaxed))
            .flatten();
        if suggestion.is_some() {
            auto_filled += 1;
        }
        let note = match (auto_family, by_design, suggestion) {
            (true, true, _) => {
                "no price by design (deprecated or blueprint) - leave it off the market".to_string()
            }
            (true, false, Some(_)) => "family median suggested".to_string(),
            (true, false, None) => "no priced sibling - write an amount".to_string(),
            _ => String::new(),
        };
        writer.write_record([
            item["type_id"]
                .as_u64()
                .map(|id| id.to_string())
                .unwrap_or_default(),
            text(item, "name"),
            text(item, "group_name"),
            text(item, "market_group_id"),
            text(&item["tq"], "state"),
            text(item, "unresolved_reason"),
            text(&item["resolution"]["policy"], "sides"),
            price_text(item, "sell_price"),
            price_text(item, "buy_price"),
            suggestion.unwrap_or("").to_string(),
            String::new(),
            note,
        ])?;
    }
    let bytes = writer.into_inner().context("finishing the CSV")?;
    Ok((
        String::from_utf8(bytes).context("CSV is not UTF-8")?,
        rows,
        auto_filled,
    ))
}

type FamilyKey = (String, u64);

/// Which name families already hold a resolvable Sell quote, keyed the way the
/// `sibling_family` resolver groups them: the exact family (base name + groupID) and the
/// relaxed one (base name + marketGroupID) it falls back to.
#[allow(clippy::type_complexity)]
fn priced_families(items: &[Value]) -> (BTreeSet<FamilyKey>, BTreeSet<FamilyKey>) {
    let mut exact = BTreeSet::new();
    let mut relaxed = BTreeSet::new();
    for item in items {
        if !item["sell_price"].is_number() {
            continue;
        }
        let Some(name) = item["name"].as_str() else {
            continue;
        };
        let Some(base) = crate::preset_references::base_name(name) else {
            continue;
        };
        if let Some(group) = item["group_id"].as_u64() {
            exact.insert((base.to_string(), group));
        }
        if let Some(group) = item["market_group_id"].as_u64() {
            relaxed.insert((base.to_string(), group));
        }
    }
    (exact, relaxed)
}

fn family_offer<'a>(
    item: &Value,
    exact: &BTreeSet<FamilyKey>,
    relaxed: &BTreeSet<FamilyKey>,
) -> Option<&'a str> {
    let base = crate::preset_references::base_name(item["name"].as_str()?)?;
    let key = |group: &str| {
        item[group]
            .as_u64()
            .map(|value| (base.to_string(), value))
    };
    let has = |set: &BTreeSet<FamilyKey>, group: &str| key(group).is_some_and(|key| set.contains(&key));
    (has(exact, "group_id") || has(relaxed, "market_group_id")).then_some(FAMILY_SUGGESTION)
}

/// The directive the export writes, and the import reads back.
pub const FAMILY_SUGGESTION: &str = "family:median";

/// Which sides a bulk action writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkSides {
    SellOnly,
    BuyOnly,
    BuySell,
}

impl BulkSides {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "" | "sell_only" | "sell" => Ok(Self::SellOnly),
            "buy_only" | "buy" => Ok(Self::BuyOnly),
            "buy_sell" | "both" => Ok(Self::BuySell),
            other => bail!("unsupported sides '{other}' (use sell_only, buy_only or buy_sell)"),
        }
    }

    fn split(self, input: &PriceInput) -> (Option<PriceInput>, Option<PriceInput>) {
        match self {
            Self::SellOnly => (Some(input.clone()), None),
            Self::BuyOnly => (None, Some(input.clone())),
            Self::BuySell => (Some(input.clone()), Some(input.clone())),
        }
    }
}

/// Items whose missing NPC price is **correct rather than a gap**: catalog leftovers the SDE
/// still marks Deprecated, and blueprints (an NPC market does not trade BPO/BPCs). Pricing
/// these by bulk would invent a market that should not exist, so they are skipped unless the
/// operator explicitly opts in.
pub fn is_no_price_by_design(item: &Value) -> bool {
    let name = item["name"].as_str().unwrap_or("");
    name.starts_with("Deprecated") || !item["blueprint"].is_null()
}

/// What a bulk action is allowed to touch.
pub struct BulkFilter<'a> {
    pub sides: BulkSides,
    /// Only these types, when the operator has narrowed the review list down.
    pub only: Option<&'a BTreeSet<u32>>,
    /// Include the items whose missing price is correct rather than a gap.
    pub include_special: bool,
}

/// The rows a bulk action produced, plus what it deliberately left alone.
#[derive(Debug, Default)]
pub struct BulkRows {
    pub rows: Vec<ImportRow>,
    pub skipped_special: usize,
    pub skipped_no_family: usize,
}

/// Every unresolved item the filter admits, as `(csv line, item)`.
fn candidate_items<'a>(preview: &'a Value, filter: &BulkFilter) -> (Vec<(usize, &'a Value)>, usize) {
    let items = preview["items"].as_array().map(Vec::as_slice).unwrap_or_default();
    let mut kept = Vec::new();
    let mut skipped_special = 0usize;
    for (index, item) in items.iter().enumerate() {
        if item["unresolved_reason"].is_null() {
            continue;
        }
        let Some(id) = item["type_id"].as_u64() else {
            continue;
        };
        if filter.only.is_some_and(|only| !only.contains(&(id as u32))) {
            continue;
        }
        if !filter.include_special && is_no_price_by_design(item) {
            skipped_special += 1;
            continue;
        }
        kept.push((index + 2, item));
    }
    (kept, skipped_special)
}

fn row_for(line: usize, item: &Value, sell: &Option<PriceInput>, buy: &Option<PriceInput>, note: &str) -> ImportRow {
    ImportRow {
        type_id: item["type_id"].as_u64().unwrap_or_default() as u32,
        sell: sell.clone(),
        buy: buy.clone(),
        note: note.to_string(),
        line,
    }
}

/// One row per admitted unresolved item, all priced by hand at the same amount. Backs the
/// "set them all to a price" button; the caller decides how many items that is.
pub fn manual_rows(preview: &Value, amount: &str, filter: &BulkFilter) -> Result<BulkRows> {
    let text = amount.trim().replace([',', ' '], "");
    ensure!(
        valid_price_lexeme(&text),
        "'{amount}' is not a plain positive amount (digits with an optional decimal part)"
    );
    let value: f64 = text.parse().context("the amount is not a number")?;
    ensure!(
        value.is_finite() && value > 0.0,
        "the amount must be a positive price"
    );
    let input = PriceInput::Manual { text, value };
    let (sell, buy) = filter.sides.split(&input);
    let (items, skipped_special) = candidate_items(preview, filter);
    Ok(BulkRows {
        rows: items
            .into_iter()
            .map(|(line, item)| row_for(line, item, &sell, &buy, "bulk manual price"))
            .collect(),
        skipped_special,
        skipped_no_family: 0,
    })
}

/// One row per admitted unresolved item whose name family already holds a priced item - the
/// automatic half of the sibling source, applied without the CSV round-trip.
pub fn family_rows(preview: &Value, filter: &BulkFilter) -> BulkRows {
    let all = preview["items"].as_array().map(Vec::as_slice).unwrap_or_default();
    let (exact, relaxed) = priced_families(all);
    let input = PriceInput::Family(Aggregate::Median);
    let (sell, buy) = filter.sides.split(&input);
    let (items, skipped_special) = candidate_items(preview, filter);
    let mut rows = Vec::new();
    let mut skipped_no_family = 0usize;
    for (line, item) in items {
        if family_offer(item, &exact, &relaxed).is_none() {
            skipped_no_family += 1;
            continue;
        }
        rows.push(row_for(line, item, &sell, &buy, "similar item price"));
    }
    BulkRows {
        rows,
        skipped_special,
        skipped_no_family,
    }
}

fn valid_price_lexeme(text: &str) -> bool {
    let mut parts = text.split('.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next();
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.is_none_or(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && parts.next().is_none()
}

/// Parse one `set_sell_price` / `set_buy_price` cell. `Err(())` means "skip this row whole".
///
/// The cell accepts an absolute ISK amount, or `family:<min|median|mean>` to borrow the
/// same-side quote of the item's name family - the bulk equivalent of picking the
/// "Similar item price" source in the item editor.
fn parse_price_input(
    raw: &str,
    line: usize,
    column: &str,
    warnings: &mut Vec<String>,
) -> Result<Option<PriceInput>, ()> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if let Some(mode) = value
        .strip_prefix("family:")
        .or_else(|| value.strip_prefix("sibling:"))
    {
        return match parse_aggregate(mode) {
            Some(aggregate) => Ok(Some(PriceInput::Family(aggregate))),
            None => {
                warnings.push(format!(
                    "line {line}: {column} '{value}' must be family:min, family:median or family:mean - row skipped"
                ));
                Err(())
            }
        };
    }
    // Spreadsheets like to add thousands separators; accept and normalise them.
    let normalised = value.replace([',', ' '], "");
    if !valid_price_lexeme(&normalised) {
        warnings.push(format!(
            "line {line}: {column} '{value}' is not a plain positive number or family:<mode> - row skipped"
        ));
        return Err(());
    }
    match normalised.parse::<f64>() {
        Ok(parsed) if parsed.is_finite() && parsed > 0.0 => Ok(Some(PriceInput::Manual {
            text: normalised,
            value: parsed,
        })),
        _ => {
            warnings.push(format!(
                "line {line}: {column} '{value}' must be a positive price - row skipped"
            ));
            Err(())
        }
    }
}

/// Parse an edited CSV. Unknown columns are ignored, so a spreadsheet may carry extra
/// notes; only `type_id` is mandatory. A row whose typed price is not a plain positive
/// number is skipped whole, with a warning, rather than applying half of it.
pub fn parse_rows(text: &str) -> Result<(Vec<ImportRow>, Vec<String>)> {
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader.headers().context("reading the CSV header")?.clone();
    let column = |name: &str| headers.iter().position(|header| header.trim() == name);
    let type_column = column("type_id")
        .context("the CSV needs a type_id column (export the file from the workbench first)")?;
    let sell_column = column("set_sell_price");
    let buy_column = column("set_buy_price");
    let note_column = column("note");
    let cell = |record: &csv::StringRecord, column: Option<usize>| {
        column
            .and_then(|index| record.get(index))
            .unwrap_or("")
            .trim()
            .to_string()
    };

    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    for (offset, record) in reader.records().enumerate() {
        let record = record.with_context(|| format!("CSV line {}", offset + 2))?;
        let line = offset + 2;
        let raw_id = record.get(type_column).unwrap_or("").trim();
        if raw_id.is_empty() {
            continue;
        }
        let Ok(type_id) = raw_id.parse::<u32>() else {
            warnings.push(format!("line {line}: '{raw_id}' is not a type id - row skipped"));
            continue;
        };
        if type_id == 0 {
            warnings.push(format!("line {line}: type id must be positive - row skipped"));
            continue;
        }
        let mut skip = false;
        let mut side = |raw: String, column: &str, skip: &mut bool| {
            match parse_price_input(&raw, line, column, &mut warnings) {
                Ok(value) => value,
                Err(()) => {
                    *skip = true;
                    None
                }
            }
        };
        let sell = side(cell(&record, sell_column), "set_sell_price", &mut skip);
        let buy = side(cell(&record, buy_column), "set_buy_price", &mut skip);
        if skip {
            continue;
        }
        let note = cell(&record, note_column);
        if note.len() > 200 {
            warnings.push(format!(
                "line {line}: note truncated to 200 characters"
            ));
        }
        rows.push(ImportRow {
            type_id,
            sell,
            buy,
            note: note.chars().take(200).collect(),
            line,
        });
    }
    ensure!(
        !rows.is_empty(),
        "no data rows found - export the file from the workbench and fill the set_* columns"
    );
    Ok((rows, warnings))
}

/// Build the side policy a row asked for: an absolute quote, or a name-family aggregate.
fn side_for(input: &PriceInput) -> SidePolicy {
    match input {
        PriceInput::Manual { text, .. } => SidePolicy {
            source: Source::ManualFixed,
            multiplier: Decimal("1".into()),
            floor: None,
            price: Some(Decimal(text.clone())),
            aggregate: None,
        },
        PriceInput::Family(mode) => SidePolicy {
            source: Source::SiblingFamily,
            multiplier: Decimal("1".into()),
            floor: None,
            price: None,
            aggregate: Some(*mode),
        },
    }
}

/// Rewrite the draft so the priced rows are seeded: lift the exact unseeded exclusion that
/// would outrank them, then add one exact `manual_fixed` override per row.
pub fn apply_import(
    policy: &mut PolicyDocument,
    rows: &[ImportRow],
    known: &BTreeSet<u32>,
) -> Result<ImportReport> {
    let mut warnings = Vec::new();
    let mut admitted = Vec::new();
    let mut skipped = 0usize;
    for row in rows {
        if row.is_empty() {
            skipped += 1;
            continue;
        }
        if !known.contains(&row.type_id) {
            warnings.push(format!(
                "type {} is not in this catalog - row {} skipped",
                row.type_id, row.line
            ));
            skipped += 1;
            continue;
        }
        admitted.push(row);
    }
    ensure!(
        !admitted.is_empty(),
        "no row carried a price, so nothing was imported"
    );
    let targets = admitted
        .iter()
        .map(|row| row.type_id)
        .collect::<BTreeSet<_>>();

    // The guard in `policy_resolve` ranks every exact-type unseeded rule first, whatever its
    // priority, so the exclusion has to go before an override can take effect.
    let mut lifted = Vec::new();
    for rule in policy.rules.iter_mut() {
        if rule.sides != Some(Sides::Unseeded) {
            continue;
        }
        let Some(ids) = rule.selector.type_ids.as_mut() else {
            continue;
        };
        let before = ids.len();
        ids.retain(|id| !targets.contains(id));
        if ids.len() != before {
            lifted.push(json!({"rule_id": rule.id, "removed": before - ids.len()}));
        }
    }
    if !lifted.is_empty() {
        // `Selector` rejects an empty id list, so a rule that lost every member goes away.
        policy
            .rules
            .retain(|rule| rule.selector.type_ids.as_ref().is_none_or(|ids| !ids.is_empty()));
    }

    let replaced = targets
        .iter()
        .map(|id| format!("override_type_{id}"))
        .collect::<BTreeSet<_>>();
    policy.rules.retain(|rule| !replaced.contains(&rule.id));
    let priority = policy
        .rules
        .iter()
        .map(|rule| rule.priority)
        .max()
        .unwrap_or(0)
        + 20;

    for row in &admitted {
        let sides = match (row.sell.is_some(), row.buy.is_some()) {
            (true, true) => Sides::BuySell,
            (true, false) => Sides::SellOnly,
            (false, true) => Sides::BuyOnly,
            (false, false) => unreachable!("an empty row is never admitted"),
        };
        let description = if row.note.is_empty() {
            format!("Imported explicit price for type {}", row.type_id)
        } else {
            format!(
                "Imported explicit price for type {}: {}",
                row.type_id, row.note
            )
        };
        policy.rules.push(Rule {
            id: format!("override_type_{}", row.type_id),
            description: Some(description),
            selector: Selector {
                type_ids: Some(vec![row.type_id]),
                group_ids: None,
                category_ids: None,
                fact: None,
            },
            priority,
            profile: None,
            sides: Some(sides),
            sell: row.sell.as_ref().map(side_for),
            buy: row.buy.as_ref().map(side_for),
        });
    }
    policy.validate()?;
    Ok(ImportReport {
        applied: admitted.len(),
        skipped,
        lifted,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyDocument;

    const DOCUMENT: &str = r#"{"format_version":1,"catalog_contract":{"sde_build":1,"fact_registry_version":1},"profiles":[],"rules":[{"id":"no_tq_reference","priority":100,"selector":{"type_ids":[4143,4154,4162]},"sides":"unseeded"},{"id":"broad","priority":10,"selector":{"category_ids":[4]},"sides":"unseeded"}]}"#;

    fn preview() -> Value {
        json!({"items":[
            {"type_id":4143,"name":"Women's 'Structure' Dress (black)","group_name":"Dress",
             "market_group_id":1405,"unresolved_reason":"NO_TQ_REFERENCE","sell_price":null,"buy_price":null,
             "resolution":{"policy":{"sides":"unseeded","sell":null,"buy":null}},"tq":{"state":"NO_TQ_REFERENCE"}},
            {"type_id":34,"name":"Tritanium","group_name":"Mineral","market_group_id":1855,
             "unresolved_reason":null,"sell_price":5.5,"buy_price":4.9}
        ]})
    }

    #[test]
    fn auto_family_export_suggests_a_price_only_where_a_family_one_exists() {
        let preview = json!({"items":[
            {"type_id":4143,"name":"Women's 'Structure' Dress (black)","group_id":1088,"market_group_id":1405,
             "unresolved_reason":"NO_TQ_REFERENCE","sell_price":null,"buy_price":null,
             "resolution":{"policy":{"sides":"unseeded"}},"tq":{"state":"NO_TQ_REFERENCE"}},
            {"type_id":3975,"name":"Women's 'Structure' Dress (navy)","group_id":1088,"market_group_id":1405,
             "unresolved_reason":null,"sell_price":2861866.67,"buy_price":null},
            {"type_id":9999,"name":"Lonely Thing (odd)","group_id":1,"market_group_id":2,
             "unresolved_reason":"NO_TQ_REFERENCE","sell_price":null,"buy_price":null,
             "resolution":{"policy":{"sides":"unseeded"}},"tq":{"state":"NO_TQ_REFERENCE"}}
        ]});
        let (csv, rows, auto_filled) = export_csv(&preview, true).unwrap();
        assert_eq!((rows, auto_filled), (2, 1));
        let lines = csv.lines().collect::<Vec<_>>();
        assert!(lines[1].starts_with("4143,"), "{}", lines[1]);
        assert!(lines[1].contains(FAMILY_SUGGESTION), "{}", lines[1]);
        assert!(lines[1].contains("family median suggested"), "{}", lines[1]);
        assert!(lines[2].starts_with("9999,"), "{}", lines[2]);
        assert!(!lines[2].contains(FAMILY_SUGGESTION), "{}", lines[2]);
        assert!(lines[2].contains("no priced sibling"), "{}", lines[2]);

        // The suggested file imports straight back in.
        let (parsed, warnings) = parse_rows(&csv).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut policy = PolicyDocument::parse(DOCUMENT).unwrap();
        let known = [4143u32, 9999].into_iter().collect();
        let report = apply_import(&mut policy, &parsed, &known).unwrap();
        assert_eq!(report.applied, 1);
        let rule = policy
            .rules
            .iter()
            .find(|r| r.id == "override_type_4143")
            .unwrap();
        assert_eq!(rule.sell.as_ref().unwrap().aggregate, Some(Aggregate::Median));
    }

    #[test]
    fn bulk_rows_back_the_two_buttons() {
        let preview = json!({"items":[
            {"type_id":4143,"name":"Women's 'Structure' Dress (black)","group_id":1088,"market_group_id":1405,
             "unresolved_reason":"NO_TQ_REFERENCE","sell_price":null,"resolution":{"policy":{"sides":"unseeded"}},"tq":{"state":"NO_TQ_REFERENCE"}},
            {"type_id":3975,"name":"Women's 'Structure' Dress (navy)","group_id":1088,"market_group_id":1405,
             "unresolved_reason":null,"sell_price":2861866.67},
            {"type_id":9999,"name":"Lonely Thing (odd)","group_id":1,"market_group_id":2,
             "unresolved_reason":"NO_TQ_REFERENCE","sell_price":null,"resolution":{"policy":{"sides":"unseeded"}},"tq":{"state":"NO_TQ_REFERENCE"}},
            {"type_id":2001,"name":"Deprecated Cynosural Suppression","group_id":3,"market_group_id":4,
             "unresolved_reason":"NO_TQ_REFERENCE","sell_price":null,"resolution":{"policy":{"sides":"unseeded"}},"tq":{"state":"NO_TQ_REFERENCE"}},
            {"type_id":8888,"name":"Raven Blueprint","group_id":5,"market_group_id":6,"blueprint":{"family":"other"},
             "unresolved_reason":"NO_TQ_REFERENCE","sell_price":null,"resolution":{"policy":{"sides":"unseeded"}},"tq":{"state":"NO_TQ_REFERENCE"}}
        ]});
        let everything = BulkFilter { sides: BulkSides::SellOnly, only: None, include_special: false };

        let manual = manual_rows(&preview, "1,250.5", &everything).unwrap();
        assert_eq!(manual.rows.len(), 2, "only the real gaps get a price");
        assert_eq!(manual.skipped_special, 2, "the deprecated item and the blueprint are left alone");
        assert!(manual.rows.iter().all(|row| row.sell
            == Some(PriceInput::Manual { text: "1250.5".into(), value: 1250.5 })));
        assert!(manual_rows(&preview, "0", &everything).is_err());
        assert!(manual_rows(&preview, "family:median", &everything).is_err(), "the bulk amount must be a number");

        let family = family_rows(&preview, &everything);
        assert_eq!(family.rows.len(), 1, "only the item whose family has a price is picked");
        assert_eq!(family.rows[0].type_id, 4143);
        assert_eq!(family.skipped_special, 2);
        assert_eq!(family.skipped_no_family, 1, "the lonely item has no priced sibling");

        // Sides, an explicit selection, and the opt-in all behave.
        let buy_both = BulkFilter { sides: BulkSides::BuySell, only: None, include_special: false };
        let manual_both = manual_rows(&preview, "10", &buy_both).unwrap();
        assert!(manual_both.rows.iter().all(|row| row.sell.is_some() && row.buy.is_some()));
        let buy_only = BulkFilter { sides: BulkSides::BuyOnly, only: None, include_special: true };
        let manual_buy = manual_rows(&preview, "10", &buy_only).unwrap();
        assert_eq!(manual_buy.rows.len(), 4, "opting in includes the deprecated item and the blueprint");
        assert!(manual_buy.rows.iter().all(|row| row.sell.is_none() && row.buy.is_some()));
        let selected = [4143u32].into_iter().collect::<BTreeSet<_>>();
        let narrowed = BulkFilter { sides: BulkSides::SellOnly, only: Some(&selected), include_special: false };
        assert_eq!(manual_rows(&preview, "10", &narrowed).unwrap().rows.len(), 1, "a narrowed list wins");
        assert!(BulkSides::parse("sideways").is_err());

        // Both feed the same verified import path.
        let mut policy = PolicyDocument::parse(DOCUMENT).unwrap();
        let known = [4143u32, 9999].into_iter().collect();
        let report = apply_import(&mut policy, &family.rows, &known).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(
            policy.rules.iter().find(|r| r.id == "override_type_4143").unwrap().sell.as_ref().unwrap().aggregate,
            Some(Aggregate::Median)
        );
    }

    #[test]
    fn export_writes_only_unresolved_items_with_empty_price_columns() {
        let (csv, rows, auto_filled) = export_csv(&preview(), false).unwrap();
        assert_eq!(rows, 1);
        assert_eq!(auto_filled, 0);
        let lines = csv.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("type_id,name,group_name"));
        assert!(lines[0].ends_with("set_sell_price,set_buy_price,note"));
        assert!(lines[1].starts_with("4143,Women's 'Structure' Dress (black),Dress,1405,"));
        assert!(lines[1].ends_with(",,,"), "price columns must be empty: {}", lines[1]);
    }

    #[test]
    fn parse_skips_blank_rows_and_tolerates_spreadsheet_numbers() {
        // A spreadsheet quotes the thousands separator, so the comma arrives inside one cell.
        let csv = "type_id,name,set_sell_price,set_buy_price,note\n4143,x,\"1,234.50\",,dress\n4154,y,,,\n4162,z,,77,\n";
        let (rows, warnings) = parse_rows(csv).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].sell.as_ref().unwrap().text(), "1234.50");
        assert_eq!(rows[0].buy, None);
        assert!(rows[1].is_empty(), "a row without prices changes nothing");
        assert_eq!(rows[2].buy.as_ref().unwrap().text(), "77");
    }

    #[test]
    fn family_directive_borrows_the_sibling_quote_instead_of_a_manual_price() {
        let mut policy = PolicyDocument::parse(DOCUMENT).unwrap();
        let (rows, warnings) =
            parse_rows("type_id,set_sell_price,note\n4154,family:median,turquoise\n").unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(rows[0].sell.as_ref().unwrap(), &PriceInput::Family(Aggregate::Median));
        let known = [4143u32, 4154, 4162].into_iter().collect();
        let report = apply_import(&mut policy, &rows, &known).unwrap();
        assert_eq!(report.applied, 1);
        let rule = policy
            .rules
            .iter()
            .find(|r| r.id == "override_type_4154")
            .unwrap();
        let sell = rule.sell.as_ref().unwrap();
        assert_eq!(sell.source, Source::SiblingFamily);
        assert_eq!(sell.aggregate, Some(Aggregate::Median));
        assert!(sell.price.is_none());
        // The exclusion was lifted for exactly the item the family price must reach.
        let guard = policy.rules.iter().find(|r| r.id == "no_tq_reference").unwrap();
        assert!(!guard.selector.type_ids.as_ref().unwrap().contains(&4154));
    }

    #[test]
    fn family_directive_rejects_an_unknown_mode() {
        let error = parse_rows("type_id,set_sell_price\n4154,family:whatever\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("no data rows found"), "{error}");

        let (rows, warnings) =
            parse_rows("type_id,set_sell_price\n4154,family:whatever\n4162,family:min\n").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sell.as_ref().unwrap(), &PriceInput::Family(Aggregate::Min));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("family:min"), "{}", warnings[0]);
    }

    #[test]
    fn parse_rejects_a_bad_price_for_the_whole_row() {
        let csv = "type_id,set_sell_price,set_buy_price\n4143,1e3,50\n";
        let error = parse_rows(csv).unwrap_err().to_string();
        assert!(error.contains("no data rows found"), "{error}");

        // With a second, valid row the bad one is dropped and reported instead.
        let csv = "type_id,set_sell_price\n4143,1e3\n4154,250\n";
        let (rows, warnings) = parse_rows(csv).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].type_id, 4154);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not a plain positive number"));
    }

    #[test]
    fn import_lifts_the_exclusion_and_prices_the_item() {
        let mut policy = PolicyDocument::parse(DOCUMENT).unwrap();
        let (rows, _) = parse_rows("type_id,set_sell_price,set_buy_price,note\n4143,2500000,,black dress\n").unwrap();
        let known = [4143u32, 4154, 4162].into_iter().collect();
        let report = apply_import(&mut policy, &rows, &known).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.lifted.len(), 1);
        assert_eq!(report.lifted[0]["removed"], 1);

        let guard = policy.rules.iter().find(|r| r.id == "no_tq_reference").unwrap();
        assert_eq!(guard.selector.type_ids.as_deref(), Some(&[4154u32, 4162][..]));
        // An unrelated broad unseeded rule is untouched.
        assert!(policy.rules.iter().any(|r| r.id == "broad"));

        let override_rule = policy
            .rules
            .iter()
            .find(|r| r.id == "override_type_4143")
            .unwrap();
        assert_eq!(override_rule.sides, Some(Sides::SellOnly));
        assert_eq!(override_rule.priority, 120);
        let sell = override_rule.sell.as_ref().unwrap();
        assert_eq!(sell.source, Source::ManualFixed);
        assert_eq!(sell.price.as_ref().unwrap().0, "2500000");
        assert!(override_rule.buy.is_none());
        assert!(override_rule.description.as_deref().unwrap().contains("black dress"));
    }

    #[test]
    fn import_drops_an_exclusion_rule_that_loses_every_member() {
        let mut policy = PolicyDocument::parse(
            r#"{"format_version":1,"catalog_contract":{"sde_build":1,"fact_registry_version":1},"profiles":[],"rules":[{"id":"no_tq_reference","priority":100,"selector":{"type_ids":[4143]},"sides":"unseeded"}]}"#,
        )
        .unwrap();
        let (rows, _) = parse_rows("type_id,set_buy_price\n4143,42\n").unwrap();
        let report = apply_import(&mut policy, &rows, &[4143u32].into_iter().collect()).unwrap();
        assert_eq!(report.applied, 1);
        assert!(!policy.rules.iter().any(|r| r.id == "no_tq_reference"));
        let rule = policy.rules.iter().find(|r| r.id == "override_type_4143").unwrap();
        assert_eq!(rule.sides, Some(Sides::BuyOnly));
        assert_eq!(rule.buy.as_ref().unwrap().source, Source::ManualFixed);
    }

    #[test]
    fn import_replaces_a_previous_manual_price_instead_of_stacking() {
        let mut policy = PolicyDocument::parse(DOCUMENT).unwrap();
        let known = [4143u32, 4154, 4162].into_iter().collect();
        let (first, _) = parse_rows("type_id,set_sell_price\n4143,100\n").unwrap();
        apply_import(&mut policy, &first, &known).unwrap();
        let (second, _) = parse_rows("type_id,set_sell_price\n4143,900\n").unwrap();
        apply_import(&mut policy, &second, &known).unwrap();
        let matching = policy
            .rules
            .iter()
            .filter(|r| r.id == "override_type_4143")
            .count();
        assert_eq!(matching, 1);
        let rule = policy.rules.iter().find(|r| r.id == "override_type_4143").unwrap();
        assert_eq!(rule.sell.as_ref().unwrap().price.as_ref().unwrap().0, "900");
    }

    #[test]
    fn import_refuses_unknown_types_and_empty_price_columns() {
        let mut policy = PolicyDocument::parse(DOCUMENT).unwrap();
        let (rows, _) = parse_rows("type_id,set_sell_price\n999999,5\n").unwrap();
        let error = apply_import(&mut policy, &rows, &[4143u32].into_iter().collect()).unwrap_err();
        assert!(error.to_string().contains("nothing was imported"));

        let (empty, _) = parse_rows("type_id,set_sell_price\n4143,\n4154,\n").unwrap();
        let report = apply_import(&mut policy, &empty, &[4143u32].into_iter().collect()).unwrap_err();
        assert!(report.to_string().contains("nothing was imported"));
    }
}
