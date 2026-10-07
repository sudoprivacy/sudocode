//! Shared manual PTY measurements against immutable release binaries.
use super as common;
use pty_expect::PtySession;
use std::time::{Duration, Instant};

pub fn measure_keys(sess: &mut PtySession, phase: &str) {
    let mut expected = String::new();
    let mut samples = Vec::new();
    for _ in 0..60 {
        expected.push('x');
        let started = Instant::now();
        sess.send("x").expect("key");
        while !sess.render(|s| common::input_line_of(&s.raw().contents()).contains(&expected)) {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "input did not echo"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "RENDER_INPUT phase={phase} samples={} median_ms={:.3} p95_ms={:.3}",
        samples.len(),
        samples[30],
        samples[56]
    );
    sess.send("\x15").expect("clear draft");
    common::expect_screen(
        sess,
        |s| common::input_line_of(s).is_empty(),
        common::DEFAULT_TIMEOUT,
        "draft cleared",
    );
}

#[cfg(unix)]
pub fn report_resources(phase: &str) {
    let output = std::process::Command::new("ps")
        .args(["-axo", "ppid=,rss=,time=,comm="])
        .output()
        .expect("process accounting");
    let parent = std::process::id().to_string();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 4 && fields[0] == parent && fields[3].contains("scode") {
            println!(
                "RENDER_RESOURCES phase={phase} rss_kib={} cpu_time={}",
                fields[1], fields[2]
            );
        }
    }
}

#[cfg(not(unix))]
pub fn report_resources(_phase: &str) {}
