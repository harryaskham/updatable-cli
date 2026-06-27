//! Report self-update status for a tool — the `<tool> status` surface.
//!
//! This is network-free: it only resolves and inspects install paths.
//!
//! Run with:
//!
//! ```sh
//! cargo run --example status
//! ```
use updatable_cli::{Updater, UpdaterConfig};

fn main() -> anyhow::Result<()> {
    // A host CLI passes its own name, running version, and GitHub release slug.
    let config = UpdaterConfig::new(
        "mytool",
        env!("CARGO_PKG_VERSION"),
        "harryaskham/updatable-cli",
    );

    let status = Updater::new(config).current_status()?;

    println!("tool:            {}", status.tool);
    println!("current version: {}", status.current_version);
    println!("install dir:     {}", status.install_dir);
    println!(
        "installed:       {} (exists: {})",
        status.installed_path, status.installed_exists
    );
    println!(
        "staged next:     {} (staged: {})",
        status.next_path, status.next_staged
    );

    Ok(())
}
