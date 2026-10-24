//! `multiprovider-auth.json`: a byte-for-byte copy into the rpi agent directory.
//!
//! The file is pooled credentials and is never parsed or rewritten. It is copied
//! only when the destination is absent or already byte-identical; a different
//! existing file is a conflict. The copy is created with owner-only permissions.

use std::path::Path;

use super::ledger::Ledger;
use super::options::Category;
use super::plan::{record, Item, Outcome, Plan};
use super::source;
use super::util::{sha256_hex, write_private};

pub(crate) const DEST_FILE: &str = "multiprovider-auth.json";

/// The bytes to write to `dest_file`.
pub(crate) type Writes = Option<Vec<u8>>;

pub(crate) fn plan(root: &Path, dest_file: &Path, ledger: &Ledger) -> Result<Plan<Writes>, String> {
    let mut plan = Plan::new(None);
    let Some(bytes) = source::read_file(root, source::MULTIPROVIDER_FILE)? else {
        return Ok(plan);
    };
    let source_path = root
        .join(source::MULTIPROVIDER_FILE)
        .to_string_lossy()
        .into_owned();
    let sha = sha256_hex(&bytes);
    let previous = ledger.lookup(Category::Multiprovider.name(), &source_path, "");

    if previous.is_some_and(|record| record.sha256 == sha) {
        plan.items.push(Item::new(
            Category::Multiprovider,
            DEST_FILE,
            Outcome::AlreadyImported,
            "same contents as the last import",
        ));
        return Ok(plan);
    }
    let changed = if previous.is_some() {
        "source changed since the last import; "
    } else {
        ""
    };

    match std::fs::read(dest_file) {
        Ok(existing) if existing == bytes => {
            plan.items.push(Item::new(
                Category::Multiprovider,
                DEST_FILE,
                Outcome::AlreadyPresent,
                format!("{changed}destination is already identical"),
            ));
            plan.records.push(record(
                Category::Multiprovider,
                &source_path,
                "",
                &sha,
                DEST_FILE,
            ));
        }
        Ok(_) => {
            plan.items.push(Item::new(
                Category::Multiprovider,
                DEST_FILE,
                Outcome::Conflict,
                format!("{changed}destination exists with different contents; not overwritten"),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            plan.items.push(Item::new(
                Category::Multiprovider,
                DEST_FILE,
                Outcome::Import,
                format!("{changed}{} bytes, copied unchanged", bytes.len()),
            ));
            plan.records.push(record(
                Category::Multiprovider,
                &source_path,
                "",
                &sha,
                DEST_FILE,
            ));
            plan.writes = Some(bytes);
        }
        Err(error) => return Err(format!("could not read destination {DEST_FILE}: {error}")),
    }
    Ok(plan)
}

/// Write the planned bytes if the destination is still absent. A destination
/// that appeared after planning is compared, never overwritten.
pub(crate) fn apply(bytes: Option<&[u8]>, dest_file: &Path) -> Result<(), String> {
    let Some(bytes) = bytes else {
        return Ok(());
    };
    match std::fs::read(dest_file) {
        Ok(existing) if existing == bytes => Ok(()),
        Ok(_) => Err(format!(
            "{DEST_FILE} appeared in the destination after planning; not overwritten"
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_private(dest_file, bytes)
        }
        Err(error) => Err(format!("could not read destination {DEST_FILE}: {error}")),
    }
}
