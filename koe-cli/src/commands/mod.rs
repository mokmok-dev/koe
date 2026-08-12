//! CLI subcommands.
//!
//! `completions` waits on later tasks.

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
use crate::config::KoeConfig;

pub trait Run {
    fn run(
        self,
        config: &KoeConfig,
    ) -> Result<(), MainError>;
}
