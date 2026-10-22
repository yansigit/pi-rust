//! Versioned prebuilt extension distribution (rpi-extension.json v1).
//! Repository discovery uses crates.io metadata. Release tags and library
//! paths are protocol fields, never guessed from asset names or repository latest.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CRATES_API: &str = "https://crates.io/api/v1/crates";
const MAX_METADATA: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_MANIFEST: u64 = 256 * 1024;
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_LIBRARY: u64 = 128 * 1024 * 1024;
const MAX_ENTRIES: usize = 128;
/// Longest accepted sidecar asset path, in bytes.
const MAX_ASSET_PATH: usize = 512;
/// The release manifest is published beside the archive and is never an asset.
const MANIFEST_FILE: &str = "rpi-extension.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrebuiltRecord {
    pub repository: String,
    pub target: String,
    pub manifest_sha256: String,
    pub source_commit: String,
    pub sdk_version: String,
    pub plugin_abi: u32,
    pub archive_sha256: String,
    pub library_sha256: String,
    /// Explicit repository installs discover versions from package-specific tags.
    #[serde(default)]
    pub custom_repository: bool,
    /// Sidecar files installed beside the library, as declared and verified.
    /// Empty for legacy single-library manifests, which keeps their records unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<PrebuiltAsset>,
}

/// One verified sidecar file, relative to the extension store directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrebuiltAsset {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
struct RegistryPackage {
    #[serde(rename = "crate")]
    metadata: RegistryMetadata,
    versions: Vec<RegistryVersion>,
}

#[derive(Debug, Deserialize)]
struct RegistryMetadata {
    id: String,
    repository: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistryVersion {
    num: String,
    yanked: bool,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    name: String,
    version: String,
    repository: String,
    tag: String,
    source_commit: String,
    sdk: Sdk,
    targets: BTreeMap<String, Distribution>,
}

#[derive(Debug, Deserialize)]
struct Sdk {
    version: String,
    plugin_abi: u32,
}

#[derive(Debug, Deserialize)]
struct Distribution {
    url: String,
    format: String,
    size: u64,
    sha256: String,
    library: Library,
    /// Absent is the legacy single-library archive. Present, even empty, means the
    /// archive must contain exactly the library plus these declared assets.
    #[serde(default)]
    assets: Option<Vec<AssetDeclaration>>,
}

#[derive(Debug, Deserialize)]
struct Library {
    path: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct AssetDeclaration {
    path: String,
    sha256: String,
}

struct ResolvedPackage {
    name: String,
    version: String,
    repository: String,
    custom_repository: bool,
}

#[derive(Deserialize)]
struct GitReference {
    #[serde(rename = "ref")]
    reference: String,
}

fn stable_tag_version(refs: &[GitReference], name: &str) -> Result<String, String> {
    let prefix = format!("refs/tags/{name}-v");
    refs.iter().filter_map(|r| r.reference.strip_prefix(&prefix))
        .filter_map(|v| semver::Version::parse(v).ok()).filter(|v| v.pre.is_empty())
        .max().map(|v| v.to_string()).ok_or_else(|| format!("no stable {name}-v<version> tag exists in this repository; specify --version explicitly"))
}

async fn repository_version(
    client: &reqwest::Client,
    api: &str,
    name: &str,
    repository: &str,
) -> Result<String, String> {
    let repository = canonical_repository(repository)?;
    let repo = repository
        .strip_prefix("https://github.com/")
        .ok_or("invalid GitHub repository")?;
    let response = client
        .get(format!(
            "{api}/repos/{repo}/git/matching-refs/tags/{name}-v"
        ))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GitHub tag lookup failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("GitHub tag lookup returned {}", response.status()));
    }
    let refs: Vec<GitReference> =
        serde_json::from_slice(&body_limited(response, MAX_METADATA).await?)
            .map_err(|e| format!("invalid GitHub tag metadata: {e}"))?;
    stable_tag_version(&refs, name)
}

/// The five supported triples are deliberately exact: GNU/musl and MSVC/GNU
/// are different native-library distributions.
pub fn host_target() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") if cfg!(target_env = "msvc") => Ok("x86_64-pc-windows-msvc"),
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => Err(
            "no supported prebuilt target for this host; use --build to compile explicitly".into(),
        ),
    }
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Accept an explicit owner/repo override or repository metadata URLs,
/// including Cargo's common .git / git+https forms and GitHub tree subpaths.
pub(crate) fn canonical_repository(value: &str) -> Result<String, String> {
    let value = value.trim().strip_prefix("git+").unwrap_or(value.trim());
    let value = if value.contains("://") {
        value.to_string()
    } else {
        format!("https://github.com/{value}")
    };
    let url = reqwest::Url::parse(&value).map_err(|_| "invalid GitHub repository".to_string())?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err("prebuilt repository must be an HTTPS GitHub repository; use --build for other repositories".into());
    }
    let segments: Vec<_> = url
        .path_segments()
        .ok_or("invalid GitHub repository path")?
        .collect();
    if segments.len() < 2 || !valid_segment(segments[0]) {
        return Err("repository must identify a GitHub owner and repository".into());
    }
    let repo = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    if !valid_segment(repo) {
        return Err("invalid GitHub repository name".into());
    }
    if segments.len() > 2
        && !segments[2..].iter().all(|s| s.is_empty())
        && !matches!(segments[2], "tree" | "blob")
    {
        return Err("unsupported GitHub repository URL path".into());
    }
    Ok(format!(
        "https://github.com/{}/{}",
        segments[0].to_ascii_lowercase(),
        repo.to_ascii_lowercase()
    ))
}

fn release_url(repository: &str, name: &str, version: &str, file: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(repository).map_err(|e| e.to_string())?;
    url.path_segments_mut()
        .map_err(|_| "invalid release repository")?
        .push("releases")
        .push("download")
        .push(&format!("{name}-v{version}"))
        .push(file);
    Ok(url.into())
}

fn select_version(versions: &[RegistryVersion], requested: Option<&str>) -> Result<String, String> {
    if let Some(requested) = requested {
        let version = semver::Version::parse(requested)
            .map_err(|_| "--version requires an exact semantic version".to_string())?;
        return versions
            .iter()
            .find(|v| !v.yanked && v.num == version.to_string())
            .map(|v| v.num.clone())
            .ok_or_else(|| format!("version {requested} is absent or yanked on crates.io"));
    }
    versions
        .iter()
        .filter(|v| !v.yanked)
        .filter_map(|v| semver::Version::parse(&v.num).ok())
        .filter(|v| v.pre.is_empty())
        .max()
        .map(|v| v.to_string())
        .ok_or_else(|| "no non-yanked stable version is available".into())
}

