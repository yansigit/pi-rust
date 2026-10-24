//! Planning types shared by every import category.
//!
//! A category planner reads its source, compares it with the destination and
//! the ledger, and returns a [`Plan`]: one [`Item`] per thing it looked at, the
//! ledger records to persist once the category has been applied, and the
//! category's own write set. Planning never writes.

use super::ledger::Record;
use super::options::Category;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Will be written by the apply step.
    Import,
    /// The ledger already holds this exact source content; nothing changes.
    AlreadyImported,
    /// The destination already holds identical content; a ledger record will
    /// be added so later rpi edits to the destination are not mistaken for
    /// source changes.
    AlreadyPresent,
    /// Not imported, for a reason rpi can state.
    Skip,
    /// A key or file rpi deliberately does not import.
    Ignore,
    /// The destination holds different content. Nothing is written when any
    /// conflict exists.
    Conflict,
}

impl Outcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Outcome::Import => "import",
            Outcome::AlreadyImported => "already imported",
            Outcome::AlreadyPresent => "already present",
            Outcome::Skip => "skip",
            Outcome::Ignore => "ignore",
            Outcome::Conflict => "conflict",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) category: Category,
    pub(crate) name: String,
    pub(crate) outcome: Outcome,
    pub(crate) detail: String,
}

impl Item {
    pub(crate) fn new(
        category: Category,
        name: impl Into<String>,
        outcome: Outcome,
        detail: impl Into<String>,
    ) -> Item {
        Item {
            category,
            name: name.into(),
            outcome,
            detail: detail.into(),
        }
    }
}

pub(crate) struct Plan<W> {
    pub(crate) items: Vec<Item>,
    /// Ledger records for `Import` and `AlreadyPresent` items. Persisted after
    /// the category has been applied.
    pub(crate) records: Vec<Record>,
    pub(crate) writes: W,
}

impl<W> Plan<W> {
    pub(crate) fn new(writes: W) -> Plan<W> {
        Plan {
            items: Vec::new(),
            records: Vec::new(),
            writes,
        }
    }
}

/// The ledger record for an item that is now accounted for.
pub(crate) fn record(
    category: Category,
    source: &str,
    unit: &str,
    sha256: &str,
    destination: &str,
) -> Record {
    Record {
        category: category.name().into(),
        source: source.into(),
        unit: unit.into(),
        sha256: sha256.into(),
        destination: destination.into(),
    }
}
