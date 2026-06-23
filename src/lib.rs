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
//!
//! ## Platform support
//!
//! This crate targets **Unix only (Linux and macOS)**. It depends on `std::os::unix`
//! APIs for executable-bit handling and `exec`-style re-spawning, and
//! [`release_target`] only resolves the `x86_64`/`aarch64` linux/darwin asset targets.
//! Windows is not supported.
//!
//! ## Example
//!
//! Report install status without any network or filesystem writes (`current_status`
//! only computes paths and stats them):
//!
//! ```
//! use std::path::PathBuf;
//! use updatable_cli::{Updater, UpdaterConfig};
//!
//! let mut config = UpdaterConfig::new("mytool", "1.2.3", "octocat/mytool");
//! // Point at any directory so the example never touches a real install path.
//! config.install_dir = Some(PathBuf::from("/tmp/updatable-cli-doc-example"));
//!
//! let status = Updater::new(config).current_status()?;
//! assert_eq!(status.tool, "mytool");
//! assert_eq!(status.current_version, "1.2.3");
//! assert!(status.installed_path.ends_with("mytool"));
//! assert!(status.next_path.ends_with("mytool_next"));
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! ## MCP tools
//!
//! Expose the same surface as `self_update_status` / `self_update_check` /
//! `self_update_run` on an existing `mcp-cli` router, where `Ctx` is your host context:
//!
//! ```
//! use mcp_cli::ToolRouter;
//! use updatable_cli::{UpdaterConfig, register_update_tool};
//!
//! struct Ctx;
//!
//! let mut router: ToolRouter<Ctx> = ToolRouter::new();
//! register_update_tool(&mut router, |_ctx: &Ctx| {
//!     UpdaterConfig::new("mytool", "1.2.3", "octocat/mytool")
//! });
//!
//! let names: Vec<String> = router.tool_metadata().into_iter().map(|m| m.name).collect();
//! assert!(names.iter().any(|n| n == "self_update_status"));
//! assert!(names.iter().any(|n| n == "self_update_check"));
//! assert!(names.iter().any(|n| n == "self_update_run"));
//! ```
#![warn(missing_docs)]

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
    /// Optional GitHub token for higher rate limits / private repos. Sent as
    /// `Authorization: Bearer <token>` on both the release-metadata request and the
    /// asset/checksum downloads. Takes precedence over `gh_account`/`gh_token_fallback`.
    pub github_token: Option<String>,
    /// Optional GitHub account/username to source a token from the local `gh` CLI when
    /// `github_token` is unset. When `Some`, the updater runs `gh auth token --user
    /// <account>` to obtain a token (useful for selecting one of several logged-in `gh`
    /// accounts, e.g. to reach a private release repo).
    pub gh_account: Option<String>,
    /// When `true` and `github_token` is unset, fall back to `gh auth token` (honoring
    /// `gh_account` if set) to source a token from the local `gh` CLI. Defaults to
    /// `false` so public-repo callers never shell out to `gh`.
    pub gh_token_fallback: bool,
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
    /// Create a config for `tool_name` (the on-disk binary name), the running
    /// `current_version`, and the GitHub `owner/repo` release slug, with all
    /// optional overrides left at their defaults.
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
            gh_account: None,
            gh_token_fallback: false,
            http_timeout: None,
        }
    }

    /// Resolve the install directory: the explicit `install_dir` override when
    /// set, otherwise the default `$HOME/.local/bin`.
    pub fn install_dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.install_dir {
            return Ok(dir.clone());
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is unset; cannot resolve default install dir"))?;
        Ok(home.join(".local").join("bin"))
    }

    /// Path to the staged next binary, `<install_dir>/<tool>_next`.
    pub fn next_binary_path(&self) -> Result<PathBuf> {
        Ok(self.install_dir()?.join(format!("{}_next", self.tool_name)))
    }

    /// Path to the installed binary, `<install_dir>/<tool>`.
    pub fn installed_binary_path(&self) -> Result<PathBuf> {
        Ok(self.install_dir()?.join(&self.tool_name))
    }

    fn user_agent(&self) -> String {
        self.user_agent
            .clone()
            .unwrap_or_else(|| format!("{}-updater/{}", self.tool_name, self.current_version))
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

    /// Set an explicit GitHub token (chainable). Takes precedence over any `gh` fallback.
    pub fn with_github_token(mut self, token: impl Into<String>) -> Self {
        self.github_token = Some(token.into());
        self
    }

    /// Select a `gh` account/username to source a token from when no explicit token is
    /// set (chainable). Implies the `gh auth token --user <account>` fallback.
    pub fn with_gh_account(mut self, account: impl Into<String>) -> Self {
        self.gh_account = Some(account.into());
        self
    }

    /// Enable/disable the `gh auth token` fallback for when no explicit token is set
    /// (chainable). Honors [`gh_account`](Self::gh_account) when set.
    pub fn with_gh_token_fallback(mut self, enabled: bool) -> Self {
        self.gh_token_fallback = enabled;
        self
    }

    /// Resolve the bearer token to use for GitHub requests.
    ///
    /// Resolution order:
    /// 1. An explicit, non-empty [`github_token`](Self::github_token).
    /// 2. Otherwise, when [`gh_account`](Self::gh_account) is set or
    ///    [`gh_token_fallback`](Self::gh_token_fallback) is `true`, the output of
    ///    `gh auth token [--user <gh_account>]`.
    /// 3. Otherwise `None`.
    ///
    /// `gh` is only invoked for case 2, so default public-repo callers never shell out.
    pub fn resolved_token(&self) -> Option<String> {
        if let Some(token) = &self.github_token {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if self.gh_account.is_some() || self.gh_token_fallback {
            return gh_auth_token(self.gh_account.as_deref());
        }
        None
    }
}

/// Describes how to derive the release asset name + checksum name for a given release.
#[derive(Clone, Default)]
pub enum AssetStrategy {
    /// `<tool>-<version>-<target>.tar.gz` + `.sha256`, where `<target>` matches Tendril/caco
    /// conventions (e.g. `x86_64-linux`, `aarch64-darwin`). The packed tarball is expected to
    /// contain `<tool>-<version>-<target>/<tool>`.
    #[default]
    TendrilStyle,
    /// Custom strategy: the closure returns `(asset_name, checksum_name, binary_path_in_tar)`.
    #[allow(clippy::type_complexity)]
    Custom(std::sync::Arc<dyn Fn(&str, &str, &str) -> Result<AssetNames> + Send + Sync>),
}

impl std::fmt::Debug for AssetStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TendrilStyle => f.write_str("TendrilStyle"),
            Self::Custom(_) => f.write_str("Custom(<fn>)"),
        }
    }
}

