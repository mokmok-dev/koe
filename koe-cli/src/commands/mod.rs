//! CLI subcommands.
//!
//! `completions` waits on later tasks (28+).

mod apps_table;
mod decode;
mod duration;
mod info;
mod list;
mod permissions;
mod record;
mod transcribe;

pub use info::InfoArgs;
pub use list::ListArgs;
pub use permissions::PermissionsArgs;
pub use record::RecordArgs;
pub use transcribe::TranscribeArgs;

use crate::MainError;

pub trait Run {
    fn run(self) -> Result<(), MainError>;
}
