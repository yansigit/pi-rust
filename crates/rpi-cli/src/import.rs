//! `rpi import pi`: an explicit, reviewable import of a Pi 1.3.0 agent directory.
//!
//! The flow is always the same:
//!
//! 1. Resolve the source (`--from`, `PI_CODING_AGENT_DIR`, or `~/.pi/agent`) and
//!    read only the allowlisted items: `auth.json`, `settings.json`,
//!    `multiprovider-auth.json` and `sessions/*.jsonl`.
//! 2. Plan every selected category without writing anything, and print the plan.
//! 3. If any item conflicts with destination content, refuse. Nothing is written.
//! 4. `--dry-run` stops here. Otherwise confirm (or require `--yes` when stdin is
//!    not a terminal), then apply each category and persist the import ledger
//!    after it.
//!
//! The ledger (`import-ledger.json` in the rpi agent directory) makes re-runs
//! idempotent and makes a changed source visible.
//!
//! The submodules are declared with explicit `#[path]` attributes because this
//! file is loaded from `app.rs` as `#[path = "import.rs"]`, which means nested
//! modules are not looked up under `import/` automatically.

#[path = "import/credentials.rs"]
mod credentials;
#[path = "import/ledger.rs"]
mod ledger;
#[path = "import/multiprovider.rs"]
mod multiprovider;
#[path = "import/options.rs"]
mod options;
#[path = "import/plan.rs"]
mod plan;
#[path = "import/report.rs"]
mod report;
#[path = "import/sessions.rs"]
mod sessions;
#[path = "import/settings.rs"]
mod settings;
#[path = "import/source.rs"]
mod source;
#[path = "import/util.rs"]
mod util;

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use super::{EXIT_RUNTIME, EXIT_USAGE};
use ledger::Ledger;
use options::{Category, Options, Parsed};
use plan::{Item, Outcome, Plan};

/// Entry point for `rpi import ...`. `args` excludes the `import` word itself.
pub(crate) async fn run(args: &[String]) -> i32 {
    match options::parse(args) {
        Parsed::Help => {
            print!("{}", report::HELP);
            0
        }
        Parsed::Usage(message) => {
            eprintln!("error: {message}");
            eprintln!("try `rpi import --help`");
            EXIT_USAGE
        }
        Parsed::Run(options) => match execute(options).await {
            Ok(code) => code,
            Err(message) => {
                eprintln!("error: {message}");
                EXIT_RUNTIME
            }
        },
    }
}

/// The planned work for one category, kept with its own write set.
enum Prepared {
    Credentials(Plan<credentials::Writes>),
    Settings(Plan<settings::Writes>),
    Multiprovider(Plan<multiprovider::Writes>),
    Sessions(Plan<sessions::Writes>),
}

impl Prepared {
    fn category(&self) -> Category {
        match self {
            Prepared::Credentials(_) => Category::Credentials,
            Prepared::Settings(_) => Category::Settings,
            Prepared::Multiprovider(_) => Category::Multiprovider,
            Prepared::Sessions(_) => Category::Sessions,
        }
    }

    fn items(&self) -> &[Item] {
        match self {
            Prepared::Credentials(plan) => &plan.items,
            Prepared::Settings(plan) => &plan.items,
            Prepared::Multiprovider(plan) => &plan.items,
            Prepared::Sessions(plan) => &plan.items,
        }
    }

    fn has_ledger_changes(&self) -> bool {
        match self {
            Prepared::Credentials(plan) => !plan.records.is_empty(),
            Prepared::Settings(plan) => !plan.records.is_empty(),
            Prepared::Multiprovider(plan) => !plan.records.is_empty(),
            Prepared::Sessions(plan) => !plan.records.is_empty(),
        }
    }

    /// `source -> destination` for the category heading.
    fn heading(&self, dest: &Path, session_dir: &Path) -> String {
        let to = |name: &str| dest.join(name).display().to_string();
        match self {
            Prepared::Credentials(_) => {
                format!("{} -> {}", source::AUTH_FILE, to(source::AUTH_FILE))
            }
            Prepared::Settings(_) => {
                format!("{} -> {}", source::SETTINGS_FILE, to(settings::DEST_FILE))
            }
            Prepared::Multiprovider(_) => format!(
                "{} -> {}",
                source::MULTIPROVIDER_FILE,
                to(multiprovider::DEST_FILE)
            ),
            Prepared::Sessions(_) => {
                format!(
                    "{}/ -> {}/",
                    source::SESSIONS_DIR,
                    session_dir.join(sessions::DEST_DIR).display()
                )
            }
        }
    }
}