pub(crate) async fn body_limited(
    mut response: reqwest::Response,
    limit: u64,
) -> Result<Vec<u8>, String> {
    if response.content_length().is_some_and(|n| n > limit) {
        return Err("download exceeds size limit".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("download failed: {e}"))?
    {
        if bytes.len() as u64 + chunk.len() as u64 > limit {
            return Err("download exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn resolve_package(
    client: &reqwest::Client,
    api: &str,
    name: &str,
    version: Option<&str>,
    repository: Option<&str>,
) -> Result<ResolvedPackage, String> {
    // An explicit repository + version also supports unpublished third-party extensions.
    if let Some(repository) = repository {
        let repository = canonical_repository(repository)?;
        let version = match version {
            Some(version) => semver::Version::parse(version)
                .map_err(|_| "--version requires an exact semantic version".to_string())?
                .to_string(),
            None => repository_version(client, "https://api.github.com", name, &repository).await?,
        };
        return Ok(ResolvedPackage {
            name: name.into(),
            version,
            repository,
            custom_repository: true,
        });
    }
    let response = client
        .get(format!("{api}/{name}"))
        .send()
        .await
        .map_err(|e| format!("crates.io lookup failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "crates.io lookup for {name} returned {}",
            response.status()
        ));
    }
    let data: RegistryPackage =
        serde_json::from_slice(&body_limited(response, MAX_METADATA).await?)
            .map_err(|e| format!("invalid crates.io metadata: {e}"))?;
    if !data
        .metadata
        .id
        .replace('-', "_")
        .eq_ignore_ascii_case(&name.replace('-', "_"))
    {
        return Err("crates.io metadata identifies a different package".into());
    }
    let version = select_version(&data.versions, version)?;
    let repository = repository
        .or(data.metadata.repository.as_deref())
        .ok_or("crate has no repository metadata; use --github owner/repo or --build")?;
    Ok(ResolvedPackage {
        name: data.metadata.id,
        version,
        repository: canonical_repository(repository)?,
        custom_repository: false,
    })
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn check_hash(bytes: &[u8], expected: &str) -> Result<(), String> {
    if expected.len() != 64
        || !expected
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("manifest SHA-256 must be 64 lowercase hexadecimal characters".into());
    }
    if hash(bytes) != expected {
        return Err("SHA-256 mismatch".into());
    }
    Ok(())
}

fn validate_manifest<'a>(
    manifest: &'a Manifest,
    package: &ResolvedPackage,
    target: &str,
) -> Result<&'a Distribution, String> {
    if manifest.schema_version != 1 {
        return Err(format!(
            "unsupported prebuilt manifest schema {}",
            manifest.schema_version
        ));
    }
    if manifest.name != package.name
        || manifest.version != package.version
        || manifest.tag != format!("{}-v{}", package.name, package.version)
        || canonical_repository(&manifest.repository)? != package.repository
    {
        return Err("prebuilt manifest package, version, repository or tag does not match the requested package".into());
    }
    if manifest.source_commit.len() != 40
        || !manifest
            .source_commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("manifest source_commit must be a full Git commit SHA".into());
    }
    semver::Version::parse(&manifest.sdk.version).map_err(|_| "invalid SDK version in manifest")?;
    if manifest.sdk.plugin_abi != rpi_plugin_sdk::RPI_PLUGIN_ABI_VERSION_UNIFIED {
        return Err(format!(
            "extension SDK ABI {} is incompatible with host ABI {}",
            manifest.sdk.plugin_abi,
            rpi_plugin_sdk::RPI_PLUGIN_ABI_VERSION_UNIFIED
        ));
    }
    let distribution = manifest.targets.get(target).ok_or_else(|| {
        format!(
            "no prebuilt extension for exact target {target}; use --build to compile explicitly"
        )
    })?;
    if distribution.size == 0
        || distribution.size > MAX_ARCHIVE
        || !matches!(distribution.format.as_str(), "zip" | "tar.gz")
    {
        return Err("unsupported archive format or size in manifest".into());
    }
    let name = &distribution.library.path;
    let extension = if target.contains("windows") {
        ".dll"
    } else if target.contains("apple") {
        ".dylib"
    } else {
        ".so"
    };
    if name.is_empty()
        || !name.ends_with(extension)
        || name.contains(['/', '\\', ':'])
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        || matches!(name.as_str(), "." | "..")
    {
        return Err(
            "manifest library.path must be a flat library filename for the selected target".into(),
        );
    }
    for expected in [&distribution.sha256, &distribution.library.sha256] {
        if expected.len() != 64
            || !expected
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid SHA-256 in manifest".into());
        }
    }
    validate_release_asset_url(&distribution.url, &package.repository, &manifest.tag)?;
    declared_assets(distribution)?;
    Ok(distribution)
}

/// Validate the optional `assets` declaration and return it in provenance form.
/// `None` is the legacy single-library layout.
fn declared_assets(distribution: &Distribution) -> Result<Option<Vec<PrebuiltAsset>>, String> {
    let Some(declared) = &distribution.assets else {
        return Ok(None);
    };
    // The library is one archive entry; every asset is another. Directory entries
    // are counted again when the archive itself is read.
    if declared.len() + 1 > MAX_ENTRIES {
        return Err("too many archive entries".into());
    }
    let library = distribution.library.path.to_ascii_lowercase();
    let mut folded: Vec<String> = Vec::with_capacity(declared.len());
    let mut assets = Vec::with_capacity(declared.len());
    for asset in declared {
        validate_asset_path(&asset.path)?;
        if !is_sha256_hex(&asset.sha256) {
            return Err("invalid asset SHA-256 in manifest".into());
        }
        let key = asset.path.to_ascii_lowercase();
        // Case-insensitive file systems would alias these names onto one another.
        if asset_paths_conflict(&key, &library) || asset_paths_conflict(&key, MANIFEST_FILE) {
            return Err(format!(
                "asset path `{}` collides with the library or manifest",
                asset.path
            ));
        }
        if folded.iter().any(|other| asset_paths_conflict(other, &key)) {
            return Err(format!(
                "duplicate or overlapping asset path `{}`",
                asset.path
            ));
        }
        folded.push(key);
        assets.push(PrebuiltAsset {
            path: asset.path.clone(),
            sha256: asset.sha256.clone(),
        });
    }
    Ok(Some(assets))
}

/// Sidecar paths are relative, forward-slash, and limited to a plain ASCII
/// alphabet. Components cannot be empty, `.`, `..` or hidden, and no component
/// may carry a dynamic-library extension because the loader scans the store.
pub(crate) fn validate_asset_path(value: &str) -> Result<(), String> {
    let invalid = || format!("invalid asset path `{value}`");
    if value.is_empty() || value.len() > MAX_ASSET_PATH {
        return Err(invalid());
    }
    for component in value.split('/') {
        if component.is_empty()
            || component.starts_with('.')
            || !component
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(invalid());
        }
        if crate::install::is_dynamic_library(Path::new(component)) {
            return Err(format!(
                "asset `{value}` has a native library extension and cannot be installed as an asset"
            ));
        }
    }
    Ok(())
}

/// Whether two lowercased store paths name the same file or nest one under the other.
pub(crate) fn asset_paths_conflict(left: &str, right: &str) -> bool {
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn validate_release_asset_url(
    value: &str,
    repository: &str,
    tag: &str,
) -> Result<(), String> {
    let url = reqwest::Url::parse(value).map_err(|_| "invalid archive URL")?;
    let segments: Vec<String> = url
        .path_segments()
        .ok_or("archive URL has no path")?
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .map(|s| s.into_owned())
                .map_err(|_| "invalid encoded archive URL")
        })
        .collect::<Result<_, _>>()?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || segments.len() != 6
        || segments[2] != "releases"
        || segments[3] != "download"
        || segments[4] != tag
        || canonical_repository(&format!(
            "https://github.com/{}/{}",
            segments[0], segments[1]
        ))? != repository
        || segments[5].is_empty()
        || matches!(segments[5].as_str(), "." | "..")
        || !segments[5]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'))
    {
        return Err(
            "archive URL must belong to the same GitHub repository and package Release".into(),
        );
    }
    Ok(())
}

async fn fetch_manifest(
    client: &reqwest::Client,
    package: &ResolvedPackage,
) -> Result<(Manifest, Vec<u8>), String> {
    let url = release_url(
        &package.repository,
        &package.name,
        &package.version,
        "rpi-extension.json",
    )?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("prebuilt manifest request failed: {e}"))?;
    read_manifest_response(response, package).await
}

async fn read_manifest_response(
    response: reqwest::Response,
    package: &ResolvedPackage,
) -> Result<(Manifest, Vec<u8>), String> {
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("prebuilt release for {} {} is not ready (rpi-extension.json is missing); retry later or use --build explicitly", package.name, package.version));
    }
    if !response.status().is_success() {
        return Err(format!(
            "prebuilt manifest request returned {}",
            response.status()
        ));
    }
    let bytes = body_limited(response, MAX_MANIFEST).await?;
    let manifest =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid rpi-extension.json: {e}"))?;
    Ok((manifest, bytes))
}

/// Only advertise an update when its exact platform manifest is available and compatible.
pub async fn latest_compatible_version(
    client: &reqwest::Client,
    package: &crate::install::InstalledNativePackage,
) -> Result<Option<String>, String> {
    let Some(record) = &package.prebuilt else {
        return Ok(None);
    };
    let repository = record
        .custom_repository
        .then_some(record.repository.as_str());
    let resolved = resolve_package(client, CRATES_API, &package.name, None, repository).await?;
    if resolved.repository != record.repository {
        return Err(
            "extension repository changed; reinstall explicitly to select a new publisher".into(),
        );
    }
    if !crate::updates::is_newer(&package.version, &resolved.version) {
        return Ok(Some(resolved.version));
    }
    let (manifest, _) = fetch_manifest(client, &resolved).await?;
    validate_manifest(&manifest, &resolved, host_target()?)?;
    Ok(Some(resolved.version))
}

