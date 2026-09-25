//! `nexusd-cohost` — the nexus cluster daemon that hosts sudocode agents
//! in-process.
//!
//! Identical to `nexusd-cluster` in every flag and every boot path; the only
//! difference is one service declaration. Where the cluster daemon installs the
//! managed-agent control plane WITHOUT a runtime body — it can register an agent,
//! stamp its procfs subtree and answer `get_session`, but nothing turns
//! `spawn` into a running loop — this binary installs it WITH
//! [`SudoCodeSpawnAdapter`], so a spawn becomes a real sudocode agent on a thread
//! inside this process, reaching the kernel through `KernelFsBackend`.
//!
//! # Why a binary, and why here
//!
//! It cannot live in nexus-vfs: that repo owns the kernel, and sudocode depends on
//! it, so linking sudocode there is a cycle. It should not be a plugin either —
//! the plugin ABI is a C dispatch seam (bytes in, bytes out), and what crosses
//! here is `Arc<Kernel>` in and a `SpawnTask<Kernel>` trait object out, which no
//! C ABI carries. Forcing it across a Rust dylib boundary would reintroduce the
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
        vec![
            a2a::service_decl(ctx.auth_armed),
            // The one line that makes this binary different from
            // `nexusd-cluster`. Built here rather than exported from
            // `managed_agent` because the provider is the caller's: the service
            // knows how to install one, and only an assembly knows which.
            kernel::kernel::ServiceDecl {
                name: "managed_agent".to_string(),
                install: Box::new(|kernel| {
                    managed_agent::install_managed_agent_with_spawn(
                        kernel,
                        Arc::new(SudoCodeSpawnAdapter),
                    )
                }),
            },
        ]
    })
}
