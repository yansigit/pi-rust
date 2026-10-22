//! Source selection and the read allowlist.
//!
//! The import reads exactly four things from a Pi agent directory: `auth.json`,
//! `settings.json`, `multiprovider-auth.json` and the `*.jsonl` files under
//! `sessions/` (one directory level). Nothing else in the source tree is opened,
//! listed or copied. Symbolic links inside the allowlist are refused rather than
//! followed, and the source is only ever read: no lock is taken and no file is
//! created, renamed or removed there.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub(crate) const AUTH_FILE: &str = "auth.json";
pub(crate) const SETTINGS_FILE: &str = "settings.json";
pub(crate) const MULTIPROVIDER_FILE: &str = "multiprovider-auth.json";
pub(crate) const SESSIONS_DIR: &str = "sessions";

/// Environment variable that names Pi's agent directory.
pub(crate) const PI_DIR_ENV: &str = "PI_CODING_AGENT_DIR";

/// Resolve the Pi agent directory: `--from`, then `PI_CODING_AGENT_DIR`, then
/// `~/.pi/agent`. Relative paths are made absolute against `cwd`.
pub(crate) fn resolve(
    from: Option<&Path>,
    env_value: Option<OsString>,
    home: Option<PathBuf>,
    cwd: &Path,
) -> Result<PathBuf, String> {
    let path = if let Some(from) = from {
        from.to_path_buf()
    } else if let Some(value) = env_value.filter(|value| !value.is_empty()) {
        PathBuf::from(value)
    } else {
        home.ok_or_else(|| {
            format!("could not resolve a home directory; pass --from or set {PI_DIR_ENV}")
        })?
        .join(".pi")
        .join("agent")
    };
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

/// Read one allowlisted file directly under `root`. A missing file is `Ok(None)`.
pub(crate) fn read_file(root: &Path, name: &str) -> Result<Option<Vec<u8>>, String> {
    let path = root.join(name);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not inspect {name}: {error}")),
    };
    if metadata.file_type().is_symlink() {
        return Err(format!("{name} is a symbolic link; refusing to follow it"));
    }
    if !metadata.is_file() {
        return Err(format!("{name} is not a regular file"));
    }
    std::fs::read(&path)
        .map(Some)
        .map_err(|error| format!("could not read {name}: {error}"))
}

/// A `.jsonl` file found under `sessions/`, or a reason it was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionEntry {
    File {
        /// Path relative to `sessions/`, for example `--home-u-proj--/a.jsonl`.
        relative: String,
        path: PathBuf,
    },
    Skipped {
        relative: String,
        reason: String,
    },
}

/// List `sessions/` under `root` (direct `.jsonl` files and one subdirectory
/// level), sorted by relative path. Missing `sessions/` lists nothing.
pub(crate) fn list_sessions(root: &Path) -> Result<Vec<SessionEntry>, String> {
    let dir = root.join(SESSIONS_DIR);
    let metadata = match std::fs::symlink_metadata(&dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("could not inspect {SESSIONS_DIR}/: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("{SESSIONS_DIR}/ is not a plain directory"));
    }
    let mut out = Vec::new();
    for entry in read_sorted(&dir)? {
        let name = entry.file_name;
        let path = entry.path;
        match entry.kind {
            EntryKind::Symlink => out.push(SessionEntry::Skipped {
                relative: name,
                reason: "symbolic link; not followed".into(),
            }),
            EntryKind::Dir => {
                for child in read_sorted(&path)? {
                    let relative = format!("{name}/{}", child.file_name);
                    match child.kind {
                        EntryKind::File if child.file_name.ends_with(".jsonl") => {
                            out.push(SessionEntry::File {
                                relative,
                                path: child.path,
                            })
                        }
                        EntryKind::Symlink if child.file_name.ends_with(".jsonl") => {
                            out.push(SessionEntry::Skipped {
                                relative,
                                reason: "symbolic link; not followed".into(),
                            })
                        }
                        _ => {}
                    }
                }
            }
            EntryKind::File if name.ends_with(".jsonl") => out.push(SessionEntry::File {
                relative: name,
                path,
            }),
            EntryKind::File | EntryKind::Other => {}
        }
    }
    out.sort_by(|a, b| relative_of(a).cmp(relative_of(b)));
    Ok(out)
}

fn relative_of(entry: &SessionEntry) -> &str {
    match entry {
        SessionEntry::File { relative, .. } | SessionEntry::Skipped { relative, .. } => relative,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

struct Child {
    file_name: String,
    path: PathBuf,
    kind: EntryKind,
}

fn read_sorted(dir: &Path) -> Result<Vec<Child>, String> {
    let read = std::fs::read_dir(dir)
        .map_err(|error| format!("could not list {}: {error}", dir.display()))?;
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|error| format!("could not list {}: {error}", dir.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("could not inspect {}: {error}", entry.path().display()))?;
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        out.push(Child {
            file_name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path(),
            kind,
        });
    }
    out.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_prefers_flag_then_env_then_home() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve(
                Some(Path::new("/flag")),
                Some("/env".into()),
                Some("/home/u".into()),
                cwd
            )
            .unwrap(),
            PathBuf::from("/flag")
        );
        assert_eq!(
            resolve(None, Some("/env".into()), Some("/home/u".into()), cwd).unwrap(),
            PathBuf::from("/env")
        );
        assert_eq!(
            resolve(None, Some("".into()), Some("/home/u".into()), cwd).unwrap(),
            PathBuf::from("/home/u/.pi/agent")
        );
        assert_eq!(
            resolve(Some(Path::new("rel")), None, None, cwd).unwrap(),
            PathBuf::from("/work/rel")
        );
        assert!(resolve(None, None, None, cwd).is_err());
    }
}