pub async fn install(
    name: &str,
    version: Option<&str>,
    repository: Option<&str>,
    force: bool,
) -> Result<(), String> {
    let records = crate::install::installed_native_packages_strict()?;
    let existing = records.iter().find(|p| p.name == name);
    let target = host_target()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
        .map_err(|e| e.to_string())?;
    let repository = repository.or_else(|| {
        existing
            .and_then(|p| p.prebuilt.as_ref())
            .filter(|record| record.custom_repository)
            .map(|r| r.repository.as_str())
    });
    let package = resolve_package(&client, CRATES_API, name, version, repository).await?;
    if let Some(record) = existing.and_then(|p| p.prebuilt.as_ref()) {
        if repository.is_none() && record.repository != package.repository {
            return Err(
                "extension repository changed; select the new repository explicitly with --github"
                    .into(),
            );
        }
    }
    if !crate::install::should_install_version(
        name,
        existing.map(|p| p.version.as_str()),
        &package.version,
        force,
    )? {
        crate::install::print_already_installed(name, &package.version);
        return Ok(());
    }
    println!(
        "downloading prebuilt {} {} for {target}",
        package.name, package.version
    );
    let (manifest, manifest_bytes) = fetch_manifest(&client, &package).await?;
    let distribution = validate_manifest(&manifest, &package, target)?;
    let response = client
        .get(&distribution.url)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| format!("archive request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("archive request returned {}", response.status()));
    }
    let archive = body_limited(response, distribution.size).await?;
    let agent_dir = crate::config::agent_dir().map_err(|e| e.to_string())?;
    let target = target.to_string();
    let installed = tokio::task::spawn_blocking(move || {
        install_archive_at(
            &package,
            &manifest,
            &manifest_bytes,
            &archive,
            &target,
            &agent_dir,
            force,
        )
    })
    .await
    .map_err(|e| format!("prebuilt installation failed: {e}"))??;
    for path in installed {
        println!("installed {}", path.display());
    }
    println!("rpi will validate the plugin entrypoint/SDK API size and load this extension on the next start.");
    Ok(())
}

fn install_archive_at(
    package: &ResolvedPackage,
    manifest: &Manifest,
    manifest_bytes: &[u8],
    archive: &[u8],
    target: &str,
    agent_dir: &Path,
    force: bool,
) -> Result<Vec<PathBuf>, String> {
    let distribution = validate_manifest(manifest, package, target)?;
    let declared = declared_assets(distribution)?;
    if archive.len() as u64 != distribution.size {
        return Err("archive byte length does not match manifest".into());
    }
    check_hash(archive, &distribution.sha256)?;
    // Every entry and asset hash is verified here, before the store is touched.
    let extracted = extract_archive(
        archive,
        &distribution.format,
        &distribution.library.path,
        declared.as_deref(),
    )?;
    check_hash(&extracted.library, &distribution.library.sha256)?;
    validate_library_target(&extracted.library, target)?;
    let provenance = PrebuiltRecord {
        repository: package.repository.clone(),
        target: target.into(),
        manifest_sha256: hash(manifest_bytes),
        source_commit: manifest.source_commit.clone(),
        sdk_version: manifest.sdk.version.clone(),
        plugin_abi: manifest.sdk.plugin_abi,
        archive_sha256: distribution.sha256.clone(),
        library_sha256: distribution.library.sha256.clone(),
        custom_repository: package.custom_repository,
        assets: declared.unwrap_or_default(),
    };
    let temporary = tempfile::tempdir().map_err(|e| e.to_string())?;
    let artifact = temporary.path().join(&distribution.library.path);
    std::fs::write(&artifact, &extracted.library).map_err(|e| e.to_string())?;
    let assets: Vec<crate::install::NativeAssetFile> = extracted
        .assets
        .into_iter()
        .map(|(path, bytes)| crate::install::NativeAssetFile { path, bytes })
        .collect();
    let record = crate::install::InstalledNativePackage {
        name: package.name.clone(),
        version: package.version.clone(),
        // Older hosts regard this as a non-registry source and will not rebuild it.
        source: Some(format!("prebuilt:{}", package.repository)),
        prebuilt: Some(provenance),
        artifacts: Vec::new(),
    };
    crate::install::install_native_package_with_assets_at(
        &agent_dir.join("extensions"),
        &agent_dir.join("native-packages.json"),
        record,
        &[artifact],
        &assets,
        force,
    )
}

fn safe_entry_name(name: &str) -> Result<PathBuf, String> {
    if name.contains(['\\', ':'])
        || Path::new(name).components().any(|p| {
            matches!(
                p,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("unsafe archive entry path".into());
    }
    Ok(Path::new(name)
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect())
}

/// Read one archive file under two budgets: the per-file cap and what remains of
/// the archive-wide expansion cap. Budgets use bytes actually produced, so a
/// header that understates its size cannot bypass the archive limit.
fn read_entry(
    reader: impl Read,
    read: &mut u64,
    archive_limit: u64,
    file_limit: u64,
    file_error: &str,
) -> Result<Vec<u8>, String> {
    let remaining = archive_limit.saturating_sub(*read);
    let limit = file_limit.min(remaining);
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(if limit == remaining {
            "expanded archive exceeds size limit".into()
        } else {
            file_error.into()
        });
    }
    *read += bytes.len() as u64;
    Ok(bytes)
}

/// Strict zip extraction cannot rely on the reader's entry map alone: it keeps one
/// entry per name and silently drops duplicates. The central directory is read
/// directly instead, so duplicate names are rejected and every record is counted.
/// The directory must be the one the end record describes, located at the end of
/// the archive, so an archive with leading bytes is rejected as malformed.
fn check_zip_directory(archive: &[u8], entries: usize) -> Result<(), String> {
    let malformed = || "malformed zip central directory".to_string();
    let u16_at = |at: usize| {
        archive
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
    };
    let u32_at = |at: usize| {
        archive
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    // The end record is the last signature whose comment length reaches the end of the file.
    let search_start = archive.len().saturating_sub(22 + u16::MAX as usize);
    let end = (search_start..archive.len().saturating_sub(21))
        .rev()
        .find(|&at| {
            archive[at..].starts_with(b"PK\x05\x06")
                && u16_at(at + 20).is_some_and(|comment| at + 22 + comment == archive.len())
        })
        .ok_or_else(malformed)?;
    let total = u16_at(end + 10).ok_or_else(malformed)?;
    let size = u32_at(end + 12).ok_or_else(malformed)?;
    let start = u32_at(end + 16).ok_or_else(malformed)?;
    if total == 0xffff || size == 0xffff_ffff || start == 0xffff_ffff {
        return Err("zip64 archives are not supported for prebuilt extensions".into());
    }
    let stop = start.checked_add(size).ok_or_else(malformed)?;
    if stop > end {
        return Err(malformed());
    }
    let mut names = Vec::with_capacity(total);
    let mut at = start;
    while at < stop {
        if archive.get(at..at + 4) != Some(b"PK\x01\x02".as_slice()) {
            return Err(malformed());
        }
        let name_len = u16_at(at + 28).ok_or_else(malformed)?;
        let extra_len = u16_at(at + 30).ok_or_else(malformed)?;
        let comment_len = u16_at(at + 32).ok_or_else(malformed)?;
        names.push(
            archive
                .get(at + 46..at + 46 + name_len)
                .ok_or_else(malformed)?,
        );
        at += 46 + name_len + extra_len + comment_len;
    }
    if at != stop || names.len() != total {
        return Err(malformed());
    }
    names.sort_unstable();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("duplicate archive entry name".into());
    }
    if entries != total {
        return Err(malformed());
    }
    Ok(())
}

/// Verified archive contents. Assets are returned in manifest order.
pub(crate) struct ExtractedArchive {
    pub library: Vec<u8>,
    pub assets: Vec<(String, Vec<u8>)>,
}

/// Legacy entry point: only the declared library is read and other entries are ignored.
pub(crate) fn extract_library(
    archive: &[u8],
    format: &str,
    library: &str,
) -> Result<Vec<u8>, String> {
    extract_archive(archive, format, library, None).map(|extracted| extracted.library)
}

/// `declared == None` is the legacy contract: entries other than the library are
/// ignored. `Some` is strict: every regular file must be the library or a declared
/// asset, each asset must match its SHA-256, and every declared file must be present.
pub(crate) fn extract_archive(
    archive: &[u8],
    format: &str,
    library: &str,
    declared: Option<&[PrebuiltAsset]>,
) -> Result<ExtractedArchive, String> {
    let mut files = ArchiveFiles::new(library, declared);
    let mut total = 0u64;
    match format {
        "zip" => {
            let mut zip = zip::ZipArchive::new(Cursor::new(archive)).map_err(|e| e.to_string())?;
            if zip.len() > MAX_ENTRIES {
                return Err("too many archive entries".into());
            }
            if files.is_strict() {
                check_zip_directory(archive, zip.len())?;
            }
            for i in 0..zip.len() {
                let file = zip.by_index(i).map_err(|e| e.to_string())?;
                let path = safe_entry_name(file.name())?;
                let mode = file.unix_mode();
                if mode.is_some_and(|mode| mode & 0o170000 == 0o120000) {
                    return Err("archive symlinks are not allowed".into());
                }
                // Only a strict archive rejects devices and FIFOs; legacy archives keep their old behavior.
                if files.is_strict()
                    && mode.is_some_and(|mode| !matches!(mode & 0o170000, 0 | 0o100000 | 0o040000))
                {
                    return Err("archive special files are not allowed".into());
                }
                total = total
                    .checked_add(file.size())
                    .ok_or("archive size overflow")?;
                if total > MAX_ARCHIVE {
                    return Err("expanded archive exceeds size limit".into());
                }
                if !file.is_dir() {
                    files.accept(&path, file)?;
                }
            }
        }
        "tar.gz" => {
            let decoder = flate2::read::GzDecoder::new(Cursor::new(archive)).take(MAX_ARCHIVE + 1);
            let mut tar = tar::Archive::new(decoder);
            for (i, entry) in tar.entries().map_err(|e| e.to_string())?.enumerate() {
                if i >= MAX_ENTRIES {
                    return Err("too many archive entries".into());
                }
                let file = entry.map_err(|e| e.to_string())?;
                let name = file
                    .path()
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned();
                let path = safe_entry_name(&name)?;
                let kind = file.header().entry_type();
                if !kind.is_file() && !kind.is_dir() {
                    return Err("archive links and special files are not allowed".into());
                }
                total = total
                    .checked_add(file.size())
                    .ok_or("archive size overflow")?;
                if total > MAX_ARCHIVE {
                    return Err("expanded archive exceeds size limit".into());
                }
                if kind.is_file() {
                    files.accept(&path, file)?;
                }
            }
        }
        _ => return Err("unsupported prebuilt archive format".into()),
    }
    files.finish()
}

struct ArchiveFiles<'a> {
    library: &'a str,
    declared: Option<&'a [PrebuiltAsset]>,
    library_bytes: Option<Vec<u8>>,
    asset_bytes: Vec<Option<Vec<u8>>>,
    read: u64,
}

