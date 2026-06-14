//! Regression benchmark for the bash subprocess pipe path.
//!
//! Two cases:
//!
//! 1. `host_pipe_roundtrip` — `nix::unistd::pipe` + `write` / `read` pair in
//!    the same thread. The floor: any bash-tool path is upper-bounded by
//!    this. Uses `nix`'s safe wrappers so the workspace `unsafe_code =
//!    "forbid"` lint stays clean (compare to `nexus-vfs/rust/kernel/benches/
//!    syscall_bench.rs`, which uses raw `libc` because the upstream kernel
//!    crate locally permits `unsafe`).
//! 2. `bash_spawn_echo_roundtrip` — full `sh -c cat` spawn, write payload
//!    to stdin, read it back from stdout, wait. This is the actual hot
//!    path `runtime::bash` exercises every time the LLM (or `!`-bash mode)
//!    issues a command.
//!
//! Reference point for DT_PIPE comparison and the rationale for keeping
//! the bash spawn path on host OS pipes live in
//! `docs/plans/active/bash-mode-design.md`.
//!
//! Unix-only: the `sh -c cat` invocation assumes a POSIX shell. Windows
//! port lives where it is needed. On Windows the bench compiles to a
//! stub `main` so `cargo bench --no-run` produces a valid target; no
//! benchmarks run there.

#[cfg(unix)]
mod unix_benches {
    use std::hint::black_box;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::process::{Command, Stdio};

    use criterion::{criterion_group, Criterion};
    use nix::unistd::{pipe, read, write};

    /// 80-byte payload — matches the nexus-vfs `bench_pipe_roundtrip`
    /// payload length so cross-bench comparisons stay meaningful.
    const PAYLOAD: &[u8] =
        b"bench-payload-80-bytes-long-for-a-typical-audit-event-json-body-padding!!!!!!!!";

    fn bench_host_pipe_roundtrip(c: &mut Criterion) {
        let (read_end, write_end) = pipe().expect("create pipe");
        let read_fd = read_end.as_raw_fd();
        let mut read_buf = [0u8; 128];

        c.bench_function("host_pipe_roundtrip", |b| {
            b.iter(|| {
                // nix 0.29 quirk: `write` takes `AsFd`, `read` takes `RawFd`.
                // Keep the `OwnedFd` reference for `write` and the raw fd for
                // `read`. The OwnedFds outlive the closure, so the raw fd
                // stays valid.
                write(&write_end, PAYLOAD).expect("write");
                let n = read(read_fd, &mut read_buf).expect("read");
                black_box(n);
                black_box(&read_buf);
            });
        });

        // `read_end` and `write_end` are `OwnedFd`s — dropping them closes
        // the underlying file descriptors.
    }

    fn bench_bash_spawn_echo_roundtrip(c: &mut Criterion) {
        let mut group = c.benchmark_group("bash_spawn_echo_roundtrip");
        // Spawning a subprocess is millisecond-scale. Keep the sample count
        // low enough that the bench finishes within CI time budgets while
        // still surfacing regressions of 5%+.
        group.sample_size(20);

        group.bench_function("sh_c_cat", |b| {
            b.iter(|| {
                let mut child = Command::new("sh")
                    .arg("-c")
                    .arg("cat")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn sh -c cat");

                child
                    .stdin
                    .as_mut()
                    .expect("child stdin")
                    .write_all(black_box(PAYLOAD))
                    .expect("write");

                // Drop stdin to let cat see EOF and exit, then read its echo.
                drop(child.stdin.take());

                let mut out = Vec::with_capacity(PAYLOAD.len());
                child
                    .stdout
                    .as_mut()
                    .expect("child stdout")
                    .read_to_end(&mut out)
                    .expect("read");

                child.wait().expect("wait");
                black_box(out);
            });
        });

        group.finish();
    }

    criterion_group!(
        pub benches,
        bench_host_pipe_roundtrip,
        bench_bash_spawn_echo_roundtrip
    );
}

#[cfg(unix)]
criterion::criterion_main!(unix_benches::benches);

// Stub `main` for Windows so `cargo bench --no-run` produces a
// valid target. Replaces the previous attempt where the file-level
// `#![cfg(unix)]` swallowed every item below it including the
// `#[cfg(not(unix))] fn main()` — inner attributes win over outer
// attributes, so the stub was unreachable. Moving the Unix code
// into a `#[cfg(unix)] mod unix_benches { ... }` lets this stub
// live at the crate root regardless of platform.
#[cfg(not(unix))]
fn main() {}
