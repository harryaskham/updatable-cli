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
- Runnable examples in `examples/` (`status`, `update`, `private_repo`).

### Changed

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