impl<'a> ArchiveFiles<'a> {
    fn new(library: &'a str, declared: Option<&'a [PrebuiltAsset]>) -> Self {
        Self {
            library,
            declared,
            library_bytes: None,
            asset_bytes: vec![None; declared.map_or(0, <[PrebuiltAsset]>::len)],
            read: 0,
        }
    }

    fn is_strict(&self) -> bool {
        self.declared.is_some()
    }

    fn accept(&mut self, path: &Path, reader: impl Read) -> Result<(), String> {
        if path == Path::new(self.library) {
            if self.library_bytes.is_some() {
                return Err("duplicate declared library in archive".into());
            }
            self.library_bytes = Some(read_entry(
                reader,
                &mut self.read,
                MAX_ARCHIVE,
                MAX_LIBRARY,
                "extracted library exceeds size limit",
            )?);
            return Ok(());
        }
        let Some(declared) = self.declared else {
            return Ok(());
        };
        let key = path.to_str().unwrap_or_default();
        let Some(index) = declared.iter().position(|asset| asset.path == key) else {
            return Err(format!("undeclared file in archive: {}", path.display()));
        };
        if self.asset_bytes[index].is_some() {
            return Err(format!("duplicate declared asset in archive: {key}"));
        }
        let bytes = read_entry(
            reader,
            &mut self.read,
            MAX_ARCHIVE,
            MAX_LIBRARY,
            "extracted asset exceeds size limit",
        )?;
        check_hash(&bytes, &declared[index].sha256).map_err(|e| format!("asset {key}: {e}"))?;
        self.asset_bytes[index] = Some(bytes);
        Ok(())
    }

    fn finish(self) -> Result<ExtractedArchive, String> {
        let library = self.library_bytes.ok_or_else(|| {
            format!(
                "archive does not contain the exact declared library {}",
                self.library
            )
        })?;
        let mut assets = Vec::new();
        if let Some(declared) = self.declared {
            for (asset, bytes) in declared.iter().zip(self.asset_bytes) {
                let bytes = bytes.ok_or_else(|| {
                    format!("archive does not contain declared asset {}", asset.path)
                })?;
                assets.push((asset.path.clone(), bytes));
            }
        }
        Ok(ExtractedArchive { library, assets })
    }
}

/// Verify the native object format/architecture without executing downloaded code.
fn validate_library_target(bytes: &[u8], target: &str) -> Result<(), String> {
    validate_native_target(bytes, target, true)
}

pub(crate) fn validate_executable_target(bytes: &[u8], target: &str) -> Result<(), String> {
    validate_native_target(bytes, target, false)
}

