//! Command-line parsing for `rpi import pi`.

use std::path::PathBuf;

/// The import categories, in the order they are planned and applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Category {
    Credentials,
    Settings,
    Multiprovider,
    Sessions,
}

impl Category {
    pub(crate) const ALL: [Category; 4] = [
        Category::Credentials,
        Category::Settings,
        Category::Multiprovider,
        Category::Sessions,
    ];

    /// The name used on the command line and in the ledger.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Category::Credentials => "credentials",
            Category::Settings => "settings",
            Category::Multiprovider => "multiprovider",
            Category::Sessions => "sessions",
        }
    }

    fn parse(text: &str) -> Option<Category> {
        Category::ALL
            .into_iter()
            .find(|category| category.name() == text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Options {
    /// `--from DIR`: the Pi agent directory (the one holding `auth.json`).
    pub(crate) from: Option<PathBuf>,
    pub(crate) dry_run: bool,
    pub(crate) yes: bool,
    /// Selected categories, in canonical order.
    pub(crate) categories: Vec<Category>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Parsed {
    Run(Options),
    Help,
    Usage(String),
}

/// Parse the arguments that follow `rpi import`.
pub(crate) fn parse(args: &[String]) -> Parsed {
    let mut rest = args.iter();
    match rest.next().map(String::as_str) {
        None => return Parsed::Usage("expected `pi` after `import`".into()),
        Some("-h" | "--help") => return Parsed::Help,
        Some("pi") => {}
        Some(other) => {
            return Parsed::Usage(format!("unknown import source `{other}`; expected `pi`"))
        }
    }

    let mut from: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut yes = false;
    let mut only: Option<Vec<Category>> = None;

    while let Some(arg) = rest.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        match flag {
            "-h" | "--help" => return Parsed::Help,
            "--dry-run" | "--yes" if inline.is_some() => {
                return Parsed::Usage(format!("{flag} does not take a value"));
            }
            "--dry-run" => dry_run = true,
            "--yes" => yes = true,
            "--from" => {
                let value = match value_for(flag, inline, &mut rest) {
                    Ok(value) => value,
                    Err(message) => return Parsed::Usage(message),
                };
                if from.is_some() {
                    return Parsed::Usage("--from may be given only once".into());
                }
                if value.is_empty() {
                    return Parsed::Usage("--from needs a directory".into());
                }
                from = Some(PathBuf::from(value));
            }
            "--only" => {
                let value = match value_for(flag, inline, &mut rest) {
                    Ok(value) => value,
                    Err(message) => return Parsed::Usage(message),
                };
                let selected = only.get_or_insert_with(Vec::new);
                for name in value.split(',') {
                    match Category::parse(name.trim()) {
                        Some(category) => {
                            if !selected.contains(&category) {
                                selected.push(category);
                            }
                        }
                        None => {
                            return Parsed::Usage(format!(
                                "unknown --only category `{name}`; expected a comma list of {}",
                                Category::ALL
                                    .iter()
                                    .map(|c| c.name())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ));
                        }
                    }
                }
            }
            other => return Parsed::Usage(format!("unknown option `{other}`")),
        }
    }

    let categories = match only {
        Some(mut selected) => {
            selected.sort();
            selected
        }
        None => Category::ALL.to_vec(),
    };
    Parsed::Run(Options {
        from,
        dry_run,
        yes,
        categories,
    })
}

fn value_for(
    flag: &str,
    inline: Option<String>,
    rest: &mut std::slice::Iter<'_, String>,
) -> Result<String, String> {
    match inline {
        Some(value) => Ok(value),
        None => rest
            .next()
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_select_every_category() {
        assert_eq!(
            parse(&args(&["pi"])),
            Parsed::Run(Options {
                from: None,
                dry_run: false,
                yes: false,
                categories: Category::ALL.to_vec(),
            })
        );
    }

    #[test]
    fn parses_flags_and_only_list() {
        let parsed = parse(&args(&[
            "pi",
            "--from",
            "/x",
            "--dry-run",
            "--only=sessions,credentials,sessions",
        ]));
        assert_eq!(
            parsed,
            Parsed::Run(Options {
                from: Some(PathBuf::from("/x")),
                dry_run: true,
                yes: false,
                categories: vec![Category::Credentials, Category::Sessions],
            })
        );
    }

    #[test]
    fn rejects_bad_usage() {
        assert!(matches!(parse(&args(&[])), Parsed::Usage(_)));
        assert!(matches!(parse(&args(&["claude"])), Parsed::Usage(_)));
        assert!(matches!(parse(&args(&["pi", "--bogus"])), Parsed::Usage(_)));
        assert!(matches!(parse(&args(&["pi", "--from"])), Parsed::Usage(_)));
        assert!(matches!(
            parse(&args(&["pi", "--only", "vault"])),
            Parsed::Usage(_)
        ));
        assert!(matches!(parse(&args(&["pi", "--yes=1"])), Parsed::Usage(_)));
        assert_eq!(parse(&args(&["pi", "--help"])), Parsed::Help);
    }
}
