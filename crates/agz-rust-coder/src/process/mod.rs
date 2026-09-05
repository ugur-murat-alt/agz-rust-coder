//! Shell-free, bounded child-process supervision.
//!
//! The module deliberately owns command construction, output draining, process
//! tree cleanup, and startup recovery together. Callers provide an executable
//! path, an argument vector, a canonical working directory, and an explicit
//! environment; no shell command string is accepted.

mod identity;
mod journal;
mod output;
pub mod root_bound;
pub mod runner;
mod supervisor;

#[allow(unused_imports)]
pub use journal::{
    JournalError, JournalRecord, ProcessGroupIdentity, ProcessJournal, RecoveryDisposition,
    RecoveryEntry, RecoveryReport,
};
pub(crate) use output::sanitize_terminal_text;
#[allow(unused_imports)]
pub use output::{DiagnosticCallback, StdoutCallback};
#[allow(unused_imports)]
pub use supervisor::{
    AsyncProcessRunner, CommandSpec, ProcessError, ProcessRunOptions, ProcessRunResult,
    ProcessSupervisor, ShutdownReport,
};
