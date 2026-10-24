//! Pi session files: archival copies with visible origin metadata.
//!
//! Each `sessions/**.jsonl` file is copied into `<session-root>/pi-import/`,
//! keeping its sub-directory name. The copy is byte-identical to the source
//! except for the header line (the first line), which gains one field:
//!
//! ```json
//! "imported": {"source": "pi", "sourcePath": "...", "sha256": "...", "sessionVersion": 3}
//! ```
//!
//! `sha256` is the digest of the Pi file as it was read. Every later line is
//! copied byte for byte, so the entry tree, custom entries and any markers they
//! carry are preserved verbatim and nothing in them is interpreted. Native ports
//! must check `imported.source` before granting resume, cleanup or cloud
//! authority to a session.
//!
//! File names carry the first 12 hex digits of the source digest
//! (`<stem>.<sha12>.jsonl`). A session that grew after an earlier import therefore
//! gets a new snapshot next to the old one instead of overwriting it.
//!
//! rpi's own session reader accepts only its v4 format and cannot open Pi v3
//! files yet, so these copies are not resumable from rpi. They are stored, and
//! their origin is recorded, so that a later reader or a native port can use them.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

use super::ledger::Ledger;
use super::options::Category;
use super::plan::{record, Item, Outcome, Plan};
use super::source::{self, SessionEntry};
use super::util::{sha256_hex, write_private};

/// Archive directory under the same resolved session root used by startup.
pub(crate) const DEST_DIR: &str = "pi-import";
/// Header field that records where a copy came from.
pub(crate) const ORIGIN_FIELD: &str = "imported";
/// Newest Pi session format this importer understands (`1` and `2` are migrated by Pi on load).
const MAX_SESSION_VERSION: u64 = 3;

/// One file to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Write {
    pub(crate) dest: PathBuf,
    pub(crate) bytes: Vec<u8>,
}

pub(crate) type Writes = Vec<Write>;

pub(crate) fn plan(
    root: &Path,
    session_dir: &Path,
    ledger: &Ledger,
) -> Result<Plan<Writes>, String> {
    let mut plan = Plan::new(Vec::new());
    let dest_root = session_dir.join(DEST_DIR);
    for entry in source::list_sessions(root)? {
        match entry {
            SessionEntry::Skipped { relative, reason } => {
                plan.items.push(Item::new(
                    Category::Sessions,
                    relative,
                    Outcome::Skip,
                    reason,
                ));
            }
            SessionEntry::File { relative, path } => {
                plan_file(&mut plan, &dest_root, ledger, &relative, &path)?;
            }
        }
    }
    Ok(plan)
}

fn plan_file(
    plan: &mut Plan<Writes>,
    dest_root: &Path,
    ledger: &Ledger,
    relative: &str,
    path: &Path,
) -> Result<(), String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            plan.items.push(Item::new(
                Category::Sessions,
                relative,
                Outcome::Skip,
                format!("could not read: {error}"),
            ));
            return Ok(());
        }
    };
    let source_path = path.to_string_lossy().into_owned();
    let sha = sha256_hex(&bytes);
    let previous = ledger.lookup(Category::Sessions.name(), &source_path, "");
    if previous.is_some_and(|record| record.sha256 == sha) {
        plan.items.push(Item::new(
            Category::Sessions,
            relative,
            Outcome::AlreadyImported,
            "same contents as the last import",
        ));
        return Ok(());
    }
    let changed = if previous.is_some() {
        "source changed since the last import; "
    } else {
        ""
    };

    let (header, version) = match validate(&bytes) {
        Ok(checked) => checked,
        Err(reason) => {
            plan.items.push(Item::new(
                Category::Sessions,
                relative,
                Outcome::Skip,
                format!("{changed}{reason}"),
            ));
            return Ok(());
        }
    };
    let rewritten = rewrite(&bytes, header, &source_path, &sha, version)?;
    let dest_relative = dest_relative(relative, &sha);
    let dest = dest_root.join(&dest_relative);
    let destination = dest.to_string_lossy().into_owned();
    let record_key = record(Category::Sessions, &source_path, "", &sha, &destination);

    match std::fs::read(&dest) {
        Ok(existing) if existing == rewritten => {
            plan.items.push(Item::new(
                Category::Sessions,
                relative,
                Outcome::AlreadyPresent,
                format!("{changed}destination is already identical ({destination})"),
            ));
            plan.records.push(record_key);
        }
        Ok(_) => {
            plan.items.push(Item::new(
                Category::Sessions,
                relative,
                Outcome::Conflict,
                format!("{changed}{destination} exists with different contents; not overwritten"),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            plan.items.push(Item::new(
                Category::Sessions,
                relative,
                Outcome::Import,
                format!("{changed}-> {destination}"),
            ));
            plan.records.push(record_key);
            plan.writes.push(Write {
                dest,
                bytes: rewritten,
            });
        }
        Err(error) => return Err(format!("could not read destination {destination}: {error}")),
    }
    Ok(())
}

/// Write the planned session copies, refusing to overwrite any file that appeared
/// after planning.
pub(crate) fn apply(writes: &[Write]) -> Result<(), String> {
    for write in writes {
        match std::fs::read(&write.dest) {
            Ok(existing) if existing == write.bytes => {}
            Ok(_) => {
                return Err(format!(
                    "{} appeared after planning with different contents; not overwritten",
                    write.dest.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_private(&write.dest, &write.bytes)?;
            }
            Err(error) => return Err(format!("could not read {}: {error}", write.dest.display())),
        }
    }
    Ok(())
}

/// Check that `bytes` is a Pi session file this importer can copy, and return its
/// header and version. Every line must be a JSON object, so a torn or corrupt
/// file is reported rather than copied.
fn validate(bytes: &[u8]) -> Result<(Map<String, Value>, u64), String> {
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let first = lines.next().unwrap_or(&[]);
    let first = std::str::from_utf8(first).map_err(|_| "header is not UTF-8".to_string())?;
    let first = first.trim_end_matches('\r');
    if first.trim().is_empty() {
        return Err("empty file".into());
    }
    let header = match serde_json::from_str::<Value>(first) {
        Ok(Value::Object(map)) => map,
        Ok(_) => return Err("first line is not a JSON object".into()),
        Err(error) => return Err(format!("first line is not JSON ({error})")),
    };
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return Err("first line is not a Pi session header".into());
    }
    let version = header
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| "session header has no integer version".to_string())?;
    if !(1..=MAX_SESSION_VERSION).contains(&version) {
        return Err(format!("unsupported Pi session version {version}"));
    }
    match header.get("id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => {}
        _ => return Err("session header has no id".into()),
    }
    if header.contains_key(ORIGIN_FIELD) {
        return Err(format!(
            "session header already has a `{ORIGIN_FIELD}` field"
        ));
    }
    for (index, line) in lines.enumerate() {
        let line = std::str::from_utf8(line).map_err(|_| "a line is not UTF-8".to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(_)) => {}
            _ => return Err(format!("line {} is not a JSON object", index + 2)),
        }
    }
    Ok((header, version))
}