/// Resolved release asset names for a given tool/version/target.
#[derive(Debug, Clone)]
pub struct AssetNames {
    /// File name of the release archive (e.g. `tool-1.2.3-x86_64-linux.tar.gz`).
    pub archive: String,
    /// File name of the sha256 checksum asset for `archive`.
    pub checksum: String,
    /// Path of the binary inside the unpacked archive.
    pub binary_in_archive: String,
}

/// Snapshot of the installed/staged state for the host CLI (`<tool> status`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpdateStatus {
    /// Tool name as it appears on disk.
    pub tool: String,
    /// Version of the running binary.
    pub current_version: String,
    /// Resolved install directory.
    pub install_dir: String,
    /// Resolved path of the installed binary.
    pub installed_path: String,
    /// Whether the installed binary currently exists on disk.
    pub installed_exists: bool,
    /// Resolved path of the staged `<tool>_next` binary.
    pub next_path: String,
    /// Whether a staged `<tool>_next` binary currently exists on disk.
    pub next_staged: bool,
}

/// Parsed metadata for the latest GitHub release.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LatestReleaseInfo {
    /// Raw release tag (e.g. `v1.2.3`).
    pub tag: String,
    /// Tag with a leading `v` stripped (e.g. `1.2.3`).
    pub version: String,
    /// Release HTML URL, when present.
    pub html_url: Option<String>,
    /// Names of the assets attached to the release.
    pub assets: Vec<String>,
    /// Whether `version` is newer than the configured current version.
    pub newer_than_current: bool,
}

