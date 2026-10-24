use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};
pub(crate) fn library() -> &'static Path {
    static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    fixture(&FIXTURE, "delegation_v2")
}
// Also compiled by runtime unit tests, which only need the handwritten fixture.
#[allow(dead_code)]
pub(crate) fn sdk_library() -> &'static Path {
    static FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    fixture(&FIXTURE, "delegation_sdk")
}
fn fixture(lock: &'static OnceLock<(tempfile::TempDir, PathBuf)>, name: &str) -> &'static Path {
    &lock
        .get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let library = directory.path().join(format!(
                "{}{name}{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            ));
            let deps = std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf();
            let mut compiler = std::process::Command::new(
                std::env::var_os("RPI_TEST_RUSTC").unwrap_or_else(|| "rustc".into()),
            );
            compiler
                .args(["--edition=2021", "--crate-type=cdylib", "-C", "debuginfo=0"])
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}.rs")),
                )
                .arg("-L")
                .arg(format!("dependency={}", deps.display()))
                .arg("-o")
                .arg(&library);
            for name in [
                "rpi_plugin_sdk",
                "rpi_ai",
                "serde_json",
                "async_trait",
                "tokio",
            ] {
                let mut candidates: Vec<_> = std::fs::read_dir(&deps)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .filter(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(&format!("lib{name}-"))
                            && path.extension().is_some_and(|extension| {
                                extension == "rlib" || (name == "async_trait" && extension == "so")
                            })
                    })
                    .collect();
                candidates.sort_by_key(|path| {
                    std::cmp::Reverse(std::fs::metadata(path).unwrap().modified().unwrap())
                });
                compiler.arg("--extern").arg(format!(
                    "{name}={}",
                    candidates
                        .first()
                        .expect("build host dependencies first")
                        .display()
                ));
            }
            let output = compiler.output().unwrap();
            assert!(
                output.status.success(),
                "independent delegation DLL must build: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            (directory, library)
        })
        .1
}
