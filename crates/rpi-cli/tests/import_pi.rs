//! End-to-end tests for `rpi import pi`. Every test builds a Pi agent directory
//! and an rpi agent directory under a temp dir, runs the real binary with
//! `HOME` and `RPI_CODING_AGENT_DIR` pointed inside that temp dir, and never
//! reads or writes the real `~/.pi` or `~/.rpi`.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const HEADER: &str = r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2026-10-01T00:00:00.000Z","cwd":"/home/u/proj"}"#;
const MESSAGE: &str = r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-10-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#;
const CUSTOM: &str = r#"{"type":"custom","id":"c1","parentId":"m1","timestamp":"2026-10-01T00:00:02.000Z","customType":"cursor-resume","data":{"cursorAgentId":"agent-1"}}"#;
const MULTIPROVIDER: &[u8] = br#"{"pool":[{"provider":"anthropic","key":"pool-1"}]}"#;

struct Fixture {
    _temp: TempDir,
    src: PathBuf,
    dest: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("pi-agent");
        let dest = temp.path().join("rpi-agent");
        let home = temp.path().join("home");
        fs::create_dir_all(home.join("project")).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        write_source(&src);
        Fixture {
            _temp: temp,
            src,
            dest,
            home,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_from(&self.src, args)
    }

    fn run_from(&self, from: &Path, args: &[&str]) -> Output {
        self.command(from, args).output().unwrap()
    }

    fn command(&self, from: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rpi"));
        command
            .current_dir(self.home.join("project"))
            .args(["import", "pi", "--from"])
            .arg(from)
            .args(args)
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("RPI_CODING_AGENT_SESSION_DIR")
            .env("RPI_CODING_AGENT_DIR", &self.dest)
            .env("HOME", &self.home)
            .stdin(Stdio::null());
        command
    }

    fn session_snapshot(&self, sha12_of: &[u8]) -> PathBuf {
        self.dest.join(format!(
            "sessions/pi-import/--home-u-proj--/s1.{}.jsonl",
            &sha256_hex(sha12_of)[..12]
        ))
    }
}

