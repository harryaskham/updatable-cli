//! Configure self-update against a **private** GitHub release repo.
//!
//! The only difference from the public flow is how the GitHub token is supplied.
//! This example shows the explicit-token source (and documents the alternatives),
//! then reports status — which is network-free, so the example runs cleanly.
//!
//! Run with:
//!
//! ```sh
//! GITHUB_TOKEN=ghp_xxx cargo run --example private_repo
//! ```
use updatable_cli::{Updater, UpdaterConfig};

fn main() -> anyhow::Result<()> {
    // Highest precedence: an explicit token (read from the environment here).
    let config = UpdaterConfig::new("mytool", env!("CARGO_PKG_VERSION"), "octocat/private-tool")
        .with_github_token(std::env::var("GITHUB_TOKEN").unwrap_or_default());

    // Alternatives, same builder chain (see crate docs for details):
    //   .with_gh_account("octocat")   // source a token from a named `gh` account
    //   .with_gh_token_fallback(true) // fall back to the active `gh auth token`
    //
    // Token resolution order: explicit github_token -> gh auth token -> anonymous.

    // A real `<tool> update` would call `.run_update()`, which fetches the private
    // release using the resolved token. We report status here to stay network-free.
    let status = Updater::new(config).current_status()?;
    println!("private-repo updater configured for {}", status.tool);
    println!("installed binary: {}", status.installed_path);

    Ok(())
}
