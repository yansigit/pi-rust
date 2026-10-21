//! Pi 1.3 values for the opt-in native auth path; legacy fallback behavior is unchanged.
use super::{parse_template, TemplatePart};
use std::{collections::BTreeMap, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) fn overlay(
    base: &BTreeMap<String, String>,
    explicit: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = base.clone();
    env.extend(
        explicit
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    env
}
pub(crate) fn configured(raw: &str, env: &BTreeMap<String, String>) -> bool {
    raw.starts_with('!')
        || parse_template(raw).iter().all(|part| match part {
            TemplatePart::Env(name) => env.contains_key(name),
            TemplatePart::Literal(_) => true,
        })
}

pub(crate) async fn resolve(
    raw: &str,
    description: &str,
    env: &BTreeMap<String, String>,
) -> Result<String, String> {
    if let Some(command) = raw.strip_prefix('!') {
        return shell(command).await.ok_or_else(|| {
            format!("Failed to resolve {description} from shell command: {command}")
        });
    }
    let mut value = String::new();
    let mut missing = Vec::new();
    for part in parse_template(raw) {
        match part {
            TemplatePart::Literal(text) => value.push_str(&text),
            TemplatePart::Env(name) => match env.get(&name).filter(|v| !v.is_empty()) {
                Some(text) => value.push_str(text),
                None if !missing.contains(&name) => missing.push(name),
                _ => {}
            },
        }
    }
    match missing.len() {
        0 => Ok(value),
        1 => Err(format!(
            "Failed to resolve {description} from environment variable: {}",
            missing[0]
        )),
        _ => Err(format!(
            "Failed to resolve {description} from environment variables: {}",
            missing.join(", ")
        )),
    }
}

async fn run(
    program: &std::ffi::OsStr,
    args: &[&str],
    command: &str,
    stdin: bool,
) -> Result<Option<String>, std::io::Error> {
    let mut process = tokio::process::Command::new(program);
    process
        .args(args)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if !stdin {
        process.arg(command);
    }
    #[cfg(windows)]
    process.creation_flags(0x0800_0000);
    let mut child = process.spawn()?;
    let mut output = Vec::new();
    let mut stream = child.stdout.take().unwrap();
    let work = async {
        if let Some(mut input) = child.stdin.take() {
            input.write_all(command.as_bytes()).await?;
        }
        // Node's synchronous command reader has a 1 MiB stdout buffer.
        (&mut stream)
            .take(1_048_577)
            .read_to_end(&mut output)
            .await?;
        if output.len() > 1_048_576 {
            return Ok::<_, std::io::Error>(false);
        }
        Ok(child.wait().await?.success())
    };
    let successful = matches!(
        tokio::time::timeout(Duration::from_secs(10), work).await,
        Ok(Ok(true))
    );
    if !successful {
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        #[cfg(not(unix))]
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Ok(None);
    }
    let output = String::from_utf8_lossy(&output);
    let value = output.trim_matches(rpi_plugin_sdk::dialogs::native::is_javascript_whitespace);
    Ok((!value.is_empty()).then(|| value.into()))
}
#[cfg(not(windows))]
async fn shell(command: &str) -> Option<String> {
    run(std::ffi::OsStr::new("/bin/sh"), &["-c"], command, false)
        .await
        .ok()
        .flatten()
}
#[cfg(windows)]
async fn shell(command: &str) -> Option<String> {
    use std::{ffi::OsString, path::PathBuf};
    let mut candidates: Vec<OsString> = ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|p| PathBuf::from(p).join("Git/bin/bash.exe").into_os_string())
        .collect();
    candidates.retain(|p| PathBuf::from(p).exists());
    if candidates.is_empty() {
        let mut locate = tokio::process::Command::new("where");
        locate
            .arg("bash.exe")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .creation_flags(0x0800_0000);
        if let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(5), locate.output()).await
        {
            if output.status.success() {
                if let Some(first) = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .lines()
                    .next()
                {
                    let path = PathBuf::from(first);
                    if path.exists() {
                        candidates.push(path.into_os_string());
                    }
                }
            }
        }
    }
    if let Some(program) = candidates.first() {
        let name = program
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        let legacy = name.as_bytes().get(1) == Some(&b':')
            && matches!(
                name.get(2..),
                Some("\\windows\\system32\\bash.exe" | "\\windows\\sysnative\\bash.exe")
            );
        match run(
            program,
            if legacy { &["-s"] } else { &["-c"] },
            command,
            legacy,
        )
        .await
        {
            Ok(result) => return result,
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return None,
            _ => {}
        }
    }
    let program = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    run(&program, &["/d", "/s", "/c"], command, false)
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_templates_preserve_empty_overlay_fallback_escapes_and_missing_order() {
        let base = [("VALUE".into(), "ambient".into())].into();
        let env = overlay(&base, &[("VALUE".into(), String::new())].into());
        assert_eq!(
            resolve("$VALUE/$!literal/$$VALUE", "key", &env)
                .await
                .unwrap(),
            "ambient/!literal/$VALUE"
        );
        assert_eq!(
            resolve("$TWO-$ONE-$TWO", "key", &env).await.unwrap_err(),
            "Failed to resolve key from environment variables: TWO, ONE"
        );
        assert_eq!(resolve("", "key", &env).await.unwrap(), "");
        assert!(configured("!exit 1", &env));
        assert!(configured("", &env));
        assert!(configured(
            "$VALUE",
            &[("VALUE".into(), String::new())].into()
        ));
        assert!(!configured("$MISSING", &env));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn native_commands_are_uncached_bounded_and_use_javascript_trim() {
        let env = BTreeMap::new();
        assert_eq!(
            resolve("!printf '\\357\\273\\277value\\342\\200\\250'", "key", &env)
                .await
                .unwrap(),
            "value"
        );
        assert!(resolve("!head -c 1048577 /dev/zero", "key", &env)
            .await
            .is_err());
        assert!(resolve("!exit 1", "key", &env).await.is_err());
        assert!(resolve("!printf ' '", "key", &env).await.is_err());
    }
}
