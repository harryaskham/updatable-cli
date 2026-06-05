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
    /// Optional override for the release **download** host base. Defaults to
    /// `https://github.com`. Set this for GitHub Enterprise, a release mirror, or an
    /// offline/air-gapped host that serves `<base>/<repo>/releases/download/<tag>/<asset>`.
    pub download_base: Option<String>,
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
            download_base: None,
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

    fn download_base(&self) -> String {
        self.download_base
            .clone()
            .unwrap_or_else(|| "https://github.com".to_string())
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
            "{}/{}/releases/download/{}/{}",
            self.config.download_base(),
            self.config.repo_slug,
            latest.tag,
            asset_names.archive
        );
        let checksum_url = format!(
            "{}/{}/releases/download/{}/{}",
            self.config.download_base(),
            self.config.repo_slug,
            latest.tag,
            asset_names.checksum
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
    String::from_utf8(download_bytes(agent, url, user_agent)?)
        .map_err(|err| anyhow!("checksum was not UTF-8: {err}"))
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

    #[test]
    fn promote_next_is_noop_when_nothing_staged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        let updater = Updater::new(config);
        // No `<tool>_next` staged yet: promotion must report that nothing happened
        // and must not create the installed binary.
        let promoted = updater.promote_next().unwrap();
        assert!(promoted.is_none(), "expected no promotion when nothing staged");
        assert!(!updater.config().installed_binary_path().unwrap().exists());
    }

    #[test]
    fn promote_next_moves_staged_binary_and_marks_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        let updater = Updater::new(config);

        // Stage a fake `<tool>_next` payload, deliberately non-executable so the
        // promotion is responsible for chmod 0755.
        let next_path = updater.config().next_binary_path().unwrap();
        fs::create_dir_all(next_path.parent().unwrap()).unwrap();
        fs::write(&next_path, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&next_path).unwrap().permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&next_path, perms).unwrap();

        let installed = updater.promote_next().unwrap().expect("promotion to occur");
        let expected = updater.config().installed_binary_path().unwrap();
        assert_eq!(installed, expected);
        // Staged path consumed, installed path created and executable.
        assert!(!next_path.exists(), "staged binary should be renamed away");
        assert!(expected.exists(), "installed binary should exist after promotion");
        let mode = fs::metadata(&expected).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "installed binary should be chmod 0755");
        assert_eq!(fs::read(&expected).unwrap(), b"#!/bin/sh\nexit 0\n");

        // A second promotion with nothing staged is a clean no-op.
        assert!(updater.promote_next().unwrap().is_none());
    }

    /// Minimal single-shot HTTP server for offline `check_latest` tests.
    ///
    /// Binds an ephemeral localhost port, serves exactly one canned `200 OK`
    /// response body, then returns. No network egress and no extra
    /// dependencies — keeps the merge-queue smoke contract intact.
    fn spawn_one_shot_http(body: String) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let base = format!("http://{addr}");
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Drain the request headers so the client write completes.
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (base, handle)
    }

    #[test]
    fn check_latest_parses_release_and_flags_newer_version() {
        let body = r#"{
            "tag_name": "v9.9.9",
            "html_url": "https://example.invalid/releases/v9.9.9",
            "assets": [
                {"name": "toolx-9.9.9-x86_64-linux.tar.gz"},
                {"name": "toolx-9.9.9-x86_64-linux.sha256"}
            ]
        }"#
        .to_string();
        let (base, handle) = spawn_one_shot_http(body);
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.api_base = Some(base);
        let info = Updater::new(config).check_latest().expect("check_latest succeeds");
        handle.join().unwrap();
        assert_eq!(info.tag, "v9.9.9");
        assert_eq!(info.version, "9.9.9");
        assert_eq!(
            info.html_url.as_deref(),
            Some("https://example.invalid/releases/v9.9.9")
        );
        assert_eq!(
            info.assets,
            vec![
                "toolx-9.9.9-x86_64-linux.tar.gz".to_string(),
                "toolx-9.9.9-x86_64-linux.sha256".to_string(),
            ]
        );
        assert!(info.newer_than_current, "9.9.9 should be newer than 0.1.0");
    }

    #[test]
    fn check_latest_reports_not_newer_for_same_version() {
        let body = r#"{"tag_name": "v0.1.0", "assets": []}"#.to_string();
        let (base, handle) = spawn_one_shot_http(body);
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.api_base = Some(base);
        let info = Updater::new(config).check_latest().expect("check_latest succeeds");
        handle.join().unwrap();
        assert_eq!(info.version, "0.1.0");
        assert!(info.assets.is_empty());
        assert!(info.html_url.is_none());
        assert!(
            !info.newer_than_current,
            "identical version must not be flagged as newer"
        );
    }

    /// Multi-route single-thread HTTP stub: serves `responses.len()` sequential
    /// connections, choosing the body whose key is a substring of the request's
    /// first request line (method + path). Returns the base URL plus the handle.
    /// Standard-library only — no extra dependencies, no external network.
    fn spawn_routed_http(
        responses: Vec<(&'static str, Vec<u8>)>,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let base = format!("http://{addr}");
        let count = responses.len();
        let handle = std::thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let mut buf = [0u8; 2048];
                let read = stream.read(&mut buf).unwrap_or(0);
                let request_line = String::from_utf8_lossy(&buf[..read])
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                let body = responses
                    .iter()
                    .find(|(needle, _)| request_line.contains(needle))
                    .map(|(_, body)| body.clone())
                    .unwrap_or_default();
                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(&body);
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });
        (base, handle)
    }

    #[test]
    fn run_update_stages_and_promotes_release_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let target = release_target().unwrap();
        let version = "9.9.9";
        let archive_name = format!("toolx-{version}-{target}.tar.gz");
        let checksum_name = format!("toolx-{version}-{target}.sha256");
        let binary_in_archive = format!("toolx-{version}-{target}/toolx");
        let payload = b"#!/bin/sh\necho updated-toolx\n";

        // Build a real gzip tarball containing `<tool>-<ver>-<target>/<tool>`.
        let mut tar_buf = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut tar_buf, flate2::Compression::default());
            let mut builder = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, &binary_in_archive, &payload[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let mut hasher = Sha256::new();
        hasher.update(&tar_buf);
        let digest = hex::encode(hasher.finalize());
        let checksum_body = format!("{digest}  {archive_name}\n").into_bytes();

        let api_body = format!(
            r#"{{"tag_name":"v{version}","assets":[{{"name":"{archive_name}"}},{{"name":"{checksum_name}"}}]}}"#
        )
        .into_bytes();

        // Order matters: check_latest hits the API first, then stage_next downloads
        // the archive and the checksum.
        let (base, handle) = spawn_routed_http(vec![
            ("releases/latest", api_body),
            (archive_name.leak(), tar_buf),
            (checksum_name.leak(), checksum_body),
        ]);

        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        config.api_base = Some(base.clone());
        config.download_base = Some(base);
        let outcome = Updater::new(config).run_update().expect("run_update succeeds");
        handle.join().unwrap();

        assert_eq!(outcome.latest_version, version);
        assert!(outcome.staged, "a newer release should be staged");
        assert!(outcome.promoted, "staged binary should be promoted");
        assert!(outcome.note.is_none());

        // The promoted binary lands at `<install>/toolx`, executable, with our payload,
        // and the staged `<install>/toolx_next` is consumed by the promotion.
        let installed = tmp.path().join("toolx");
        assert!(installed.exists(), "installed binary should exist");
        assert!(!tmp.path().join("toolx_next").exists());
        assert_eq!(fs::read(&installed).unwrap(), payload);
        let mode = fs::metadata(&installed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "installed binary should be chmod 0755");
    }

    #[test]
    fn run_update_is_noop_when_latest_is_not_newer() {
        let tmp = tempfile::tempdir().unwrap();
        let api_body = br#"{"tag_name":"v0.1.0","assets":[]}"#.to_vec();
        let (base, handle) = spawn_routed_http(vec![("releases/latest", api_body)]);
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        config.api_base = Some(base.clone());
        config.download_base = Some(base);
        let outcome = Updater::new(config).run_update().expect("run_update succeeds");
        handle.join().unwrap();
        assert!(!outcome.staged, "same version must not stage");
        assert!(!outcome.promoted);
        assert!(outcome.note.is_some(), "no-op should carry an explanatory note");
        assert!(!tmp.path().join("toolx").exists());
        assert!(!tmp.path().join("toolx_next").exists());
    }

    /// Build a gzip tarball containing a single entry at `inner_path`. Returns the
    /// compressed bytes; used by the failure-mode tests to craft good and bad archives.
    fn gzip_tar_with_entry(inner_path: &str, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let enc = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, inner_path, payload).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        out
    }

    #[test]
    fn stage_next_errors_when_release_is_missing_the_expected_asset() {
        // The release advertises no matching archive, so stage_next must bail before
        // any download and must not stage a binary. No HTTP server needed.
        let tmp = tempfile::tempdir().unwrap();
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        let updater = Updater::new(config);
        let latest = LatestReleaseInfo {
            tag: "v9.9.9".to_string(),
            version: "9.9.9".to_string(),
            html_url: None,
            assets: vec!["some-unrelated-asset.txt".to_string()],
            newer_than_current: true,
        };
        let err = updater.stage_next(&latest).unwrap_err();
        assert!(
            err.to_string().contains("has no asset"),
            "unexpected error: {err}"
        );
        assert!(
            !updater.config().next_binary_path().unwrap().exists(),
            "nothing should be staged when the asset is missing"
        );
    }

    #[test]
    fn stage_next_rejects_checksum_mismatch_and_stages_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let target = release_target().unwrap();
        let version = "9.9.9";
        let archive_name = format!("toolx-{version}-{target}.tar.gz");
        let checksum_name = format!("toolx-{version}-{target}.sha256");
        let inner = format!("toolx-{version}-{target}/toolx");
        let tarball = gzip_tar_with_entry(&inner, b"#!/bin/sh\nexit 0\n");
        // Deliberately wrong digest for the archive.
        let bad_checksum = format!("{}  {archive_name}\n", "0".repeat(64)).into_bytes();
        let (base, handle) = spawn_routed_http(vec![
            (archive_name.clone().leak(), tarball),
            (checksum_name.clone().leak(), bad_checksum),
        ]);
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        config.download_base = Some(base);
        let latest = LatestReleaseInfo {
            tag: format!("v{version}"),
            version: version.to_string(),
            html_url: None,
            assets: vec![archive_name, checksum_name],
            newer_than_current: true,
        };
        let err = Updater::new(config).stage_next(&latest).unwrap_err();
        handle.join().unwrap();
        assert!(
            err.to_string().contains("checksum mismatch"),
            "unexpected error: {err}"
        );
        assert!(
            !tmp.path().join("toolx_next").exists(),
            "a checksum mismatch must never stage <tool>_next"
        );
    }

    #[test]
    fn stage_next_errors_when_archive_lacks_expected_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let target = release_target().unwrap();
        let version = "9.9.9";
        let archive_name = format!("toolx-{version}-{target}.tar.gz");
        let checksum_name = format!("toolx-{version}-{target}.sha256");
        // Valid archive + correct checksum, but the inner path is wrong, so the
        // expected `<tool>-<ver>-<target>/<tool>` is absent.
        let tarball = gzip_tar_with_entry("some-other-dir/not-toolx", b"payload\n");
        let mut hasher = Sha256::new();
        hasher.update(&tarball);
        let good_checksum = format!("{}  {archive_name}\n", hex::encode(hasher.finalize()))
            .into_bytes();
        let (base, handle) = spawn_routed_http(vec![
            (archive_name.clone().leak(), tarball),
            (checksum_name.clone().leak(), good_checksum),
        ]);
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        config.download_base = Some(base);
        let latest = LatestReleaseInfo {
            tag: format!("v{version}"),
            version: version.to_string(),
            html_url: None,
            assets: vec![archive_name, checksum_name],
            newer_than_current: true,
        };
        let err = Updater::new(config).stage_next(&latest).unwrap_err();
        handle.join().unwrap();
        assert!(
            err.to_string().contains("did not contain"),
            "unexpected error: {err}"
        );
        assert!(
            !tmp.path().join("toolx_next").exists(),
            "an archive missing the binary must not stage <tool>_next"
        );
    }
}
