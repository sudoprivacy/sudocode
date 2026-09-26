//! `nexusd-cohost` — the nexus cluster daemon that hosts sudocode agents
//! in-process.
//!
//! It is `nexusd-cluster` plus a runtime body, and that is literal: it asks the
//! cluster crate for its own default service set and replaces ONE entry. Where the
//! cluster daemon installs the managed-agent control plane WITHOUT a runtime body
//! — it can register an agent, stamp its procfs subtree and answer `get_session`,
//! but nothing turns `spawn` into a running loop — this binary installs it WITH
//! [`SudoCodeSpawnAdapter`], so a spawn becomes a real sudocode agent on a thread
//! inside this process, reaching the kernel through `KernelFsBackend`.
//!
//! Asking rather than re-listing is the point. A hand-written list is a copy of
//! nexus-vfs's, and a copy drifts the moment a service is added there: this binary
//! would keep booting the old set, silently, with every test green. The first
//! draft of this file had already lost the `driver-ai` `llm_mount` entry that way.
//!
//! # Why a binary, and why here
//!
//! It cannot live in nexus-vfs: that repo owns the kernel, and sudocode depends on
//! it, so linking sudocode there is a cycle. It should not be a plugin either —
//! the plugin ABI is a C dispatch seam (bytes in, bytes out), and what crosses
//! here is `Arc<Kernel>` in and a `SpawnTask<Kernel>` trait object out, which no C
//! ABI carries. Forcing it across a Rust dylib boundary would reintroduce the
//! failure the whole pin discipline exists to prevent: two `Kernel` types in one
//! process.
//!
//! So it is statically linked, and it lives in sudocode because that makes the
//! nexus-vfs rev a single fact. Built from nexus, this crate would pin
//! `sudocode@X` and `nexus-vfs@R` while `X` itself pins `R` — one constraint
//! written in two repositories, which must never disagree and has no compiler to
//! keep it honest until the link step. Here, `engine-host` is a path dependency
//! and `nexus-cluster` comes from the workspace pin: one rev, one place, and the
//! co-host CI gate lives in the same repository as the runtime it guards.

use std::sync::Arc;

use anyhow::Result;
use engine_host::managed_agent::SudoCodeSpawnAdapter;

fn main() -> Result<()> {
    nexus_cluster::run_with_services(|ctx| {
        let mut services = nexus_cluster::default_service_decls(ctx);
        // By NAME, not by position: the default set's order is nexus-vfs's
        // business, and an index would silently pick the wrong service the first
        // time that order changed.
        services.retain(|decl| decl.name != managed_agent::SERVICE_NAME);
        services.push(managed_agent::service_decl_with_spawn(Arc::new(
            SudoCodeSpawnAdapter,
        )));
        services
    })
}
