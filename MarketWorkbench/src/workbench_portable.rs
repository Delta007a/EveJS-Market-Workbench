//! One deterministic, path-free interchange file. Local operational configuration is never exported.
use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::distribution::Document;
use crate::policy::PolicyDocument;

pub const FORMAT: &str = "evejs-market-workbench-preset";
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
    pub base_preset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub format: String,
    pub format_version: u32,
    pub metadata: Metadata,
    pub required_price_sources: Vec<String>,
    pub item_policy: PolicyDocument,
    pub distribution_policy: Document,
}

pub fn optional_text(value: Option<&str>, limit: usize, label: &str) -> Result<Option<String>> {
    match value.filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(text) => {
            ensure!(
                text.chars().count() <= limit,
                "{label} exceeds {limit} characters"
            );
            ensure!(
                !text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t'),
                "{label} contains control characters"
            );
            Ok(Some(text.to_owned()))
        }
    }
}

impl Metadata {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.base_preset == "GENERAL_TQ" || self.base_preset == "LEGACY_V1",
            "unsupported base preset {}",
            self.base_preset
        );
        ensure!(
            !self.name.trim().is_empty()
                && self.name.chars().count() <= 80
                && !self.name.chars().any(char::is_control),
            "Preset name must be 1–80 readable characters"
        );
        optional_text(self.description.as_deref(), 2000, "Description")?;
        optional_text(self.author.as_deref(), 160, "Author")?;
        Ok(())
    }
}

