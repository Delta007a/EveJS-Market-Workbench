//! Explicit installation of an audited candidate. Targets come from the launcher,
//! never a browser-supplied path. No server/process control and no policy changes.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

#[cfg(not(windows))]
use anyhow::bail;
use anyhow::{Context, Result, ensure};
use ring::digest::{self, Context as HashContext};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::policy_preview::sha256_hex;

pub struct Targets(Vec<PathBuf>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub target_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    pub target_id: String,
    pub preview_sha256: String,
    pub confirm_replace: bool,
    pub server_stopped: bool,
}

struct Destination {
    root: PathBuf,
    config: PathBuf,
    config_sha256: String,
    database: PathBuf,
}

impl Targets {
    pub fn new(roots: &[PathBuf]) -> Result<Self> {
        let defaults;
        let roots = if roots.is_empty() {
            defaults = vec![std::env::current_dir()?.join("../..")];
            &defaults
        } else {
            roots
        };
        let mut resolved = Vec::new();
        for root in roots {
            let root = fs::canonicalize(root).context("EveJS installation folder not found")?;
            ensure!(
                root.join("StartMarketServer.bat").is_file()
                    && root
                        .join("externalservices/market-server/Cargo.toml")
                        .is_file(),
                "Not an EveJS installation: {}",
                root.display()
            );
            if !resolved.contains(&root) {
                resolved.push(root);
            }
        }
        Ok(Self(resolved))
    }