async fn execute(options: Options) -> Result<i32, String> {
    let cwd = std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?;
    let source = source::resolve(
        options.from.as_deref(),
        std::env::var_os(source::PI_DIR_ENV),
        dirs::home_dir(),
        &cwd,
    )?;
    let dest = crate::config::agent_dir().map_err(|error| error.to_string())?;
    if same_location(&source, &dest) {
        return Err(format!(
            "the source and the rpi agent directory are the same ({}); nothing to import",
            source.display()
        ));
    }
    if !source.is_dir() {
        return Err(format!(
            "no Pi agent directory at {}; pass --from DIR",
            source.display()
        ));
    }

    // Match normal startup, including a saved sessionDir and the environment
    // fallback. Resolve before applying settings (Pi's sessionDir is ignored).
    let session_dir = crate::session::configured_session_dir(&crate::args::Args::default(), &cwd);
    let mut import_ledger = Ledger::load(&dest.join(ledger::LEDGER_FILE))?;
    let mut prepared = Vec::new();
    for category in &options.categories {
        prepared.push(match category {
            Category::Credentials => Prepared::Credentials(credentials::plan(
                &source,
                &dest.join(source::AUTH_FILE),
                &import_ledger,
            )?),
            Category::Settings => Prepared::Settings(settings::plan(
                &source,
                &dest.join(settings::DEST_FILE),
                &import_ledger,
            )?),
            Category::Multiprovider => Prepared::Multiprovider(multiprovider::plan(
                &source,
                &dest.join(multiprovider::DEST_FILE),
                &import_ledger,
            )?),
            Category::Sessions => {
                Prepared::Sessions(sessions::plan(&source, &session_dir, &import_ledger)?)
            }
        });
    }

    report::print_header(&source, &dest);
    for item_group in &prepared {
        let heading = item_group.heading(&dest, &session_dir);
        report::print_category(item_group.category(), &heading, item_group.items());
    }
    let all_items: Vec<&Item> = prepared.iter().flat_map(|p| p.items().iter()).collect();
    report::print_summary(&all_items);

    let conflicts = all_items
        .iter()
        .filter(|item| item.outcome == Outcome::Conflict)
        .count();
    if prepared.iter().any(has_oauth_write) {
        println!("{}", report::OAUTH_NOTICE);
    }
    if conflicts > 0 {
        eprintln!(
            "refusing to import: {conflicts} conflict(s); nothing was written. Resolve the \
             destination, or leave that category out with --only."
        );
        return Ok(EXIT_RUNTIME);
    }
    if !prepared.iter().any(Prepared::has_ledger_changes) {
        println!("nothing to import: every item is already imported or skipped");
        return Ok(0);
    }
    if options.dry_run {
        println!("dry run: nothing was written");
        return Ok(0);
    }
    if !options.yes {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            eprintln!(
                "refusing to import on a non-interactive input without --yes. Review with \
                 --dry-run, then re-run with --yes."
            );
            return Ok(EXIT_RUNTIME);
        }
        let mut out = std::io::stdout();
        if !confirm(&mut stdin.lock(), &mut out) {
            println!("import cancelled; nothing was written");
            return Ok(EXIT_RUNTIME);
        }
    }

    for group in prepared {
        let category = group.category();
        let applied = apply_group(group, &dest, &mut import_ledger).await;
        if let Err(message) = applied {
            eprintln!("error: {message}");
            eprintln!(
                "import stopped during {}; categories already completed are recorded, so \
                 re-running is safe",
                category.name()
            );
            return Ok(EXIT_RUNTIME);
        }
    }
    println!(
        "import complete; ledger: {}",
        dest.join(ledger::LEDGER_FILE).display()
    );
    Ok(0)
}

async fn apply_group(group: Prepared, dest: &Path, ledger: &mut Ledger) -> Result<(), String> {
    let records = match group {
        Prepared::Credentials(plan) => {
            let Plan {
                records, writes, ..
            } = plan;
            credentials::apply(&writes, &dest.join(source::AUTH_FILE)).await?;
            records
        }
        Prepared::Settings(plan) => {
            let Plan {
                records, writes, ..
            } = plan;
            settings::apply(&writes, &dest.join(settings::DEST_FILE))?;
            records
        }
        Prepared::Multiprovider(plan) => {
            let Plan {
                records, writes, ..
            } = plan;
            multiprovider::apply(writes.as_deref(), &dest.join(multiprovider::DEST_FILE))?;
            records
        }
        Prepared::Sessions(plan) => {
            let Plan {
                records, writes, ..
            } = plan;
            sessions::apply(&writes)?;
            records
        }
    };
    for record in records {
        ledger.record(record);
    }
    ledger.save()
}

fn has_oauth_write(group: &Prepared) -> bool {
    match group {
        Prepared::Credentials(plan) => plan.writes.iter().any(|write| write.oauth),
        _ => false,
    }
}

/// Ask for confirmation. Only an explicit `y` or `yes` proceeds.
fn confirm(input: &mut dyn BufRead, out: &mut dyn Write) -> bool {
    let _ = write!(out, "Apply these changes? [y/N] ");
    let _ = out.flush();
    let mut line = String::new();
    if input.read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn same_location(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_requires_an_explicit_yes() {
        for (answer, expected) in [
            ("y\n", true),
            ("YES\n", true),
            ("\n", false),
            ("n\n", false),
            ("", false),
        ] {
            let mut input = answer.as_bytes();
            let mut out = Vec::new();
            assert_eq!(confirm(&mut input, &mut out), expected, "answer {answer:?}");
        }
    }
}
