//! Reusable self-update plumbing for Rust CLIs that ship as binaries via GitHub releases.
//!
//! The host crate provides a [`UpdaterConfig`] describing how to fetch the latest binary, where
//! to stage it, and which tool/version to advertise. From there it gets:
//!
//! - [`Updater::current_status`] for `<tool> status`-style reporting.
//! - [`Updater::check_latest`] for resolving the newest release that carries assets for the
//!   running platform (multi-platform releases do not publish atomically, so this walks the
//!   releases feed newest-first within a bounded lookback instead of trusting one tag).
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
//! Linux, macOS, and x86_64 Windows are supported. Unix hosts preserve executable bits,
//! atomically promote `<tool>_next`, and re-exec after startup promotion. Windows uses
//! `tool.exe` / `tool_next.exe` and the canonical `x86_64-windows` release suffix. Because
//! Windows may lock a running executable, an update is left safely staged when `tool.exe`
//! already exists; host installers or launchers should replace it after all tool processes exit.
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
//! assert!(status.installed_path.contains("mytool"));
//! assert!(status.next_path.contains("mytool_next"));
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
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mcp_cli::{ErrorCategory, StructuredError, ToolRouter};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod mcp;

/// Default number of releases inspected, newest first, when resolving the newest release
/// that carries assets for the running platform.
///
/// See [`UpdaterConfig::release_lookback`].
pub const DEFAULT_RELEASE_LOOKBACK: usize = 10;

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
    /// `Authorization: Bearer <token>` on release-metadata requests and authenticated
    /// release-asset API requests. Takes precedence over `gh_account`/`gh_token_fallback`.
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
    /// How many releases to inspect, newest first, when resolving the newest release that
    /// actually carries assets for the running platform. Defaults to
    /// [`DEFAULT_RELEASE_LOOKBACK`] and is clamped to `1..=100` (GitHub's page limit).
    ///
    /// Multi-platform releases do not publish atomically: the newest tag can be missing
    /// this platform's asset while an older tag has it. The lookback bounds how far back
    /// the updater is willing to fall back before declaring the platform's release
    /// pipeline broken.
    pub release_lookback: Option<usize>,
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
            release_lookback: None,
        }
    }

    /// Number of releases inspected when resolving a platform-complete release,
    /// clamped to `1..=100`.
    pub fn release_lookback(&self) -> usize {
        self.release_lookback
            .unwrap_or(DEFAULT_RELEASE_LOOKBACK)
            .clamp(1, 100)
    }

    /// Set how many releases to inspect, newest first, when looking for one that carries
    /// assets for the running platform (chainable). Clamped to `1..=100`.
    pub fn with_release_lookback(mut self, releases: usize) -> Self {
        self.release_lookback = Some(releases);
        self
    }

    /// Resolve the install directory: the explicit `install_dir` override when set,
    /// otherwise `$HOME/.local/bin` on Unix or `%LOCALAPPDATA%/Programs/<tool>` on Windows.
    pub fn install_dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.install_dir {
            return Ok(dir.clone());
        }
        #[cfg(windows)]
        {
            let local_app_data = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("USERPROFILE")
                        .map(PathBuf::from)
                        .map(|profile| profile.join("AppData").join("Local"))
                })
                .ok_or_else(|| {
                    anyhow!(
                        "LOCALAPPDATA and USERPROFILE are unset; cannot resolve default Windows install dir"
                    )
                })?;
            return Ok(local_app_data.join("Programs").join(&self.tool_name));
        }
        #[cfg(not(windows))]
        {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| anyhow!("HOME is unset; cannot resolve default install dir"))?;
            Ok(home.join(".local").join("bin"))
        }
    }

    /// Path to the staged next binary: `<tool>_next` on Unix or `<tool>_next.exe` on Windows.
    pub fn next_binary_path(&self) -> Result<PathBuf> {
        Ok(self
            .install_dir()?
            .join(staged_executable_file_name(&self.tool_name)))
    }

    /// Path to the installed binary: `<tool>` on Unix or `<tool>.exe` on Windows.
    pub fn installed_binary_path(&self) -> Result<PathBuf> {
        Ok(self
            .install_dir()?
            .join(executable_file_name(&self.tool_name)))
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

/// Download metadata for one asset attached to a GitHub release.
///
/// This metadata is retained by [`LatestReleaseInfo`] so authenticated updates can use
/// GitHub's release-assets API instead of the public `browser_download_url` endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAssetInfo {
    /// File name of the release asset.
    pub name: String,
    /// GitHub's numeric release asset ID, when the metadata endpoint supplied one.
    pub id: Option<u64>,
    /// Public browser download URL, when the metadata endpoint supplied one.
    pub browser_download_url: Option<String>,
}

/// A newer release that was skipped because it does not carry the assets this platform
/// needs (a release that is still publishing, or whose build for this platform failed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkippedRelease {
    /// Raw release tag (e.g. `v1.2.3`).
    pub tag: String,
    /// Tag with a leading `v` stripped (e.g. `1.2.3`).
    pub version: String,
    /// Asset names this platform needed but the release does not publish.
    pub missing_assets: Vec<String>,
}