    fn destination(&self, id: &str) -> Result<Destination> {
        let index = id
            .strip_prefix("evejs-")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|i| *i > 0)
            .context("Unknown EveJS target; reload the target list")?;
        let root = self
            .0
            .get(index - 1)
            .context("Unknown EveJS target")?
            .clone();
        ensure!(id == format!("evejs-{index}"), "Invalid EveJS target ID");
        let server = root.join("externalservices/market-server");
        let config = confined(&root, &server.join("config/market-server.local.toml"))?;
        let bytes = fs::read(&config).context("Cannot read market-server.local.toml")?;
        let parsed: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)?;
        let path = Path::new(
            parsed
                .get("storage")
                .and_then(|s| s.get("database_path"))
                .and_then(|s| s.as_str())
                .context("Missing [storage].database_path in market-server config")?,
        );
        ensure!(!path.as_os_str().is_empty(), "Empty market database path");
        ensure!(
            !path.components().any(|c| matches!(c, Component::ParentDir)),
            "Parent-directory paths are not allowed in market-server database_path"
        );
        let database = confined(
            &root,
            &if path.is_absolute() {
                path.to_owned()
            } else {
                server.join(path)
            },
        )?;
        ensure!(
            database.extension().is_some_and(|s| s == "sqlite"),
            "Market database must be a .sqlite file"
        );
        Ok(Destination {
            root,
            config,
            config_sha256: sha256_hex(&bytes),
            database,
        })
    }

    pub fn list(&self) -> Value {
        json!({"targets":self.0.iter().enumerate().map(|(i,root)| {
            let id = format!("evejs-{}",i+1);
            let label = root.file_name().unwrap_or_default().to_string_lossy();
            match self.destination(&id) {
                Ok(d) => json!({"id":id,"label":label,"root":display(root),"database_path":display(&d.database),"available":true}),
                Err(e) => json!({"id":id,"label":label,"root":display(root),"available":false,"error":format!("{e:#}")})
            }
        }).collect::<Vec<_>>()})
    }

    pub fn preview(&self, dir: &Path, candidate_id: &str, target_id: &str) -> Result<Value> {
        let destination = self.destination(target_id)?;
        let mut source = candidate(dir, candidate_id)?;
        let source_sha = hash(&mut source)?;
        let current = destination_state(&destination.database)?.0;
        let mut preview = json!({"candidate_id":candidate_id,"target_id":target_id,
            "evejs_root":display(&destination.root),"config_path":display(&destination.config),
            "config_sha256":destination.config_sha256,"database_path":display(&destination.database),
            "candidate_sha256":source_sha,"candidate_bytes":source.metadata()?.len(),"existing_database":current,
            "replaces_existing":current["exists"] == true,"audit_passed":true,
            "warnings":["Stop the target market-server and EveJS before installing. Workbench does not stop or start services.",
                "Replacing a database resets that market's player orders, events and remaining stock. An existing database is backed up first.",
                "The original Workbench candidate remains unchanged."]});
        preview["preview_sha256"] = json!(preview_sha(&preview)?);
        Ok(preview)
    }

    pub fn install(
        &self,
        dir: &Path,
        candidate_id: &str,
        request: &InstallRequest,
    ) -> Result<Value> {
        ensure!(
            request.server_stopped,
            "Confirm that the target market-server and EveJS are stopped"
        );
        let expected = self.preview(dir, candidate_id, &request.target_id)?;
        ensure!(
            expected["preview_sha256"] == request.preview_sha256,
            "Candidate, configuration or destination changed; review installation again"
        );
        ensure!(
            expected["replaces_existing"] != true || request.confirm_replace,
            "Confirm replacement of the existing market database"
        );
        let destination = self.destination(&request.target_id)?;
        let parent = destination
            .database
            .parent()
            .context("Invalid database folder")?;
        // Every existing ancestor is checked before creation, and again afterwards.
        fs::create_dir_all(parent)?;
        confined(&destination.root, &destination.database)?;
        let lock_path = parent.join(".market-workbench-install.lock");
        let lock = OpenOptions::new().write(true).create_new(true).open(&lock_path)
            .context("Another installation is in progress (or a previous interrupted installation left a lock file)")?;
        let _lock = Temporary {
            path: lock_path,
            file: Some(lock),
        };
        let (current, mut old) = destination_state(&destination.database)?;
        ensure!(
            current == expected["existing_database"],
            "Destination changed; review installation again"
        );
        let mut source = candidate(dir, candidate_id)?;
        let stamp = super::workbench::unique_stamp()?;
        let staging = parent.join(format!(".market-workbench-{stamp}.tmp"));
        let mut staged = Temporary {
            path: staging.clone(),
            file: Some(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&staging)?,
            ),
        };
        let copied_sha = copy_and_hash(&mut source, staged.file.as_mut().unwrap())?;
        ensure!(
            json!(copied_sha) == expected["candidate_sha256"],
            "Candidate changed while copying; installation cancelled"
        );
        drop(staged.file.take());
        ensure!(
            hash(&mut File::open(&staging)?)? == copied_sha,
            "Copied database verification failed; original database preserved"
        );
        let backup = if let Some(ref mut file) = old {
            let backup_path = parent.join(format!(
                "{}.backup-{stamp}",
                destination.database.file_name().unwrap().to_string_lossy()
            ));
            let mut backup = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&backup_path)?;
            let backup_sha = copy_and_hash(file, &mut backup)?;
            ensure!(
                json!(backup_sha) == current["sha256"],
                "Backup verification failed; original database preserved"
            );
            Some(backup_path)
        } else {
            None
        };
        // Re-read the server config before the atomic switch. Never rewrite it.
        ensure!(
            self.destination(&request.target_id)?.config_sha256 == destination.config_sha256,
            "Server configuration changed; review installation again"
        );
        replace_database(&staging, &destination.database, old.is_some()).context(
            "Could not install database; stop market-server and close all database viewers",
        )?;
        drop(old);
        let mut result = json!({"installed":true,"candidate_id":candidate_id,"target_id":request.target_id,
            "database_path":display(&destination.database),"market_sha256":copied_sha,"bytes":source.metadata()?.len(),
            "backup_path":backup.as_ref().map(|p|display(p)),"installed_at_unix_ns":stamp,
            "config_path":display(&destination.config),"config_sha256":destination.config_sha256,
            "services_started":false,"services_stopped":false,"candidate_preserved":true,
            "next_step":"StartMarketServer.bat in the selected EveJS folder will now use this database."});
        let receipt = parent.join(format!("market-workbench-install-{stamp}.json"));
        // A receipt failure must not misreport a completed atomic installation as failed.
        if let Err(error) = fs::write(&receipt, serde_json::to_vec_pretty(&result)?) {
            result["warning"] = json!(format!(
                "Database installed, but receipt could not be written: {error}"
            ));
        } else {
            result["receipt_path"] = json!(display(&receipt));
        }
        Ok(result)
    }
}

