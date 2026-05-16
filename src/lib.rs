//! Reusable self-update plumbing for Rust CLIs that ship as binaries via GitHub releases.
//!
//! The host crate provides a [`UpdaterConfig`] describing how to fetch the latest binary, where
//! to stage it, and which tool/version to advertise. From there it gets:
//!
//! - [`Updater::current_status`] for `<tool> status`-style reporting.
//! - [`Updater::check_latest`] for polling the GitHub release latest endpoint.
//! - [`Updater::stage_next`] to download a new binary into `<install_dir>/<tool>_next` after
//!   verifying its sha256, mirroring caco's `caco_next` staging contract.
//! - [`Updater::promote_next`] to atomically rename the staged binary to `<install_dir>/<tool>`.
//! - [`Updater::run_update`] for the high-level `<tool> update` flow.
//! - [`mcp::register_update_tool`] to expose the same surface as an `mcp-cli` tool.
//!
//! The host runtime is also expected to call [`maybe_apply_staged_update`] at process entry
//! so that a freshly staged `<tool>_next` is promoted before the rest of the binary runs.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mcp_cli::{ErrorCategory, StructuredError, ToolRouter};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod mcp;

/// Description of a GitHub-released CLI binary that can self-update itself.
#[derive(Clone)]
pub struct UpdaterConfig {
    /// Tool name as it appears on disk (e.g. `"ring"` for `ring`/`ring_next`).
    pub tool_name: String,
    /// Version of the running binary (`env!("CARGO_PKG_VERSION")` in the host crate).
    pub current_version: String,
    /// GitHub `owner/repo` slug for the release feed.
    pub repo_slug: String,
    /// Release asset naming strategy. Defaults to Tendril-style
    /// `<tool>-<version>-<target>.tar.gz`.
    pub asset_strategy: AssetStrategy,
    /// Optional override for the install directory. Defaults to `$HOME/.local/bin`.
    pub install_dir: Option<PathBuf>,
    /// Optional override for the GitHub API base. Defaults to `https://api.github.com`.
    pub api_base: Option<String>,
    /// Optional User-Agent header. Defaults to `<tool>-updater/<current_version>`.
    pub user_agent: Option<String>,
    /// Optional GitHub token for higher rate limits / private repos.
    pub github_token: Option<String>,
    /// HTTP request timeout. Defaults to 60 seconds.
    pub http_timeout: Option<Duration>,
}

impl std::fmt::Debug for UpdaterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdaterConfig")
            .field("tool_name", &self.tool_name)
            .field("current_version", &self.current_version)
            .field("repo_slug", &self.repo_slug)
            .field("asset_strategy", &self.asset_strategy)
            .field("install_dir", &self.install_dir)
            .finish()
    }
}

impl UpdaterConfig {
    pub fn new(
        tool_name: impl Into<String>,
        current_version: impl Into<String>,
        repo_slug: impl Into<String>,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            current_version: current_version.into(),
            repo_slug: repo_slug.into(),
            asset_strategy: AssetStrategy::default(),
            install_dir: None,
            api_base: None,
            user_agent: None,
            github_token: None,
            http_timeout: None,
        }
    }

    pub fn install_dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.install_dir {
            return Ok(dir.clone());
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is unset; cannot resolve default install dir"))?;
        Ok(home.join(".local").join("bin"))
    }

    pub fn next_binary_path(&self) -> Result<PathBuf> {
        Ok(self
            .install_dir()?
            .join(format!("{}_next", self.tool_name)))
    }

    pub fn installed_binary_path(&self) -> Result<PathBuf> {
        Ok(self.install_dir()?.join(&self.tool_name))
    }

    fn user_agent(&self) -> String {
        self.user_agent.clone().unwrap_or_else(|| {
            format!("{}-updater/{}", self.tool_name, self.current_version)
        })
    }

    fn api_base(&self) -> String {
        self.api_base
            .clone()
            .unwrap_or_else(|| "https://api.github.com".to_string())
    }
}

/// Describes how to derive the release asset name + checksum name for a given release.
#[derive(Clone)]
pub enum AssetStrategy {
    /// `<tool>-<version>-<target>.tar.gz` + `.sha256`, where `<target>` matches Tendril/caco
    /// conventions (e.g. `x86_64-linux`, `aarch64-darwin`). The packed tarball is expected to
    /// contain `<tool>-<version>-<target>/<tool>`.
    TendrilStyle,
    /// Custom strategy: the closure returns `(asset_name, checksum_name, binary_path_in_tar)`.
    #[allow(clippy::type_complexity)]
    Custom(
        std::sync::Arc<dyn Fn(&str, &str, &str) -> Result<AssetNames> + Send + Sync>,
    ),
}