fn write_source(src: &Path) {
    fs::create_dir_all(src.join("sessions/--home-u-proj--")).unwrap();
    fs::create_dir_all(src.join("vault")).unwrap();
    fs::create_dir_all(src.join("credential-backups")).unwrap();
    let auth = json!({
        "anthropic": {
            "type": "oauth",
            "access": "acc-1",
            "refresh": "ref-1",
            "expires": 1800000000999_i64,
            "projectId": "proj-1"
        },
        "openai": {"type": "api_key", "key": "sk-test-openai"},
        "half": {"type": "oauth", "access": "only-access"},
        "mystery": {"type": "saml", "assertion": "x"}
    });
    fs::write(
        src.join("auth.json"),
        serde_json::to_vec_pretty(&auth).unwrap(),
    )
    .unwrap();
    fs::write(src.join("auth.json.lock"), b"held by pi").unwrap();
    fs::write(src.join("credential-backups/auth.json.bak"), b"stash").unwrap();
    fs::write(src.join("vault/secret.txt"), b"vault secret").unwrap();
    fs::write(
        src.join("settings.json"),
        r#"{"defaultProvider":"anthropic","theme":"dark","retry":"oops","sessionDir":"/elsewhere","extensions":["/x"],"lastRpiChangelogVersion":"9.9.9","unknownKey":true}"#,
    )
    .unwrap();
    fs::write(src.join("multiprovider-auth.json"), MULTIPROVIDER).unwrap();
    fs::write(
        src.join("sessions/--home-u-proj--/s1.jsonl"),
        format!("{HEADER}\n{MESSAGE}\n{CUSTOM}\n"),
    )
    .unwrap();
    fs::write(
        src.join("sessions/--home-u-proj--/broken.jsonl"),
        format!("{HEADER}\n{{\"type\":\"message\"\n"),
    )
    .unwrap();
    fs::write(
        src.join("sessions/--home-u-proj--/notes.txt"),
        b"not a session",
    )
    .unwrap();
    symlink(
        "../../vault/secret.txt",
        src.join("sessions/--home-u-proj--/link.jsonl"),
    )
    .unwrap();
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Every path under `root` with its kind, mode, mtime and content digest. An
/// absent root is an empty tree, so "nothing created" is also checked.
fn tree(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if root.exists() {
        walk(root, root, &mut out);
    }
    out
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let meta = fs::symlink_metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path).unwrap();
            out.insert(relative, format!("symlink {}", target.display()));
        } else if meta.is_dir() {
            out.insert(relative.clone(), format!("dir {mode:o}"));
            walk(root, &path, out);
        } else {
            let mtime = meta
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let bytes = fs::read(&path).unwrap();
            out.insert(
                relative,
                format!("file {mode:o} mtime {mtime} sha {}", sha256_hex(&bytes)),
            );
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn dry_run_prints_the_plan_and_writes_nothing() {
    let fixture = Fixture::new();
    let before = tree(fixture._temp.path());

    let output = fixture.run(&["--dry-run"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("dry run: nothing was written"), "{text}");
    assert!(text.contains("summary:"), "{text}");
    assert!(
        text.contains("copied OAuth refresh credentials may rotate"),
        "{text}"
    );
    assert_eq!(tree(fixture._temp.path()), before);
}

#[test]
fn full_import_copies_each_category_and_reports_every_skip_reason() {
    let fixture = Fixture::new();
    let src_before = tree(&fixture.src);

    let output = fixture.run(&["--yes"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    for expected in [
        "incomplete oauth entry: refresh is missing",
        "unsupported credential type `saml`",
        "expected an object",
        "would point rpi's session storage at Pi's directory",
        "Pi extension paths are not rpi cdylib directories",
        "rpi's own release-note baseline is never taken from Pi",
        "not a setting rpi understands",
        "is not a JSON object",
        "symbolic link; not followed",
        "import complete",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
    assert!(
        !text.contains("notes.txt"),
        "non-session files are not listed:\n{text}"
    );

    let auth = read_json(&fixture.dest.join("auth.json"));
    assert_eq!(
        auth["anthropic"],
        json!({
            "type": "oauth",
            "access": "acc-1",
            "refresh": "ref-1",
            "expires": 1800000000,
            "projectId": "proj-1"
        })
    );
    assert_eq!(
        auth["openai"],
        json!({"type": "api_key", "key": "sk-test-openai"})
    );
    assert!(auth.get("half").is_none());
    assert!(auth.get("mystery").is_none());
    assert_eq!(mode_of(&fixture.dest.join("auth.json")), 0o600);

    let settings = read_json(&fixture.dest.join("settings.json"));
    assert_eq!(
        settings,
        json!({"defaultProvider": "anthropic", "theme": "dark"})
    );

    assert_eq!(
        fs::read(fixture.dest.join("multiprovider-auth.json")).unwrap(),
        MULTIPROVIDER
    );
    assert_eq!(
        mode_of(&fixture.dest.join("multiprovider-auth.json")),
        0o600
    );

    let ledger = read_json(&fixture.dest.join("import-ledger.json"));
    assert_eq!(ledger["version"], 1);
    assert_eq!(mode_of(&fixture.dest.join("import-ledger.json")), 0o600);

    let session_bytes = fs::read(fixture.src.join("sessions/--home-u-proj--/s1.jsonl")).unwrap();
    assert_eq!(mode_of(&fixture.home), 0o700);
    assert_eq!(mode_of(&fixture.session_snapshot(&session_bytes)), 0o600);
    let snapshot =
        String::from_utf8(fs::read(fixture.session_snapshot(&session_bytes)).unwrap()).unwrap();
    let (first, rest) = snapshot.split_once('\n').unwrap();
    let header: Value = serde_json::from_str(first).unwrap();
    assert_eq!(header["id"], "sess-1");
    assert_eq!(header["imported"]["source"], "pi");
    assert_eq!(header["imported"]["sessionVersion"], 3);
    assert_eq!(header["imported"]["sha256"], sha256_hex(&session_bytes));
    assert_eq!(rest.as_bytes(), &session_bytes[HEADER.len() + 1..]);
    assert!(!fixture
        .dest
        .join("sessions/pi-import/--home-u-proj--/broken.jsonl")
        .exists());
    assert!(!fixture.home.join("project/.rpi").exists());

    assert_eq!(
        tree(&fixture.src),
        src_before,
        "source tree must be untouched"
    );
}

#[test]
fn only_limits_the_categories_that_are_imported() {
    let fixture = Fixture::new();

    let output = fixture.run(&["--yes", "--only", "credentials"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(fixture.dest.join("auth.json").is_file());
    assert!(!fixture.dest.join("settings.json").exists());
    assert!(!fixture.dest.join("multiprovider-auth.json").exists());
    assert!(!fixture.dest.join("sessions").exists());
    assert!(!fixture.home.join("project/.rpi").exists());
}

#[test]
fn rerunning_a_completed_import_changes_nothing() {
    let fixture = Fixture::new();
    assert_eq!(fixture.run(&["--yes"]).status.code(), Some(0));
    let before = tree(fixture._temp.path());

    let output = fixture.run(&["--yes"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("already imported"), "{text}");
    assert!(text.contains("nothing to import"), "{text}");
    assert_eq!(tree(fixture._temp.path()), before);
}

#[test]
fn custom_session_env_root_is_used_for_dry_run_import_and_rerun() {
    for relative in [false, true] {
        let fixture = Fixture::new();
        let root = fixture.home.join("project/custom-sessions");
        let configured = if relative {
            Path::new("custom-sessions")
        } else {
            &root
        };
        let run = |args: &[&str]| {
            fixture
                .command(&fixture.src, args)
                .env("RPI_CODING_AGENT_SESSION_DIR", configured)
                .output()
                .unwrap()
        };
        let before = tree(fixture._temp.path());
        let dry = run(&["--dry-run"]);
        assert_eq!(dry.status.code(), Some(0), "{}", stderr(&dry));
        assert!(stdout(&dry).contains(&root.display().to_string()));
        assert_eq!(tree(fixture._temp.path()), before);

        let output = run(&["--yes"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let source = fs::read(fixture.src.join("sessions/--home-u-proj--/s1.jsonl")).unwrap();
        let snapshot = root.join(format!(
            "pi-import/--home-u-proj--/s1.{}.jsonl",
            &sha256_hex(&source)[..12]
        ));
        assert_eq!(mode_of(&snapshot), 0o600);
        let bytes = fs::read(&snapshot).unwrap();
        let newline = bytes.iter().position(|byte| *byte == b'\n').unwrap();
        let header: Value = serde_json::from_slice(&bytes[..newline]).unwrap();
        assert_eq!(header["imported"]["source"], "pi");
        assert_eq!(&bytes[newline + 1..], &source[HEADER.len() + 1..]);
        let ledger = read_json(&fixture.dest.join("import-ledger.json"));
        assert!(ledger.to_string().contains(&snapshot.display().to_string()));
        assert!(!fixture.dest.join("sessions").exists());
        assert!(!fixture.home.join("project/.rpi").exists());

        let before = tree(fixture._temp.path());
        let again = run(&["--yes"]);
        assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
        assert!(stdout(&again).contains("already imported"));
        assert_eq!(tree(fixture._temp.path()), before);
    }
}

#[test]
fn saved_session_dir_wins_over_environment_and_is_not_imported_from_pi() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.dest).unwrap();
    fs::write(
        fixture.dest.join("settings.json"),
        br#"{"sessionDir":"saved-sessions"}"#,
    )
    .unwrap();
    let output = fixture
        .command(&fixture.src, &["--yes"])
        .env(
            "RPI_CODING_AGENT_SESSION_DIR",
            fixture.home.join("unused-sessions"),
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let root = fixture
        .home
        .join("project/saved-sessions/pi-import/--home-u-proj--");
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
    assert!(!fixture.home.join("unused-sessions").exists());
    assert!(!fixture.dest.join("sessions").exists());
    assert!(!fixture.home.join("project/.rpi").exists());
    assert_eq!(
        read_json(&fixture.dest.join("settings.json"))["sessionDir"],
        "saved-sessions"
    );
}

#[test]
fn a_changed_session_is_detected_and_stored_beside_the_old_copy() {
    let fixture = Fixture::new();
    assert_eq!(fixture.run(&["--yes"]).status.code(), Some(0));
    let old_bytes = fs::read(fixture.src.join("sessions/--home-u-proj--/s1.jsonl")).unwrap();
    let old_snapshot = fixture.session_snapshot(&old_bytes);
    let old_snapshot_bytes = fs::read(&old_snapshot).unwrap();

    let grown = format!(
        "{HEADER}\n{MESSAGE}\n{CUSTOM}\n{}\n",
        r#"{"type":"message","id":"m2","parentId":"c1","timestamp":"2026-10-01T00:00:03.000Z","message":{"role":"assistant","content":"ok"}}"#
    );
    fs::write(
        fixture.src.join("sessions/--home-u-proj--/s1.jsonl"),
        &grown,
    )
    .unwrap();

    let dry = fixture.run(&["--dry-run"]);
    assert_eq!(dry.status.code(), Some(0), "{}", stderr(&dry));
    assert!(
        stdout(&dry).contains("source changed since the last import"),
        "{}",
        stdout(&dry)
    );

    let output = fixture.run(&["--yes"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::read(&old_snapshot).unwrap(), old_snapshot_bytes);
    assert!(fixture.session_snapshot(grown.as_bytes()).is_file());
}

#[test]
fn credential_conflict_refuses_the_whole_run_and_writes_nothing() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.dest).unwrap();
    fs::write(
        fixture.dest.join("auth.json"),
        r#"{"anthropic":{"type":"api_key","key":"already-here"}}"#,
    )
    .unwrap();
    let dest_before = tree(&fixture.dest);
    let src_before = tree(&fixture.src);

    let output = fixture.run(&["--yes"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("destination holds a different credential"));
    assert!(
        stderr(&output).contains("nothing was written"),
        "{}",
        stderr(&output)
    );
    assert_eq!(tree(&fixture.dest), dest_before);
    assert_eq!(tree(&fixture.src), src_before);
}

#[test]
fn settings_conflict_refuses_and_leaves_the_destination_alone() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.dest).unwrap();
    fs::write(fixture.dest.join("settings.json"), r#"{"theme":"light"}"#).unwrap();
    let dest_before = tree(&fixture.dest);

    let output = fixture.run(&["--yes", "--only", "settings"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("destination sets a different value; not overwritten"));
    assert_eq!(tree(&fixture.dest), dest_before);
}

#[test]
fn multiprovider_conflict_refuses_and_leaves_the_destination_alone() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.dest).unwrap();
    fs::write(
        fixture.dest.join("multiprovider-auth.json"),
        b"{\"pool\":[]}",
    )
    .unwrap();
    let dest_before = tree(&fixture.dest);

    let output = fixture.run(&["--yes", "--only", "multiprovider"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("exists with different contents; not overwritten"));
    assert_eq!(tree(&fixture.dest), dest_before);
}

#[test]
fn session_conflict_refuses_and_leaves_the_destination_alone() {
    let fixture = Fixture::new();
    let session_bytes = fs::read(fixture.src.join("sessions/--home-u-proj--/s1.jsonl")).unwrap();
    let snapshot = fixture.session_snapshot(&session_bytes);
    fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
    fs::write(&snapshot, b"someone else's file").unwrap();
    let before = tree(fixture._temp.path());

    let output = fixture.run(&["--yes", "--only", "sessions"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("exists with different contents; not overwritten"));
    assert_eq!(tree(fixture._temp.path()), before);
}

#[test]
fn non_terminal_without_yes_refuses_before_writing() {
    let fixture = Fixture::new();
    let src_before = tree(&fixture.src);

    let output = fixture.run(&[]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("non-interactive"),
        "{}",
        stderr(&output)
    );
    assert!(!fixture.dest.exists());
    assert_eq!(tree(&fixture.src), src_before);
}

#[test]
fn usage_errors_exit_with_usage_status() {
    let fixture = Fixture::new();
    let output = fixture.run(&["--only", "vault"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("unknown --only category"));
    assert!(!fixture.dest.exists());
}

#[test]
fn the_source_and_the_rpi_agent_directory_must_differ() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.dest).unwrap();

    let output = fixture.run_from(&fixture.dest, &["--yes"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("nothing to import"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn no_configured_root_archives_under_agent_dir_and_creates_nothing_in_cwd() {
    let fixture = Fixture::new();
    let before = tree(fixture._temp.path());
    let dry = fixture.run(&["--dry-run", "--only", "sessions"]);
    assert_eq!(dry.status.code(), Some(0), "{}", stderr(&dry));
    let dry_text = stdout(&dry);
    let expected_dest = fixture.dest.join("sessions/pi-import");
    assert!(
        dry_text.contains(&expected_dest.display().to_string()),
        "{dry_text}"
    );
    assert_eq!(tree(fixture._temp.path()), before);

    let output = fixture.run(&["--yes", "--only", "sessions"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let source = fs::read(fixture.src.join("sessions/--home-u-proj--/s1.jsonl")).unwrap();
    let snapshot = fixture.session_snapshot(&source);
    assert!(snapshot.is_file());
    assert!(snapshot.starts_with(&expected_dest));
    assert_eq!(mode_of(&snapshot), 0o600);
    assert!(!fixture.home.join("project/.rpi").exists());
}
