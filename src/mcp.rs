//! `mcp` re-exports the same `register_update_tool` API but in a dedicated module so host
//! crates can write `use updatable_cli::mcp::register_update_tool;` without pulling in unrelated
//! helpers from the crate root.

pub use crate::register_update_tool;
