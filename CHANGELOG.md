# Changelog

All notable changes to `updatable-cli` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

`0.1.0` has not yet been tagged or published, so all work to date is listed here
and will move under the first released version when `0.1.0` is cut.

### Added

- Core `Updater` flow over GitHub releases: `current_status`, `check_latest`,
  `stage_next` (downloads + sha256-verifies the release tarball), `promote_next`
  (atomic rename of the staged binary), and the high-level `run_update`.
- `maybe_apply_staged_update("<tool>")` startup hook that promotes a staged
  `<tool>_next` and re-execs, mirroring caco's startup behaviour.
- MCP surface via `register_update_tool`, exposing `self_update_status`,
  `self_update_check`, and `self_update_run` on an `mcp-cli` `ToolRouter`.
- `AssetStrategy::TendrilStyle` default plus `AssetStrategy::Custom` for bespoke
  release-asset naming, and `release_target()` for the `<arch>-<os>` suffix.
- Private-repository support: `with_github_token`, `with_gh_account`, and
  `with_gh_token_fallback`. Authenticated downloads use GitHub's release-asset API IDs
  with the binary media type; the bearer is dropped automatically on a cross-host redirect
  so it never leaks to a signed object-store CDN. Anonymous public assets continue through
  their browser download URLs.
- Host overrides on `UpdaterConfig`: `api_base`, `download_base`, `install_dir`,
  and `http_timeout`.
- Continuous integration workflows for Unix/Nix (rustfmt, clippy `-D warnings`, tests)
  and native `x86_64-pc-windows-msvc` compile/test coverage.
- Windows-safe staging support: canonical `x86_64-windows` assets containing `<tool>.exe`,
  `<tool>.exe` / `<tool>_next.exe` paths, and non-destructive deferred promotion when the
  existing executable may be locked.
- Test coverage: mock-HTTP `check_latest`/`stage_next` happy paths and
  `promote_next` / `maybe_apply_staged_update` integration tests.
- `Updater::install_latest_to_dir` / `Updater::install_release_to_dir`: resolve, download,
  and sha256-verify a release, then install it into an explicit caller-supplied directory
  instead of over the running executable, returning an `InstallReceipt` (resolved source
  asset, tag/version, destination path, verified archive sha256, written-binary sha256, and
  whether an existing file was replaced, and the platform-fallback `selection_note`). Intended
  for hosts whose running binary is immutable or package-managed. The crate applies no
  package-manager, `PATH`-precedence, or store policy — callers own that decision.
- Runnable examples in `examples/` (`status`, `update`, `private_repo`, `install_to_dir`).

### Fixed

- `atomic_write` no longer pre-deletes the Windows destination before persisting the staged
  binary (bd-ab07b8). The comment justifying it claimed Windows `persist` cannot replace an
  existing file, which is not true of the pinned `tempfile`: `NamedTempFile::persist`
  forwards `overwrite = true`, and the Windows implementation sets
  `MOVEFILE_REPLACE_EXISTING`. The pre-delete turned a single atomic replace into a
  delete-then-create with a window where the staged path did not exist. Replacement of an
  existing destination is now pinned by a cross-platform test rather than a Windows-only one.
  `install_binary` still moves an incumbent aside on Windows — that path can target a running
  image, which cannot be deleted (and therefore cannot be replaced in place) but can be
  renamed — and its comment now states that reason rather than the incorrect one.

### Changed

- **Self-update resolves against the newest release that carries assets for the running
  platform, not the newest release overall** (bd-0497f6). Multi-platform releases do not
  publish atomically: per-platform jobs finish at different times and can fail or be starved
  independently, so the newest tag is routinely missing some platform's build. Previously
  `check_latest`/`run_update` read only `/releases/latest` and refused outright
  (`release v0.0.42 has no asset tool-0.0.42-x86_64-linux.tar.gz`), which blocked a node from
  updating at all while a usable build sat one tag back — and it blocked hardest on whichever
  platform is slowest or flakiest to build. `check_latest` now walks the releases feed
  newest-first within a bounded lookback and selects the first release whose archive and
  checksum for this platform are both present. Drafts and prereleases are ignored, as they
  were under the `latest` endpoint.
  - The fallback is never silent: `LatestReleaseInfo::selection_note` and
    `UpdateOutcome::note` report `"v0.0.42 has no x86_64-linux release asset; selecting
    v0.0.41 instead"`, and `LatestReleaseInfo::skipped_newer` lists each skipped tag with the
    exact asset names it was missing, so consumers can tell "still publishing" from "this
    platform's build failed".
  - The search stays bounded by `UpdaterConfig::release_lookback`
    (`with_release_lookback`, default `DEFAULT_RELEASE_LOOKBACK` = 10, clamped to `1..=100`).
    Exhausting the window is a real, loud failure naming every inspected tag: the platform's
    release pipeline is broken, which is a different and more serious condition than one late
    tag.
  - Because selection can now return an *older* release, a non-semver host is no longer
    allowed to treat "different tag" as "newer" in that case (bd-d52a2a). When a fallback is
    selected and neither its version nor `current_version` parses as semver, the ordering is
    unprovable, so the update is declined rather than silently installing a possible
    downgrade: `newer_than_current` is `false` and `LatestReleaseInfo::downgrade_risk_note`
    (also on `InstallReceipt`) explains why. `UpdaterConfig::allow_unprovable_fallback`
    (`with_allow_unprovable_fallback`, default `false`) opts in, and the resulting
    `UpdateOutcome::note` still reports that the install could be a downgrade. Semver hosts
    and the non-fallback non-semver path are unaffected.
- Extended platform support from Unix-only to Linux, macOS, and x86_64 Windows while
  preserving Unix chmod/atomic-rename/re-exec behavior. Windows uses native `.exe` naming,
  leaves a verified update staged when the current executable exists, and reports actionable
  replacement guidance instead of attempting an unsafe in-process overwrite.
- `check_latest` returns actionable errors for the two most common GitHub
  release-polling failures: a clear "no published releases yet" message on HTTP
  404, and a "set a token to raise the limit" hint on HTTP 403/429 rate-limiting.
- Marked the crate `publish = false`: it is consumed git-only across the CLI
  suite (the `mcp-cli` dependency is a git dependency without a crates.io
  version), so it is intentionally not published to crates.io. Removed the
  crates.io-only `keywords`/`categories` metadata accordingly.

[Unreleased]: https://github.com/harryaskham/updatable-cli/commits/main
