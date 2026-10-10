//! GUI-independent git access for Tine.

pub mod autostash;
pub mod blame;
pub mod branches;
pub mod diff;
pub mod file_history;
pub mod fuzzy;
pub mod graph;
pub mod integrate;
pub mod log;
pub mod maintenance;
pub mod model;
pub mod ops;
pub mod patch;
pub mod reader;
pub mod refs;
pub mod remote;
pub mod runner;
pub mod search;
pub mod sequencer;
pub mod stash;
pub mod status;

pub use reader::{CliReader, RepoPaths, RepoReader};
pub use runner::{CancelToken, Cancelled, CommandRecord, GitError, GitRunner, Recorder};
