//! Human-readable output for the import plan and its result.

use std::path::Path;

use super::options::Category;
use super::plan::{Item, Outcome};

pub(crate) const HELP: &str = "\
rpi import pi - import a Pi agent directory into rpi's storage

Usage: rpi import pi [--from DIR] [--dry-run] [--yes] [--only LIST]

Source directory: --from DIR, else $PI_CODING_AGENT_DIR, else ~/.pi/agent.
Only these are read: auth.json, settings.json, multiprovider-auth.json and
sessions/*.jsonl. The source is never written to.

Options:
  --from DIR      Pi agent directory (the one that holds auth.json)
  --dry-run       Print the reviewable summary and stop; writes nothing
  --yes           Apply without the confirmation prompt. Required when stdin
                  is not a terminal.
  --only LIST     Comma list of categories: credentials, settings,
                  multiprovider, sessions (default: all of them)
  -h, --help      Show this help

A destination that already holds different content for an item refuses the
whole run, and nothing is written. Re-running a completed import reports
\"already imported\" and changes nothing.

See docs/provider-parity/import.md for the formats, skip reasons and ledger.
";

pub(crate) fn print_header(source: &Path, destination: &Path) {
    println!("rpi import pi");
    println!("  source:       {}", source.display());
    println!("  destination:  {}", destination.display());
    println!();
}

/// Print one category's items under its heading.
pub(crate) fn print_category(category: Category, heading: &str, items: &[Item]) {
    println!("{} ({heading})", category.name());
    if items.is_empty() {
        println!("  nothing to import (not present in the source)");
    }
    for item in items {
        println!(
            "  {:<18}{:<28}{}",
            item.outcome.label(),
            truncate(&item.name, 26),
            item.detail
        );
    }
    println!();
}

/// Print the totals line.
pub(crate) fn print_summary(items: &[&Item]) {
    let count = |outcome: Outcome| items.iter().filter(|item| item.outcome == outcome).count();
    println!(
        "summary: {} to import, {} already imported, {} already present, {} skipped, {} ignored, {} conflict(s)",
        count(Outcome::Import),
        count(Outcome::AlreadyImported),
        count(Outcome::AlreadyPresent),
        count(Outcome::Skip),
        count(Outcome::Ignore),
        count(Outcome::Conflict),
    );
}

pub(crate) const OAUTH_NOTICE: &str = "\
notice: copied OAuth refresh credentials may rotate. Pi and rpi each refresh
tokens on their own. After either side refreshes, the other copy can stop
working, so a fresh login may be required and the source login may stop
working.";

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}~")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_short_names_and_marks_long_ones() {
        assert_eq!(truncate("short", 26), "short");
        let long = "x".repeat(40);
        let cut = truncate(&long, 26);
        assert_eq!(cut.chars().count(), 26);
        assert!(cut.ends_with('~'));
    }
}