fn preview_sha(value: &Value) -> Result<String> {
    let mut value = value.clone();
    value.as_object_mut().unwrap().remove("preview_sha256");
    Ok(sha256_hex(&serde_json::to_vec(&value)?))
}

fn display(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    if let Some(unc) = text.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{unc}");
    }
    text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned()
}

fn confined(root: &Path, path: &Path) -> Result<PathBuf> {
    // Verbatim Windows paths disable slash/.. normalization. Validate a regular,
    // normalized path first, then canonicalize its existing ancestor.
    let normalized = PathBuf::from(display(path));
    let path = normalized.as_path();
    ensure!(
        !path.components().any(|c| matches!(c, Component::ParentDir)),
        "Parent-directory paths are not allowed in market-server database_path"
    );
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                #[cfg(windows)]
                let reparse = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let reparse = metadata.file_type().is_symlink();
                ensure!(
                    !reparse,
                    "Symlink/junction installation paths are not allowed: {}",
                    ancestor.display()
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    let mut missing = Vec::new();
    let mut ancestor = path;
    while !ancestor.exists() {
        missing.push(
            ancestor
                .file_name()
                .context("Invalid installation path")?
                .to_owned(),
        );
        ancestor = ancestor.parent().context("Invalid installation path")?;
    }
    let mut resolved = fs::canonicalize(ancestor)?;
    for part in missing.into_iter().rev() {
        resolved.push(part);
    }
    ensure!(
        resolved.starts_with(root),
        "Configured market database is outside the selected EveJS folder"
    );
    Ok(resolved)
}

fn sidecars(path: &Path) -> [PathBuf; 2] {
    [
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ]
}

fn candidate(dir: &Path, id: &str) -> Result<File> {
    let db = dir.join("market.sqlite");
    for path in [
        dir.to_owned(),
        db.clone(),
        dir.join("manifest.json"),
        dir.join("audit.json"),
    ] {
        let metadata =
            fs::symlink_metadata(&path).context("Candidate must be built and audited first")?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "Candidate links are not allowed"
            );
        }
        ensure!(
            !metadata.file_type().is_symlink(),
            "Candidate links are not allowed"
        );
    }
    let manifest: Value = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
    let audit: Value = serde_json::from_slice(&fs::read(dir.join("audit.json"))?)?;
    ensure!(
        manifest["id"] == id && audit["id"] == id,
        "Candidate identity mismatch"
    );
    ensure!(
        audit["passed"] == true && audit["market_sha256"] == manifest["market_sha256"],
        "Candidate needs a current successful audit before installation"
    );
    ensure!(
        manifest["market_sha256"]
            .as_str()
            .is_some_and(|s| s.len() == 64),
        "Candidate manifest has no database SHA256"
    );
    let mut file = exclusive_read(&db)?;
    ensure!(
        json!(hash(&mut file)?) == manifest["market_sha256"],
        "Candidate database changed since build/audit"
    );
    for path in sidecars(&db) {
        if path.exists() && path.to_string_lossy().ends_with("-wal") {
            ensure!(
                path.metadata()?.len() == 0,
                "Candidate has pending WAL data; audit/finalize it before installation"
            );
        }
    }
    Ok(file)
}

fn destination_state(path: &Path) -> Result<(Value, Option<File>)> {
    for sidecar in sidecars(path) {
        ensure!(
            !sidecar.exists(),
            "Target has SQLite WAL/SHM state. Stop market-server and close database viewers before installing; do not delete pending WAL data."
        );
    }
    if !path.exists() {
        return Ok((json!({"exists":false}), None));
    }
    let mut file = destination_guard(path)?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header)
        .context("Existing target is not a SQLite database")?;
    ensure!(
        &header == b"SQLite format 3\0",
        "Existing target is not a SQLite database"
    );
    let sha = hash(&mut file)?;
    Ok((
        json!({"exists":true,"bytes":file.metadata()?.len(),"sha256":sha}),
        Some(file),
    ))
}

fn exclusive_read(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0x4);
    } // FILE_SHARE_DELETE only: blocks existing/new SQLite readers & writers, permits atomic replacement.
    #[cfg(not(windows))]
    bail!("Safe market installation currently requires Windows file-sharing locks");
    options
        .open(path)
        .context("Database is busy or unreadable; stop market-server and close database viewers")
}

