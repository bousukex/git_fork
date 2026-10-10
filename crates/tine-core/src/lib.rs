//! GUI-independent git access for Tine.

pub mod autostash;
pub mod branches;
pub mod diff;
pub mod graph;
pub mod integrate;
pub mod log;
pub mod maintenance;
pub mod model;
pub mod ops;
pub mod patch;
pub mod reader;
pub mod refs;
pub mod runner;
pub mod sequencer;
pub mod stash;
pub mod status;

pub use reader::{CliReader, RepoPaths, RepoReader};
pub use runner::{CancelToken, Cancelled, CommandRecord, GitError, GitRunner, Recorder};