impl std::fmt::Debug for AssetStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TendrilStyle => f.write_str("TendrilStyle"),
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

impl Default for AssetStrategy {
    fn default() -> Self {
        Self::TendrilStyle
    }
}

#[derive(Debug, Clone)]
pub struct AssetNames {
    pub archive: String,
    pub checksum: String,
    pub binary_in_archive: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpdateStatus {
    pub tool: String,
    pub current_version: String,
    pub install_dir: String,
    pub installed_path: String,
    pub installed_exists: bool,
    pub next_path: String,
    pub next_staged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LatestReleaseInfo {
    pub tag: String,
    pub version: String,
    pub html_url: Option<String>,
    pub assets: Vec<String>,
    pub newer_than_current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpdateOutcome {
    pub current_version: String,
    pub latest_version: String,
    pub staged: bool,
    pub promoted: bool,
    pub next_path: String,
    pub installed_path: String,
    pub note: Option<String>,
}

pub struct Updater {
    config: UpdaterConfig,
}

impl Updater {
    pub fn new(config: UpdaterConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &UpdaterConfig {
        &self.config
    }

    pub fn current_status(&self) -> Result<UpdateStatus> {
        let install_dir = self.config.install_dir()?;
        let installed = self.config.installed_binary_path()?;
        let next = self.config.next_binary_path()?;
        Ok(UpdateStatus {
            tool: self.config.tool_name.clone(),
            current_version: self.config.current_version.clone(),
            install_dir: install_dir.display().to_string(),
            installed_path: installed.display().to_string(),
            installed_exists: installed.exists(),
            next_path: next.display().to_string(),
            next_staged: next.exists(),
        })
    }

    pub fn check_latest(&self) -> Result<LatestReleaseInfo> {
        let url = format!(
            "{}/repos/{}/releases/latest",
            self.config.api_base(),
            self.config.repo_slug
        );
        let agent = self.http_agent();
        let mut request = agent.get(&url).set("User-Agent", &self.config.user_agent());
        if let Some(token) = &self.config.github_token {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        let response = request
            .call()
            .with_context(|| format!("GET {url}"))?
            .into_json::<serde_json::Value>()?;
        let tag = response
            .get("tag_name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow!("github release missing tag_name"))?
            .to_string();
        let version = tag.trim_start_matches('v').to_string();
        let html_url = response
            .get("html_url")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        let assets = response
            .get("assets")
            .and_then(|value| value.as_array())
            .map(|array| {
                array
                    .iter()
                    .filter_map(|item| item.get("name").and_then(|value| value.as_str()))
                    .map(|value| value.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let newer_than_current = self.is_newer(&version);
        Ok(LatestReleaseInfo {
            tag,
            version,
            html_url,
            assets,
            newer_than_current,
        })
    }

    fn is_newer(&self, latest: &str) -> bool {
        match (
            semver::Version::parse(latest),
            semver::Version::parse(&self.config.current_version),
        ) {
            (Ok(latest), Ok(current)) => latest > current,
            _ => latest != self.config.current_version,
        }
    }

    pub fn stage_next(&self, latest: &LatestReleaseInfo) -> Result<PathBuf> {
        let install_dir = self.config.install_dir()?;
        fs::create_dir_all(&install_dir)
            .with_context(|| format!("create {}", install_dir.display()))?;
        let target = release_target()?;
        let asset_names = match &self.config.asset_strategy {
            AssetStrategy::TendrilStyle => AssetNames {
                archive: format!("{}-{}-{}.tar.gz", self.config.tool_name, latest.version, target),
                checksum: format!("{}-{}-{}.sha256", self.config.tool_name, latest.version, target),
                binary_in_archive: format!(
                    "{}-{}-{}/{}",
                    self.config.tool_name, latest.version, target, self.config.tool_name
                ),
            },
            AssetStrategy::Custom(strategy) => {
                strategy(&self.config.tool_name, &latest.version, &target)?
            }
        };
        if !latest.assets.iter().any(|name| name == &asset_names.archive) {
            bail!(
                "release {} has no asset {} (available: {:?})",
                latest.tag,
                asset_names.archive,
                latest.assets
            );
        }
        let archive_url = format!(
            "https://github.com/{}/releases/download/{}/{}",
            self.config.repo_slug, latest.tag, asset_names.archive
        );
        let checksum_url = format!(
            "https://github.com/{}/releases/download/{}/{}",
            self.config.repo_slug, latest.tag, asset_names.checksum
        );
        let agent = self.http_agent();
        let archive_bytes = download_bytes(&agent, &archive_url, &self.config.user_agent())?;
        let checksum_text = download_text(&agent, &checksum_url, &self.config.user_agent())?;
        verify_sha256(&archive_bytes, &checksum_text, &asset_names.archive)?;

        let tmp = tempfile::tempdir().context("create tempdir for staging release tarball")?;
        let tar_gz = flate2::read::GzDecoder::new(archive_bytes.as_slice());
        let mut archive = tar::Archive::new(tar_gz);
        archive
            .unpack(tmp.path())
            .with_context(|| format!("unpack {}", asset_names.archive))?;
        let binary_path = tmp.path().join(&asset_names.binary_in_archive);
        if !binary_path.exists() {
            bail!(
                "release archive {} did not contain {}",
                asset_names.archive,
                asset_names.binary_in_archive
            );
        }
        let next_path = self.config.next_binary_path()?;
        if let Some(parent) = next_path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&next_path, &binary_path)?;
        set_executable(&next_path)?;
        Ok(next_path)
    }

    /// Promote `<install>/<tool>_next` to `<install>/<tool>`. Returns the installed path
    /// when a promotion happened, `None` when there was nothing staged.
    pub fn promote_next(&self) -> Result<Option<PathBuf>> {
        let next = self.config.next_binary_path()?;
        if !next.exists() {
            return Ok(None);
        }
        let installed = self.config.installed_binary_path()?;
        if let Some(parent) = installed.parent() {
            fs::create_dir_all(parent)?;
        }
        set_executable(&next)?;
        fs::rename(&next, &installed)
            .with_context(|| format!("promote {} -> {}", next.display(), installed.display()))?;
        set_executable(&installed)?;
        Ok(Some(installed))
    }

    pub fn run_update(&self) -> Result<UpdateOutcome> {
        let latest = self.check_latest()?;
        let installed_path = self.config.installed_binary_path()?;
        let next_path = self.config.next_binary_path()?;
        if !latest.newer_than_current {
            return Ok(UpdateOutcome {
                current_version: self.config.current_version.clone(),
                latest_version: latest.version.clone(),
                staged: false,
                promoted: false,
                next_path: next_path.display().to_string(),
                installed_path: installed_path.display().to_string(),
                note: Some(format!(
                    "no update needed; latest is {} and current is {}",
                    latest.version, self.config.current_version
                )),
            });
        }
        self.stage_next(&latest)?;
        let promoted = self.promote_next()?;
        Ok(UpdateOutcome {
            current_version: self.config.current_version.clone(),
            latest_version: latest.version.clone(),
            staged: true,
            promoted: promoted.is_some(),
            next_path: next_path.display().to_string(),
            installed_path: installed_path.display().to_string(),
            note: None,
        })
    }

    fn http_agent(&self) -> ureq::Agent {
        let timeout = self.config.http_timeout.unwrap_or(Duration::from_secs(60));
        ureq::AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .build()
    }
}

fn download_bytes(agent: &ureq::Agent, url: &str, user_agent: &str) -> Result<Vec<u8>> {
    let response = agent
        .get(url)
        .set("User-Agent", user_agent)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut buf = Vec::new();
    response.into_reader().read_to_end(&mut buf)?;
    Ok(buf)
}

fn download_text(agent: &ureq::Agent, url: &str, user_agent: &str) -> Result<String> {
    Ok(String::from_utf8(download_bytes(agent, url, user_agent)?)
        .map_err(|err| anyhow!("checksum was not UTF-8: {err}"))?)
}

fn verify_sha256(bytes: &[u8], checksum_text: &str, asset_name: &str) -> Result<()> {
    let expected = checksum_text
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("checksum file for {asset_name} was empty"))?
        .to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = hex::encode(hasher.finalize());
    if expected != actual {
        bail!(
            "checksum mismatch for {asset_name}: expected {expected}, got {actual}"
        );
    }
    Ok(())
}

fn atomic_write(destination: &Path, source: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("destination {} has no parent", destination.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    let mut src = fs::File::open(source)
        .with_context(|| format!("open source {}", source.display()))?;
    std::io::copy(&mut src, tmp.as_file_mut())?;
    tmp.flush()?;
    tmp.persist(destination)
        .map_err(|err| anyhow!("persist {} failed: {err}", destination.display()))?;
    Ok(())
}

fn set_executable(path: &Path) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

/// Look up the running binary, promote any staged `<tool>_next` next to it, and re-exec.
///
/// This mirrors caco's startup hook. Hosts should call this at the very top of `main`.
/// The function is intentionally best-effort: failures only print warnings and return
/// `Ok(())` so the rest of the CLI still starts.
pub fn maybe_apply_staged_update(tool_name: &str) -> Result<()> {
    let current = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("warning: {tool_name} could not resolve current_exe: {error}");
            return Ok(());
        }
    };
    let staged_name = format!("{tool_name}_next");
    let staged = current.with_file_name(&staged_name);
    if !staged.exists() {
        return Ok(());
    }
    if let Err(error) = set_executable(&staged) {
        eprintln!(
            "warning: staged {tool_name} update {} is not promotable: chmod 0755 failed: {error}",
            staged.display()
        );
        return Ok(());
    }
    if let Err(error) = fs::rename(&staged, &current) {
        eprintln!(
            "warning: failed to promote staged {tool_name} update {}: {error}",
            staged.display()
        );
        return Ok(());
    }
    if let Err(error) = set_executable(&current) {
        eprintln!(
            "warning: promoted {tool_name} update {} may not be executable: chmod 0755 failed: {error}",
            current.display()
        );
    }
    eprintln!("Applied staged {tool_name} update");
    let exe = current.into_os_string();
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let err = exec_replace(&exe, &args);
    eprintln!("warning: failed to re-exec after staged {tool_name} update: {err}");
    Ok(())
}

#[cfg(unix)]
fn exec_replace(program: &std::ffi::OsStr, args: &[std::ffi::OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    cmd.exec()
}

#[cfg(not(unix))]
fn exec_replace(program: &std::ffi::OsStr, args: &[std::ffi::OsString]) -> std::io::Error {
    match std::process::Command::new(program).args(args).status() {
        Ok(status) => {
            std::process::exit(status.code().unwrap_or(0));
        }
        Err(err) => err,
    }
}

/// Caco/Tendril-style platform suffix: `<arch>-<os>` (`x86_64-linux`, `aarch64-darwin`, ...).
pub fn release_target() -> Result<String> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => bail!("unsupported updater OS {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "arm64" => "aarch64",
        other => bail!("unsupported updater arch {other}"),
    };
    Ok(format!("{arch}-{os}"))
}

/// MCP tool surface.
pub fn register_update_tool<C: Send + Sync + 'static>(
    router: &mut ToolRouter<C>,
    config_builder: impl Fn(&C) -> UpdaterConfig + Send + Sync + 'static,
) {
    let config_builder = std::sync::Arc::new(config_builder);
    let status_builder = config_builder.clone();
    router.add_typed_tool(
        "self_update_status",
        "Report the current self-update status of this CLI.",
        move |context: &C, _input: EmptyArgs| {
            let config = status_builder(context);
            Updater::new(config)
                .current_status()
                .map_err(UpdateError::from)
        },
    );
    let check_builder = config_builder.clone();
    router.add_typed_tool(
        "self_update_check",
        "Check the GitHub releases feed for a newer version of this CLI.",
        move |context: &C, _input: EmptyArgs| {
            let config = check_builder(context);
            Updater::new(config)
                .check_latest()
                .map_err(UpdateError::from)
        },
    );
    let update_builder = config_builder;
    router.add_typed_tool(
        "self_update_run",
        "Stage the latest release as <tool>_next and atomically promote it.",
        move |context: &C, _input: EmptyArgs| {
            let config = update_builder(context);
            Updater::new(config)
                .run_update()
                .map_err(UpdateError::from)
        },
    );
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[derive(Debug, Clone)]
pub struct UpdateError(pub String);

impl From<anyhow::Error> for UpdateError {
    fn from(value: anyhow::Error) -> Self {
        Self(format!("{value:#}"))
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UpdateError {}

impl StructuredError for UpdateError {
    fn category(&self) -> ErrorCategory {
        ErrorCategory::ExecutionFailure
    }
    fn code(&self) -> String {
        "self_update_failed".to_string()
    }
    fn message(&self) -> String {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_target_returns_canonical_string() {
        let value = release_target().unwrap();
        assert!(value.contains('-'), "expected arch-os, got {value}");
    }

    #[test]
    fn sha256_verify_accepts_matching_digest() {
        let bytes = b"hello world";
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hex::encode(hasher.finalize());
        verify_sha256(bytes, &format!("{digest}  asset.tar.gz"), "asset.tar.gz").unwrap();
    }

    #[test]
    fn sha256_verify_rejects_bad_digest() {
        let err = verify_sha256(b"hi", "0000  asset.tar.gz", "asset.tar.gz").unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn current_status_reports_install_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        let status = Updater::new(config).current_status().unwrap();
        assert_eq!(status.tool, "toolx");
        assert!(status.installed_path.ends_with("toolx"));
        assert!(status.next_path.ends_with("toolx_next"));
        assert!(!status.installed_exists);
        assert!(!status.next_staged);
    }
}
