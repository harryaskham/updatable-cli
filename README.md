# updatable-cli

Reusable self-update plumbing for Rust CLIs that ship as binaries through GitHub releases.

It composes with the [`mcp-cli`](https://github.com/harryaskham/mcp-cli) crate: hosts get both a
synchronous Rust API (`<tool> update`, `<tool> status`, …) and matching MCP tools
(`self_update_status`, `self_update_check`, `self_update_run`) for free.

## What it provides

- A typed `UpdaterConfig` describing the tool name, current version, GitHub `owner/repo`, and
  optional install dir / asset strategy.
- `Updater::current_status`, `Updater::check_latest`, `Updater::stage_next`,
  `Updater::promote_next`, and `Updater::run_update` for the host CLI.
- `maybe_apply_staged_update("<tool>")` to swap any staged `<tool>_next` into `<tool>` and
  re-exec on next launch, mirroring caco's startup hook.
- `register_update_tool` to expose the same surface as MCP tools via `mcp-cli`'s `ToolRouter`.

## Install path contract

- Default install dir: `$HOME/.local/bin`.
- Staged binary: `$HOME/.local/bin/<tool>_next` (verified via sha256 against the release
  checksum asset).
- Promoted binary: `$HOME/.local/bin/<tool>`.

This is the same shape used by `caco update`. Service modules that prefer the local binary can
simply prepend `$HOME/.local/bin` to `PATH`.

## Asset naming

By default the crate expects Tendril-style release assets:

```text
<tool>-<version>-<target>.tar.gz
<tool>-<version>-<target>.sha256
```

where `<target>` is `x86_64-linux` / `aarch64-linux` / `aarch64-darwin` / `x86_64-darwin`.
Custom strategies are supported via `AssetStrategy::Custom`.
