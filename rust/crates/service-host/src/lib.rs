//! Sudocode's shared local services. Frontends consume capabilities over stdio;
//! this process owns runtime installation, discovery, and Nexus transport.

mod artifacts;
mod capabilities;
mod diagnostics;
mod supervisor;

pub use supervisor::{run_daemon, run_stdio};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