/// The copy: the header with its origin field appended, then every later line
/// exactly as it appeared in the source.
fn rewrite(
    bytes: &[u8],
    mut header: Map<String, Value>,
    source_path: &str,
    sha: &str,
    version: u64,
) -> Result<Vec<u8>, String> {
    header.insert(
        ORIGIN_FIELD.into(),
        json!({
            "source": "pi",
            "sourcePath": source_path,
            "sha256": sha,
            "sessionVersion": version,
        }),
    );
    let mut out = serde_json::to_vec(&Value::Object(header)).map_err(|error| error.to_string())?;
    out.push(b'\n');
    if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
        out.extend_from_slice(&bytes[newline + 1..]);
    }
    Ok(out)
}

/// `dir/stem.sha12.jsonl` for a source `dir/stem.jsonl`.
fn dest_relative(relative: &str, sha: &str) -> String {
    let (directory, file) = match relative.rsplit_once('/') {
        Some((directory, file)) => (Some(directory), file),
        None => (None, relative),
    };
    let stem = file.strip_suffix(".jsonl").unwrap_or(file);
    let short = &sha[..12];
    match directory {
        Some(directory) => format!("{directory}/{stem}.{short}.jsonl"),
        None => format!("{stem}.{short}.jsonl"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = r#"{"type":"session","version":3,"id":"abc","timestamp":"2026-10-01T00:00:00.000Z","cwd":"/home/u/proj"}"#;

    #[test]
    fn copy_keeps_every_later_line_byte_for_byte() {
        let source = format!("{HEADER}\n{{\"type\":\"custom\",\"id\":\"c1\",\"parentId\":null,\"customType\":\"x\",\"data\":{{\"a\": 1}}}}\n");
        let (header, version) = validate(source.as_bytes()).unwrap();
        let out = rewrite(
            source.as_bytes(),
            header,
            "/p/a.jsonl",
            &sha256_hex(source.as_bytes()),
            version,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let (first, rest) = text.split_once('\n').unwrap();
        assert_eq!(rest.as_bytes(), &source.as_bytes()[HEADER.len() + 1..]);
        let parsed: Value = serde_json::from_str(first).unwrap();
        assert_eq!(parsed["imported"]["source"], "pi");
        assert_eq!(parsed["imported"]["sessionVersion"], 3);
        assert_eq!(parsed["id"], "abc");
    }

    #[test]
    fn validation_reasons() {
        assert!(validate(b"").unwrap_err().contains("empty"));
        assert!(
            validate(b"{\"type\":\"message\",\"version\":3,\"id\":\"x\"}\n")
                .unwrap_err()
                .contains("not a Pi session header")
        );
        assert!(
            validate(b"{\"type\":\"session\",\"version\":4,\"id\":\"x\"}\n")
                .unwrap_err()
                .contains("unsupported")
        );
        assert!(
            validate(b"{\"type\":\"session\",\"version\":3,\"id\":\"\"}\n")
                .unwrap_err()
                .contains("no id")
        );
        let torn = format!("{HEADER}\n{{\"type\":\"message\"\n");
        assert!(validate(torn.as_bytes()).unwrap_err().contains("line 2"));
        let stamped = HEADER.replace("}", ",\"imported\":{}}");
        assert!(validate(stamped.as_bytes())
            .unwrap_err()
            .contains("already has"));
    }

    #[test]
    fn snapshot_names_carry_the_digest() {
        assert_eq!(
            dest_relative("--p--/a.jsonl", "0123456789abcdef"),
            "--p--/a.0123456789ab.jsonl"
        );
        assert_eq!(
            dest_relative("a.jsonl", "0123456789abcdef"),
            "a.0123456789ab.jsonl"
        );
    }
}