fn validate_native_target(bytes: &[u8], target: &str, library: bool) -> Result<(), String> {
    let u16_at = |n: usize| {
        bytes
            .get(n..n.checked_add(2)?)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |n: usize| {
        bytes
            .get(n..n.checked_add(4)?)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let valid = match target {
        "x86_64-pc-windows-msvc" => {
            let offset = u32_at(0x3c).unwrap_or(u32::MAX) as usize;
            bytes.starts_with(b"MZ")
                && bytes.get(offset..offset.saturating_add(4)) == Some(b"PE\0\0".as_slice())
                && u16_at(offset.saturating_add(4)) == Some(0x8664)
                && u16_at(offset.saturating_add(22)).is_some_and(|flags| {
                    if library {
                        flags & 0x2000 != 0
                    } else {
                        flags & 0x2000 == 0 && flags & 2 != 0
                    }
                })
        }
        "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu" => {
            bytes.starts_with(b"\x7fELF\x02\x01")
                && u16_at(16).is_some_and(|kind| kind == 3 || (!library && kind == 2))
                && u16_at(18)
                    == Some(if target.starts_with("aarch64") {
                        183
                    } else {
                        62
                    })
        }
        "x86_64-apple-darwin" | "aarch64-apple-darwin" => {
            u32_at(0) == Some(0xfeedfacf)
                && u32_at(12) == Some(if library { 6 } else { 2 })
                && u32_at(4)
                    == Some(if target.starts_with("aarch64") {
                        0x0100000c
                    } else {
                        0x01000007
                    })
        }
        _ => false,
    };
    if !valid {
        return Err(format!(
            "declared artifact is not a compatible native {} for {target}",
            if library {
                "shared library"
            } else {
                "executable"
            }
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const TARGETS: [&str; 5] = [
        "x86_64-pc-windows-msvc",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ];

    fn object(target: &str) -> Vec<u8> {
        let mut bytes = vec![0u8; 128];
        if target.contains("windows") {
            bytes[..2].copy_from_slice(b"MZ");
            bytes[0x3c..0x40].copy_from_slice(&64u32.to_le_bytes());
            bytes[64..68].copy_from_slice(b"PE\0\0");
            bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
            bytes[86..88].copy_from_slice(&0x2000u16.to_le_bytes());
        } else if target.contains("linux") {
            bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
            bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
            bytes[18..20].copy_from_slice(
                &(if target.starts_with("aarch64") {
                    183u16
                } else {
                    62u16
                })
                .to_le_bytes(),
            );
        } else {
            bytes[..4].copy_from_slice(&0xfeedfacfu32.to_le_bytes());
            bytes[4..8].copy_from_slice(
                &(if target.starts_with("aarch64") {
                    0x0100000cu32
                } else {
                    0x01000007u32
                })
                .to_le_bytes(),
            );
            bytes[12..16].copy_from_slice(&6u32.to_le_bytes());
        }
        bytes
    }

    fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn tgz(name: &str, bytes: &[u8]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes).unwrap();
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn fixture(target: &str) -> (ResolvedPackage, serde_json::Value, Vec<u8>) {
        let package = ResolvedPackage {
            name: "independent-plugin".into(),
            version: "1.2.3".into(),
            repository: "https://github.com/publisher/monorepo".into(),
            custom_repository: false,
        };
        // The library target deliberately differs from the crate name.
        let (format, library, file) = if target.contains("windows") {
            ("zip", "custom_runtime.dll", "package.zip")
        } else if target.contains("apple") {
            ("tar.gz", "libcustom_runtime.dylib", "package.tar.gz")
        } else {
            ("tar.gz", "libcustom_runtime.so", "package.tar.gz")
        };
        let bytes = object(target);
        let archive = if format == "zip" {
            zip(&[(library, &bytes)])
        } else {
            tgz(library, &bytes)
        };
        let manifest = serde_json::json!({
            "schema_version":1, "name":package.name, "version":package.version,
            "repository":package.repository, "tag":"independent-plugin-v1.2.3", "source_commit":"a".repeat(40),
            "sdk":{"version":"0.3.23", "plugin_abi":rpi_plugin_sdk::RPI_PLUGIN_ABI_VERSION_UNIFIED},
            "targets":{target:{"url":release_url(&package.repository,&package.name,&package.version,file).unwrap(),
                "format":format,"size":archive.len(),"sha256":hash(&archive),"library":{"path":library,"sha256":hash(&bytes)}}}
        });
        (package, manifest, archive)
    }

    #[test]
    fn all_five_distributions_install_and_record_exact_provenance() {
        for target in TARGETS {
            let (package, value, archive) = fixture(target);
            let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
            let raw = serde_json::to_vec(&value).unwrap();
            let agent = tempfile::tempdir().unwrap();
            let paths = install_archive_at(
                &package,
                &manifest,
                &raw,
                &archive,
                target,
                agent.path(),
                false,
            )
            .unwrap();
            assert_eq!(paths.len(), 1);
            assert_eq!(std::fs::read(&paths[0]).unwrap(), object(target));
            let records: Vec<crate::install::InstalledNativePackage> = serde_json::from_slice(
                &std::fs::read(agent.path().join("native-packages.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(records[0].name, "independent-plugin");
            assert_eq!(records[0].version, "1.2.3");
            assert_eq!(
                records[0].artifacts,
                [manifest.targets[target].library.path.clone()]
            );
            let origin = records[0].prebuilt.as_ref().unwrap();
            assert_eq!(origin.target, target);
            assert_eq!(origin.manifest_sha256, hash(&raw));
            assert_eq!(origin.repository, package.repository);
            assert_eq!(origin.archive_sha256, hash(&archive));
            assert_eq!(origin.library_sha256, hash(&object(target)));
        }
    }

    #[test]
    fn prebuilt_packages_upgrade_without_force_and_same_version_is_unchanged() {
        for target in TARGETS {
            let (mut package, mut value, archive) = fixture(target);
            let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
            let agent = tempfile::tempdir().unwrap();
            install_archive_at(
                &package,
                &manifest,
                &serde_json::to_vec(&value).unwrap(),
                &archive,
                target,
                agent.path(),
                false,
            )
            .unwrap();
            package.version = "1.2.4".into();
            value["version"] = serde_json::json!(package.version);
            value["tag"] = serde_json::json!("independent-plugin-v1.2.4");
            let distribution = value.pointer_mut(&format!("/targets/{target}")).unwrap();
            let format = distribution["format"].as_str().unwrap();
            let file = if format == "zip" {
                "package.zip"
            } else {
                "package.tar.gz"
            };
            distribution["url"] = serde_json::json!(release_url(
                &package.repository,
                &package.name,
                &package.version,
                file,
            )
            .unwrap());
            let manifest = serde_json::from_value(value.clone()).unwrap();
            let raw = serde_json::to_vec(&value).unwrap();
            assert_eq!(
                install_archive_at(
                    &package,
                    &manifest,
                    &raw,
                    &archive,
                    target,
                    agent.path(),
                    false,
                )
                .unwrap()
                .len(),
                1
            );
            let metadata = agent.path().join("native-packages.json");
            let registry = std::fs::read(&metadata).unwrap();
            let records: Vec<crate::install::InstalledNativePackage> =
                serde_json::from_slice(&registry).unwrap();
            assert_eq!(records[0].version, "1.2.4");
            assert!(install_archive_at(
                &package,
                &manifest,
                &raw,
                &archive,
                target,
                agent.path(),
                false,
            )
            .unwrap()
            .is_empty());
            assert_eq!(std::fs::read(&metadata).unwrap(), registry);
        }
    }

    #[test]
    fn bad_release_identity_hash_abi_and_target_leave_installed_version_intact() {
        let target = TARGETS[0];
        let (package, good, archive) = fixture(target);
        let manifest: Manifest = serde_json::from_value(good.clone()).unwrap();
        let agent = tempfile::tempdir().unwrap();
        let path = install_archive_at(
            &package,
            &manifest,
            &serde_json::to_vec(&good).unwrap(),
            &archive,
            target,
            agent.path(),
            false,
        )
        .unwrap()
        .remove(0);
        let original_record = std::fs::read(agent.path().join("native-packages.json")).unwrap();
        let mutations = [
            ("/schema_version",serde_json::json!(99)), ("/name",serde_json::json!("other-plugin")),
            ("/version",serde_json::json!("9.9.9")), ("/tag",serde_json::json!("v1.2.3")),
            ("/repository",serde_json::json!("https://github.com/other/repo")),
            ("/sdk/plugin_abi",serde_json::json!(99)),
            ("/targets/x86_64-pc-windows-msvc/sha256",serde_json::json!("0".repeat(64))),
            ("/targets/x86_64-pc-windows-msvc/library/sha256",serde_json::json!("0".repeat(64))),
            ("/targets/x86_64-pc-windows-msvc/library/path",serde_json::json!("../custom_runtime.dll")),
            ("/targets/x86_64-pc-windows-msvc/url",serde_json::json!("https://github.com/other/repo/releases/download/independent-plugin-v1.2.3/package.zip")),
            ("/targets/x86_64-pc-windows-msvc/size",serde_json::json!(archive.len()+1)),
        ];
        for (pointer, value) in mutations {
            let mut bad = good.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            let bad_manifest = serde_json::from_value(bad.clone()).unwrap();
            assert!(
                install_archive_at(
                    &package,
                    &bad_manifest,
                    &serde_json::to_vec(&bad).unwrap(),
                    &archive,
                    target,
                    agent.path(),
                    true
                )
                .is_err(),
                "{pointer}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), object(target));
            assert_eq!(
                std::fs::read(agent.path().join("native-packages.json")).unwrap(),
                original_record
            );
        }
        assert!(install_archive_at(
            &package,
            &manifest,
            &[],
            &archive,
            TARGETS[1],
            agent.path(),
            true
        )
        .is_err());
    }

    #[test]
    fn same_abi_but_wrong_binary_architecture_is_rejected_before_activation() {
        let (package, mut value, _) = fixture(TARGETS[0]);
        let wrong = object(TARGETS[2]);
        let archive = zip(&[("custom_runtime.dll", &wrong)]);
        let distribution = value
            .pointer_mut("/targets/x86_64-pc-windows-msvc")
            .unwrap();
        distribution["size"] = serde_json::json!(archive.len());
        distribution["sha256"] = serde_json::json!(hash(&archive));
        distribution["library"]["sha256"] = serde_json::json!(hash(&wrong));
        let manifest = serde_json::from_value(value).unwrap();
        let agent = tempfile::tempdir().unwrap();
        assert!(install_archive_at(
            &package,
            &manifest,
            &[],
            &archive,
            TARGETS[0],
            agent.path(),
            false
        )
        .unwrap_err()
        .contains("shared library"));
        assert!(!agent.path().join("native-packages.json").exists());
        assert!(!agent.path().join("extensions").exists());
    }

    #[test]
    fn archives_cannot_traverse_retain_links_or_substitute_another_library() {
        let library = object(TARGETS[0]);
        assert!(
            extract_library(&zip(&[("../escape.dll", &library)]), "zip", "escape.dll").is_err()
        );
        assert!(
            extract_library(&zip(&[("C:\\escape.dll", &library)]), "zip", "escape.dll").is_err()
        );
        assert!(extract_library(&zip(&[("other.dll", &library)]), "zip", "declared.dll").is_err());
        let mut linked = zip::ZipWriter::new(Cursor::new(Vec::new()));
        linked
            .add_symlink(
                "declared.dll",
                "outside.dll",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        assert!(extract_library(
            &linked.finish().unwrap().into_inner(),
            "zip",
            "declared.dll"
        )
        .is_err());
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Link);
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_link(&mut header, "declared.so", "outside.so")
            .unwrap();
        assert!(extract_library(
            &tar.into_inner().unwrap().finish().unwrap(),
            "tar.gz",
            "declared.so"
        )
        .is_err());
    }

    #[test]
    fn object_headers_cannot_cross_platform_or_architecture() {
        for target in TARGETS {
            assert!(validate_library_target(&object(target), target).is_ok());
            for other in TARGETS {
                if other != target {
                    assert!(validate_library_target(&object(other), target).is_err());
                }
            }
        }
        assert!(validate_library_target(&[0u8; 8], TARGETS[0]).is_err());
        let mut damaged = object(TARGETS[0]);
        damaged[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(validate_library_target(&damaged, TARGETS[0]).is_err());
    }

    #[test]
    fn exact_versions_ignore_yanked_and_prerelease_latest() {
        let versions = [
            RegistryVersion {
                num: "1.0.0".into(),
                yanked: false,
            },
            RegistryVersion {
                num: "9.0.0".into(),
                yanked: true,
            },
            RegistryVersion {
                num: "2.0.0-beta.1".into(),
                yanked: false,
            },
        ];
        assert_eq!(select_version(&versions, None).unwrap(), "1.0.0");
        assert_eq!(
            select_version(&versions, Some("2.0.0-beta.1")).unwrap(),
            "2.0.0-beta.1"
        );
        assert!(select_version(&versions, Some("9.0.0")).is_err());
        assert!(select_version(&versions, Some("^1")).is_err());
    }

    #[test]
    fn github_case_and_encoded_semver_build_metadata_preserve_release_identity() {
        let (mut package, mut value, _) = fixture(TARGETS[0]);
        package.version = "1.2.3+build.1".into();
        value["version"] = serde_json::json!(package.version);
        value["tag"] = serde_json::json!("independent-plugin-v1.2.3+build.1");
        value["repository"] = serde_json::json!("https://github.com/Publisher/MonoRepo");
        value["targets"][TARGETS[0]]["url"] = serde_json::json!("https://github.com/Publisher/MonoRepo/releases/download/independent-plugin-v1.2.3%2Bbuild.1/package.zip");
        let manifest = serde_json::from_value(value).unwrap();
        assert!(validate_manifest(&manifest, &package, TARGETS[0]).is_ok());
        assert_eq!(
            canonical_repository("Publisher/MonoRepo").unwrap(),
            package.repository
        );
    }

    #[test]
    fn repository_urls_are_explicit_and_release_tags_use_package_versions() {
        assert_eq!(
            canonical_repository("git+https://github.com/publisher/monorepo.git").unwrap(),
            "https://github.com/publisher/monorepo"
        );
        assert_eq!(
            canonical_repository("https://github.com/publisher/monorepo/tree/main/packages/plugin")
                .unwrap(),
            "https://github.com/publisher/monorepo"
        );
        for invalid in [
            "https://gitlab.com/x/y",
            "https://user@github.com/x/y",
            "https://github.com/x",
            "https://github.com/x/y/releases/latest",
            "https://github.com:8443/x/y",
        ] {
            assert!(canonical_repository(invalid).is_err());
        }
        assert_eq!(release_url("https://github.com/publisher/monorepo","plugin-one","1.2.3","rpi-extension.json").unwrap(),"https://github.com/publisher/monorepo/releases/download/plugin-one-v1.2.3/rpi-extension.json");
        assert!(release_url(
            "https://github.com/publisher/monorepo",
            "plugin-two",
            "4.5.6",
            "rpi-extension.json"
        )
        .unwrap()
        .contains("plugin-two-v4.5.6"));
    }

    fn one_response(body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        response_with_status("200 OK", body)
    }

    fn response_with_status(
        status: &'static str,
        body: Vec<u8>,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            write!(
                socket,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(&body).unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, thread)
    }

    #[tokio::test]
    async fn downloaded_manifest_and_archive_install_into_an_isolated_profile() {
        let target = host_target().unwrap();
        let (package, value, archive) = fixture(target);
        let raw = serde_json::to_vec(&value).unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let (url, server) = one_response(raw.clone());
        let response = client.get(url).send().await.unwrap();
        let (manifest, received) = read_manifest_response(response, &package).await.unwrap();
        server.join().unwrap();
        assert_eq!(received, raw);
        let distribution = validate_manifest(&manifest, &package, target).unwrap();
        let (url, server) = one_response(archive);
        let response = client.get(url).send().await.unwrap();
        let downloaded = body_limited(response, distribution.size).await.unwrap();
        server.join().unwrap();
        let profile = tempfile::tempdir().unwrap();
        let paths = install_archive_at(
            &package,
            &manifest,
            &received,
            &downloaded,
            target,
            profile.path(),
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read(&paths[0]).unwrap(), object(target));
        assert!(profile.path().join("native-packages.json").is_file());
    }

    #[tokio::test]
    async fn missing_invalid_and_oversized_manifests_fail_before_installation() {
        let (package, _, _) = fixture(TARGETS[0]);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for (status, body, expected) in [
            (
                "404 Not Found",
                b"missing".to_vec(),
                "rpi-extension.json is missing",
            ),
            ("403 Forbidden", b"denied".to_vec(), "403 Forbidden"),
            ("200 OK", b"not json".to_vec(), "invalid rpi-extension.json"),
            (
                "200 OK",
                vec![b' '; MAX_MANIFEST as usize + 1],
                "size limit",
            ),
        ] {
            let (url, server) = response_with_status(status, body);
            let response = client.get(url).send().await.unwrap();
            let error = read_manifest_response(response, &package)
                .await
                .err()
                .unwrap();
            assert!(error.contains(expected), "{error}");
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn explicit_repository_and_version_do_not_require_a_registry_record() {
        let client = reqwest::Client::new();
        let resolved = resolve_package(
            &client,
            "http://127.0.0.1:1",
            "private-extension",
            Some("2.3.4"),
            Some("https://github.com/custom-owner/custom-repo.git"),
        )
        .await
        .unwrap();
        assert_eq!(
            resolved.repository,
            "https://github.com/custom-owner/custom-repo"
        );
        assert_eq!(resolved.name, "private-extension");
        assert_eq!(resolved.version, "2.3.4");
        assert!(release_url(
            &resolved.repository,
            &resolved.name,
            &resolved.version,
            "rpi-extension.json"
        )
        .unwrap()
        .contains("custom-repo/releases/download/private-extension-v2.3.4/"));
    }

    #[tokio::test]
    async fn custom_repository_versions_are_scoped_to_the_package() {
        let refs = serde_json::to_vec(&serde_json::json!([
            {"ref":"refs/tags/independent-plugin-v1.2.3"},
            {"ref":"refs/tags/independent-plugin-v1.4.0"},
            {"ref":"refs/tags/other-plugin-v99.0.0"},
            {"ref":"refs/tags/independent-plugin-v2.0.0-beta.1"}
        ]))
        .unwrap();
        let (api, server) = one_response(refs);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            repository_version(
                &client,
                &api,
                "independent-plugin",
                "https://github.com/publisher/monorepo"
            )
            .await
            .unwrap(),
            "1.4.0"
        );
        assert!(server.join().unwrap().starts_with(
            "GET /repos/publisher/monorepo/git/matching-refs/tags/independent-plugin-v "
        ));
    }

    #[tokio::test]
    async fn registry_discovery_uses_repository_metadata_and_downloads_are_bounded() {
        let payload = serde_json::to_vec(&serde_json::json!({"crate":{"id":"independent-plugin","repository":"https://github.com/publisher/monorepo.git"},"versions":[{"num":"1.2.3","yanked":false},{"num":"3.0.0-beta.1","yanked":false},{"num":"9.0.0","yanked":true}]})).unwrap();
        let (url, server) = one_response(payload);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let package = resolve_package(&client, &url, "independent-plugin", None, None)
            .await
            .unwrap();
        assert_eq!(package.repository, "https://github.com/publisher/monorepo");
        assert_eq!(package.version, "1.2.3");
        assert!(server
            .join()
            .unwrap()
            .starts_with("GET /independent-plugin "));
        let (url, server) = one_response(vec![0u8; 32]);
        let response = client.get(url).send().await.unwrap();
        assert!(body_limited(response, 8).await.is_err());
        server.join().unwrap();
    }

    fn library_for(target: &str) -> &'static str {
        if target.contains("windows") {
            "custom_runtime.dll"
        } else if target.contains("apple") {
            "libcustom_runtime.dylib"
        } else {
            "libcustom_runtime.so"
        }
    }

    fn archive_for(target: &str, entries: &[(&str, &[u8])]) -> Vec<u8> {
        if target.contains("windows") {
            zip(entries)
        } else {
            tgz_with(|tar| {
                for (name, bytes) in entries {
                    tar_file(tar, name, bytes);
                }
            })
        }
    }

    type TarBuilder = tar::Builder<flate2::write::GzEncoder<Vec<u8>>>;

    fn tgz_with(build: impl FnOnce(&mut TarBuilder)) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        build(&mut tar);
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn tar_file(tar: &mut TarBuilder, name: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes).unwrap();
    }

    /// Writes `name` into the header verbatim, bypassing the writer's path checks.
    fn tar_file_raw_name(tar: &mut TarBuilder, name: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        tar.append(&header, bytes).unwrap();
    }

    fn tar_link(tar: &mut TarBuilder, name: &str, kind: tar::EntryType, target: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        tar.append_link(&mut header, name, target).unwrap();
    }

    /// A zip whose single entry carries a Unix character-device mode. The writer
    /// cannot express type bits, so the central directory's external attributes
    /// are patched after writing.
    fn zip_with_device_entry(name: &str) -> Vec<u8> {
        let mut archive = zip(&[(name, b"device")]);
        let central = archive
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        let mode: u32 = 0o020644;
        archive[central + 38..central + 42].copy_from_slice(&(mode << 16).to_le_bytes());
        archive
    }

    /// The zip writer refuses duplicate names, so duplicates are made by writing a
    /// same-length alias and rewriting it in both the local and central headers.
    fn replace_bytes(bytes: &mut [u8], from: &[u8], to: &[u8]) {
        assert_eq!(from.len(), to.len());
        for start in 0..=bytes.len().saturating_sub(from.len()) {
            if bytes[start..].starts_with(from) {
                bytes[start..start + to.len()].copy_from_slice(to);
            }
        }
    }

    fn declaration(assets: &[(&str, &[u8])]) -> serde_json::Value {
        serde_json::Value::Array(
            assets
                .iter()
                .map(|(path, bytes)| serde_json::json!({"path": path, "sha256": hash(bytes)}))
                .collect(),
        )
    }

    /// The fixture manifest for `target`, pointed at `archive` and declaring `assets`.
    fn manifest_case(
        target: &str,
        archive: &[u8],
        assets: Option<serde_json::Value>,
    ) -> (ResolvedPackage, serde_json::Value) {
        let (package, mut value, _) = fixture(target);
        value["targets"][target]["size"] = serde_json::json!(archive.len());
        value["targets"][target]["sha256"] = serde_json::json!(hash(archive));
        if let Some(assets) = assets {
            value["targets"][target]["assets"] = assets;
        }
        (package, value)
    }

    fn install_case(
        target: &str,
        archive: &[u8],
        assets: Option<serde_json::Value>,
        agent: &Path,
    ) -> Result<Vec<PathBuf>, String> {
        let (package, value) = manifest_case(target, archive, assets);
        let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
        let raw = serde_json::to_vec(&value).unwrap();
        install_archive_at(&package, &manifest, &raw, archive, target, agent, false)
    }

    fn assert_rejected(
        target: &str,
        archive: &[u8],
        assets: Option<serde_json::Value>,
        expected: &str,
    ) {
        let agent = tempfile::tempdir().unwrap();
        let error = install_case(target, archive, assets, agent.path()).unwrap_err();
        assert!(
            error.contains(expected),
            "expected `{expected}` in `{error}`"
        );
        assert!(!agent.path().join("extensions").exists(), "{error}");
        assert!(
            !agent.path().join("native-packages.json").exists(),
            "{error}"
        );
    }

    const SIDECAR: &[u8] = b"export {};";

    #[test]
    fn manifests_without_assets_keep_the_single_library_layout() {
        for target in TARGETS {
            let (package, value, archive) = fixture(target);
            assert!(value["targets"][target].get("assets").is_none());
            let manifest: Manifest = serde_json::from_value(value.clone()).unwrap();
            assert!(manifest.targets[target].assets.is_none());
            let agent = tempfile::tempdir().unwrap();
            let paths = install_archive_at(
                &package,
                &manifest,
                &serde_json::to_vec(&value).unwrap(),
                &archive,
                target,
                agent.path(),
                false,
            )
            .unwrap();
            assert_eq!(paths.len(), 1, "{target}");
            let installed: Vec<String> = std::fs::read_dir(agent.path().join("extensions"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(installed, [library_for(target)], "{target}");
            let registry =
                std::fs::read_to_string(agent.path().join("native-packages.json")).unwrap();
            assert!(!registry.contains("assets"), "{registry}");
        }
    }

    #[test]
    fn legacy_archives_still_ignore_undeclared_entries() {
        let target = TARGETS[1];
        let library = object(target);
        let archive = archive_for(
            target,
            &[(library_for(target), &library), ("README.md", b"docs")],
        );
        let agent = tempfile::tempdir().unwrap();
        assert_eq!(
            install_case(target, &archive, None, agent.path())
                .unwrap()
                .len(),
            1
        );
        assert!(!agent.path().join("extensions").join("README.md").exists());
    }

    #[test]
    fn declared_assets_install_beside_the_library_with_their_relative_layout() {
        let assets: &[(&str, &[u8])] = &[
            ("runtime/main.mjs", SIDECAR),
            ("runtime/dist/index.mjs", b"export const dist = 1;"),
            ("runtime/shared/util.mjs", b"export const util = 2;"),
            ("runtime/package.json", br#"{"type":"module"}"#),
            ("runtime/package-lock.json", b"{}"),
            ("LICENSE", b"MIT"),
        ];
        for target in TARGETS {
            let library = object(target);
            let mut entries: Vec<(&str, &[u8])> = vec![(library_for(target), &library)];
            entries.extend_from_slice(assets);
            let archive = archive_for(target, &entries);
            let agent = tempfile::tempdir().unwrap();
            let paths =
                install_case(target, &archive, Some(declaration(assets)), agent.path()).unwrap();
            assert_eq!(paths.len(), 1 + assets.len(), "{target}");
            let extensions = agent.path().join("extensions");
            assert_eq!(
                std::fs::read(extensions.join(library_for(target))).unwrap(),
                library
            );
            for (path, bytes) in assets {
                assert_eq!(
                    std::fs::read(extensions.join(path)).unwrap(),
                    *bytes,
                    "{path}"
                );
            }
            let records: Vec<crate::install::InstalledNativePackage> = serde_json::from_slice(
                &std::fs::read(agent.path().join("native-packages.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(records[0].artifacts, [library_for(target)]);
            let provenance = records[0].prebuilt.as_ref().unwrap();
            assert_eq!(
                provenance.assets,
                assets
                    .iter()
                    .map(|(path, bytes)| PrebuiltAsset {
                        path: (*path).into(),
                        sha256: hash(bytes),
                    })
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn unsafe_asset_declarations_are_rejected_before_anything_is_written() {
        let target = TARGETS[1];
        let library = object(target);
        let archive = archive_for(target, &[(library_for(target), &library)]);
        let digest = hash(b"x");
        let cases: [(&[&str], &str); 17] = [
            (&["/etc/main.mjs"], "invalid asset path"),
            (&["../escape.mjs"], "invalid asset path"),
            (&["runtime/../escape.mjs"], "invalid asset path"),
            (&["./main.mjs"], "invalid asset path"),
            (&["runtime//main.mjs"], "invalid asset path"),
            (&["runtime/"], "invalid asset path"),
            (&[""], "invalid asset path"),
            (&["runtime\\main.mjs"], "invalid asset path"),
            (&["C:main.mjs"], "invalid asset path"),
            (&[".hidden.mjs"], "invalid asset path"),
            (&["has space.mjs"], "invalid asset path"),
            (&["runtime/plugin.so"], "native library extension"),
            (&["plugin.dll"], "native library extension"),
            (
                &["runtime/main.mjs", "runtime/main.mjs"],
                "duplicate or overlapping",
            ),
            (
                &["runtime/main.mjs", "Runtime/MAIN.mjs"],
                "duplicate or overlapping",
            ),
            (&["runtime", "runtime/main.mjs"], "duplicate or overlapping"),
            (
                &["RPI-EXTENSION.JSON"],
                "collides with the library or manifest",
            ),
        ];
        for (paths, expected) in cases {
            let declared: Vec<serde_json::Value> = paths
                .iter()
                .map(|path| serde_json::json!({"path": path, "sha256": digest}))
                .collect();
            assert_rejected(
                target,
                &archive,
                Some(serde_json::Value::Array(declared)),
                expected,
            );
        }
        assert_rejected(
            target,
            &archive,
            Some(serde_json::json!([{"path": "runtime/main.mjs", "sha256": "ABC"}])),
            "invalid asset SHA-256",
        );
    }

    #[test]
    fn asset_declarations_are_bounded_by_the_archive_entry_cap() {
        let target = TARGETS[1];
        let library = object(target);
        let archive = archive_for(target, &[(library_for(target), &library)]);
        let declared: Vec<serde_json::Value> = (0..MAX_ENTRIES)
            .map(|index| serde_json::json!({"path": format!("runtime/f{index}.mjs"), "sha256": hash(b"")}))
            .collect();
        assert_rejected(
            target,
            &archive,
            Some(serde_json::Value::Array(declared)),
            "too many archive entries",
        );
    }

    #[test]
    fn strict_archives_reject_unsafe_or_undeclared_entries() {
        let zip_target = TARGETS[0];
        let tgz_target = TARGETS[1];
        let lib_zip = object(zip_target);
        let lib_tgz = object(tgz_target);
        let main: &[u8] = SIDECAR;
        let decl_zip = Some(declaration(&[("runtime/main.mjs", main)]));
        let decl_tgz = Some(declaration(&[("runtime/main.mjs", main)]));

        let traversal = zip(&[(library_for(zip_target), &lib_zip), ("../escape.mjs", b"x")]);
        assert_rejected(
            zip_target,
            &traversal,
            decl_zip.clone(),
            "unsafe archive entry path",
        );
        let absolute = zip(&[(library_for(zip_target), &lib_zip), ("/abs.mjs", b"x")]);
        assert_rejected(
            zip_target,
            &absolute,
            decl_zip.clone(),
            "unsafe archive entry path",
        );
        let windows_path = zip(&[
            (library_for(zip_target), &lib_zip),
            ("runtime\\main.mjs", main),
        ]);
        assert_rejected(
            zip_target,
            &windows_path,
            decl_zip.clone(),
            "unsafe archive entry path",
        );

        let undeclared = zip(&[
            (library_for(zip_target), &lib_zip),
            ("runtime/main.mjs", main),
            ("extra.mjs", b"x"),
        ]);
        assert_rejected(
            zip_target,
            &undeclared,
            decl_zip.clone(),
            "undeclared file in archive: extra.mjs",
        );
        let missing = zip(&[(library_for(zip_target), &lib_zip)]);
        assert_rejected(
            zip_target,
            &missing,
            decl_zip.clone(),
            "does not contain declared asset runtime/main.mjs",
        );
        let tampered = zip(&[
            (library_for(zip_target), &lib_zip),
            ("runtime/main.mjs", b"tampered"),
        ]);
        assert_rejected(zip_target, &tampered, decl_zip.clone(), "SHA-256 mismatch");
        let mut duplicated = zip(&[
            (library_for(zip_target), &lib_zip),
            ("runtime/main.mjs", main),
            ("runtime/mein.mjs", main),
        ]);
        replace_bytes(&mut duplicated, b"runtime/mein.mjs", b"runtime/main.mjs");
        assert_rejected(
            zip_target,
            &duplicated,
            decl_zip.clone(),
            "duplicate archive entry name",
        );
        let mut duplicate_library = zip(&[
            (library_for(zip_target), &lib_zip),
            ("custom_runtime.dlx", &lib_zip),
        ]);
        replace_bytes(
            &mut duplicate_library,
            b"custom_runtime.dlx",
            library_for(zip_target).as_bytes(),
        );
        assert_rejected(
            zip_target,
            &duplicate_library,
            Some(serde_json::json!([])),
            "duplicate archive entry name",
        );

        let mut linked = zip::ZipWriter::new(Cursor::new(Vec::new()));
        linked
            .add_symlink(
                "runtime/main.mjs",
                "/etc/passwd",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        let linked = linked.finish().unwrap().into_inner();
        assert_rejected(
            zip_target,
            &linked,
            decl_zip.clone(),
            "symlinks are not allowed",
        );
        let device = zip_with_device_entry("runtime/main.mjs");
        assert_rejected(
            zip_target,
            &device,
            decl_zip.clone(),
            "special files are not allowed",
        );

        let mut many = vec![(library_for(zip_target), lib_zip.as_slice())];
        let names: Vec<String> = (0..MAX_ENTRIES)
            .map(|index| format!("extra{index}.mjs"))
            .collect();
        for name in &names {
            many.push((name.as_str(), b"x"));
        }
        assert_rejected(
            zip_target,
            &zip(&many),
            decl_zip.clone(),
            "too many archive entries",
        );

        let traversal = tgz_with(|tar| {
            tar_file(tar, library_for(tgz_target), &lib_tgz);
            tar_file_raw_name(tar, "../escape.mjs", b"x");
        });
        assert_rejected(
            tgz_target,
            &traversal,
            decl_tgz.clone(),
            "unsafe archive entry path",
        );
        let undeclared = tgz_with(|tar| {
            tar_file(tar, library_for(tgz_target), &lib_tgz);
            tar_file(tar, "runtime/main.mjs", main);
            tar_file(tar, "extra.mjs", b"x");
        });
        assert_rejected(
            tgz_target,
            &undeclared,
            decl_tgz.clone(),
            "undeclared file in archive",
        );
        let hard = tgz_with(|tar| {
            tar_file(tar, library_for(tgz_target), &lib_tgz);
            tar_link(
                tar,
                "runtime/main.mjs",
                tar::EntryType::Link,
                library_for(tgz_target),
            );
        });
        assert_rejected(
            tgz_target,
            &hard,
            decl_tgz.clone(),
            "links and special files",
        );
        let symbolic = tgz_with(|tar| {
            tar_file(tar, library_for(tgz_target), &lib_tgz);
            tar_link(
                tar,
                "runtime/main.mjs",
                tar::EntryType::Symlink,
                "/etc/passwd",
            );
        });
        assert_rejected(
            tgz_target,
            &symbolic,
            decl_tgz.clone(),
            "links and special files",
        );
    }

    #[test]
    fn strict_archives_accept_directory_entries_and_exact_contents() {
        let zip_target = TARGETS[0];
        let lib_zip = object(zip_target);
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_directory("runtime/", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer
            .start_file("runtime/main.mjs", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(SIDECAR).unwrap();
        writer
            .start_file(
                library_for(zip_target),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.write_all(&lib_zip).unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let agent = tempfile::tempdir().unwrap();
        let paths = install_case(
            zip_target,
            &archive,
            Some(declaration(&[("runtime/main.mjs", SIDECAR)])),
            agent.path(),
        )
        .unwrap();
        assert_eq!(paths.len(), 2);

        let lib_tgz = object(TARGETS[1]);
        let archive = tgz_with(|tar| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "runtime/", std::io::empty())
                .unwrap();
            tar_file(tar, "runtime/main.mjs", SIDECAR);
            tar_file(tar, library_for(TARGETS[1]), &lib_tgz);
        });
        let agent = tempfile::tempdir().unwrap();
        assert_eq!(
            install_case(
                TARGETS[1],
                &archive,
                Some(declaration(&[("runtime/main.mjs", SIDECAR)])),
                agent.path(),
            )
            .unwrap()
            .len(),
            2
        );
    }

    #[test]
    fn expanded_size_budget_counts_bytes_produced_not_declared_sizes() {
        let mut read = 0;
        let error =
            read_entry(&b"0123456789"[..], &mut read, 8, 100, "file too large").unwrap_err();
        assert_eq!(error, "expanded archive exceeds size limit");
        let mut read = 0;
        let error =
            read_entry(&b"0123456789"[..], &mut read, 100, 4, "file too large").unwrap_err();
        assert_eq!(error, "file too large");
        let mut read = 6;
        assert!(read_entry(&b"0123"[..], &mut read, 8, 100, "file too large").is_err());
        let mut read = 0;
        assert_eq!(
            read_entry(&b"0123"[..], &mut read, 8, 100, "file too large").unwrap(),
            b"0123"
        );
        assert_eq!(read, 4);
    }
}
