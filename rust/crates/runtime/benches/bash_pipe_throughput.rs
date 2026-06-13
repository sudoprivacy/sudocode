//! Regression benchmark for the bash subprocess pipe path.
//!
//! Two cases:
//!
//! 1. `host_pipe_roundtrip` — raw `libc::pipe` write/read pair in the same
//!    thread. The floor: any bash-tool path is upper-bounded by this.
//! 2. `bash_spawn_echo_roundtrip` — full `sh -c cat` spawn, write payload to
//!    stdin, read it back from stdout, wait. This is the actual hot path
//!    `runtime::bash` exercises every time the LLM (or `!`-bash mode) issues
//!    a command.
//!
//! Reference point for DT_PIPE comparison and the rationale for keeping the
//! bash spawn path on host OS pipes live in
//! `docs/plans/active/bash-mode-design.md`.
//!
//! Unix-only: Windows `libc::pipe` has a different signature, and the
//! `sh -c cat` invocation assumes a POSIX shell. Windows port lives where
//! it is needed.

#![cfg(unix)]

use std::hint::black_box;
use std::io::{Read, Write};
use std::process::{Command, Stdio};

use criterion::{criterion_group, criterion_main, Criterion};

/// 80-byte payload — matches the nexus-vfs `bench_pipe_roundtrip` payload
/// length so cross-bench comparisons stay meaningful.
const PAYLOAD: &[u8] =
    b"bench-payload-80-bytes-long-for-a-typical-audit-event-json-body-padding!!!!!!!!";

fn bench_host_pipe_roundtrip(c: &mut Criterion) {
    let (r_fd, w_fd) = unsafe {
        let mut fds = [0i32; 2];
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        (fds[0], fds[1])
    };
    let mut read_buf = [0u8; 128];

    c.bench_function("host_pipe_roundtrip", |b| {
        b.iter(|| unsafe {
            libc::write(w_fd, PAYLOAD.as_ptr() as *const _, PAYLOAD.len());
            libc::read(r_fd, read_buf.as_mut_ptr() as *mut _, PAYLOAD.len());
            black_box(&read_buf);
        });
    });

    unsafe {
        libc::close(r_fd);
        libc::close(w_fd);
    }
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

criterion_group!(benches, bench_host_pipe_roundtrip, bench_bash_spawn_echo_roundtrip);
criterion_main!(benches);