pub fn sources(policy: &PolicyDocument) -> Vec<String> {
    policy
        .profiles
        .iter()
        .flat_map(|p| [&p.sell, &p.buy])
        .chain(policy.rules.iter().flat_map(|r| [&r.sell, &r.buy]))
        .flatten()
        .map(|s| {
            serde_json::to_value(s.source)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

impl Preset {
    pub fn new(
        metadata: Metadata,
        item_policy: PolicyDocument,
        distribution_policy: Document,
    ) -> Result<Self> {
        let package = Self {
            format: FORMAT.into(),
            format_version: VERSION,
            required_price_sources: sources(&item_policy),
            metadata,
            item_policy,
            distribution_policy,
        };
        package.validate()?;
        Ok(package)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let value = crate::policy::strict_json(text).context("Invalid preset JSON")?;
        let mut package: Self =
            serde_json::from_value(value).context("Invalid portable preset schema")?;
        // The authoritative policy parser also canonicalizes rule/selector ordering.
        package.item_policy = PolicyDocument::parse(&serde_json::to_string(&package.item_policy)?)?;
        package.validate()?;
        Ok(package)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format == FORMAT,
            "Unsupported preset format {}",
            self.format
        );
        ensure!(
            self.format_version == VERSION,
            "Unsupported preset format_version {} (supported: {VERSION})",
            self.format_version
        );
        self.metadata.validate()?;
        self.item_policy.validate()?;
        ensure!(
            self.distribution_policy.distribution_format_version == 1,
            "Unsupported Distribution format version"
        );
        ensure!(
            self.required_price_sources == sources(&self.item_policy),
            "Required price-source list must exactly match the Item Policy; dependencies cannot be hidden"
        );
        reject_local_paths(&serde_json::to_value(self)?, "preset")
    }

    pub fn canonical_json(&self) -> Result<String> {
        self.validate()?;
        let mut copy = self.clone();
        copy.item_policy = PolicyDocument::parse(&self.item_policy.canonical_json()?)?;
        Ok(serde_json::to_string_pretty(&copy)? + "\n")
    }
}

fn local_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.trim_start().starts_with('/')
        || text.contains("\\\\")
        || text
            .split_whitespace()
            .any(|s| s.to_ascii_lowercase().starts_with("file:"))
        || bytes.windows(3).enumerate().any(|(i, w)| {
            w[0].is_ascii_alphabetic()
                && w[1] == b':'
                && (w[2] == b'/' || w[2] == b'\\')
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        })
        || text
            .split_whitespace()
            .any(|s| s.len() > 1 && s.starts_with('/'))
}

fn reject_local_paths(value: &Value, field: &str) -> Result<()> {
    match value {
        Value::String(text) => ensure!(
            !local_path(text),
            "Portable presets cannot contain absolute local paths ({field}); remove the path from this text field"
        ),
        Value::Array(values) => {
            for value in values {
                reject_local_paths(value, field)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                ensure!(
                    !local_path(key),
                    "Portable preset contains a local path as an object key"
                );
                reject_local_paths(value, &format!("{field}.{key}"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Used with the installation's pinned input inventory, never a browser-provided path.
pub fn verify_local_input(path: &std::path::Path, expected_sha: &str) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("Required local dependency is missing or unreadable: {}. Install this input; no price fallback was applied", path.display()))?;
    ensure!(
        crate::policy_preview::sha256_hex(&bytes) == expected_sha,
        "Required local dependency changed: {}. Restart Workbench with valid pinned inputs; no price fallback was applied",
        path.display()
    );
    Ok(())
}

/// App startup already hashes and pins these files. Reuse that identity while the
/// file remains readable with unchanged size/mtime; rehash any changed input.
/// Candidate build keeps its existing unconditional full input verification.
pub struct VerifiedInput {
    path: std::path::PathBuf,
    sha256: String,
    length: u64,
    modified: Option<std::time::SystemTime>,
}

impl VerifiedInput {
    pub fn remember_verified(path: &std::path::Path, sha256: &str) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            path: path.into(),
            sha256: sha256.into(),
            length: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    pub fn verify(&self) -> Result<()> {
        let file = std::fs::File::open(&self.path).with_context(|| format!("Required local dependency is missing or unreadable: {}. Install this input; no price fallback was applied", self.path.display()))?;
        let metadata = file.metadata()?;
        if self.modified.is_some()
            && metadata.len() == self.length
            && metadata.modified().ok() == self.modified
        {
            return Ok(());
        }
        verify_local_input(&self.path, &self.sha256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Preset {
        let item_policy = PolicyDocument::parse(r#"{"format_version":1,"catalog_contract":{"sde_build":1,"fact_registry_version":1},"profiles":[{"id":"tq","sides":"buy_sell","sell":{"source":"tq_snapshot","multiplier":"1.00"},"buy":{"source":"funded_cost","multiplier":"0.80"}}],"rules":[{"id":"exact","selector":{"type_ids":[35,34]},"priority":900,"profile":"tq"}]}"#).unwrap();
        Preset::new(
            Metadata {
                name: "Мой рынок".into(),
                base_preset: "GENERAL_TQ".into(),
                description: Some("Test\nПереносимый preset".into()),
                author: Some("Delta".into()),
            },
            item_policy,
            Document::default(),
        )
        .unwrap()
    }
    #[test]
    fn deterministic_round_trip_preserves_policy_and_distribution() {
        let mut p = fixture();
        p.distribution_policy.seed = "my-seed-λ".into();
        let text = p.canonical_json().unwrap();
        let out = Preset::parse(&text).unwrap();
        assert_eq!(text, out.canonical_json().unwrap());
        assert_eq!(
            p.item_policy.canonical_json().unwrap(),
            out.item_policy.canonical_json().unwrap()
        );
        assert_eq!(p.distribution_policy, out.distribution_policy);
        assert_eq!(
            out.required_price_sources,
            vec!["funded_cost", "tq_snapshot"]
        );
    }
    #[test]
    fn strict_format_versions_unknown_fields_and_duplicates() {
        let p = fixture();
        let value = serde_json::to_value(&p).unwrap();
        for (key, bad) in [
            ("format", serde_json::json!("other")),
            ("format_version", serde_json::json!(2)),
            ("absolute_output_path", serde_json::json!("anything")),
        ] {
            let mut v = value.clone();
            v[key] = bad;
            assert!(Preset::parse(&v.to_string()).is_err());
        }
        let dup = p
            .canonical_json()
            .unwrap()
            .replacen("{", "{\"format_version\":1,", 1);
        assert!(Preset::parse(&dup).is_err());
        let mut v = value;
        v["distribution_policy"]["seed_typo"] = serde_json::json!(1);
        assert!(Preset::parse(&v.to_string()).is_err());
    }
    #[test]
    fn rejects_hidden_dependencies_and_unsupported_sources() {
        let p = fixture();
        let mut v = serde_json::to_value(&p).unwrap();
        v["required_price_sources"] = serde_json::json!([]);
        assert!(Preset::parse(&v.to_string()).is_err());
        let mut v = serde_json::to_value(&p).unwrap();
        v["item_policy"]["profiles"][0]["buy"]["source"] = serde_json::json!("made_up");
        assert!(Preset::parse(&v.to_string()).is_err());
    }
    #[test]
    fn no_local_paths_even_in_free_text_or_seed() {
        for path in [
            "C:\\private\\data",
            "G:/work/data",
            "\\\\server\\share",
            "/home/data",
            "Use /tmp/data",
            "file:///c:/data",
        ] {
            let mut p = fixture();
            p.metadata.description = Some(path.into());
            assert!(p.canonical_json().is_err(), "{path}");
            let mut p = fixture();
            p.distribution_policy.seed = path.into();
            assert!(p.canonical_json().is_err(), "{path}");
        }
        assert!(!local_path("T1 / T2, Buy/Sell"));
        assert!(!local_path("https://example.com/preset"));
    }
    #[test]
    fn missing_dependency_fails_without_fallback() {
        let missing = std::env::temp_dir().join(format!("wb-missing-{}", std::process::id()));
        let error = verify_local_input(&missing, "none")
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing or unreadable") && error.contains("no price fallback"));
    }
    #[test]
    fn verified_input_cache_checks_readability_and_changed_bytes() {
        let path = std::env::temp_dir().join(format!("wb-source-probe-{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, b"original").unwrap();
        drop(file);
        let sha = crate::policy_preview::sha256_hex(b"original");
        verify_local_input(&path, &sha).unwrap();
        let cached = VerifiedInput::remember_verified(&path, &sha).unwrap();
        cached.verify().unwrap();
        std::fs::write(&path, b"changed input").unwrap();
        assert!(
            cached
                .verify()
                .unwrap_err()
                .to_string()
                .contains("dependency changed")
        );
        std::fs::write(&path, b"original").unwrap();
        cached.verify().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(
            cached
                .verify()
                .unwrap_err()
                .to_string()
                .contains("missing or unreadable")
        );
    }
    #[test]
    fn metadata_limits_and_base_preset_validation() {
        let mut p = fixture();
        p.metadata.name = " ".into();
        assert!(p.validate().is_err());
        let mut p = fixture();
        p.metadata.base_preset = "not-installed".into();
        assert!(p.validate().is_err());
        let mut p = fixture();
        p.metadata.author = Some("a".repeat(161));
        assert!(p.validate().is_err());
    }
}