fn destination_guard(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0x1 | 0x4);
    }
    // Denies SQLite's read/write server handles. READ + DELETE sharing allows
    // ReplaceFileW to perform the atomic switch while this guard stays open.
    options
        .open(path)
        .context("Database is busy or unreadable; stop market-server and close database viewers")
}

#[cfg(windows)]
fn replace_database(staged: &Path, target: &Path, exists: bool) -> Result<()> {
    if !exists {
        return crate::workbench::atomic_install(staged, target);
    }
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let staged = staged
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let ok = unsafe {
        ReplaceFileW(
            target.as_ptr(),
            staged.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    ensure!(
        ok != 0,
        "atomic database replacement failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
#[cfg(not(windows))]
fn replace_database(_: &Path, _: &Path, _: bool) -> Result<()> {
    bail!("Safe market installation currently requires Windows file-sharing locks")
}

fn hash(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut context = HashContext::new(&digest::SHA256);
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        context.update(&buffer[..n]);
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(context
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub(crate) fn file_sha256(path: &Path) -> Result<String> {
    hash(&mut File::open(path)?)
}

fn copy_and_hash(source: &mut File, target: &mut File) -> Result<String> {
    source.seek(SeekFrom::Start(0))?;
    let mut context = HashContext::new(&digest::SHA256);
    let mut buffer = [0u8; 65536];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        target.write_all(&buffer[..n])?;
        context.update(&buffer[..n]);
    }
    target.sync_all()?;
    Ok(context
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

struct Temporary {
    path: PathBuf,
    file: Option<File>,
}
impl Drop for Temporary {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        dir: PathBuf,
        targets: Targets,
        db: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "wb-install-{}",
                crate::workbench::unique_stamp().unwrap()
            ));
            let server = root.join("externalservices/market-server");
            fs::create_dir_all(server.join("config")).unwrap();
            fs::write(root.join("StartMarketServer.bat"), "test fixture").unwrap();
            fs::write(server.join("Cargo.toml"), "test fixture").unwrap();
            fs::write(
                server.join("config/market-server.local.toml"),
                "[storage]\ndatabase_path = 'data/generated/market.sqlite'\n",
            )
            .unwrap();
            let dir = root.join("candidate");
            fs::create_dir(&dir).unwrap();
            {
                let c = rusqlite::Connection::open(dir.join("market.sqlite")).unwrap();
                c.execute_batch(
                    "CREATE TABLE fixture(value TEXT); INSERT INTO fixture VALUES('candidate');",
                )
                .unwrap();
            }
            let sha = sha256_hex(&fs::read(dir.join("market.sqlite")).unwrap());
            fs::write(
                dir.join("manifest.json"),
                json!({"id":"fixture","market_sha256":sha}).to_string(),
            )
            .unwrap();
            fs::write(
                dir.join("audit.json"),
                json!({"id":"fixture","passed":true,"market_sha256":sha}).to_string(),
            )
            .unwrap();
            let targets = Targets::new(std::slice::from_ref(&root)).unwrap();
            let db = server.join("data/generated/market.sqlite");
            Self {
                root,
                dir,
                targets,
                db,
            }
        }
        fn preview(&self) -> Value {
            self.targets
                .preview(&self.dir, "fixture", "evejs-1")
                .unwrap()
        }
        fn request(&self, preview: &Value) -> InstallRequest {
            InstallRequest {
                target_id: "evejs-1".into(),
                preview_sha256: preview["preview_sha256"].as_str().unwrap().into(),
                confirm_replace: true,
                server_stopped: true,
            }
        }
        fn existing(&self) -> Vec<u8> {
            fs::create_dir_all(self.db.parent().unwrap()).unwrap();
            {
                let c = rusqlite::Connection::open(&self.db).unwrap();
                c.execute_batch("CREATE TABLE player_data(v TEXT); INSERT INTO player_data VALUES('keep in backup');").unwrap();
            }
            fs::read(&self.db).unwrap()
        }
    }
    #[test]
    fn preview_uses_server_workdir_and_does_not_create_destination() {
        let f = Fixture::new();
        let p = f.preview();
        assert_eq!(p["database_path"], display(&f.db));
        assert_eq!(p["replaces_existing"], false);
        assert!(!f.db.parent().unwrap().exists());
        assert_eq!(
            f.targets.list()["targets"][0]["database_path"],
            p["database_path"]
        );
    }
    #[test]
    fn fresh_install_is_exact_and_preserves_candidate_and_config() {
        let f = Fixture::new();
        let p = f.preview();
        let before = fs::read(f.dir.join("market.sqlite")).unwrap();
        let config = fs::read(
            f.root
                .join("externalservices/market-server/config/market-server.local.toml"),
        )
        .unwrap();
        let result = f
            .targets
            .install(&f.dir, "fixture", &f.request(&p))
            .unwrap();
        assert_eq!(fs::read(&f.db).unwrap(), before);
        assert_eq!(fs::read(f.dir.join("market.sqlite")).unwrap(), before);
        assert_eq!(
            fs::read(
                f.root
                    .join("externalservices/market-server/config/market-server.local.toml")
            )
            .unwrap(),
            config
        );
        assert_eq!(result["backup_path"], Value::Null);
        assert_eq!(result["services_started"], false);
        assert_eq!(result["services_stopped"], false);
        assert!(
            !f.db
                .parent()
                .unwrap()
                .join(".market-workbench-install.lock")
                .exists()
        );
    }
    #[test]
    fn replacement_backs_up_exact_old_database() {
        let f = Fixture::new();
        let old = f.existing();
        let p = f.preview();
        let result = f
            .targets
            .install(&f.dir, "fixture", &f.request(&p))
            .unwrap();
        assert_eq!(
            fs::read(result["backup_path"].as_str().unwrap()).unwrap(),
            old
        );
        assert_eq!(
            fs::read(&f.db).unwrap(),
            fs::read(f.dir.join("market.sqlite")).unwrap()
        );
    }
    #[test]
    fn replacement_requires_explicit_confirmation() {
        let f = Fixture::new();
        let old = f.existing();
        let p = f.preview();
        let mut r = f.request(&p);
        r.confirm_replace = false;
        assert!(
            f.targets
                .install(&f.dir, "fixture", &r)
                .unwrap_err()
                .to_string()
                .contains("Confirm replacement")
        );
        assert_eq!(fs::read(&f.db).unwrap(), old);
    }
    #[test]
    fn requires_stopped_server_acknowledgement() {
        let f = Fixture::new();
        let p = f.preview();
        let mut r = f.request(&p);
        r.server_stopped = false;
        assert!(f.targets.install(&f.dir, "fixture", &r).is_err());
        assert!(!f.db.exists());
    }
    #[test]
    fn changed_destination_invalidates_confirmation() {
        let f = Fixture::new();
        let p = f.preview();
        let old = f.existing();
        assert!(
            f.targets
                .install(&f.dir, "fixture", &f.request(&p))
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        assert_eq!(fs::read(&f.db).unwrap(), old);
    }
    #[test]
    fn changed_server_config_invalidates_confirmation() {
        let f = Fixture::new();
        let p = f.preview();
        fs::write(
            f.root
                .join("externalservices/market-server/config/market-server.local.toml"),
            "[storage]\ndatabase_path = 'data/other.sqlite'\n",
        )
        .unwrap();
        assert!(
            f.targets
                .install(&f.dir, "fixture", &f.request(&p))
                .is_err()
        );
        assert!(
            !f.root
                .join("externalservices/market-server/data/other.sqlite")
                .exists()
        );
    }
    #[test]
    fn changed_candidate_or_failed_audit_blocks_installation() {
        let f = Fixture::new();
        let p = f.preview();
        fs::write(f.dir.join("market.sqlite"), "tampered").unwrap();
        assert!(
            f.targets
                .install(&f.dir, "fixture", &f.request(&p))
                .is_err()
        );
        assert!(!f.db.exists());
        let f = Fixture::new();
        let mut audit: Value =
            serde_json::from_slice(&fs::read(f.dir.join("audit.json")).unwrap()).unwrap();
        audit["passed"] = json!(false);
        fs::write(f.dir.join("audit.json"), audit.to_string()).unwrap();
        assert!(f.targets.preview(&f.dir, "fixture", "evejs-1").is_err());
    }
    #[test]
    fn rejects_wrong_candidate_or_target_identity_and_browser_paths() {
        let f = Fixture::new();
        assert!(f.targets.preview(&f.dir, "other", "evejs-1").is_err());
        for id in ["../evejs-1", "evejs-0", "evejs-01", "evejs-2", "C:/game"] {
            assert!(f.targets.preview(&f.dir, "fixture", id).is_err());
        }
        assert!(
            serde_json::from_value::<PreviewRequest>(
                json!({"target_id":"evejs-1","output_path":"C:/game/market.sqlite"})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<InstallRequest>(json!({"target_id":"evejs-1","preview_sha256":"x","confirm_replace":true,"server_stopped":true,"deploy":true})).is_err());
    }
    #[test]
    fn rejects_config_output_outside_installation_and_parent_traversal() {
        let f = Fixture::new();
        let config = f
            .root
            .join("externalservices/market-server/config/market-server.local.toml");
        for path in [
            "../../outside.sqlite",
            "C:/outside.sqlite",
            "data/overwrite.toml",
        ] {
            fs::write(&config, format!("[storage]\ndatabase_path = '{path}'\n")).unwrap();
            assert!(f.targets.preview(&f.dir, "fixture", "evejs-1").is_err());
        }
    }
    #[test]
    fn open_database_blocks_replacement_without_wal() {
        let f = Fixture::new();
        let old = f.existing();
        let p = f.preview();
        let _open = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&f.db)
            .unwrap();
        assert!(
            f.targets
                .install(&f.dir, "fixture", &f.request(&p))
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        assert_eq!(fs::read(&f.db).unwrap(), old);
    }
    #[test]
    fn target_wal_or_shm_is_preserved_and_blocks_installation() {
        let f = Fixture::new();
        let old = f.existing();
        let p = f.preview();
        for sidecar in sidecars(&f.db) {
            fs::write(&sidecar, "pending state").unwrap();
            assert!(
                f.targets
                    .install(&f.dir, "fixture", &f.request(&p))
                    .is_err()
            );
            assert_eq!(fs::read(&sidecar).unwrap(), b"pending state");
            assert_eq!(fs::read(&f.db).unwrap(), old);
            fs::remove_file(&sidecar).unwrap();
        }
    }
    #[test]
    fn candidate_with_pending_wal_is_not_copied() {
        let f = Fixture::new();
        fs::write(sidecars(&f.dir.join("market.sqlite"))[0].clone(), "pending").unwrap();
        assert!(f.targets.preview(&f.dir, "fixture", "evejs-1").is_err());
        assert!(!f.db.exists());
    }
    #[test]
    fn absolute_config_path_inside_registered_root_is_supported() {
        let f = Fixture::new();
        let path = display(&f.root.join("data/custom.sqlite"));
        fs::write(
            f.root
                .join("externalservices/market-server/config/market-server.local.toml"),
            format!("[storage]\ndatabase_path = '{path}'\n"),
        )
        .unwrap();
        let p = f.preview();
        assert_eq!(p["database_path"], path);
    }
    #[test]
    fn existing_installer_lock_preserves_database() {
        let f = Fixture::new();
        let old = f.existing();
        let p = f.preview();
        let lock =
            f.db.parent()
                .unwrap()
                .join(".market-workbench-install.lock");
        fs::write(&lock, "other installation").unwrap();
        assert!(
            f.targets
                .install(&f.dir, "fixture", &f.request(&p))
                .is_err()
        );
        assert_eq!(fs::read(&f.db).unwrap(), old);
        assert!(lock.exists());
    }
    #[test]
    fn streaming_hash_matches_core_and_copied_file() {
        let f = Fixture::new();
        let bytes = fs::read(f.dir.join("market.sqlite")).unwrap();
        assert_eq!(
            hash(&mut File::open(f.dir.join("market.sqlite")).unwrap()).unwrap(),
            sha256_hex(&bytes)
        );
        assert!(Targets::new(&[f.dir.clone()]).is_err());
    }
}
