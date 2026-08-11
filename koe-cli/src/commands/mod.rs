//! CLI subcommands backed by APIs delivered in tasks 01–17.
//!
//! `record` / `transcribe` / `completions` are intentionally absent until their
//! remaining dependencies (tasks 20–22, 24, 26, 28–30) land.

mod info;
mod list;
mod permissions;

pub use info::InfoArgs;
pub use list::ListArgs;
pub use permissions::PermissionsArgs;

use crate::MainError;

pub trait Run {
    fn run(self) -> Result<(), MainError>;
}
