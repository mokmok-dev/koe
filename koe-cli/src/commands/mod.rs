//! CLI subcommands.
//!
//! `transcribe` / `completions` wait on later tasks (26, 28+).

mod apps_table;
mod info;
mod list;
mod permissions;
mod record;

pub use info::InfoArgs;
pub use list::ListArgs;
pub use permissions::PermissionsArgs;
pub use record::RecordArgs;

use crate::MainError;

pub trait Run {
    fn run(self) -> Result<(), MainError>;
}