/// Outcome of a high-level `run_update` call (the `<tool> update` flow).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpdateOutcome {
    /// Version of the running binary at the time of the update.
    pub current_version: String,
    /// Latest version observed on GitHub.
    pub latest_version: String,
    /// Whether a new binary was staged this run.
    pub staged: bool,
    /// Whether the staged binary was promoted into place this run.
    pub promoted: bool,
    /// Resolved path of the staged `<tool>_next` binary.
    pub next_path: String,
    /// Resolved path of the installed binary.
    pub installed_path: String,
    /// Optional human-readable note (e.g. "no update needed").
    pub note: Option<String>,
}

/// Drives the self-update flow for a single configured tool.
pub struct Updater {
    config: UpdaterConfig,
}

impl Updater {
    /// Create an updater from a fully-built config.
    pub fn new(config: UpdaterConfig) -> Self {
        Self { config }
    }

    /// Borrow the underlying config.
    pub fn config(&self) -> &UpdaterConfig {
        &self.config
    }

    /// Report install/staging status without any network access.
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

    /// Query the GitHub "latest release" endpoint and report its tag, assets,
    /// and whether it is newer than the configured current version.
    pub fn check_latest(&self) -> Result<LatestReleaseInfo> {
        let url = format!(
            "{}/repos/{}/releases/latest",
            self.config.api_base(),
            self.config.repo_slug
        );
        let agent = self.http_agent();
        let mut request = agent.get(&url).set("User-Agent", &self.config.user_agent());
        if let Some(token) = self.config.resolved_token() {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(404, _)) => {
                bail!(
                    "no published GitHub releases for {} yet (GET {url} returned 404)",
                    self.config.repo_slug
                );
            }
            Err(ureq::Error::Status(code, _)) if code == 403 || code == 429 => {
                bail!(
                    "GitHub API request was rate-limited or forbidden (HTTP {code}) for {url}; \
                     set a token via UpdaterConfig::with_github_token or with_gh_token_fallback to raise the limit"
                );
            }
            Err(ureq::Error::Status(code, _)) => {
                bail!("GET {url} returned HTTP {code}");
            }
            Err(err) => return Err(anyhow!("GET {url}: {err}")),
        }
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

