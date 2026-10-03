//! Minimal process reference for the startup harness, built outside sampling.
use std::io::{self, Write};

fn main() -> io::Result<()> {
    let output = std::env::var("SCODE_STARTUP_REFERENCE_STDOUT").unwrap();
    io::stdout().lock().write_all(output.as_bytes())
}
