//! The output streams a command writes to.

use std::fmt::Arguments;
use std::io::Write;

/// The stdout and stderr sinks for one command run.
pub struct Io<'a> {
    /// Where normal output goes.
    pub out: &'a mut dyn Write,
    /// Where diagnostics go.
    pub err: &'a mut dyn Write,
}

impl Io<'_> {
    /// Writes one line to stdout.
    pub fn line(&mut self, arguments: Arguments<'_>) {
        let _ = writeln!(self.out, "{arguments}");
    }

    /// Writes one warning line to stderr.
    pub fn warning(&mut self, arguments: Arguments<'_>) {
        let _ = writeln!(self.err, "warning: {arguments}");
    }
}