    /// Download the release archive for `latest`, verify its sha256, and write
    /// the binary to `<install_dir>/<tool>_next`. Returns the staged path.
    pub fn stage_next(&self, latest: &LatestReleaseInfo) -> Result<PathBuf> {
        let install_dir = self.config.install_dir()?;
        fs::create_dir_all(&install_dir)
            .with_context(|| format!("create {}", install_dir.display()))?;
        let target = release_target()?;
        let asset_names = match &self.config.asset_strategy {
            AssetStrategy::TendrilStyle => AssetNames {
                archive: format!(
                    "{}-{}-{}.tar.gz",
                    self.config.tool_name, latest.version, target
                ),
                checksum: format!(
                    "{}-{}-{}.sha256",
                    self.config.tool_name, latest.version, target
                ),
                binary_in_archive: format!(
                    "{}-{}-{}/{}",
                    self.config.tool_name, latest.version, target, self.config.tool_name
                ),
            },
            AssetStrategy::Custom(strategy) => {
                strategy(&self.config.tool_name, &latest.version, &target)?
            }
        };
        if !latest
            .assets
            .iter()
            .any(|name| name == &asset_names.archive)
        {
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
        let token = self.config.resolved_token();
        let timeout = self.config.http_timeout.unwrap_or(Duration::from_secs(60));
        let archive_bytes = download_bytes(
            &archive_url,
            &self.config.user_agent(),
            token.as_deref(),
            timeout,
        )?;
        let checksum_text = download_text(
            &checksum_url,
            &self.config.user_agent(),
            token.as_deref(),
            timeout,
        )?;
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

    /// High-level `<tool> update`: check the latest release and, when newer,
    /// stage and promote it. A no-op when already up to date.
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

/// Download `url` following redirects manually so that an `Authorization` bearer is
/// only ever sent to the original host. GitHub release-asset downloads 302 to a signed
/// object-store URL that rejects (or does not need) the GitHub credential, so credentials
/// MUST NOT be forwarded across a host change.
fn download_bytes(
    url: &str,
    user_agent: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    // redirects(0): we follow them ourselves to control credential forwarding.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .redirects(0)
        .build();
    let origin_host = url_host(url);
    let mut current = url.to_string();
    let mut send_auth = token.is_some();
    for _ in 0..10 {
        let mut request = agent.get(&current).set("User-Agent", user_agent);
        if send_auth {
            if let Some(token) = token {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(code, _)) => {
                bail!("GET {current} returned HTTP {code}");
            }
            Err(err) => return Err(anyhow!("GET {current}: {err}")),
        };
        let status = response.status();
        if (300..400).contains(&status) {
            let location = response
                .header("location")
                .ok_or_else(|| anyhow!("HTTP {status} redirect without Location for {current}"))?
                .to_string();
            let next = resolve_location(&current, &location)?;
            // Never forward the bearer to a different host (e.g. GitHub -> signed S3 URL).
            if url_host(&next) != origin_host {
                send_auth = false;
            }
            current = next;
            continue;
        }
        let mut buf = Vec::new();
        response.into_reader().read_to_end(&mut buf)?;
        return Ok(buf);
    }
    bail!("too many redirects while fetching {url}")
}

fn download_text(
    url: &str,
    user_agent: &str,
    token: Option<&str>,
    timeout: Duration,
) -> Result<String> {
    String::from_utf8(download_bytes(url, user_agent, token, timeout)?)
        .map_err(|err| anyhow!("checksum was not UTF-8: {err}"))
}

/// Lowercased `host[:port]` of an absolute URL, or `""` when it cannot be parsed.
fn url_host(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Resolve a `Location` header against the request URL. Handles absolute URLs,
/// protocol-relative (`//host/...`), absolute-path (`/...`), and simple relative paths.
fn resolve_location(base: &str, location: &str) -> Result<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_string());
    }
    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| anyhow!("cannot resolve redirect against non-absolute base {base}"))?;
    if let Some(after) = location.strip_prefix("//") {
        return Ok(format!("{scheme}://{after}"));
    }
    let host = rest.split('/').next().unwrap_or(rest);
    if let Some(path) = location.strip_prefix('/') {
        return Ok(format!("{scheme}://{host}/{path}"));
    }
    // Relative path: replace the last segment of the base path.
    let base_no_query = base.split('?').next().unwrap_or(base);
    let parent = base_no_query
        .rsplit_once('/')
        .map(|(left, _)| left)
        .unwrap_or(base_no_query);
    Ok(format!("{parent}/{location}"))
}

/// Best-effort: ask the locally-installed `gh` CLI for an auth token.
///
/// Runs `gh auth token` (optionally scoped to `--user <account>`), returning the trimmed
/// token on success. Returns `None` when `gh` is missing, the user is not authenticated,
/// or the account is unknown — callers treat that as "no token available".
pub fn gh_auth_token(account: Option<&str>) -> Option<String> {
    let mut command = std::process::Command::new("gh");
    command.arg("auth").arg("token");
    if let Some(account) = account {
        if !account.is_empty() {
            command.arg("--user").arg(account);
        }
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?;
    let token = token.trim().to_string();
    if token.is_empty() { None } else { Some(token) }
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
        bail!("checksum mismatch for {asset_name}: expected {expected}, got {actual}");
    }
    Ok(())
}

