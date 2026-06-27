//! Run the high-level self-update flow — the `<tool> update` surface.
//!
//! Checks the latest GitHub release and, when it is newer, stages the new binary
//! (verifying its sha256) and atomically promotes it into place.
//!
//! This performs network I/O, so it is only run on demand:
//!
//! ```sh
//! cargo run --example update
//! ```
use updatable_cli::{Updater, UpdaterConfig};

fn main() -> anyhow::Result<()> {
    let config = UpdaterConfig::new(
        "mytool",
        env!("CARGO_PKG_VERSION"),
        "harryaskham/updatable-cli",
    );

    let outcome = Updater::new(config).run_update()?;

    if outcome.promoted {
        println!(
            "updated {} -> {}",
            outcome.current_version, outcome.latest_version
        );
    } else {
        println!(
            "{}",
            outcome
                .note
                .unwrap_or_else(|| "no update performed".to_string())
        );
    }

    Ok(())
}
