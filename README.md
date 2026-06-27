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

## Examples

Runnable examples live in [`examples/`](examples/):

- `cargo run --example status` — print install/staging status (network-free).
- `cargo run --example update` — run the full check → stage → promote flow.
- `cargo run --example private_repo` — configure updates from a private GitHub repo (token sources).

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

## Platform support

This crate is **Unix-only (Linux and macOS)**. It relies on `std::os::unix` APIs for the
executable bit and `exec`-style re-spawn, and `release_target()` only resolves the
`x86_64`/`aarch64` linux/darwin asset targets above. Windows is not supported.

## Host overrides

`UpdaterConfig` exposes a few optional overrides (all default to the standard GitHub setup):

- `api_base` — release-metadata host. Defaults to `https://api.github.com`.
- `download_base` — release **download** host. Defaults to `https://github.com`. Point it at
  GitHub Enterprise, a release mirror, or an air-gapped host that serves
  `<base>/<owner>/<repo>/releases/download/<tag>/<asset>`.
- `install_dir` — overrides the default `$HOME/.local/bin`.
- `github_token` — sent as `Authorization: Bearer <token>` on both the release-metadata
  request **and** the asset/checksum downloads, so private-repo releases update
  end-to-end. The bearer is dropped automatically when GitHub redirects the asset to a
  signed object-store URL on a different host, so the credential is never leaked to the
  CDN.
- `gh_account` — GitHub username to source a token from the local `gh` CLI when
  `github_token` is unset. The updater runs `gh auth token --user <account>`, which is
  handy for selecting one of several logged-in `gh` accounts (e.g. the one with access to
  a private release repo).
- `gh_token_fallback` — when `true` and `github_token` is unset, fall back to
  `gh auth token` (honoring `gh_account` if set). Defaults to `false`, so public-repo
  callers never shell out to `gh`.
- `http_timeout` — per-request timeout (default 60s).

### Private repositories

The simplest path is to hand the crate a token directly:

```rust,ignore
let config = UpdaterConfig::new("mytool", env!("CARGO_PKG_VERSION"), "owner/private-tool")
    .with_github_token(std::env::var("GITHUB_TOKEN")?);
```

If you would rather reuse whatever the operator's `gh` CLI is already authenticated with —
including picking a specific account when several are logged in — configure an account and
let the crate fetch the token on demand:

```rust,ignore
// Uses `gh auth token --user octocat` only when no explicit github_token is set.
let config = UpdaterConfig::new("mytool", env!("CARGO_PKG_VERSION"), "owner/private-tool")
    .with_gh_account("octocat");

// Or fall back to the active `gh` account without naming one:
let config = UpdaterConfig::new("mytool", env!("CARGO_PKG_VERSION"), "owner/private-tool")
    .with_gh_token_fallback(true);
```

Resolution order is: explicit `github_token`, then `gh auth token [--user <gh_account>]`
(only when `gh_account` is set or `gh_token_fallback` is `true`), then anonymous. The
standalone `gh_auth_token(account)` helper is also exported if you want to resolve a token
yourself.
