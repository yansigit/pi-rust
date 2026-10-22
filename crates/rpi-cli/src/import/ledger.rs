//! The import ledger: `import-ledger.json` in the rpi agent directory.
//!
//! Each record says "this source unit, with this content hash, was imported
//! (or was already present) and is accounted for". The key is
//! `(category, source path, unit)`; the unit is a provider id for credentials,
//! a setting key for settings, and empty for whole-file categories.
//!
//! A repeat import whose source hash matches a record reports *already imported*
//! and writes nothing. A record with a different hash means the source changed
//! since it was imported, which the planner reports as such.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) const LEDGER_FILE: &str = "import-ledger.json";
const LEDGER_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) category: String,
    pub(crate) source: String,
    pub(crate) unit: String,
    pub(crate) sha256: String,
    pub(crate) destination: String,
}

#[derive(Serialize, Deserialize)]
struct LedgerFile {
    version: u32,
    records: Vec<Record>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ledger {
    path: PathBuf,
    records: BTreeMap<(String, String, String), Record>,
}

impl Ledger {
    /// Load the ledger at `path`. A missing file is an empty ledger; a file that
    /// cannot be parsed is an error, because guessing would risk re-importing or
    /// hiding a conflict.
    pub(crate) fn load(path: &Path) -> Result<Ledger, String> {
        let mut ledger = Ledger {
            path: path.to_path_buf(),
            records: BTreeMap::new(),
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ledger),
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        let file: LedgerFile = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "{} is not a valid import ledger ({error}); fix or move it aside",
                path.display()
            )
        })?;
        if file.version != LEDGER_VERSION {
            return Err(format!(
                "{} has unsupported ledger version {}",
                path.display(),
                file.version
            ));
        }
        for record in file.records {
            ledger.records.insert(key_of(&record), record);
        }
        Ok(ledger)
    }

    /// The record for `(category, source, unit)`, if any.
    pub(crate) fn lookup(&self, category: &str, source: &str, unit: &str) -> Option<&Record> {
        self.records
            .get(&(category.to_string(), source.to_string(), unit.to_string()))
    }

    /// Insert or replace a record.
    pub(crate) fn record(&mut self, record: Record) {
        self.records.insert(key_of(&record), record);
    }

    /// Persist the ledger atomically with owner-only permissions. Records are
    /// written in key order, so the file is stable across runs.
    pub(crate) fn save(&self) -> Result<(), String> {
        let file = LedgerFile {
            version: LEDGER_VERSION,
            records: self.records.values().cloned().collect(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        super::util::write_private(&self.path, &bytes)
    }
}

fn key_of(record: &Record) -> (String, String, String) {
    (
        record.category.clone(),
        record.source.clone(),
        record.unit.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(unit: &str, sha: &str) -> Record {
        Record {
            category: "credentials".into(),
            source: "/src/auth.json".into(),
            unit: unit.into(),
            sha256: sha.into(),
            destination: "auth.json".into(),
        }
    }

    #[test]
    fn round_trips_and_looks_up_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);
        let mut ledger = Ledger::load(&path).unwrap();
        assert!(ledger
            .lookup("credentials", "/src/auth.json", "a")
            .is_none());
        ledger.record(record("a", "1"));
        ledger.record(record("b", "2"));
        ledger.save().unwrap();

        let loaded = Ledger::load(&path).unwrap();
        assert_eq!(loaded, ledger);
        assert_eq!(
            loaded
                .lookup("credentials", "/src/auth.json", "b")
                .unwrap()
                .sha256,
            "2"
        );
    }

    #[test]
    fn rejects_corrupt_and_unknown_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);
        std::fs::write(&path, "{not json").unwrap();
        assert!(Ledger::load(&path).is_err());
        std::fs::write(&path, r#"{"version":9,"records":[]}"#).unwrap();
        assert!(Ledger::load(&path).is_err());
    }
}