fn atomic_write(destination: &Path, source: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("destination {} has no parent", destination.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    let mut src =
        fs::File::open(source).with_context(|| format!("open source {}", source.display()))?;
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

// This crate is Unix-only (Linux/macOS): it unconditionally relies on
// `std::os::unix` APIs (`PermissionsExt::set_mode`, `CommandExt::exec`) and
// `release_target` only maps the linux/darwin asset targets, so there is no
// non-Unix build to fall back to.
fn exec_replace(program: &std::ffi::OsStr, args: &[std::ffi::OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    cmd.exec()
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
            Updater::new(config).run_update().map_err(UpdateError::from)
        },
    );
}

/// Empty argument type for the parameterless MCP update tools.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

/// Error wrapper returned by the MCP update tools (a flattened message string).
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
        assert!(
            promoted.is_none(),
            "expected no promotion when nothing staged"
        );
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
        assert!(
            expected.exists(),
            "installed binary should exist after promotion"
        );
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
        let info = Updater::new(config)
            .check_latest()
            .expect("check_latest succeeds");
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
        let info = Updater::new(config)
            .check_latest()
            .expect("check_latest succeeds");
        handle.join().unwrap();
        assert_eq!(info.version, "0.1.0");
        assert!(info.assets.is_empty());
        assert!(info.html_url.is_none());
        assert!(
            !info.newer_than_current,
            "identical version must not be flagged as newer"
        );
    }

    /// Like `spawn_one_shot_http` but responds with an arbitrary status line, for
    /// exercising check_latest error paths (404 no-releases, 403 rate-limit, ...).
    fn spawn_one_shot_http_status(
        status_line: &'static str,
        body: String,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let base = format!("http://{addr}");
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
    fn check_latest_reports_friendly_error_when_no_releases() {
        let (base, handle) =
            spawn_one_shot_http_status("404 Not Found", r#"{"message":"Not Found"}"#.to_string());
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.api_base = Some(base);
        let err = Updater::new(config)
            .check_latest()
            .expect_err("a 404 from the releases endpoint must surface as an error");
        handle.join().unwrap();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no published GitHub releases for octocat/example"),
            "expected a friendly no-releases message, got: {msg}"
        );
    }

    #[test]
    fn check_latest_reports_rate_limit_hint_on_403() {
        let (base, handle) = spawn_one_shot_http_status(
            "403 Forbidden",
            r#"{"message":"API rate limit exceeded"}"#.to_string(),
        );
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.api_base = Some(base);
        let err = Updater::new(config)
            .check_latest()
            .expect_err("a 403 from the releases endpoint must surface as an error");
        handle.join().unwrap();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("rate-limited or forbidden") && msg.contains("with_github_token"),
            "expected a rate-limit/token hint, got: {msg}"
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
        let outcome = Updater::new(config)
            .run_update()
            .expect("run_update succeeds");
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
        let outcome = Updater::new(config)
            .run_update()
            .expect("run_update succeeds");
        handle.join().unwrap();
        assert!(!outcome.staged, "same version must not stage");
        assert!(!outcome.promoted);
        assert!(
            outcome.note.is_some(),
            "no-op should carry an explanatory note"
        );
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
        builder
            .append_data(&mut header, inner_path, payload)
            .unwrap();
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
        let good_checksum =
            format!("{}  {archive_name}\n", hex::encode(hasher.finalize())).into_bytes();
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

    #[test]
    fn is_newer_handles_semver_ordering_and_non_semver_fallback() {
        // Helper: an Updater whose running version is `current`.
        let updater_at =
            |current: &str| Updater::new(UpdaterConfig::new("toolx", current, "octocat/example"));

        let u = updater_at("1.2.3");
        // Strictly greater across patch/minor/major is newer.
        assert!(u.is_newer("1.2.4"));
        assert!(u.is_newer("1.3.0"));
        assert!(u.is_newer("2.0.0"));
        // Equal or lower is not newer.
        assert!(!u.is_newer("1.2.3"));
        assert!(!u.is_newer("1.2.2"));
        assert!(!u.is_newer("1.1.9"));
        // Semver pre-release rules: a stable release outranks its pre-release, and a
        // pre-release is older than the corresponding stable current.
        assert!(updater_at("1.0.0-rc.1").is_newer("1.0.0"));
        assert!(!updater_at("1.0.0").is_newer("1.0.0-rc.1"));

        // Non-semver fallback: when either side does not parse, "newer" degrades to a
        // plain string inequality (any different tag is treated as an update).
        let n = updater_at("0.1.0");
        assert!(
            n.is_newer("nightly"),
            "different non-semver tag => treated as newer"
        );
        assert!(
            !updater_at("nightly").is_newer("nightly"),
            "identical non-semver tag => not newer"
        );
        assert!(
            updater_at("nightly").is_newer("rolling"),
            "two distinct non-semver tags => treated as newer"
        );
    }

    #[test]
    fn register_update_tool_registers_the_three_self_update_tools() {
        use mcp_cli::ToolRouter;

        // A trivial host context; the builder closure ignores it.
        struct Ctx;
        let mut router: ToolRouter<Ctx> = ToolRouter::new();
        register_update_tool(&mut router, |_ctx: &Ctx| {
            UpdaterConfig::new("toolx", "0.1.0", "octocat/example")
        });

        let names: Vec<String> = router
            .tool_metadata()
            .into_iter()
            .map(|meta| meta.name)
            .collect();
        assert_eq!(
            names.len(),
            3,
            "expected exactly three tools, got {names:?}"
        );
        for expected in ["self_update_status", "self_update_check", "self_update_run"] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing tool {expected}; registered: {names:?}"
            );
        }
        // Every registered tool should carry a non-empty human description.
        for meta in router.tool_metadata() {
            assert!(
                !meta.description.trim().is_empty(),
                "tool {} has an empty description",
                meta.name
            );
        }
    }

    #[test]
    fn maybe_apply_staged_update_is_a_clean_noop_when_nothing_is_staged() {
        // In the test binary, current_exe() is the test runner and there is no
        // `<runner>_next` sibling, so the startup hook must return Ok(()) without
        // erroring or touching the filesystem. This guards the early-return guard
        // that every normal program launch relies on. Use a unique, unlikely tool
        // name so we never collide with a real staged sibling.
        let result = maybe_apply_staged_update("updatable_cli_unlikely_tool_name_xyz");
        assert!(
            result.is_ok(),
            "no-op startup hook should not error: {result:?}"
        );
    }

    /// Reply shape for the recording HTTP stub.
    #[derive(Clone)]
    enum MockReply {
        Body(Vec<u8>),
        Redirect(String),
    }

    type AuthLog = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    /// Like `spawn_routed_http`, but also records each request's first line and its
    /// `Authorization` header (if any), and can answer with a 302 redirect. Used to
    /// assert credential forwarding behaviour without external network.
    fn spawn_recording_http(
        routes: Vec<(&'static str, MockReply)>,
    ) -> (String, AuthLog, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let base = format!("http://{addr}");
        let log: AuthLog = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_thread = log.clone();
        let count = routes.len();
        let handle = std::thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let mut buf = [0u8; 4096];
                let read = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let request_line = request.lines().next().unwrap_or("").to_string();
                let authorization = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                    .and_then(|line| line.split_once(':'))
                    .map(|(_, value)| value.trim().to_string());
                log_thread
                    .lock()
                    .unwrap()
                    .push((request_line.clone(), authorization));
                let reply = routes
                    .iter()
                    .find(|(needle, _)| request_line.contains(needle))
                    .map(|(_, reply)| reply.clone())
                    .unwrap_or(MockReply::Body(Vec::new()));
                let response = match reply {
                    MockReply::Body(body) => {
                        let mut bytes = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        bytes.extend_from_slice(&body);
                        bytes
                    }
                    MockReply::Redirect(location) => format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .into_bytes(),
                };
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });
        (base, log, handle)
    }

    #[test]
    fn resolved_token_prefers_explicit_token_and_trims_it() {
        // An explicit token wins and is whitespace-trimmed; `gh` is never consulted
        // (no gh_account / gh_token_fallback opt-in set).
        let config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example")
            .with_github_token("  tok-123  ");
        assert_eq!(config.resolved_token().as_deref(), Some("tok-123"));

        // No token and no gh opt-in => None, without shelling out to gh.
        let bare = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        assert_eq!(bare.resolved_token(), None);

        // An empty/whitespace token is treated as absent (and still no gh opt-in).
        let blank =
            UpdaterConfig::new("toolx", "0.1.0", "octocat/example").with_github_token("   ");
        assert_eq!(blank.resolved_token(), None);
    }

    #[test]
    fn download_bytes_sends_bearer_to_origin_and_omits_it_without_token() {
        // With a token: Authorization: Bearer <token> is sent to the origin host.
        let (base, log, handle) =
            spawn_recording_http(vec![("asset", MockReply::Body(b"payload".to_vec()))]);
        let url = format!("{base}/asset");
        let bytes =
            download_bytes(&url, "ua/1.0", Some("secret-xyz"), Duration::from_secs(5)).unwrap();
        handle.join().unwrap();
        assert_eq!(bytes, b"payload");
        let recorded = log.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].1.as_deref(), Some("Bearer secret-xyz"));

        // Without a token: no Authorization header is sent at all.
        let (base, log, handle) =
            spawn_recording_http(vec![("asset", MockReply::Body(b"payload".to_vec()))]);
        let url = format!("{base}/asset");
        let bytes = download_bytes(&url, "ua/1.0", None, Duration::from_secs(5)).unwrap();
        handle.join().unwrap();
        assert_eq!(bytes, b"payload");
        assert_eq!(log.lock().unwrap()[0].1, None);
    }

    #[test]
    fn download_bytes_strips_authorization_on_cross_host_redirect() {
        // The signed-object host (server B) must NOT receive the GitHub bearer.
        let (base_b, log_b, handle_b) =
            spawn_recording_http(vec![("signed", MockReply::Body(b"object-bytes".to_vec()))]);
        let (base_a, log_a, handle_a) = spawn_recording_http(vec![(
            "archive",
            MockReply::Redirect(format!("{base_b}/signed")),
        )]);

        let url = format!("{base_a}/archive");
        let bytes =
            download_bytes(&url, "ua/1.0", Some("secret-xyz"), Duration::from_secs(5)).unwrap();
        handle_a.join().unwrap();
        handle_b.join().unwrap();

        assert_eq!(
            bytes, b"object-bytes",
            "final object bytes should be returned"
        );
        // Origin (GitHub) leg carried the bearer.
        assert_eq!(
            log_a.lock().unwrap()[0].1.as_deref(),
            Some("Bearer secret-xyz")
        );
        // Redirected (signed-URL) leg must have NO Authorization header.
        assert_eq!(
            log_b.lock().unwrap()[0].1,
            None,
            "credentials must not be forwarded across a host change"
        );
    }

    #[test]
    fn run_update_attaches_token_to_metadata_and_asset_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let target = release_target().unwrap();
        let version = "9.9.9";
        let archive_name = format!("toolx-{version}-{target}.tar.gz");
        let checksum_name = format!("toolx-{version}-{target}.sha256");
        let binary_in_archive = format!("toolx-{version}-{target}/toolx");
        let tarball = gzip_tar_with_entry(&binary_in_archive, b"#!/bin/sh\nexit 0\n");
        let mut hasher = Sha256::new();
        hasher.update(&tarball);
        let checksum_body =
            format!("{}  {archive_name}\n", hex::encode(hasher.finalize())).into_bytes();
        let api_body = format!(
            r#"{{"tag_name":"v{version}","assets":[{{"name":"{archive_name}"}},{{"name":"{checksum_name}"}}]}}"#
        )
        .into_bytes();

        let (base, log, handle) = spawn_recording_http(vec![
            ("releases/latest", MockReply::Body(api_body)),
            (archive_name.clone().leak(), MockReply::Body(tarball)),
            (checksum_name.clone().leak(), MockReply::Body(checksum_body)),
        ]);
        let mut config =
            UpdaterConfig::new("toolx", "0.1.0", "octocat/example").with_github_token("secret-xyz");
        config.install_dir = Some(tmp.path().to_path_buf());
        config.api_base = Some(base.clone());
        config.download_base = Some(base);
        let outcome = Updater::new(config)
            .run_update()
            .expect("run_update succeeds");
        handle.join().unwrap();

        assert!(
            outcome.promoted,
            "a newer private release should be promoted"
        );
        let recorded = log.lock().unwrap();
        assert_eq!(recorded.len(), 3, "metadata + archive + checksum");
        for (line, authorization) in recorded.iter() {
            assert_eq!(
                authorization.as_deref(),
                Some("Bearer secret-xyz"),
                "request {line:?} should carry the bearer token"
            );
        }
    }
}
