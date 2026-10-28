//! Sudocode's shared local services. Frontends consume capabilities over stdio;
//! this process owns runtime installation, discovery, and Nexus transport.

mod artifacts;
mod capabilities;
mod diagnostics;
mod local_box;
mod supervisor;

pub use local_box::register_local_box;
pub use supervisor::{
    ensure_local_box, inspect_local_services, inspect_local_services_at, run_daemon, run_stdio,
    start_local_services, stop_local_box,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
