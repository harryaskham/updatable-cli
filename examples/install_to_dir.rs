//! Install a release into an explicit target directory instead of over the running binary.
//!
//! This is the path for hosts whose running executable is immutable or package-managed
//! (a `/nix/store/...` path, a Homebrew cellar, a read-only image): the running binary is
//! never touched, and the release lands wherever the caller asked for.
//!
//! The crate deliberately applies **no** policy here — no package-manager detection, no
//! `PATH`-precedence checking, no store protection. It installs where it is told and returns
//! a receipt describing exactly what it wrote, so the caller can enforce its own rules.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example install_to_dir -- /tmp/updatable-cli-demo-bin
//! ```
use std::path::PathBuf;

use updatable_cli::{Updater, UpdaterConfig};

fn main() -> anyhow::Result<()> {
    let target_dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/updatable-cli-demo-bin"));

    let config = UpdaterConfig::new(
        "mytool",
        env!("CARGO_PKG_VERSION"),
        "harryaskham/updatable-cli",
    );

    // Callers own policy. This is the shape of the check a host should make *before*
    // installing, since the crate itself will happily write wherever it is pointed.
    if target_dir.starts_with("/nix/store") {
        anyhow::bail!(
            "refusing to install into {}: the Nix store is immutable and must not be written",
            target_dir.display()
        );
    }

    // Resolves the latest release, verifies its sha256 against the published checksum
    // asset, and installs it as `<target_dir>/mytool`. Unlike `run_update`, this does not
    // require the release to be newer than the running version.
    let receipt = Updater::new(config).install_latest_to_dir(&target_dir)?;

    println!("installed:        {}", receipt.destination);
    println!(
        "from:             {} ({} @ {})",
        receipt.source_asset, receipt.repo_slug, receipt.tag
    );
    println!("version:          {}", receipt.version);
    println!("archive sha256:   {}", receipt.archive_sha256);
    println!("binary sha256:    {}", receipt.binary_sha256);
    println!("replaced existing:{}", receipt.replaced_existing);

    // Set when selection deliberately skipped a newer release that had no asset for this
    // platform. Always surface it: otherwise a fallback looks like an install of the latest.
    if let Some(note) = &receipt.selection_note {
        println!("note: {note}");
    }

    if receipt.replaced_existing {
        println!("note: an existing binary at that path was replaced");
    }

    Ok(())
}
