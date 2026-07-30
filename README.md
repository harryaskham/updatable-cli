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
- Platform-aware release selection: updates resolve against the newest release that actually
  carries an asset for the running platform, with a bounded lookback (see
  [Platform-incomplete releases](#platform-incomplete-releases)).
- `Updater::install_latest_to_dir` / `Updater::install_release_to_dir` to install a release
  into an explicit target directory instead of over the running binary, returning an
  `InstallReceipt` describing exactly what was written.
- `maybe_apply_staged_update("<tool>")` to swap any staged `<tool>_next` into `<tool>` and
  re-exec on the next Unix launch. On Windows it detects `tool_next.exe`, leaves the locked
  current `tool.exe` untouched, and prints actionable deferred-replacement guidance.
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
// the platform-native next binary, sha256-verifies it, and promotes it when safe.
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

Call `maybe_apply_staged_update("mytool")` early in `main`. Unix hosts swap any staged
`<tool>_next` into place and re-exec before the rest of the program runs. Windows hosts
leave `mytool_next.exe` staged because the running `mytool.exe` may be locked; the hook is
nonfatal and prints the exact follow-up needed instead of risking corruption.

## Examples

Runnable examples live in [`examples/`](examples/):

- `cargo run --example status` — print install/staging status (network-free).
- `cargo run --example update` — run the full check → stage → promote flow.
- `cargo run --example private_repo` — configure updates from a private GitHub repo (token sources).
- `cargo run --example install_to_dir -- <dir>` — install a release into an explicit directory
  instead of over the running binary.

## Installing to an explicit directory

The default flow replaces the running executable in place. That is impossible when the
running binary is immutable or package-managed — a `/nix/store/...-mytool-1.2.3/bin/mytool`
path, a Homebrew cellar, a read-only image — where an in-place write would fail, or worse,
corrupt the closure if it somehow succeeded.

For those hosts, install a resolved release into a directory you choose:

```rust,ignore
use updatable_cli::{Updater, UpdaterConfig};

let updater = Updater::new(UpdaterConfig::new("mytool", env!("CARGO_PKG_VERSION"), "owner/mytool"));

// Resolves the latest release, verifies its sha256, and writes `<dir>/mytool`.
let receipt = updater.install_latest_to_dir("/home/me/.local/bin")?;

println!("installed {} ({}) to {}", receipt.version, receipt.source_asset, receipt.destination);
println!("archive sha256 {} / binary sha256 {}", receipt.archive_sha256, receipt.binary_sha256);
```

Every check the in-place update performs is preserved: version resolution, platform asset
selection, sha256 verification against the published checksum asset, executable permissions,
and an atomic replace performed *within* the target directory, so the destination is never
observed half-written. The target directory is created when missing, and
`install_release_to_dir` takes an already-resolved `LatestReleaseInfo` when you want to pin
the release yourself.

Unlike `run_update`, this does **not** require the release to be newer than the running
version: an explicit destination is being written on purpose, including to seed a directory
that has no copy yet.

`install_latest_to_dir` resolves through `check_latest`, so it inherits the platform-aware
selection described in [Platform-incomplete releases](#platform-incomplete-releases): if the
newest published release has no asset for this platform, the newest one that does is
installed and `receipt.selection_note` explains why. Surface that note when rendering a
receipt, otherwise a deliberate fallback looks like an install of the latest release.

### Scope boundary: callers own policy

This crate does **not** decide where installing is acceptable. It performs no
package-manager detection, no `PATH`-precedence checking, and no store protection. It
installs where it is told and reports exactly what it wrote — resolved source asset,
version, destination path, and artifact hashes — via `InstallReceipt`.

Deciding that `/nix/store` and Homebrew destinations must be refused, that the destination
must precede a package-managed binary on `PATH`, and that shadowing deserves a loud receipt
is the **host's** responsibility, because only the host knows its own installation contract.

## Install path contract

### Linux and macOS

- Default install dir: `$HOME/.local/bin`.
- Staged binary: `$HOME/.local/bin/<tool>_next` (verified via sha256 against the release
  checksum asset).
- Promoted binary: `$HOME/.local/bin/<tool>`.

This is the same shape used by `caco update`. Service modules that prefer the local binary can
simply prepend `$HOME/.local/bin` to `PATH`.

### Windows

- Default install dir: `%LOCALAPPDATA%\\Programs\\<tool>` (falling back through
  `%USERPROFILE%\\AppData\\Local` when needed).
- Current binary: `<tool>.exe`.
- Verified staged binary: `<tool>_next.exe`.

Windows can lock a running `.exe`, so the updater never removes or overwrites an existing
`<tool>.exe` in-process. `run_update` returns `staged=true`, `promoted=false`, and an actionable
`note`; `current_status` continues to expose both paths; and `maybe_apply_staged_update` logs the
same nonfatal guidance at startup. A downstream installer/bootstrapper should wait for all tool
processes to exit, replace `<tool>.exe` with `<tool>_next.exe`, then launch the new executable.
Hosts with their own safe updater may use `stage_next` and perform that final swap themselves.

## Asset naming

By default the crate expects Tendril-style release assets:

```text
<tool>-<version>-<target>.tar.gz
<tool>-<version>-<target>.sha256
```

where `<target>` is `x86_64-linux` / `aarch64-linux` / `aarch64-darwin` /
`x86_64-darwin` / **`x86_64-windows`**. The Windows archive must contain
`<tool>-<version>-x86_64-windows/<tool>.exe`; its checksum asset uses the same canonical suffix.
Custom strategies are supported via `AssetStrategy::Custom`.

## Platform-incomplete releases

Multi-platform releases do not publish atomically. Per-platform release jobs finish at
different times, and some fail or get starved independently, so the newest tag is routinely
missing one platform's build. Resolving updates against "the newest release" alone means a
node can be blocked from updating at all while a perfectly good build sits one tag back —
and it blocks hardest on whichever platform is slowest or flakiest to build.

`check_latest` therefore resolves **the newest release carrying an asset for _this_
platform**:

- It walks the releases feed newest-first (drafts and prereleases ignored, matching GitHub's
  "latest release" semantics) and picks the first release whose `<tool>-<version>-<target>`
  archive **and** checksum are both published.
- The choice is always explicit. `LatestReleaseInfo::selection_note` and
  `UpdateOutcome::note` carry e.g. `v0.0.42 has no x86_64-linux release asset; selecting
  v0.0.41 instead`, and `LatestReleaseInfo::skipped_newer` lists each skipped tag together
  with the exact asset names it was missing — enough to distinguish "still publishing" from
  "this platform's build failed". Silently installing an older version would be its own
  problem.
- The search is bounded by `UpdaterConfig::release_lookback` (default
  `DEFAULT_RELEASE_LOOKBACK` = 10, clamped to `1..=100`):

  ```rust,ignore
  let config = UpdaterConfig::new("mytool", env!("CARGO_PKG_VERSION"), "owner/mytool")
      .with_release_lookback(25);
  ```

  If no release inside that window carries this platform's assets, that is a real failure and
  is reported as one — naming every inspected tag — because the platform's release pipeline
  is broken, which is a different and more serious condition than one late tag.

### Non-semver hosts: a fallback that cannot be proven to be an upgrade

Because selection can deliberately return an **older** release, "the resolved tag differs
from the running version" no longer implies "newer". That distinction is invisible to
semver hosts — ordering is provable, so an older fallback is simply not an update — but it
matters for a host versioned `nightly`, `2026-07-30-a`, or any other date/channel scheme:

```text
current:  nightly-b
releases: nightly-c   newest, no asset for this platform  -> skipped
          nightly-a   has this platform's asset           -> selected
```

Nothing here can establish whether `nightly-a` is newer or older than `nightly-b`, so
installing it could be a silent downgrade. The default is therefore to **decline**:
`newer_than_current` is `false`, `LatestReleaseInfo::downgrade_risk_note` explains why, and
`run_update` reports `not updating: … cannot be proven newer …` instead of quietly staging
it.

A host that knows its own ordering (a date scheme that only moves forward, say) can opt in,
and the install stays loud — the note still says it could be a downgrade:

```rust,ignore
let config = UpdaterConfig::new("mytool", env!("CARGO_PKG_VERSION"), "owner/mytool")
    .with_allow_unprovable_fallback(true);
```

This guard applies **only** to the fallback case. When the newest release does carry this
platform's assets, a non-semver host keeps the original "any different tag is an update"
behaviour, because that candidate really is the newest published release.

## Platform support

Linux, macOS, and x86_64 Windows (`x86_64-pc-windows-msvc`) compile and retain the complete
Updater, status/check/download/checksum, MCP registration, and staging APIs. Unix keeps its
existing chmod + atomic rename + `exec` behavior. Windows has no executable bit and uses the
safe deferred-promotion contract above. Other Windows architectures are rejected until a
canonical release suffix and downstream asset contract are agreed.

## Host overrides

`UpdaterConfig` exposes a few optional overrides (all default to the standard GitHub setup):

- `api_base` — release-metadata host. Defaults to `https://api.github.com`.
- `download_base` — release **download** host. Defaults to `https://github.com`. Point it at
  GitHub Enterprise, a release mirror, or an air-gapped host that serves
  `<base>/<owner>/<repo>/releases/download/<tag>/<asset>`.
- `install_dir` — overrides the default `$HOME/.local/bin`.
- `allow_unprovable_fallback` — install a platform-fallback release even when its version
  cannot be proven newer than the running one. Defaults to `false`; see
  [Non-semver hosts](#non-semver-hosts-a-fallback-that-cannot-be-proven-to-be-an-upgrade).
- `release_lookback` — how many releases to inspect, newest first, when resolving the newest
  release that carries this platform's assets. Defaults to `DEFAULT_RELEASE_LOOKBACK` (10),
  clamped to `1..=100`. See [Platform-incomplete releases](#platform-incomplete-releases).
- `github_token` — sent as `Authorization: Bearer <token>` on release-metadata requests.
  For each authenticated asset/checksum download, the updater uses the asset's numeric ID
  from the release metadata to call GitHub's `/releases/assets/{asset_id}` API with
  `Accept: application/octet-stream`. It follows the resulting signed object-store redirect
  but drops the bearer automatically when the host changes, so the credential is never
  leaked to the CDN. Anonymous public releases continue to use `browser_download_url` (or
  the configured `download_base`).
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