/// Parsed metadata for the release this platform should update to.
///
/// This is the newest published release that actually carries the assets for the running
/// platform — not necessarily the newest release overall. Multi-platform releases do not
/// publish atomically, so the newest tag is routinely missing some platform's build;
/// [`skipped_newer`](Self::skipped_newer) records exactly which newer releases were passed
/// over and why.
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
    /// API IDs and browser download URLs for the attached assets.
    ///
    /// This transport metadata is intentionally omitted from serialized status/MCP output;
    /// callers still see the stable asset-name list in [`assets`](Self::assets).
    #[serde(skip)]
    #[schemars(skip)]
    pub release_assets: Vec<ReleaseAssetInfo>,
    /// Whether `version` is newer than the configured current version.
    pub newer_than_current: bool,
    /// Releases newer than this one that were skipped because they publish no assets for
    /// the running platform, newest first.
    #[serde(default)]
    pub skipped_newer: Vec<SkippedRelease>,
    /// Human-readable explanation when a newer release was passed over, e.g.
    /// `"v0.0.42 has no x86_64-linux release asset; selecting v0.0.41 instead"`.
    #[serde(default)]
    pub selection_note: Option<String>,
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

    /// Resolve the release this platform should update to: the newest published release
    /// that actually carries assets for the running platform.
    ///
    /// Multi-platform releases do not publish atomically — per-platform jobs finish at
    /// different times and can fail or be starved independently — so the newest tag is
    /// routinely missing some platform's build. Resolving against "newest release" alone
    /// blocks those platforms from updating at all while a perfectly good build sits one
    /// tag back. This walks the releases feed newest first, within the bounded
    /// [`UpdaterConfig::release_lookback`] window, and returns the first release whose
    /// archive and checksum assets for this platform are both present.
    ///
    /// Any newer releases that were passed over are reported in
    /// [`LatestReleaseInfo::skipped_newer`] and summarized in
    /// [`LatestReleaseInfo::selection_note`], so falling back is never silent. When no
    /// release inside the lookback window carries this platform's assets, that is a real
    /// failure (the platform's release pipeline is broken) and it is reported as such.
    ///
    /// Draft and prerelease entries are ignored, matching GitHub's "latest release"
    /// semantics.
    pub fn check_latest(&self) -> Result<LatestReleaseInfo> {
        let target = release_target()?;
        let releases = self.fetch_releases()?;
        self.select_release(&target, releases)
    }

    /// Fetch the newest `release_lookback` releases, newest first, as parsed candidates.
    fn fetch_releases(&self) -> Result<Vec<ParsedRelease>> {
        let url = format!(
            "{}/repos/{}/releases?per_page={}",
            self.config.api_base(),
            self.config.repo_slug,
            self.config.release_lookback()
        );
        let body = self.get_release_json(&url)?;
        // A single-release object is accepted too, so hosts pointing `api_base` at a
        // minimal mirror that only serves one release keep working.
        let entries: Vec<&serde_json::Value> = match body.as_array() {
            Some(array) => array.iter().collect(),
            None => vec![&body],
        };
        entries.into_iter().map(parse_release).collect()
    }

    /// GET `url` as JSON with the shared GitHub auth headers and error mapping.
    fn get_release_json(&self, url: &str) -> Result<serde_json::Value> {
        let agent = self.http_agent();
        let mut request = agent.get(url).set("User-Agent", &self.config.user_agent());
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
        };
        Ok(response.into_json::<serde_json::Value>()?)
    }

    /// Pick the newest candidate release carrying every asset this platform needs.
    fn select_release(
        &self,
        target: &str,
        candidates: Vec<ParsedRelease>,
    ) -> Result<LatestReleaseInfo> {
        let mut candidates: Vec<ParsedRelease> = candidates
            .into_iter()
            .filter(|release| !release.draft && !release.prerelease)
            .collect();
        if candidates.is_empty() {
            bail!(
                "no published GitHub releases for {} yet (drafts and prereleases are ignored)",
                self.config.repo_slug
            );
        }
        // Newest first. `sort_by` is stable, so releases whose tags do not parse as semver
        // keep the order GitHub returned them in (already newest-first by creation).
        candidates.sort_by(|a, b| {
            semver::Version::parse(&b.version)
                .ok()
                .cmp(&semver::Version::parse(&a.version).ok())
        });

        let mut skipped: Vec<SkippedRelease> = Vec::new();
        for candidate in &candidates {
            let names = self.asset_names_for(&candidate.version, target)?;
            let missing_assets: Vec<String> = [names.archive, names.checksum]
                .into_iter()
                .filter(|name| !candidate.assets.iter().any(|asset| &asset.name == name))
                .collect();
            if !missing_assets.is_empty() {
                skipped.push(SkippedRelease {
                    tag: candidate.tag.clone(),
                    version: candidate.version.clone(),
                    missing_assets,
                });
                continue;
            }
            let selection_note = platform_fallback_note(&skipped, target, &candidate.tag);
            let newer_than_current = self.is_newer(&candidate.version);
            return Ok(LatestReleaseInfo {
                tag: candidate.tag.clone(),
                version: candidate.version.clone(),
                html_url: candidate.html_url.clone(),
                assets: candidate
                    .assets
                    .iter()
                    .map(|asset| asset.name.clone())
                    .collect(),
                release_assets: candidate.assets.clone(),
                newer_than_current,
                skipped_newer: skipped,
                selection_note,
            });
        }

        // Falling back past every inspected release is a different, more serious condition
        // than one late tag: this platform's release pipeline is not producing artifacts.
        let inspected = skipped
            .iter()
            .map(|release| release.tag.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "no release of {} within the last {} carries a {target} asset (inspected: {inspected}); \
             this platform's release pipeline is likely broken",
            self.config.repo_slug,
            candidates.len()
        );
    }

    /// Resolve the asset names this platform expects for `version`.
    fn asset_names_for(&self, version: &str, target: &str) -> Result<AssetNames> {
        Ok(match &self.config.asset_strategy {
            AssetStrategy::TendrilStyle => AssetNames {
                archive: format!("{}-{version}-{target}.tar.gz", self.config.tool_name),
                checksum: format!("{}-{version}-{target}.sha256", self.config.tool_name),
                binary_in_archive: format!(
                    "{}-{version}-{target}/{}",
                    self.config.tool_name,
                    executable_file_name(&self.config.tool_name)
                ),
            },
            AssetStrategy::Custom(strategy) => strategy(&self.config.tool_name, version, target)?,
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
        let asset_names = self.asset_names_for(&latest.version, &target)?;
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
        let token = self.config.resolved_token();
        let (archive_url, archive_accept) =
            self.asset_download_request(latest, &asset_names.archive, token.is_some());
        let (checksum_url, checksum_accept) =
            self.asset_download_request(latest, &asset_names.checksum, token.is_some());
        let timeout = self.config.http_timeout.unwrap_or(Duration::from_secs(60));
        let archive_bytes = download_bytes(
            &archive_url,
            &self.config.user_agent(),
            token.as_deref(),
            archive_accept,
            timeout,
        )?;
        let checksum_text = download_text(
            &checksum_url,
            &self.config.user_agent(),
            token.as_deref(),
            checksum_accept,
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
    ///
    /// On Windows, replacing an existing executable is deliberately deferred because the
    /// running image may be locked. In that case this returns `Ok(None)` and leaves both
    /// `tool.exe` and the verified `tool_next.exe` untouched; the host should replace the
    /// executable after all tool processes exit (typically via an installer/bootstrapper).
    pub fn promote_next(&self) -> Result<Option<PathBuf>> {
        let next = self.config.next_binary_path()?;
        if !next.exists() {
            return Ok(None);
        }
        let installed = self.config.installed_binary_path()?;
        if let Some(parent) = installed.parent() {
            fs::create_dir_all(parent)?;
        }
        #[cfg(windows)]
        if installed.exists() {
            return Ok(None);
        }
        set_executable(&next)?;
        fs::rename(&next, &installed)
            .with_context(|| format!("promote {} -> {}", next.display(), installed.display()))?;
        set_executable(&installed)?;
        Ok(Some(installed))
    }

    /// High-level `<tool> update`: resolve the newest release carrying this platform's
    /// assets and, when newer, stage and promote it. A no-op when already up to date.
    ///
    /// When a newer release was skipped because it publishes nothing for this platform,
    /// the returned [`UpdateOutcome::note`] says so explicitly — taking an older release
    /// silently would be its own problem.
    pub fn run_update(&self) -> Result<UpdateOutcome> {
        let latest = self.check_latest()?;
        let installed_path = self.config.installed_binary_path()?;
        let next_path = self.config.next_binary_path()?;
        if !latest.newer_than_current {
            let mut note = format!(
                "no update needed; latest is {} and current is {}",
                latest.version, self.config.current_version
            );
            if let Some(selection) = &latest.selection_note {
                note.push_str("; ");
                note.push_str(selection);
            }
            return Ok(UpdateOutcome {
                current_version: self.config.current_version.clone(),
                latest_version: latest.version.clone(),
                staged: false,
                promoted: false,
                next_path: next_path.display().to_string(),
                installed_path: installed_path.display().to_string(),
                note: Some(note),
            });
        }
        self.stage_next(&latest)?;
        let promoted = self.promote_next()?;
        let mut notes: Vec<String> = Vec::new();
        if let Some(selection) = &latest.selection_note {
            notes.push(selection.clone());
        }
        if cfg!(windows) && promoted.is_none() {
            notes.push(windows_deferred_promotion_note(&next_path, &installed_path));
        }
        let note = if notes.is_empty() {
            None
        } else {
            Some(notes.join("; "))
        };
        Ok(UpdateOutcome {
            current_version: self.config.current_version.clone(),
            latest_version: latest.version.clone(),
            staged: true,
            promoted: promoted.is_some(),
            next_path: next_path.display().to_string(),
            installed_path: installed_path.display().to_string(),
            note,
        })
    }

    fn asset_download_request(
        &self,
        latest: &LatestReleaseInfo,
        asset_name: &str,
        authenticated: bool,
    ) -> (String, Option<&'static str>) {
        let metadata = latest
            .release_assets
            .iter()
            .find(|asset| asset.name == asset_name);
        if authenticated {
            if let Some(asset_id) = metadata.and_then(|asset| asset.id) {
                return (
                    format!(
                        "{}/repos/{}/releases/assets/{asset_id}",
                        self.config.api_base(),
                        self.config.repo_slug
                    ),
                    Some("application/octet-stream"),
                );
            }
        }

        // An explicit download-base override retains the existing mirror/air-gapped
        // contract. Otherwise prefer GitHub's browser URL and fall back to deriving it
        // for old fixtures or non-GitHub metadata that only includes asset names.
        let browser_url = if self.config.download_base.is_some() {
            None
        } else {
            metadata.and_then(|asset| asset.browser_download_url.clone())
        };
        let url = browser_url.unwrap_or_else(|| {
            format!(
                "{}/{}/releases/download/{}/{}",
                self.config.download_base(),
                self.config.repo_slug,
                latest.tag,
                asset_name
            )
        });
        (url, None)
    }

    fn http_agent(&self) -> ureq::Agent {
        let timeout = self.config.http_timeout.unwrap_or(Duration::from_secs(60));
        ureq::AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout_read(timeout)
            .build()
    }
}

/// One release parsed out of the GitHub releases feed, before platform selection.
#[derive(Debug, Clone)]
struct ParsedRelease {
    tag: String,
    version: String,
    html_url: Option<String>,
    assets: Vec<ReleaseAssetInfo>,
    draft: bool,
    prerelease: bool,
}

/// Parse one GitHub release JSON object into a [`ParsedRelease`].
fn parse_release(value: &serde_json::Value) -> Result<ParsedRelease> {
    let tag = value
        .get("tag_name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("github release missing tag_name"))?
        .to_string();
    let version = tag.trim_start_matches('v').to_string();
    let html_url = value
        .get("html_url")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let assets: Vec<ReleaseAssetInfo> = value
        .get("assets")
        .and_then(|value| value.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|item| {
                    let name = item.get("name")?.as_str()?.to_string();
                    Some(ReleaseAssetInfo {
                        name,
                        id: item.get("id").and_then(|value| value.as_u64()),
                        browser_download_url: item
                            .get("browser_download_url")
                            .and_then(|value| value.as_str())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let flag = |key: &str| {
        value
            .get(key)
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    };
    Ok(ParsedRelease {
        tag,
        version,
        html_url,
        assets,
        draft: flag("draft"),
        prerelease: flag("prerelease"),
    })
}

/// Human-readable explanation of a platform fallback, or `None` when the newest release
/// was usable as-is.
fn platform_fallback_note(
    skipped: &[SkippedRelease],
    target: &str,
    selected_tag: &str,
) -> Option<String> {
    match skipped {
        [] => None,
        [one] => Some(format!(
            "{} has no {target} release asset; selecting {selected_tag} instead",
            one.tag
        )),
        many => Some(format!(
            "{} have no {target} release assets; selecting {selected_tag} instead",
            many.iter()
                .map(|release| release.tag.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
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
    accept: Option<&str>,
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
        if let Some(accept) = accept {
            request = request.set("Accept", accept);
        }
        if send_auth {
            if let Some(token) = token {
                request = request.set("Authorization", &format!("Bearer {token}"));
            }
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(404, _)) if token.is_none() => {
                bail!(
                    "GET {current} returned HTTP 404; GitHub returns 404 for unauthenticated private release assets; configure a token with UpdaterConfig::with_github_token, with_gh_account, or with_gh_token_fallback"
                );
            }
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
    accept: Option<&str>,
    timeout: Duration,
) -> Result<String> {
    String::from_utf8(download_bytes(url, user_agent, token, accept, timeout)?)
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
    // Windows rename/persist cannot replace an existing destination. This is only the
    // disposable staged path, never the installed executable, so removing an older staged
    // payload before persisting the newly checksum-verified one is safe.
    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(destination)
            .with_context(|| format!("remove older staged update {}", destination.display()))?;
    }
    tmp.persist(destination)
        .map_err(|err| anyhow!("persist {} failed: {err}", destination.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(path: &Path) -> Result<()> {
    // Windows has no Unix executable bit. Still stat the path so callers retain the same
    // missing-file/error contract as Unix without trying to mutate meaningless permissions.
    fs::metadata(path)?;
    Ok(())
}

fn executable_file_name(tool_name: &str) -> String {
    if cfg!(windows) {
        format!("{tool_name}.exe")
    } else {
        tool_name.to_string()
    }
}

fn staged_executable_file_name(tool_name: &str) -> String {
    if cfg!(windows) {
        format!("{tool_name}_next.exe")
    } else {
        format!("{tool_name}_next")
    }
}

fn windows_deferred_promotion_note(next: &Path, installed: &Path) -> String {
    format!(
        "update staged at {}; Windows cannot safely replace an existing/running executable; \
         close all tool processes, then replace {} with the staged file (or use the host installer/bootstrapper)",
        next.display(),
        installed.display()
    )
}

/// Look up the running binary and apply any staged sibling update.
///
/// Unix hosts promote `<tool>_next` and re-exec. Windows hosts detect
/// `<tool>_next.exe` but leave it staged, print actionable replacement guidance, and continue:
/// a running `.exe` may be locked and must never be corrupted by an unsafe in-process swap.
/// The function is intentionally best-effort on every platform: failures only print warnings
/// and return `Ok(())` so the rest of the CLI still starts.
pub fn maybe_apply_staged_update(tool_name: &str) -> Result<()> {
    let current = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("warning: {tool_name} could not resolve current_exe: {error}");
            return Ok(());
        }
    };
    let staged = current.with_file_name(staged_executable_file_name(tool_name));
    if !staged.exists() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        eprintln!(
            "warning: {}",
            windows_deferred_promotion_note(&staged, &current)
        );
        return Ok(());
    }
    #[cfg(unix)]
    {
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
    #[cfg(not(any(unix, windows)))]
    {
        eprintln!(
            "warning: staged {tool_name} update {} cannot be promoted on this platform",
            staged.display()
        );
        Ok(())
    }
}

#[cfg(unix)]
fn exec_replace(program: &std::ffi::OsStr, args: &[std::ffi::OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    cmd.exec()
}

fn release_target_for(os: &str, arch: &str) -> Result<String> {
    let os = match os {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "windows",
        other => bail!("unsupported updater OS {other}"),
    };
    let arch = match arch {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => bail!("unsupported updater arch {other}"),
    };
    if os == "windows" && arch != "x86_64" {
        bail!("unsupported Windows updater arch {arch}; canonical assets use x86_64-windows");
    }
    Ok(format!("{arch}-{os}"))
}

/// Caco/Tendril-style platform suffix: `<arch>-<os>`, including the canonical
/// Windows target `x86_64-windows` used by Ring and other downstream CLIs.
pub fn release_target() -> Result<String> {
    release_target_for(std::env::consts::OS, std::env::consts::ARCH)
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
    fn release_target_uses_canonical_windows_suffix() {
        assert_eq!(
            release_target_for("windows", "x86_64").unwrap(),
            "x86_64-windows"
        );
        let error = release_target_for("windows", "aarch64").unwrap_err();
        assert!(error.to_string().contains("x86_64-windows"));
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
        assert!(
            status
                .installed_path
                .ends_with(&executable_file_name("toolx"))
        );
        assert!(
            status
                .next_path
                .ends_with(&staged_executable_file_name("toolx"))
        );
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

    #[cfg(unix)]
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

    #[cfg(windows)]
    #[test]
    fn windows_paths_use_exe_names_and_existing_binary_defers_promotion() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        let updater = Updater::new(config);
        let installed = updater.config().installed_binary_path().unwrap();
        let next = updater.config().next_binary_path().unwrap();
        assert_eq!(installed.file_name().unwrap(), "toolx.exe");
        assert_eq!(next.file_name().unwrap(), "toolx_next.exe");

        fs::write(&installed, b"current executable").unwrap();
        fs::write(&next, b"verified staged executable").unwrap();
        assert!(
            updater.promote_next().unwrap().is_none(),
            "Windows must defer replacing an existing executable"
        );
        assert_eq!(fs::read(&installed).unwrap(), b"current executable");
        assert_eq!(fs::read(&next).unwrap(), b"verified staged executable");
        let note = windows_deferred_promotion_note(&next, &installed);
        assert!(note.contains("close all tool processes"));
        assert!(note.contains(&next.display().to_string()));
        assert!(note.contains(&installed.display().to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_install_dir_is_local_app_data_programs_tool() {
        let config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        let install_dir = config.install_dir().unwrap();
        assert!(install_dir.ends_with(Path::new("Programs").join("toolx")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_atomic_write_replaces_only_the_staged_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.exe");
        let staged = tmp.path().join("toolx_next.exe");
        fs::write(&source, b"new verified payload").unwrap();
        fs::write(&staged, b"older staged payload").unwrap();

        atomic_write(&staged, &source).unwrap();
        assert_eq!(fs::read(staged).unwrap(), b"new verified payload");
    }

    #[cfg(windows)]
    #[test]
    fn windows_promotes_when_no_installed_binary_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        let updater = Updater::new(config);
        let next = updater.config().next_binary_path().unwrap();
        fs::write(&next, b"new executable").unwrap();

        let promoted = updater.promote_next().unwrap().expect("safe first install");
        assert_eq!(promoted.file_name().unwrap(), "toolx.exe");
        assert!(!next.exists());
        assert_eq!(fs::read(promoted).unwrap(), b"new executable");
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
        let target = release_target().unwrap();
        let archive = format!("toolx-9.9.9-{target}.tar.gz");
        let checksum = format!("toolx-9.9.9-{target}.sha256");
        let body = format!(
            r#"[{{
            "tag_name": "v9.9.9",
            "html_url": "https://example.invalid/releases/v9.9.9",
            "assets": [
                {{
                    "id": 101,
                    "name": "{archive}",
                    "browser_download_url": "https://example.invalid/download/archive"
                }},
                {{
                    "id": 102,
                    "name": "{checksum}",
                    "browser_download_url": "https://example.invalid/download/checksum"
                }}
            ]
        }}]"#
        );
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
        assert_eq!(info.assets, vec![archive.clone(), checksum.clone()]);
        assert_eq!(
            info.release_assets,
            vec![
                ReleaseAssetInfo {
                    name: archive,
                    id: Some(101),
                    browser_download_url: Some(
                        "https://example.invalid/download/archive".to_string()
                    ),
                },
                ReleaseAssetInfo {
                    name: checksum,
                    id: Some(102),
                    browser_download_url: Some(
                        "https://example.invalid/download/checksum".to_string()
                    ),
                },
            ]
        );
        assert!(info.newer_than_current, "9.9.9 should be newer than 0.1.0");
        assert!(
            info.skipped_newer.is_empty(),
            "nothing newer to skip: {:?}",
            info.skipped_newer
        );
        assert!(
            info.selection_note.is_none(),
            "the newest release was usable, so there is nothing to explain"
        );
    }

    /// A minimal mirror that serves a single release object rather than a feed array is
    /// still understood (the release just has to carry this platform's assets).
    #[test]
    fn check_latest_accepts_a_single_release_object_body() {
        let target = release_target().unwrap();
        let body = format!(
            r#"{{"tag_name":"v9.9.9","assets":[{{"name":"toolx-9.9.9-{target}.tar.gz"}},{{"name":"toolx-9.9.9-{target}.sha256"}}]}}"#
        );
        let (base, handle) = spawn_one_shot_http(body);
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.api_base = Some(base);
        let info = Updater::new(config)
            .check_latest()
            .expect("a single-object release body is accepted");
        handle.join().unwrap();
        assert_eq!(info.version, "9.9.9");
    }

    #[test]
    fn check_latest_reports_not_newer_for_same_version() {
        let target = release_target().unwrap();
        let body = format!(
            r#"[{{"tag_name":"v0.1.0","assets":[{{"name":"toolx-0.1.0-{target}.tar.gz"}},{{"name":"toolx-0.1.0-{target}.sha256"}}]}}]"#
        );
        let (base, handle) = spawn_one_shot_http(body);
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.api_base = Some(base);
        let info = Updater::new(config)
            .check_latest()
            .expect("check_latest succeeds");
        handle.join().unwrap();
        assert_eq!(info.version, "0.1.0");
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
        let binary_in_archive =
            format!("toolx-{version}-{target}/{}", executable_file_name("toolx"));
        let payload = b"portable updated-toolx payload\n";

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
            r#"[{{"tag_name":"v{version}","assets":[{{"name":"{archive_name}"}},{{"name":"{checksum_name}"}}]}}]"#
        )
        .into_bytes();

        // Order matters: check_latest hits the API first, then stage_next downloads
        // the archive and the checksum.
        let (base, handle) = spawn_routed_http(vec![
            ("releases?", api_body),
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

        // The promoted binary lands at the platform-native installed name with our payload,
        // and the staged path is consumed by the promotion.
        let installed = tmp.path().join(executable_file_name("toolx"));
        assert!(installed.exists(), "installed binary should exist");
        assert!(
            !tmp.path()
                .join(staged_executable_file_name("toolx"))
                .exists()
        );
        assert_eq!(fs::read(&installed).unwrap(), payload);
        #[cfg(unix)]
        {
            let mode = fs::metadata(&installed).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "installed binary should be chmod 0755");
        }
    }

    #[test]
    fn run_update_is_noop_when_latest_is_not_newer() {
        let tmp = tempfile::tempdir().unwrap();
        let target = release_target().unwrap();
        let api_body = format!(
            r#"[{{"tag_name":"v0.1.0","assets":[{{"name":"toolx-0.1.0-{target}.tar.gz"}},{{"name":"toolx-0.1.0-{target}.sha256"}}]}}]"#
        )
        .into_bytes();
        let (base, handle) = spawn_routed_http(vec![("releases?", api_body)]);
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
        assert!(!tmp.path().join(executable_file_name("toolx")).exists());
        assert!(
            !tmp.path()
                .join(staged_executable_file_name("toolx"))
                .exists()
        );
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
            release_assets: Vec::new(),
            newer_than_current: true,
            skipped_newer: Vec::new(),
            selection_note: None,
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
        let inner = format!("toolx-{version}-{target}/{}", executable_file_name("toolx"));
        let tarball = gzip_tar_with_entry(&inner, b"portable payload\n");
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
            release_assets: Vec::new(),
            newer_than_current: true,
            skipped_newer: Vec::new(),
            selection_note: None,
        };
        let err = Updater::new(config).stage_next(&latest).unwrap_err();
        handle.join().unwrap();
        assert!(
            err.to_string().contains("checksum mismatch"),
            "unexpected error: {err}"
        );
        assert!(
            !tmp.path()
                .join(staged_executable_file_name("toolx"))
                .exists(),
            "a checksum mismatch must never stage the next binary"
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
            release_assets: Vec::new(),
            newer_than_current: true,
            skipped_newer: Vec::new(),
            selection_note: None,
        };
        let err = Updater::new(config).stage_next(&latest).unwrap_err();
        handle.join().unwrap();
        assert!(
            err.to_string().contains("did not contain"),
            "unexpected error: {err}"
        );
        assert!(
            !tmp.path()
                .join(staged_executable_file_name("toolx"))
                .exists(),
            "an archive missing the binary must not stage the next binary"
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

    /// Build a releases-feed JSON body from `(tag, asset-suffixes)` pairs, where each
    /// suffix is a full platform suffix such as `x86_64-linux`.
    fn releases_feed(entries: &[(&str, &[&str])]) -> String {
        let releases: Vec<String> = entries
            .iter()
            .map(|(tag, targets)| {
                let version = tag.trim_start_matches('v');
                let assets: Vec<String> = targets
                    .iter()
                    .flat_map(|target| {
                        [
                            format!(r#"{{"name":"toolx-{version}-{target}.tar.gz"}}"#),
                            format!(r#"{{"name":"toolx-{version}-{target}.sha256"}}"#),
                        ]
                    })
                    .collect();
                format!(r#"{{"tag_name":"{tag}","assets":[{}]}}"#, assets.join(","))
            })
            .collect();
        format!("[{}]", releases.join(","))
    }

    /// The helsinki case (bd-0497f6): the newest tag published only one platform's build,
    /// so a node on the other platform must fall back to the newest release that actually
    /// carries its asset instead of refusing to update at all.
    #[test]
    fn check_latest_falls_back_to_newest_release_carrying_this_platform() {
        let target = release_target().unwrap();
        let other = if target == "aarch64-darwin" {
            "x86_64-linux"
        } else {
            "aarch64-darwin"
        };
        let body = releases_feed(&[
            ("v0.0.42", &[other]),
            ("v0.0.41", &[other, &target]),
            ("v0.0.40", &[&target]),
        ]);
        let (base, handle) = spawn_one_shot_http(body);
        let mut config = UpdaterConfig::new("toolx", "0.0.27", "octocat/example");
        config.api_base = Some(base);
        let info = Updater::new(config)
            .check_latest()
            .expect("a platform-incomplete newest release must not block the update");
        handle.join().unwrap();

        assert_eq!(info.tag, "v0.0.41", "newest release carrying {target}");
        assert!(info.newer_than_current);
        assert_eq!(
            info.skipped_newer,
            vec![SkippedRelease {
                tag: "v0.0.42".to_string(),
                version: "0.0.42".to_string(),
                missing_assets: vec![
                    format!("toolx-0.0.42-{target}.tar.gz"),
                    format!("toolx-0.0.42-{target}.sha256"),
                ],
            }],
            "the skipped newer release must be reported, not silently dropped"
        );
        let note = info.selection_note.expect("a fallback must be explained");
        assert!(
            note.contains("v0.0.42") && note.contains(&target) && note.contains("v0.0.41"),
            "unexpected note: {note}"
        );
    }

    /// A release feed where *no* inspected release carries this platform's asset is a
    /// different, more serious condition than one late tag: the platform's release
    /// pipeline is broken, and the error must say so rather than look like a lag.
    #[test]
    fn check_latest_errors_when_no_release_in_lookback_carries_platform_assets() {
        let target = release_target().unwrap();
        let other = if target == "aarch64-darwin" {
            "x86_64-linux"
        } else {
            "aarch64-darwin"
        };
        let body = releases_feed(&[("v0.0.42", &[other]), ("v0.0.41", &[other])]);
        let (base, handle) = spawn_one_shot_http(body);
        let mut config = UpdaterConfig::new("toolx", "0.0.27", "octocat/example");
        config.api_base = Some(base);
        let err = Updater::new(config)
            .check_latest()
            .expect_err("an entirely missing platform must surface as a real failure");
        handle.join().unwrap();
        let message = format!("{err:#}");
        assert!(
            message.contains(&target)
                && message.contains("v0.0.42")
                && message.contains("v0.0.41")
                && message.contains("release pipeline"),
            "unexpected error: {message}"
        );
    }

    /// The releases feed includes drafts and prereleases; GitHub's "latest release"
    /// semantics do not, so neither does the platform-aware resolution.
    #[test]
    fn check_latest_ignores_drafts_and_prereleases() {
        let target = release_target().unwrap();
        let body = format!(
            r#"[{{"tag_name":"v9.9.9","draft":true,"assets":[{{"name":"toolx-9.9.9-{target}.tar.gz"}},{{"name":"toolx-9.9.9-{target}.sha256"}}]}},
                {{"tag_name":"v9.9.8","prerelease":true,"assets":[{{"name":"toolx-9.9.8-{target}.tar.gz"}},{{"name":"toolx-9.9.8-{target}.sha256"}}]}},
                {{"tag_name":"v9.9.7","assets":[{{"name":"toolx-9.9.7-{target}.tar.gz"}},{{"name":"toolx-9.9.7-{target}.sha256"}}]}}]"#
        );
        let (base, handle) = spawn_one_shot_http(body);
        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.api_base = Some(base);
        let info = Updater::new(config)
            .check_latest()
            .expect("check_latest succeeds");
        handle.join().unwrap();
        assert_eq!(info.tag, "v9.9.7");
        assert!(
            info.skipped_newer.is_empty(),
            "drafts/prereleases are filtered out, not reported as platform-incomplete"
        );
    }

    /// The search stays bounded: the configured lookback is what the feed request asks
    /// for, and it is clamped to GitHub's page limit.
    #[test]
    fn release_lookback_bounds_the_feed_request() {
        let target = release_target().unwrap();
        let body = releases_feed(&[("v9.9.9", &[&target])]).into_bytes();
        let (base, log, handle) = spawn_recording_http(vec![("releases?", MockReply::Body(body))]);
        let config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example")
            .with_release_lookback(3)
            .with_github_token("");
        let mut config = config;
        config.api_base = Some(base);
        assert_eq!(config.release_lookback(), 3);
        Updater::new(config).check_latest().expect("succeeds");
        handle.join().unwrap();
        let requests = log.lock().unwrap();
        assert!(
            requests[0].0.contains("per_page=3"),
            "unexpected request: {:?}",
            requests[0].0
        );
        // Out-of-range lookbacks are clamped rather than sent verbatim.
        assert_eq!(
            UpdaterConfig::new("toolx", "0.1.0", "octocat/example")
                .with_release_lookback(0)
                .release_lookback(),
            1
        );
        assert_eq!(
            UpdaterConfig::new("toolx", "0.1.0", "octocat/example")
                .with_release_lookback(5_000)
                .release_lookback(),
            100
        );
        assert_eq!(
            UpdaterConfig::new("toolx", "0.1.0", "octocat/example").release_lookback(),
            DEFAULT_RELEASE_LOOKBACK
        );
    }

    /// End-to-end: a platform-incomplete newest release still produces a promoted binary
    /// from the newest release that has this platform's asset, and `run_update` says so.
    #[test]
    fn run_update_installs_the_fallback_release_and_explains_why() {
        let tmp = tempfile::tempdir().unwrap();
        let target = release_target().unwrap();
        let other = if target == "aarch64-darwin" {
            "x86_64-linux"
        } else {
            "aarch64-darwin"
        };
        let version = "0.0.41";
        let archive_name = format!("toolx-{version}-{target}.tar.gz");
        let checksum_name = format!("toolx-{version}-{target}.sha256");
        let inner = format!("toolx-{version}-{target}/{}", executable_file_name("toolx"));
        let payload = b"fallback release payload\n";
        let tarball = gzip_tar_with_entry(&inner, payload);
        let mut hasher = Sha256::new();
        hasher.update(&tarball);
        let checksum_body =
            format!("{}  {archive_name}\n", hex::encode(hasher.finalize())).into_bytes();
        let api_body =
            releases_feed(&[("v0.0.42", &[other]), ("v0.0.41", &[&target])]).into_bytes();

        let (base, handle) = spawn_routed_http(vec![
            ("releases?", api_body),
            (archive_name.clone().leak(), tarball),
            (checksum_name.leak(), checksum_body),
        ]);
        let mut config = UpdaterConfig::new("toolx", "0.0.27", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        config.api_base = Some(base.clone());
        config.download_base = Some(base);
        let outcome = Updater::new(config)
            .run_update()
            .expect("run_update falls back instead of refusing");
        handle.join().unwrap();

        assert_eq!(outcome.latest_version, version);
        assert!(outcome.staged && outcome.promoted);
        let note = outcome
            .note
            .expect("installing an older release must never be silent");
        assert!(
            note.contains("v0.0.42") && note.contains(&target) && note.contains("v0.0.41"),
            "unexpected note: {note}"
        );
        let installed = tmp.path().join(executable_file_name("toolx"));
        assert_eq!(fs::read(&installed).unwrap(), payload);
    }

    /// Already running the newest release this platform has: no update, and the note
    /// still explains that a newer tag exists but publishes nothing for this platform.
    #[test]
    fn run_update_noop_note_explains_a_platform_incomplete_newer_release() {
        let tmp = tempfile::tempdir().unwrap();
        let target = release_target().unwrap();
        let other = if target == "aarch64-darwin" {
            "x86_64-linux"
        } else {
            "aarch64-darwin"
        };
        let api_body =
            releases_feed(&[("v0.0.42", &[other]), ("v0.0.41", &[&target])]).into_bytes();
        let (base, handle) = spawn_routed_http(vec![("releases?", api_body)]);
        let mut config = UpdaterConfig::new("toolx", "0.0.41", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        config.api_base = Some(base.clone());
        config.download_base = Some(base);
        let outcome = Updater::new(config)
            .run_update()
            .expect("run_update succeeds");
        handle.join().unwrap();
        assert!(!outcome.staged && !outcome.promoted);
        let note = outcome.note.expect("a no-op should carry a note");
        assert!(
            note.contains("no update needed") && note.contains("v0.0.42"),
            "unexpected note: {note}"
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
        Status(&'static str, Vec<u8>),
    }

    type RequestLog =
        std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>, Option<String>)>>>;

    /// Like `spawn_routed_http`, but also records each request's first line and its
    /// `Authorization` header (if any), and can answer with a 302 redirect. Used to
    /// assert credential forwarding behaviour without external network.
    fn spawn_recording_http(
        routes: Vec<(&'static str, MockReply)>,
    ) -> (String, RequestLog, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let base = format!("http://{addr}");
        let log: RequestLog = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
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
                let header = |name: &str| {
                    request
                        .lines()
                        .find(|line| {
                            line.split_once(':')
                                .is_some_and(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
                        })
                        .and_then(|line| line.split_once(':'))
                        .map(|(_, value)| value.trim().to_string())
                };
                let authorization = header("authorization");
                let accept = header("accept");
                log_thread
                    .lock()
                    .unwrap()
                    .push((request_line.clone(), authorization, accept));
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
                    MockReply::Status(status_line, body) => {
                        let mut bytes = format!(
                            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        bytes.extend_from_slice(&body);
                        bytes
                    }
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
        let bytes = download_bytes(
            &url,
            "ua/1.0",
            Some("secret-xyz"),
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(bytes, b"payload");
        let recorded = log.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].1.as_deref(), Some("Bearer secret-xyz"));

        // Without a token: no Authorization header is sent at all.
        let (base, log, handle) =
            spawn_recording_http(vec![("asset", MockReply::Body(b"payload".to_vec()))]);
        let url = format!("{base}/asset");
        let bytes = download_bytes(&url, "ua/1.0", None, None, Duration::from_secs(5)).unwrap();
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
        let bytes = download_bytes(
            &url,
            "ua/1.0",
            Some("secret-xyz"),
            None,
            Duration::from_secs(5),
        )
        .unwrap();
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
    fn private_update_uses_asset_api_ids_and_strips_auth_on_signed_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let target = release_target().unwrap();
        let version = "9.9.9";
        let archive_name = format!("toolx-{version}-{target}.tar.gz");
        let checksum_name = format!("toolx-{version}-{target}.sha256");
        let binary_in_archive =
            format!("toolx-{version}-{target}/{}", executable_file_name("toolx"));
        let payload = b"private portable payload\n";
        let tarball = gzip_tar_with_entry(&binary_in_archive, payload);
        let mut hasher = Sha256::new();
        hasher.update(&tarball);
        let checksum_body =
            format!("{}  {archive_name}\n", hex::encode(hasher.finalize())).into_bytes();
        let api_body = format!(
            r#"[{{"tag_name":"v{version}","assets":[{{"id":1001,"name":"{archive_name}","browser_download_url":"https://github.invalid/browser-archive"}},{{"id":1002,"name":"{checksum_name}","browser_download_url":"https://github.invalid/browser-checksum"}}]}}]"#
        )
        .into_bytes();

        let (signed_base, signed_log, signed_handle) =
            spawn_recording_http(vec![("signed-archive", MockReply::Body(tarball))]);
        let (api_base, api_log, api_handle) = spawn_recording_http(vec![
            ("releases?", MockReply::Body(api_body)),
            (
                "/releases/assets/1001",
                MockReply::Redirect(format!("{signed_base}/signed-archive?signature=opaque")),
            ),
            ("/releases/assets/1002", MockReply::Body(checksum_body)),
        ]);
        let mut config =
            UpdaterConfig::new("toolx", "0.1.0", "octocat/example").with_github_token("secret-xyz");
        config.install_dir = Some(tmp.path().to_path_buf());
        config.api_base = Some(api_base);
        let outcome = Updater::new(config)
            .run_update()
            .expect("private run_update succeeds");
        api_handle.join().unwrap();
        signed_handle.join().unwrap();

        assert!(outcome.promoted, "the private release should be promoted");
        assert_eq!(
            fs::read(tmp.path().join(executable_file_name("toolx"))).unwrap(),
            payload
        );

        let api_requests = api_log.lock().unwrap();
        assert_eq!(
            api_requests.len(),
            3,
            "metadata + archive API + checksum API"
        );
        assert!(
            api_requests[1]
                .0
                .contains("/repos/octocat/example/releases/assets/1001")
        );
        assert!(
            api_requests[2]
                .0
                .contains("/repos/octocat/example/releases/assets/1002")
        );
        for (line, authorization, accept) in api_requests.iter() {
            assert_eq!(
                authorization.as_deref(),
                Some("Bearer secret-xyz"),
                "request {line:?} should carry the bearer token"
            );
            if line.contains("/releases/assets/") {
                assert_eq!(
                    accept.as_deref(),
                    Some("application/octet-stream"),
                    "asset API request {line:?} needs the binary media type"
                );
            }
        }

        let signed_requests = signed_log.lock().unwrap();
        assert_eq!(signed_requests.len(), 1);
        assert_eq!(
            signed_requests[0].1, None,
            "the bearer must not reach the cross-origin signed URL"
        );
    }

    #[test]
    fn public_update_uses_browser_download_urls_without_authentication() {
        let tmp = tempfile::tempdir().unwrap();
        let target = release_target().unwrap();
        let version = "9.9.9";
        let archive_name = format!("toolx-{version}-{target}.tar.gz");
        let checksum_name = format!("toolx-{version}-{target}.sha256");
        let binary_in_archive =
            format!("toolx-{version}-{target}/{}", executable_file_name("toolx"));
        let tarball = gzip_tar_with_entry(&binary_in_archive, b"public portable payload\n");
        let mut hasher = Sha256::new();
        hasher.update(&tarball);
        let checksum_body =
            format!("{}  {archive_name}\n", hex::encode(hasher.finalize())).into_bytes();

        let (download_base, download_log, download_handle) = spawn_recording_http(vec![
            ("/public/archive", MockReply::Body(tarball)),
            ("/public/checksum", MockReply::Body(checksum_body)),
        ]);
        let api_body = format!(
            r#"[{{"tag_name":"v{version}","assets":[{{"id":2001,"name":"{archive_name}","browser_download_url":"{download_base}/public/archive"}},{{"id":2002,"name":"{checksum_name}","browser_download_url":"{download_base}/public/checksum"}}]}}]"#
        )
        .into_bytes();
        let (api_base, api_log, api_handle) =
            spawn_recording_http(vec![("releases?", MockReply::Body(api_body))]);

        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        config.api_base = Some(api_base);
        config.http_timeout = Some(Duration::from_secs(5));
        let outcome = Updater::new(config)
            .run_update()
            .expect("public run_update succeeds");
        api_handle.join().unwrap();
        download_handle.join().unwrap();

        assert!(outcome.promoted);
        assert_eq!(api_log.lock().unwrap()[0].1, None);
        let requests = download_log.lock().unwrap();
        assert_eq!(requests.len(), 2, "archive + checksum browser URLs");
        assert!(requests[0].0.contains("/public/archive"));
        assert!(requests[1].0.contains("/public/checksum"));
        for (line, authorization, accept) in requests.iter() {
            assert_eq!(
                authorization, &None,
                "public request {line:?} must be anonymous"
            );
            assert_ne!(
                accept.as_deref(),
                Some("application/octet-stream"),
                "public browser URL {line:?} must retain browser-download behavior"
            );
        }
    }

    #[test]
    fn anonymous_asset_404_suggests_private_release_authentication() {
        let tmp = tempfile::tempdir().unwrap();
        let target = release_target().unwrap();
        let version = "9.9.9";
        let archive_name = format!("toolx-{version}-{target}.tar.gz");
        let checksum_name = format!("toolx-{version}-{target}.sha256");
        let (download_base, _download_log, download_handle) = spawn_recording_http(vec![(
            "/private/archive",
            MockReply::Status("404 Not Found", br#"{"message":"Not Found"}"#.to_vec()),
        )]);
        let api_body = format!(
            r#"[{{"tag_name":"v{version}","assets":[{{"id":3001,"name":"{archive_name}","browser_download_url":"{download_base}/private/archive"}},{{"id":3002,"name":"{checksum_name}","browser_download_url":"{download_base}/private/checksum"}}]}}]"#
        )
        .into_bytes();
        let (api_base, _api_log, api_handle) =
            spawn_recording_http(vec![("releases?", MockReply::Body(api_body))]);

        let mut config = UpdaterConfig::new("toolx", "0.1.0", "octocat/example");
        config.install_dir = Some(tmp.path().to_path_buf());
        config.api_base = Some(api_base);
        let error = Updater::new(config)
            .run_update()
            .expect_err("an anonymous private asset must fail");
        api_handle.join().unwrap();
        download_handle.join().unwrap();

        let message = format!("{error:#}");
        assert!(message.contains("HTTP 404"), "unexpected error: {message}");
        assert!(
            message.contains("private release"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("with_github_token"),
            "missing authentication guidance: {message}"
        );
    }
}
