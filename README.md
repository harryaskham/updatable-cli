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

## Usage

Drive the update flow directly from a host CLI:

```rust
use updatable_cli::{Updater, UpdaterConfig};

let config = UpdaterConfig::new("mytool", env!("CARGO_PKG_VERSION"), "owner/mytool");
let updater = Updater::new(config);

// `mytool status`
let status = updater.current_status()?;
println!("installed: {}", status.installed_path);

// `mytool update` — checks the latest release, and if newer stages
// `<tool>_next`, sha256-verifies it, and atomically promotes it.
let outcome = updater.run_update()?;
if outcome.promoted {
    println!("updated to {}", outcome.latest_version);
}
# Ok::<(), anyhow::Error>(())
```

Expose the same surface as MCP tools (`self_update_status`, `self_update_check`,
`self_update_run`) on an existing `mcp-cli` router, where `Ctx` is your host context:

```rust,ignore
use updatable_cli::register_update_tool;

register_update_tool(&mut router, |_ctx: &Ctx| {
    UpdaterConfig::new("mytool", env!("CARGO_PKG_VERSION"), "owner/mytool")
});
```

Call `maybe_apply_staged_update("mytool")` early in `main` to swap any staged
`<tool>_next` into place and re-exec before the rest of the program runs.

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

## Host overrides

`UpdaterConfig` exposes a few optional overrides (all default to the standard GitHub setup):

- `api_base` — release-metadata host. Defaults to `https://api.github.com`.
- `download_base` — release **download** host. Defaults to `https://github.com`. Point it at
  GitHub Enterprise, a release mirror, or an air-gapped host that serves
  `<base>/<owner>/<repo>/releases/download/<tag>/<asset>`.
- `install_dir` — overrides the default `$HOME/.local/bin`.
- `github_token` — sent as a bearer token for higher rate limits or private repos.
- `http_timeout` — per-request timeout (default 60s).
